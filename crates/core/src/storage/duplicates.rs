//! Duplicate finder: files with identical content among the large files of a finished space
//! scan. Read-only; it writes no audit rows and never changes a file.
//!
//! Files are compared by size first, then by a SHA-256 of their size, first 64 KiB and last
//! 64 KiB, and only then by a SHA-256 of their whole content, read in 1 MiB chunks. Online-only
//! OneDrive files are never opened for reading, so nothing is downloaded; a file that changed
//! since the scan is left out.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::files::{
    file_id_hash, is_online_only, open_exact, standard_info, tag_info, ticks_rfc3339,
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_LOCK_VIOLATION, ERROR_PATH_NOT_FOUND,
    ERROR_SHARING_VIOLATION, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FLAG_OPEN_REPARSE_POINT,
    FLAG_SEQUENTIAL_SCAN, SHARE_ALL, SYNCHRONIZE,
};
use super::hash::{hex, Sha256, DIGEST_LEN};
use super::scan::ScanResult;
use super::{detail, grouped, size_text, AwakeGuard, KIND_DUPLICATES, KIND_SCAN};
use crate::jobs::{HostJobId, HostJobSnapshot, JobContext, JobHost, JobSpec, JobState, WorkEnd};
use crate::win::error_mode::ErrorModeGuard;
use crate::win::storage::MediaKind;
use crate::{Error, Result};

/// Minimum sizes the UI offers.
pub const MIN_SIZE_CHOICES: [u64; 3] = [1 << 20, 10 << 20, 100 << 20];
/// Smallest minimum size: the scan keeps files individually from 1 MiB.
pub const MIN_SIZE: u64 = 1 << 20;
/// Bytes read from the start and from the end of a file to sample it.
pub const SAMPLE: u64 = 64 << 10;
/// Read size while hashing a whole file.
pub const CHUNK: usize = 1 << 20;
/// Groups and files per group a result lists.
pub const MAX_GROUPS: usize = 1000;
pub const MAX_FILES_PER_GROUP: usize = 100;

pub const RESULTS_GONE_TEXT: &str = "The scan results are no longer available; scan again.";
pub const SCAN_RUNNING_TEXT: &str = "Wait for the scan to finish.";
pub const STOPPED_EARLY_NOTE: &str =
    "The scan was stopped early, so only the files it reached are compared.";
pub const FILE_LIMIT_NOTE: &str =
    "The scan reached its limit of large files, so files past it aren't compared.";
pub const STOPPED_TEXT: &str = "Stopped: only the files compared so far are listed.";

/// Least time between two progress reports.
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// Which scan to search and from what size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicatesRequest {
    pub scan_job: HostJobId,
    pub min_size: u64,
}

impl DuplicatesRequest {
    /// `min_size` must be at least 1 MiB.
    pub fn new(scan_job: HostJobId, min_size: u64) -> Result<DuplicatesRequest> {
        if min_size < MIN_SIZE {
            return Err(Error::Other(format!(
                "the minimum size must be at least 1 MB, got {min_size} bytes"
            )));
        }
        Ok(DuplicatesRequest { scan_job, min_size })
    }
}

/// What a duplicate search would compare, and why it cannot start now, if it cannot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DuplicatesPlan {
    pub scan_job: HostJobId,
    pub root: String,
    pub min_size: u64,
    /// Files that share their size with another file.
    pub candidates: u64,
    /// Distinct sizes among them.
    pub size_groups: u64,
    /// Bytes read if every candidate were read in full.
    pub bytes_to_read_max: u64,
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

/// Live state of a running search (the `duplicates` block of the job's detail).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DuplicatesProgress {
    pub files_total: u64,
    pub files_done: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub groups_found: u64,
    pub current: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateFile {
    pub path: String,
    /// Last write, RFC 3339.
    pub modified: Option<String>,
    pub allocated: u64,
}

/// Files with identical content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub size: u64,
    pub count: u64,
    /// (count - 1) × size: freed by keeping one copy.
    pub wasted: u64,
    /// First 16 hex digits of the content's SHA-256.
    pub hash: String,
    /// At most [`MAX_FILES_PER_GROUP`], by path.
    pub files: Vec<DuplicateFile>,
    pub more_files: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicatesResult {
    pub scan_job: HostJobId,
    pub root: String,
    pub min_size: u64,
    pub completed: bool,
    /// The [`MAX_GROUPS`] groups that free the most, largest first.
    pub groups: Vec<DuplicateGroup>,
    /// Groups found, including those past the list.
    pub group_count: u64,
    pub wasted_bytes: u64,
    pub files_compared: u64,
    pub bytes_read: u64,
    pub skipped_in_use: u64,
    pub skipped_unreadable: u64,
    pub skipped_changed: u64,
    pub skipped_online_only: u64,
}

// ───────────────────────────── Reading content ─────────────────────────────

/// A readable, seekable file.
pub(crate) trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

/// Why a file was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skip {
    InUse,
    Unreadable(String),
    /// It is no longer the file the scan saw (size, id or kind changed).
    Changed,
    OnlineOnly,
    Gone,
}

/// What the scan saw of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Expect {
    pub id_hash: Option<u64>,
    pub size: u64,
}

/// Opens files for reading their content.
pub(crate) trait ContentSource: Send + Sync {
    fn open(&self, path: &Path, expect: &Expect) -> std::result::Result<Box<dyn ReadSeek>, Skip>;
}

/// This PC's files. A probe through a handle that does not follow reparse points comes first,
/// so an online-only file is never opened for reading (which would download it). Paths come
/// from the scan's listed names and are opened exactly as listed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveContent;

impl ContentSource for LiveContent {
    fn open(&self, path: &Path, expect: &Expect) -> std::result::Result<Box<dyn ReadSeek>, Skip> {
        let probe = open_exact(
            path,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_OPEN_REPARSE_POINT,
        )
        .map_err(skip_of)?;
        check(&probe, expect)?;
        drop(probe);
        let data = open_exact(
            path,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_SEQUENTIAL_SCAN,
        )
        .map_err(skip_of)?;
        check(&data, expect)?;
        Ok(Box::new(data))
    }
}

/// The open entry is still the plain, stored file the scan saw.
fn check(file: &File, expect: &Expect) -> std::result::Result<(), Skip> {
    let tag = tag_info(file).map_err(skip_of)?;
    if is_online_only(tag.attrs) {
        return Err(Skip::OnlineOnly);
    }
    if tag.is_dir() || tag.is_link() {
        return Err(Skip::Changed);
    }
    if let Some(expected) = expect.id_hash {
        if file_id_hash(file).is_ok_and(|found| found != expected) {
            return Err(Skip::Changed);
        }
    }
    let info = standard_info(file).map_err(skip_of)?;
    if info.size != expect.size {
        return Err(Skip::Changed);
    }
    Ok(())
}

fn skip_of(e: io::Error) -> Skip {
    match e.raw_os_error() {
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => Skip::InUse,
        Some(ERROR_ACCESS_DENIED) => Skip::Unreadable("access denied".to_string()),
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => Skip::Gone,
        _ if e.kind() == io::ErrorKind::NotFound => Skip::Gone,
        _ => Skip::Unreadable(e.to_string()),
    }
}

// ───────────────────────────── Comparing ─────────────────────────────

/// A file to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    pub allocated: u64,
    /// Last write, FILETIME ticks.
    pub modified: i64,
    pub id_hash: Option<u64>,
}

impl Candidate {
    fn expect(&self) -> Expect {
        Expect {
            id_hash: self.id_hash,
            size: self.size,
        }
    }
}

/// The large files of `scan` of at least `min_size` that share their size with another,
/// ordered by size.
pub(crate) fn candidates_of(scan: &ScanResult, min_size: u64) -> Vec<Candidate> {
    scan.candidates
        .iter()
        .map(|&i| &scan.big[i as usize])
        .filter(|f| f.logical >= min_size)
        .map(|f| Candidate {
            path: scan.file_path(f),
            size: f.logical,
            allocated: f.allocated,
            modified: f.modified,
            id_hash: f.id_hash,
        })
        .collect()
}

/// How reading one file ended.
enum Outcome {
    Done {
        digest: [u8; DIGEST_LEN],
        bytes: u64,
    },
    Skipped(Skip),
    Stopped,
}

/// What a search found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// Content digest and the indices of the files that have it (two or more).
    pub groups: Vec<([u8; DIGEST_LEN], Vec<usize>)>,
    pub completed: bool,
    pub files_compared: u64,
    pub bytes_read: u64,
    pub skipped_in_use: u64,
    pub skipped_unreadable: u64,
    pub skipped_changed: u64,
    pub skipped_online_only: u64,
}

impl Found {
    fn count(&mut self, skip: &Skip) {
        match skip {
            Skip::InUse => self.skipped_in_use += 1,
            Skip::Unreadable(_) => self.skipped_unreadable += 1,
            Skip::Changed | Skip::Gone => self.skipped_changed += 1,
            Skip::OnlineOnly => self.skipped_online_only += 1,
        }
    }
}

/// Receives the phase and progress of a search.
pub(crate) type Report<'a> = &'a (dyn Fn(&'static str, &DuplicatesProgress) + Sync);

/// Reports progress from the reading threads, at most every 100 ms.
struct Meter<'a> {
    phase: &'static str,
    report: Report<'a>,
    state: Mutex<DuplicatesProgress>,
    last: Mutex<Option<Instant>>,
}

impl<'a> Meter<'a> {
    fn new(
        phase: &'static str,
        files_total: u64,
        bytes_total: u64,
        groups_found: u64,
        report: Report<'a>,
    ) -> Meter<'a> {
        let meter = Meter {
            phase,
            report,
            state: Mutex::new(DuplicatesProgress {
                files_total,
                bytes_total,
                groups_found,
                ..DuplicatesProgress::default()
            }),
            last: Mutex::new(None),
        };
        meter.publish(true);
        meter
    }

    fn bytes(&self, bytes: u64, current: &Path) {
        {
            let mut state = self.state.lock();
            state.bytes_done += bytes;
            state.current = current.display().to_string();
        }
        self.publish(false);
    }

    fn file_done(&self, found_group: bool) {
        {
            let mut state = self.state.lock();
            state.files_done += 1;
            if found_group {
                state.groups_found += 1;
            }
        }
        self.publish(false);
    }

    fn publish(&self, force: bool) {
        {
            let mut last = self.last.lock();
            if !force && last.is_some_and(|at| at.elapsed() < REPORT_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let progress = self.state.lock().clone();
        (self.report)(self.phase, &progress);
    }
}

/// Runs `work` on items `0..count` with up to `threads` threads, each with its own 1 MiB
/// buffer; items not started before a stop stay `None`.
fn parallel<T: Send>(
    count: usize,
    threads: u32,
    cancel: &AtomicBool,
    work: &(dyn Fn(usize, &mut [u8]) -> T + Sync),
) -> Vec<Option<T>> {
    let next = AtomicUsize::new(0);
    let workers = (threads.max(1) as usize).min(count.max(1));
    let parts: Vec<Vec<(usize, T)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut buf = vec![0u8; CHUNK];
                    let mut done = Vec::new();
                    while !cancel.load(Ordering::SeqCst) {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= count {
                            break;
                        }
                        done.push((i, work(i, &mut buf)));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    let mut out: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for (i, value) in parts.into_iter().flatten() {
        out[i] = Some(value);
    }
    out
}

/// Fills `buf` from `reader`; fewer bytes only at the end of the file.
fn fill(reader: &mut dyn ReadSeek, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// SHA-256 of the size, the first and the last 64 KiB (the whole file up to 128 KiB).
fn sample_file(
    content: &dyn ContentSource,
    file: &Candidate,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> Outcome {
    if cancel.load(Ordering::SeqCst) {
        return Outcome::Stopped;
    }
    let mut reader = match content.open(&file.path, &file.expect()) {
        Ok(reader) => reader,
        Err(skip) => return Outcome::Skipped(skip),
    };
    let mut hash = match Sha256::new() {
        Ok(hash) => hash,
        Err(e) => return Outcome::Skipped(Skip::Unreadable(e.to_string())),
    };
    if let Err(e) = hash.update(&file.size.to_le_bytes()) {
        return Outcome::Skipped(Skip::Unreadable(e.to_string()));
    }
    let parts: &[(u64, usize)] = if file.size <= 2 * SAMPLE {
        &[(0, file.size as usize)]
    } else {
        &[(0, SAMPLE as usize), (file.size - SAMPLE, SAMPLE as usize)]
    };
    let mut bytes = 0u64;
    for &(offset, len) in parts {
        if offset > 0 {
            if let Err(e) = reader.seek(SeekFrom::Start(offset)) {
                return Outcome::Skipped(skip_of(e));
            }
        }
        let got = match fill(reader.as_mut(), &mut buf[..len]) {
            Ok(got) => got,
            Err(e) => return Outcome::Skipped(skip_of(e)),
        };
        if got != len {
            return Outcome::Skipped(Skip::Changed);
        }
        if let Err(e) = hash.update(&buf[..len]) {
            return Outcome::Skipped(Skip::Unreadable(e.to_string()));
        }
        bytes += len as u64;
    }
    if file.size <= 2 * SAMPLE {
        // The whole file was read: anything past its size means it grew.
        match reader.read(&mut buf[..1]) {
            Ok(0) => {}
            Ok(_) => return Outcome::Skipped(Skip::Changed),
            Err(e) => return Outcome::Skipped(skip_of(e)),
        }
    }
    match hash.finish() {
        Ok(digest) => Outcome::Done { digest, bytes },
        Err(e) => Outcome::Skipped(Skip::Unreadable(e.to_string())),
    }
}

/// SHA-256 of the whole content, read in 1 MiB chunks; the stop flag is read between chunks.
fn hash_file(
    content: &dyn ContentSource,
    file: &Candidate,
    buf: &mut [u8],
    cancel: &AtomicBool,
    on_bytes: &dyn Fn(u64),
) -> Outcome {
    let mut reader = match content.open(&file.path, &file.expect()) {
        Ok(reader) => reader,
        Err(skip) => return Outcome::Skipped(skip),
    };
    let mut hash = match Sha256::new() {
        Ok(hash) => hash,
        Err(e) => return Outcome::Skipped(Skip::Unreadable(e.to_string())),
    };
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Outcome::Stopped;
        }
        let n = match fill(reader.as_mut(), buf) {
            Ok(n) => n,
            Err(e) => return Outcome::Skipped(skip_of(e)),
        };
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > file.size {
            return Outcome::Skipped(Skip::Changed);
        }
        if let Err(e) = hash.update(&buf[..n]) {
            return Outcome::Skipped(Skip::Unreadable(e.to_string()));
        }
        on_bytes(n as u64);
        if n < buf.len() {
            break;
        }
    }
    if total != file.size {
        return Outcome::Skipped(Skip::Changed);
    }
    match hash.finish() {
        Ok(digest) => Outcome::Done {
            digest,
            bytes: total,
        },
        Err(e) => Outcome::Skipped(Skip::Unreadable(e.to_string())),
    }
}

/// Indices of `files` grouped by `key`, keeping groups of two or more, in a stable order.
fn groups_by<K: Ord>(indices: &[usize], key: impl Fn(usize) -> Option<K>) -> Vec<(K, Vec<usize>)> {
    let mut map: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for &i in indices {
        if let Some(k) = key(i) {
            map.entry(k).or_default().push(i);
        }
    }
    map.into_iter().filter(|(_, v)| v.len() >= 2).collect()
}

/// Compares `files`: by size, then by a sample, then by their whole content. Stops early (with
/// `completed` false) when `cancel` is set; groups found by then are kept.
pub(crate) fn find(
    files: &[Candidate],
    content: &dyn ContentSource,
    threads: u32,
    cancel: &AtomicBool,
    report: Report<'_>,
) -> Found {
    let mut found = Found::default();
    let all: Vec<usize> = (0..files.len()).collect();
    let by_size = groups_by(&all, |i| Some(files[i].size));
    let sampled: Vec<usize> = by_size.into_iter().flat_map(|(_, v)| v).collect();

    // Sampling.
    let meter = Meter::new(
        "sampling",
        sampled.len() as u64,
        sampled.iter().map(|&i| files[i].size.min(2 * SAMPLE)).sum(),
        0,
        report,
    );
    let outcomes = parallel(
        sampled.len(),
        threads,
        cancel,
        &|k: usize, buf: &mut [u8]| {
            let file = &files[sampled[k]];
            let outcome = sample_file(content, file, buf, cancel);
            if let Outcome::Done { bytes, .. } = &outcome {
                meter.bytes(*bytes, &file.path);
            }
            meter.file_done(false);
            outcome
        },
    );
    let mut samples: HashMap<usize, [u8; DIGEST_LEN]> = HashMap::new();
    for (k, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Some(Outcome::Done { digest, bytes }) => {
                found.files_compared += 1;
                found.bytes_read += bytes;
                samples.insert(sampled[k], digest);
            }
            Some(Outcome::Skipped(skip)) => found.count(&skip),
            Some(Outcome::Stopped) | None => {}
        }
    }
    let sample_groups = groups_by(&sampled, |i| samples.get(&i).map(|d| (files[i].size, *d)));
    let mut to_hash: Vec<usize> = Vec::new();
    for ((size, digest), members) in sample_groups {
        if size <= 2 * SAMPLE {
            // The sample covered the whole file.
            found.groups.push((digest, members));
        } else {
            to_hash.extend(members);
        }
    }
    if cancel.load(Ordering::SeqCst) {
        found.completed = false;
        return found;
    }

    // Hashing.
    let groups_so_far = found.groups.len() as u64;
    let meter = Meter::new(
        "hashing",
        to_hash.len() as u64,
        to_hash.iter().map(|&i| files[i].size).sum(),
        groups_so_far,
        report,
    );
    let seen: Mutex<HashMap<(u64, [u8; DIGEST_LEN]), u32>> = Mutex::new(HashMap::new());
    let outcomes = parallel(
        to_hash.len(),
        threads,
        cancel,
        &|k: usize, buf: &mut [u8]| {
            let file = &files[to_hash[k]];
            let outcome = hash_file(content, file, buf, cancel, &|n: u64| {
                meter.bytes(n, &file.path)
            });
            let new_group = match &outcome {
                Outcome::Done { digest, .. } => {
                    let mut seen = seen.lock();
                    let count = seen.entry((file.size, *digest)).or_insert(0);
                    *count += 1;
                    *count == 2
                }
                _ => false,
            };
            meter.file_done(new_group);
            outcome
        },
    );
    let mut digests: HashMap<usize, [u8; DIGEST_LEN]> = HashMap::new();
    for (k, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Some(Outcome::Done { digest, bytes }) => {
                found.bytes_read += bytes;
                digests.insert(to_hash[k], digest);
            }
            Some(Outcome::Skipped(skip)) => found.count(&skip),
            Some(Outcome::Stopped) | None => {}
        }
    }
    for ((_, digest), members) in
        groups_by(&to_hash, |i| digests.get(&i).map(|d| (files[i].size, *d)))
    {
        found.groups.push((digest, members));
    }
    meter.publish(true);
    found.completed = !cancel.load(Ordering::SeqCst);
    found
}

/// The listed result of a search: groups by the space they free, capped.
pub(crate) fn build_result(
    scan_job: HostJobId,
    root: &str,
    min_size: u64,
    files: &[Candidate],
    found: Found,
) -> DuplicatesResult {
    let mut groups: Vec<DuplicateGroup> = found
        .groups
        .iter()
        .map(|(digest, members)| {
            let size = files[members[0]].size;
            let count = members.len() as u64;
            let mut listed: Vec<&Candidate> = members.iter().map(|&i| &files[i]).collect();
            listed.sort_by(|a, b| a.path.cmp(&b.path));
            let more_files = listed.len().saturating_sub(MAX_FILES_PER_GROUP) as u64;
            listed.truncate(MAX_FILES_PER_GROUP);
            DuplicateGroup {
                size,
                count,
                wasted: (count - 1) * size,
                hash: hex(&digest[..8]),
                files: listed
                    .into_iter()
                    .map(|f| DuplicateFile {
                        path: f.path.display().to_string(),
                        modified: ticks_rfc3339(f.modified),
                        allocated: f.allocated,
                    })
                    .collect(),
                more_files,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.wasted
            .cmp(&a.wasted)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.hash.cmp(&b.hash))
    });
    let group_count = groups.len() as u64;
    let wasted_bytes = groups.iter().map(|g| g.wasted).sum();
    groups.truncate(MAX_GROUPS);
    DuplicatesResult {
        scan_job,
        root: root.to_string(),
        min_size,
        completed: found.completed,
        groups,
        group_count,
        wasted_bytes,
        files_compared: found.files_compared,
        bytes_read: found.bytes_read,
        skipped_in_use: found.skipped_in_use,
        skipped_unreadable: found.skipped_unreadable,
        skipped_changed: found.skipped_changed,
        skipped_online_only: found.skipped_online_only,
    }
}

// ───────────────────────────── Plan and start ─────────────────────────────

/// Title of a search in the scan of `root`.
pub fn duplicates_title(root: &str) -> String {
    format!("Duplicate search in {root}")
}

fn plan(host: &JobHost, request: &DuplicatesRequest) -> (DuplicatesPlan, Option<Arc<ScanResult>>) {
    let mut plan = DuplicatesPlan {
        scan_job: request.scan_job,
        root: String::new(),
        min_size: request.min_size,
        candidates: 0,
        size_groups: 0,
        bytes_to_read_max: 0,
        blocked_reason: None,
        notes: Vec::new(),
    };
    let snapshot = host.snapshot(request.scan_job);
    if snapshot
        .as_ref()
        .is_some_and(|j| j.kind == KIND_SCAN && j.state == JobState::Running)
    {
        plan.blocked_reason = Some(SCAN_RUNNING_TEXT.to_string());
        return (plan, None);
    }
    let scan = snapshot
        .filter(|j| j.kind == KIND_SCAN)
        .and_then(|_| host.typed::<ScanResult>(request.scan_job));
    let Some(scan) = scan else {
        plan.blocked_reason = Some(RESULTS_GONE_TEXT.to_string());
        return (plan, None);
    };
    plan.root = scan.summary().root.clone();
    let files = candidates_of(&scan, request.min_size);
    plan.candidates = files.len() as u64;
    plan.size_groups = {
        let mut sizes: Vec<u64> = files.iter().map(|f| f.size).collect();
        sizes.dedup();
        sizes.len() as u64
    };
    plan.bytes_to_read_max = files.iter().map(|f| f.size).sum();
    if let Some(job) = host.running() {
        plan.blocked_reason = Some(format!(
            "{} is running; wait for it to finish or stop it.",
            job.title
        ));
    }
    if !scan.summary().completed {
        plan.notes.push(STOPPED_EARLY_NOTE.to_string());
    }
    if scan.summary().big_file_limit_reached {
        plan.notes.push(FILE_LIMIT_NOTE.to_string());
    }
    (plan, Some(scan))
}

/// Plans a search in the finished scan `request.scan_job` of `host` and, unless `dry_run`,
/// starts it as a job of `host`. Read-only: no journal, no rows. A blocked plan is refused.
pub fn plan_or_start_duplicates(
    host: &JobHost,
    request: &DuplicatesRequest,
    dry_run: bool,
) -> Result<(DuplicatesPlan, Option<HostJobSnapshot>)> {
    plan_or_start_duplicates_with(host, request, dry_run, Arc::new(LiveContent))
}

pub(crate) fn plan_or_start_duplicates_with(
    host: &JobHost,
    request: &DuplicatesRequest,
    dry_run: bool,
    content: Arc<dyn ContentSource>,
) -> Result<(DuplicatesPlan, Option<HostJobSnapshot>)> {
    let (plan, scan) = plan(host, request);
    if dry_run {
        return Ok((plan, None));
    }
    if let Some(reason) = &plan.blocked_reason {
        return Err(Error::Other(reason.clone()));
    }
    let Some(scan) = scan else {
        return Err(Error::Other(RESULTS_GONE_TEXT.to_string()));
    };
    let spec = JobSpec {
        kind: KIND_DUPLICATES,
        title: duplicates_title(&plan.root),
        command_line: format!(
            "optctl storage duplicates \"{}\" --min-size {}M",
            plan.root,
            request.min_size >> 20
        ),
        cancellable: true,
        audit: None,
        needs_journal: false,
        log: false,
    };
    let work = DuplicatesWork {
        scan,
        scan_job: request.scan_job,
        min_size: request.min_size,
        content,
    };
    let job = host.start(
        spec,
        || {
            Err(Error::Other(
                "a duplicate search keeps no journal".to_string(),
            ))
        },
        Box::new(move |ctx| work.run(ctx)),
    )?;
    Ok((plan, Some(job)))
}

struct DuplicatesWork {
    /// The scan's tree, kept alive by this job even when the host drops the scan's result.
    scan: Arc<ScanResult>,
    scan_job: HostJobId,
    min_size: u64,
    content: Arc<dyn ContentSource>,
}

impl fmt::Debug for DuplicatesWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DuplicatesWork")
            .field("scan_job", &self.scan_job)
            .field("min_size", &self.min_size)
            .finish_non_exhaustive()
    }
}

/// The progress line of a search.
fn progress_line(phase: &str, p: &DuplicatesProgress) -> String {
    match phase {
        "sampling" => format!(
            "Comparing the start and end of {} files that share a size…",
            grouped(p.files_total)
        ),
        _ => format!(
            "Comparing {} files in full  ·  {} of {} read",
            grouped(p.files_total),
            size_text(p.bytes_done),
            size_text(p.bytes_total)
        ),
    }
}

impl DuplicatesWork {
    fn run(self, ctx: &JobContext) -> WorkEnd {
        let _awake = AwakeGuard::new();
        let _mode = ErrorModeGuard::new();
        let files = candidates_of(&self.scan, self.min_size);
        let threads = if self.scan.media == MediaKind::Hdd {
            1
        } else {
            3
        };
        let cancel = ctx.cancel_flag();
        // The newest progress block; the finished job keeps showing it.
        let last_block: Mutex<Option<serde_json::Value>> = Mutex::new(None);
        let report = |phase: &'static str, p: &DuplicatesProgress| {
            // The byte count restarts with each phase.
            let percent = if p.bytes_total == 0 {
                0.0
            } else {
                (p.bytes_done as f64 / p.bytes_total as f64 * 100.0).min(100.0)
            };
            ctx.progress(Some(percent), Some(&progress_line(phase, p)));
            let block = serde_json::to_value(p).ok();
            last_block.lock().clone_from(&block);
            ctx.set_detail(detail(phase, None, None, block));
        };
        let found = find(&files, self.content.as_ref(), threads, &cancel, &report);
        let root = self.scan.summary().root.clone();
        let result = build_result(self.scan_job, &root, self.min_size, &files, found);
        let mut published = serde_json::to_value(&result).unwrap_or_else(|_| json!({}));
        if let Some(map) = published.as_object_mut() {
            map.insert("kind".to_string(), json!("duplicates"));
            map.insert("job_id".to_string(), json!(ctx.id().0));
        }
        ctx.publish(published);
        ctx.set_detail(detail("done", None, None, last_block.into_inner()));
        let summary = if !result.completed {
            STOPPED_TEXT.to_string()
        } else if result.group_count == 0 {
            format!(
                "No duplicate files of {} or more.",
                size_text(self.min_size)
            )
        } else {
            format!(
                "{} group{} of identical files  ·  {} would be freed by keeping one copy of each",
                grouped(result.group_count),
                if result.group_count == 1 { "" } else { "s" },
                size_text(result.wasted_bytes)
            )
        };
        WorkEnd {
            state: if result.completed {
                JobState::Succeeded
            } else {
                JobState::Cancelled
            },
            summary,
            hint: None,
            restart_required: false,
            audit_detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::files::verbatim;
    use crate::storage::scan::{plan_or_start_scan, ScanRequest};
    use crate::storage::speed::tests::test_host;
    use std::io::Cursor;
    use std::os::windows::fs::OpenOptionsExt;

    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;

    /// Files kept in memory; records every open.
    #[derive(Default)]
    struct MemContent {
        files: HashMap<String, std::result::Result<Vec<u8>, Skip>>,
        opened: Mutex<Vec<String>>,
        /// Set once this many opens happened.
        stop: Option<(usize, Arc<AtomicBool>)>,
    }

    impl MemContent {
        fn add(&mut self, path: &str, data: Vec<u8>) -> Candidate {
            let size = data.len() as u64;
            self.files.insert(path.to_string(), Ok(data));
            candidate(path, size)
        }

        fn fail(&mut self, path: &str, size: u64, skip: Skip) -> Candidate {
            self.files.insert(path.to_string(), Err(skip));
            candidate(path, size)
        }

        fn opens(&self, path: &str) -> usize {
            self.opened.lock().iter().filter(|p| *p == path).count()
        }
    }

    impl ContentSource for MemContent {
        fn open(
            &self,
            path: &Path,
            expect: &Expect,
        ) -> std::result::Result<Box<dyn ReadSeek>, Skip> {
            let key = path.display().to_string();
            let count = {
                let mut opened = self.opened.lock();
                opened.push(key.clone());
                opened.len()
            };
            if let Some((after, flag)) = &self.stop {
                if count >= *after {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            match self.files.get(&key) {
                None => Err(Skip::Gone),
                Some(Err(skip)) => Err(skip.clone()),
                Some(Ok(data)) if data.len() as u64 != expect.size => Err(Skip::Changed),
                Some(Ok(data)) => Ok(Box::new(Cursor::new(data.clone()))),
            }
        }
    }

    fn candidate(path: &str, size: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            size,
            allocated: size.div_ceil(4096) * 4096,
            modified: 133_500_000_000_000_000,
            id_hash: None,
        }
    }

    fn pattern(len: u64, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ seed).collect()
    }

    fn quiet(_: &'static str, _: &DuplicatesProgress) {}

    fn run(files: &[Candidate], content: &MemContent, threads: u32) -> Found {
        find(files, content, threads, &AtomicBool::new(false), &quiet)
    }

    fn members(files: &[Candidate], found: &Found) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = found
            .groups
            .iter()
            .map(|(_, m)| {
                let mut names: Vec<String> = m
                    .iter()
                    .map(|&i| files[i].path.display().to_string())
                    .collect();
                names.sort();
                names
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn requests_need_a_minimum_of_one_megabyte() {
        assert!(DuplicatesRequest::new(HostJobId(1), MIB).is_ok());
        assert!(DuplicatesRequest::new(HostJobId(1), MIB - 1).is_err());
        assert_eq!(MIN_SIZE_CHOICES[0], MIN_SIZE);
    }

    #[test]
    fn a_different_head_splits_at_sampling_and_a_different_tail_at_hashing() {
        let mut content = MemContent::default();
        let base = pattern(2 * MIB, 0);
        let mut head = base.clone();
        head[10] ^= 0xFF;
        let mut middle = base.clone();
        middle[MIB as usize] ^= 0xFF;
        let files = vec![
            content.add(r"C:\a.bin", base.clone()),
            content.add(r"C:\b.bin", base.clone()),
            content.add(r"C:\head.bin", head),
            content.add(r"C:\middle.bin", middle),
        ];
        let found = run(&files, &content, 2);
        assert!(found.completed);
        assert_eq!(members(&files, &found), [[r"C:\a.bin", r"C:\b.bin"]]);
        // The head differs in the sample, so that file is read only once.
        assert_eq!(content.opens(r"C:\head.bin"), 1);
        // The middle differs only in the full hash.
        assert_eq!(content.opens(r"C:\middle.bin"), 2);
        assert_eq!(content.opens(r"C:\a.bin"), 2);
        assert_eq!(found.files_compared, 4);
        assert_eq!(found.bytes_read, 4 * 2 * SAMPLE + 3 * 2 * MIB);
    }

    #[test]
    fn a_different_tail_splits_at_sampling() {
        let mut content = MemContent::default();
        let base = pattern(MIB, 3);
        let mut tail = base.clone();
        *tail.last_mut().unwrap() ^= 1;
        let files = vec![
            content.add(r"C:\a.bin", base.clone()),
            content.add(r"C:\b.bin", tail),
        ];
        let found = run(&files, &content, 1);
        assert!(found.groups.is_empty());
        assert_eq!(content.opens(r"C:\a.bin"), 1);
    }

    #[test]
    fn small_files_are_hashed_once() {
        let mut content = MemContent::default();
        let data = pattern(100 * KIB, 7);
        let files = vec![
            content.add(r"C:\one.txt", data.clone()),
            content.add(r"C:\two.txt", data.clone()),
            content.add(r"C:\three.txt", data),
            content.add(r"C:\alone.txt", pattern(3 * KIB, 1)),
        ];
        let found = run(&files, &content, 3);
        assert_eq!(
            members(&files, &found),
            [[r"C:\one.txt", r"C:\three.txt", r"C:\two.txt"]]
        );
        assert_eq!(content.opens(r"C:\one.txt"), 1);
        // A file with a size of its own is never opened.
        assert_eq!(content.opens(r"C:\alone.txt"), 0);
    }

    #[test]
    fn skipped_files_are_counted() {
        let mut content = MemContent::default();
        let data = pattern(MIB, 9);
        let mut files = vec![
            content.add(r"C:\a.bin", data.clone()),
            content.add(r"C:\b.bin", data.clone()),
            content.fail(r"C:\busy.bin", MIB, Skip::InUse),
            content.fail(
                r"C:\denied.bin",
                MIB,
                Skip::Unreadable("access denied".into()),
            ),
            content.fail(r"C:\cloud.bin", MIB, Skip::OnlineOnly),
            candidate(r"C:\gone.bin", MIB),
        ];
        // The scan saw a different size than the file has now.
        content
            .files
            .insert(r"C:\grown.bin".to_string(), Ok(pattern(MIB + 5, 9)));
        files.push(candidate(r"C:\grown.bin", MIB));
        let found = run(&files, &content, 2);
        assert_eq!(members(&files, &found), [[r"C:\a.bin", r"C:\b.bin"]]);
        assert_eq!(found.skipped_in_use, 1);
        assert_eq!(found.skipped_unreadable, 1);
        assert_eq!(found.skipped_online_only, 1);
        assert_eq!(found.skipped_changed, 2);
        assert_eq!(found.files_compared, 2);
    }

    #[test]
    fn a_stop_keeps_what_was_found() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut content = MemContent::default();
        let mut files = Vec::new();
        for i in 0..20 {
            files.push(content.add(&format!(r"C:\f{i:02}.bin"), pattern(MIB, 5)));
        }
        content.stop = Some((5, Arc::clone(&flag)));
        let found = find(&files, &content, 1, &flag, &quiet);
        assert!(!found.completed);
        assert!(found.groups.is_empty());
        assert!(content.opened.lock().len() < 10);

        // Stopped while hashing: sampling finished, hashing did not.
        let flag = Arc::new(AtomicBool::new(false));
        content.opened.lock().clear();
        content.stop = Some((25, Arc::clone(&flag)));
        let found = find(&files, &content, 1, &flag, &quiet);
        assert!(!found.completed);
        assert_eq!(found.files_compared, 20);
        let hashed: usize = found.groups.iter().map(|(_, m)| m.len()).sum();
        assert!(hashed < 20);
    }

    #[test]
    fn progress_is_reported_by_phase() {
        let mut content = MemContent::default();
        let files = vec![
            content.add(r"C:\a.bin", pattern(2 * MIB, 1)),
            content.add(r"C:\b.bin", pattern(2 * MIB, 1)),
        ];
        let seen: Mutex<Vec<(&'static str, DuplicatesProgress)>> = Mutex::new(Vec::new());
        let record = |phase: &'static str, p: &DuplicatesProgress| {
            seen.lock().push((phase, p.clone()));
        };
        let found = find(&files, &content, 2, &AtomicBool::new(false), &record);
        assert_eq!(found.groups.len(), 1);
        let seen = seen.into_inner();
        let first = &seen[0];
        assert_eq!(first.0, "sampling");
        assert_eq!(first.1.files_total, 2);
        assert_eq!(first.1.bytes_total, 4 * SAMPLE);
        let last = seen.last().unwrap();
        assert_eq!(last.0, "hashing");
        assert_eq!(last.1.bytes_total, 4 * MIB);
        assert_eq!(last.1.bytes_done, 4 * MIB);
        assert_eq!(last.1.files_done, 2);
        assert_eq!(last.1.groups_found, 1);
        assert_eq!(
            progress_line("sampling", &first.1),
            "Comparing the start and end of 2 files that share a size…"
        );
        assert_eq!(
            progress_line("hashing", &last.1),
            "Comparing 2 files in full  ·  4 MB of 4 MB read"
        );
        let mut keys: Vec<String> = serde_json::to_value(&last.1)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "bytes_done",
                "bytes_total",
                "current",
                "files_done",
                "files_total",
                "groups_found"
            ]
        );
    }

    #[test]
    fn results_are_sorted_by_wasted_space_and_capped() {
        let mut content = MemContent::default();
        let mut files = Vec::new();
        // 101 copies of one small file, and pairs of other sizes.
        for i in 0..101 {
            files.push(content.add(&format!(r"C:\many\{i:03}.txt"), pattern(1000, 1)));
        }
        for i in 0..1001u64 {
            let data = pattern(2000 + i, 2);
            files.push(content.add(&format!(r"C:\pairs\{i:04}a.txt"), data.clone()));
            files.push(content.add(&format!(r"C:\pairs\{i:04}b.txt"), data));
        }
        let found = run(&files, &content, 3);
        assert!(found.completed);
        let result = build_result(HostJobId(7), r"C:\", MIB, &files, found);
        assert_eq!(result.group_count, 1002);
        assert_eq!(result.groups.len(), MAX_GROUPS);
        assert!(result.groups.windows(2).all(|w| w[0].wasted >= w[1].wasted));
        let many = result.groups.iter().find(|g| g.count == 101).unwrap();
        assert_eq!(many.wasted, 100 * 1000);
        assert_eq!(many.files.len(), MAX_FILES_PER_GROUP);
        assert_eq!(many.more_files, 1);
        assert_eq!(many.files[0].path, r"C:\many\000.txt");
        assert_eq!(many.hash.len(), 16);
        let expected: u64 = 100 * 1000 + (0..1001u64).map(|i| 2000 + i).sum::<u64>();
        assert_eq!(result.wasted_bytes, expected);
        // The 101 copies free the most, then the largest pair; the smallest pairs are cut.
        assert_eq!(result.groups[0].count, 101);
        assert_eq!(result.groups[1].size, 3000);
        assert_eq!(result.groups.last().unwrap().size, 2002);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["scan_job"], 7);
        assert_eq!(json.as_object().unwrap().len(), 13);
    }

    #[test]
    fn live_files_are_compared_through_their_handles() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(300 * KIB, 4);
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        let busy = dir.path().join("busy.bin");
        std::fs::write(&a, &data).unwrap();
        std::fs::write(&b, &data).unwrap();
        std::fs::write(&busy, &data).unwrap();
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&busy)
            .unwrap();
        let live = |path: &Path| {
            let file = open_exact(path, FILE_READ_ATTRIBUTES | SYNCHRONIZE, SHARE_ALL, 0).unwrap();
            Candidate {
                path: path.to_path_buf(),
                size: 300 * KIB,
                allocated: 300 * KIB,
                modified: 0,
                id_hash: file_id_hash(&file).ok(),
            }
        };
        let mut files = vec![live(&a), live(&b), live(&busy)];
        let mut other = files[0].clone();
        other.path = dir.path().join("missing.bin");
        files.push(other);
        // Same size, but the id says it is another file than the scan saw.
        let mut moved = files[1].clone();
        moved.id_hash = moved.id_hash.map(|h| h ^ 1).or(Some(1));
        let c = dir.path().join("c.bin");
        std::fs::write(&c, &data).unwrap();
        moved.path = c;
        files.push(moved);
        let found = find(&files, &LiveContent, 2, &AtomicBool::new(false), &quiet);
        assert_eq!(found.groups.len(), 1);
        assert_eq!(found.groups[0].1.len(), 2);
        assert_eq!(found.skipped_in_use, 1);
        assert_eq!(found.skipped_changed, 2);
        let mut digest = Sha256::new().unwrap();
        digest.update(&data).unwrap();
        assert_eq!(found.groups[0].0, digest.finish().unwrap());
        // Nothing was changed.
        assert_eq!(std::fs::read(&a).unwrap(), data);
    }

    #[test]
    fn live_files_named_with_a_trailing_dot_or_space_are_compared() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(2 * MIB, 6);
        // Names a plain path loses to Win32 normalization; they exist only through `\\?\`
        // paths (as WSL and SMB clients create them).
        let trail = dir.path().join("trail.");
        std::fs::create_dir(verbatim(&trail)).unwrap();
        let paths = [
            dir.path().join("one.bin"),
            dir.path().join("two."),
            dir.path().join("three "),
            trail.join("four.bin"),
        ];
        for path in &paths {
            std::fs::write(verbatim(path), &data).unwrap();
        }
        let files: Vec<Candidate> = paths
            .iter()
            .map(|path| {
                let file =
                    open_exact(path, FILE_READ_ATTRIBUTES | SYNCHRONIZE, SHARE_ALL, 0).unwrap();
                Candidate {
                    path: path.clone(),
                    size: 2 * MIB,
                    allocated: 2 * MIB,
                    modified: 0,
                    id_hash: file_id_hash(&file).ok(),
                }
            })
            .collect();
        let found = find(&files, &LiveContent, 2, &AtomicBool::new(false), &quiet);
        assert_eq!(found.skipped_changed, 0);
        assert_eq!(found.skipped_unreadable, 0);
        assert_eq!(found.groups.len(), 1);
        assert_eq!(found.groups[0].1.len(), 4, "every copy is in the group");
    }

    #[test]
    fn plans_need_a_finished_scan() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let request = DuplicatesRequest::new(HostJobId(u64::MAX), MIB).unwrap();
        let (plan, job) = plan_or_start_duplicates(&host, &request, true).unwrap();
        assert!(job.is_none());
        assert_eq!(plan.blocked_reason.as_deref(), Some(RESULTS_GONE_TEXT));
        let err = plan_or_start_duplicates(&host, &request, false).unwrap_err();
        assert_eq!(err.to_string(), RESULTS_GONE_TEXT);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 8);
    }

    #[test]
    fn a_running_scan_and_other_jobs_block_the_search() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let scan = host
            .start(
                JobSpec {
                    kind: KIND_SCAN,
                    title: r"Scan of C:\Users\Test".to_string(),
                    command_line: String::new(),
                    cancellable: true,
                    audit: None,
                    needs_journal: false,
                    log: false,
                },
                || panic!("no journal"),
                Box::new(move |_| {
                    let _ = wait.recv();
                    WorkEnd {
                        state: JobState::Succeeded,
                        summary: String::new(),
                        hint: None,
                        restart_required: false,
                        audit_detail: None,
                    }
                }),
            )
            .unwrap();
        let request = DuplicatesRequest::new(scan.id, MIB).unwrap();
        let (plan, _) = plan_or_start_duplicates(&host, &request, true).unwrap();
        assert_eq!(plan.blocked_reason.as_deref(), Some(SCAN_RUNNING_TEXT));
        release.send(()).unwrap();
        host.wait(scan.id, Duration::from_secs(10)).unwrap();
        // It finished without publishing a tree.
        let (plan, _) = plan_or_start_duplicates(&host, &request, true).unwrap();
        assert_eq!(plan.blocked_reason.as_deref(), Some(RESULTS_GONE_TEXT));
    }

    #[test]
    fn a_search_in_a_scanned_folder_finds_the_copies() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let root = dir.path().join("scan");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let data = pattern(3 * MIB / 2, 8);
        std::fs::write(root.join("one.bin"), &data).unwrap();
        std::fs::write(root.join("sub").join("two.bin"), &data).unwrap();
        let mut other = data.clone();
        other[0] ^= 1;
        std::fs::write(root.join("other.bin"), &other).unwrap();
        std::fs::write(root.join("unique.bin"), pattern(2 * MIB, 1)).unwrap();
        let (_, scan) =
            plan_or_start_scan(&host, &ScanRequest { path: root.clone() }, false).unwrap();
        let scan = scan.unwrap();
        host.wait(scan.id, Duration::from_secs(30)).unwrap();

        let request = DuplicatesRequest::new(scan.id, MIB).unwrap();
        let (plan, job) = plan_or_start_duplicates(&host, &request, true).unwrap();
        assert!(job.is_none());
        assert_eq!(plan.blocked_reason, None);
        assert_eq!(plan.candidates, 3);
        assert_eq!(plan.size_groups, 1);
        assert_eq!(plan.bytes_to_read_max, 3 * data.len() as u64);
        assert_eq!(plan.root, root.display().to_string());
        let big = DuplicatesRequest::new(scan.id, 10 * MIB).unwrap();
        assert_eq!(
            plan_or_start_duplicates(&host, &big, true)
                .unwrap()
                .0
                .candidates,
            0
        );

        let (_, job) = plan_or_start_duplicates(&host, &request, false).unwrap();
        let job = job.unwrap();
        assert_eq!(job.kind, KIND_DUPLICATES);
        assert_eq!(job.title, duplicates_title(&root.display().to_string()));
        let end = host.wait(job.id, Duration::from_secs(30)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        assert_eq!(
            end.summary.as_deref(),
            Some(
                "1 group of identical files  ·  1.5 MB would be freed by keeping one copy of each"
            )
        );
        // The finished job keeps the search's last progress block; the other kinds' are null.
        let detail = end.detail.unwrap();
        assert_eq!(detail["phase"], "done");
        assert!(detail["duplicates"].is_object(), "{detail}");
        assert_eq!(detail["duplicates"]["groups_found"], 1);
        assert_eq!(detail["duplicates"]["files_done"], 2);
        assert!(detail["speed"].is_null() && detail["scan"].is_null());
        let (_, json) = host.result(job.id, 0).unwrap();
        assert_eq!(json["kind"], "duplicates");
        assert_eq!(json["job_id"], job.id.0);
        assert_eq!(json["scan_job"], scan.id.0);
        assert_eq!(json["group_count"], 1);
        assert_eq!(json["groups"][0]["count"], 2);
        let paths: Vec<&str> = json["groups"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                root.join("one.bin").display().to_string(),
                root.join("sub").join("two.bin").display().to_string()
            ]
        );
        assert!(json["groups"][0]["files"][0]["modified"].is_string());
        // The scan keeps its tree beside the newer search.
        assert!(host.typed::<ScanResult>(scan.id).is_some());
    }
}
