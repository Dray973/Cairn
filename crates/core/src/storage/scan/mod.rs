//! Space analyzer: what takes up space on a drive or in a folder. Read-only; it writes no
//! audit rows.
//!
//! The walk ([`walk`]) keeps every folder as a node of an arena, every file of 1 MiB or more
//! individually and the smaller files of each folder as one aggregate. The finished
//! [`ScanResult`] is immutable and shared through an `Arc`, so reading a folder's children
//! never waits.

pub(crate) mod walk;

use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};

use self::walk::{walk, DirSource, Limits, LiveDirs, WalkSpec, Walked};
use super::files::{
    is_cloud_tag, open_exact, tag_info, ticks_rfc3339, wide_path, FILE_READ_ATTRIBUTES,
    FLAG_BACKUP_SEMANTICS, FLAG_OPEN_REPARSE_POINT, SHARE_ALL, SYNCHRONIZE,
};
use super::volumes::{file_system, volume_space};
use super::{counted, detail, size_text, AwakeGuard, KIND_SCAN};
use crate::jobs::{HostJobSnapshot, JobContext, JobHost, JobSpec, JobState, WorkEnd};
use crate::win::error_mode::ErrorModeGuard;
use crate::win::storage::MediaKind;
use crate::{Error, Result};

/// Files at least this large are kept individually (and may be duplicates).
pub const BIG_FILE_MIN: u64 = 1 << 20;
/// Length of the largest-files lists.
pub const LARGEST_FILES: usize = 200;
pub const MAX_NODES: usize = 16_000_000;
pub const MAX_BIG_FILES: usize = 4_000_000;
/// File ids kept to count hard links once (about 16 bytes each while scanning).
pub const MAX_FILE_IDS: usize = 8_000_000;
/// Default and largest page of a folder's children.
pub const CHILDREN_LIMIT: usize = 500;
pub const CHILDREN_LIMIT_MAX: usize = 5000;

pub(crate) const FLAG_DENIED: u8 = 1;
pub(crate) const FLAG_ERROR: u8 = 2;
pub(crate) const FLAG_LINK: u8 = 4;
pub(crate) const FLAG_IN_WINDOWS: u8 = 8;
pub(crate) const FLAG_IN_RECYCLE: u8 = 16;

pub const NOT_LOCAL_TEXT: &str = "Cairn scans folders on this PC's own drives only.";
pub const NOT_ELEVATED_NOTE: &str = "Cairn runs without administrator rights, so some folders \
     can't be read. Restart as administrator to include them.";
pub const CLOUD_NOTE: &str = "Online-only OneDrive files count toward Size but not Size on \
     disk; they aren't downloaded.";
pub const MOUNTED_NOTE: &str =
    "Drives mounted in a folder aren't entered; scan them by their own letter.";

/// What to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    pub path: PathBuf,
}

/// What a scan would do, and why it cannot start now, if it cannot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanPlan {
    pub root: String,
    /// "C:".
    pub volume: String,
    /// The root is the volume's root folder.
    pub whole_volume: bool,
    pub file_system: String,
    pub media: MediaKind,
    pub threads: u32,
    pub elevated: bool,
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

/// How a folder's entries are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Size on disk.
    Allocated,
    /// Size.
    Logical,
}

impl SortOrder {
    /// "allocated" or "logical".
    pub fn parse(text: &str) -> Option<SortOrder> {
        match text.trim() {
            "allocated" => Some(SortOrder::Allocated),
            "logical" => Some(SortOrder::Logical),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeRowKind {
    Folder,
    /// A link to another folder, not followed.
    Link,
    File,
    /// A folder's files smaller than 1 MiB.
    SmallFiles,
    /// Entries past the page's limit.
    More,
}

/// One row of the folder tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TreeRow {
    pub kind: TreeRowKind,
    /// The folder's node, for its children; `None` for other rows.
    pub node: Option<u32>,
    pub name: String,
    pub path: Option<String>,
    pub logical: u64,
    pub allocated: u64,
    pub online_only: u64,
    pub files: u64,
    pub folders: u64,
    /// Entries a SmallFiles or More row stands for.
    pub count: u64,
    pub has_children: bool,
    pub denied: bool,
    pub error: Option<String>,
    /// Last write of a file, RFC 3339.
    pub modified: Option<String>,
}

/// One folder's entries.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChildrenPage {
    pub node: TreeRow,
    pub order: SortOrder,
    pub children: Vec<TreeRow>,
    /// Entries before the limit cut them.
    pub total: u64,
}

/// Totals of a finished (or stopped) scan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanSummary {
    pub root: String,
    pub volume: String,
    pub whole_volume: bool,
    pub completed: bool,
    pub files: u64,
    pub folders: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub online_only_bytes: u64,
    pub online_only_files: u64,
    pub hard_links_counted_once: u64,
    pub links_skipped: u64,
    pub denied_folders: u64,
    pub unreadable_folders: u64,
    pub volume_size: Option<u64>,
    pub volume_used: Option<u64>,
    /// Used space the scan did not reach (whole-volume scans that finished).
    pub not_reached_bytes: Option<u64>,
    pub elapsed_ms: u64,
    pub threads: u32,
    pub id_limit_reached: bool,
    pub node_limit_reached: bool,
    pub big_file_limit_reached: bool,
    pub duplicate_candidates: u64,
}

/// Live state of a running scan (the `scan` block of the job's detail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanProgress {
    pub files: u64,
    pub folders: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub denied_folders: u64,
    pub current: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub files: u64,
    pub folders: u64,
    pub logical: u64,
    pub allocated: u64,
    pub online_only: u64,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.files += other.files;
        self.folders += other.folders;
        self.logical += other.logical;
        self.allocated += other.allocated;
        self.online_only += other.online_only;
    }
}

/// A folder's files smaller than [`BIG_FILE_MIN`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Small {
    pub count: u64,
    pub logical: u64,
    pub allocated: u64,
    pub online_only: u64,
}

/// A folder of the arena. A child's index is always above its parent's.
#[derive(Debug, Clone, Default)]
pub(crate) struct Node {
    pub parent: u32,
    pub name: Box<str>,
    pub flags: u8,
    /// Files directly in the folder (large and small).
    pub own: Totals,
    /// The folder and everything below it.
    pub sub: Totals,
    pub small: Small,
    pub files_start: u32,
    pub files_len: u32,
    pub children_start: u32,
    pub children_len: u32,
    pub error: Option<Box<str>>,
}

impl Node {
    fn is_link(&self) -> bool {
        self.flags & FLAG_LINK != 0
    }
}

/// A file of 1 MiB or more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BigFile {
    pub node: u32,
    pub name: Box<str>,
    pub logical: u64,
    pub allocated: u64,
    pub online_only: bool,
    /// Last write, FILETIME ticks.
    pub modified: i64,
    /// May be compared for duplicates: stored here, not a system file, not in the Windows
    /// folder or the Recycle Bin, not an extra hard link.
    pub candidate: bool,
    pub id_hash: Option<u64>,
}

/// A finished scan: an immutable tree of folders and large files.
#[derive(Debug)]
pub struct ScanResult {
    summary: ScanSummary,
    root: PathBuf,
    pub(crate) media: MediaKind,
    nodes: Vec<Node>,
    pub(crate) big: Vec<BigFile>,
    /// For each folder, its children's node indices in `[children_start ..][.. children_len]`,
    /// by size on disk, largest first.
    child_order: Vec<u32>,
    largest_allocated: Vec<u32>,
    largest_logical: Vec<u32>,
    /// Large files that may be duplicates, by size, only sizes that occur more than once.
    pub(crate) candidates: Vec<u32>,
    warnings: Vec<String>,
}

/// Facts the walk does not know.
#[derive(Debug, Clone)]
pub(crate) struct ScanFacts {
    pub root: PathBuf,
    pub volume: String,
    pub whole_volume: bool,
    pub media: MediaKind,
    pub threads: u32,
    pub volume_size: Option<u64>,
    pub volume_used: Option<u64>,
    pub elapsed_ms: u64,
    pub warnings: Vec<String>,
}

impl ScanResult {
    /// Sums the walk's folders, orders every folder's children and picks the largest files
    /// and the duplicate candidates.
    pub(crate) fn finish(walked: Walked, facts: ScanFacts) -> ScanResult {
        let Walked {
            mut nodes,
            big,
            completed,
            hard_links_counted_once,
            links_skipped,
            denied_folders,
            unreadable_folders,
            online_only_files,
            id_limit_reached,
            node_limit_reached,
            big_file_limit_reached,
        } = walked;
        for node in nodes.iter_mut() {
            node.sub = node.own;
            node.sub.folders = 0;
        }
        for i in (1..nodes.len()).rev() {
            let child = nodes[i].sub;
            let counted = !nodes[i].is_link();
            let parent = nodes[i].parent as usize;
            nodes[parent].sub.add(&child);
            if counted {
                nodes[parent].sub.folders += 1;
            }
        }
        let mut child_order: Vec<u32> = (0..nodes.len() as u32).collect();
        for node in &nodes {
            let start = node.children_start as usize;
            let end = start + node.children_len as usize;
            if node.children_len > 1 && end <= child_order.len() {
                child_order[start..end].sort_by(|&a, &b| {
                    nodes[b as usize]
                        .sub
                        .allocated
                        .cmp(&nodes[a as usize].sub.allocated)
                });
            }
        }
        let largest = |key: fn(&BigFile) -> u64| {
            let mut order: Vec<u32> = (0..big.len() as u32).collect();
            let cmp = |a: &u32, b: &u32| key(&big[*b as usize]).cmp(&key(&big[*a as usize]));
            if order.len() > LARGEST_FILES {
                order.select_nth_unstable_by(LARGEST_FILES - 1, cmp);
                order.truncate(LARGEST_FILES);
            }
            order.sort_by(cmp);
            order
        };
        let largest_allocated = largest(|f| f.allocated);
        let largest_logical = largest(|f| f.logical);
        let mut candidates: Vec<u32> = (0..big.len() as u32)
            .filter(|&i| big[i as usize].candidate)
            .collect();
        candidates.sort_by_key(|&i| big[i as usize].logical);
        let candidates: Vec<u32> = candidates
            .iter()
            .enumerate()
            .filter(|(k, &i)| {
                let size = big[i as usize].logical;
                let before = *k > 0 && big[candidates[k - 1] as usize].logical == size;
                let after = candidates
                    .get(k + 1)
                    .is_some_and(|&j| big[j as usize].logical == size);
                before || after
            })
            .map(|(_, &i)| i)
            .collect();
        let root = nodes.first().map(|n| n.sub).unwrap_or_default();
        let not_reached_bytes = match (facts.whole_volume && completed, facts.volume_used) {
            (true, Some(used)) => Some(used.saturating_sub(root.allocated)),
            _ => None,
        };
        let summary = ScanSummary {
            root: facts.root.display().to_string(),
            volume: facts.volume.clone(),
            whole_volume: facts.whole_volume,
            completed,
            files: root.files,
            folders: root.folders,
            logical_bytes: root.logical,
            allocated_bytes: root.allocated,
            online_only_bytes: root.online_only,
            online_only_files,
            hard_links_counted_once,
            links_skipped,
            denied_folders,
            unreadable_folders,
            volume_size: facts.volume_size,
            volume_used: facts.volume_used,
            not_reached_bytes,
            elapsed_ms: facts.elapsed_ms,
            threads: facts.threads,
            id_limit_reached,
            node_limit_reached,
            big_file_limit_reached,
            duplicate_candidates: candidates.len() as u64,
        };
        ScanResult {
            summary,
            root: facts.root,
            media: facts.media,
            nodes,
            big,
            child_order,
            largest_allocated,
            largest_logical,
            candidates,
            warnings: facts.warnings,
        }
    }

    pub fn summary(&self) -> &ScanSummary {
        &self.summary
    }

    /// The scanned folder's row.
    pub fn root_row(&self) -> TreeRow {
        self.node_row(0)
    }

    /// Folder `node`'s entries by `order`, largest first: its subfolders and links, its
    /// large files and one row for its smaller files. Past `limit` (1 to 5000) the rest
    /// becomes one "More" row. `None` for an unknown node.
    pub fn children(&self, node: u32, order: SortOrder, limit: usize) -> Option<ChildrenPage> {
        let folder = self.nodes.get(node as usize)?;
        let limit = limit.clamp(1, CHILDREN_LIMIT_MAX);
        let mut rows: Vec<TreeRow> = Vec::new();
        if !folder.is_link() {
            let start = folder.children_start as usize;
            let end = start + folder.children_len as usize;
            rows.extend(
                self.child_order
                    .get(start..end)
                    .unwrap_or(&[])
                    .iter()
                    .map(|&c| self.node_row(c)),
            );
            let start = folder.files_start as usize;
            let end = start + folder.files_len as usize;
            rows.extend(
                self.big
                    .get(start..end)
                    .unwrap_or(&[])
                    .iter()
                    .map(|f| self.file_row(f)),
            );
            if folder.small.count > 0 {
                let small = folder.small;
                rows.push(TreeRow {
                    kind: TreeRowKind::SmallFiles,
                    node: None,
                    name: counted(small.count, "smaller file"),
                    path: None,
                    logical: small.logical,
                    allocated: small.allocated,
                    online_only: small.online_only,
                    files: small.count,
                    folders: 0,
                    count: small.count,
                    has_children: false,
                    denied: false,
                    error: None,
                    modified: None,
                });
            }
        }
        let key = |row: &TreeRow| match order {
            SortOrder::Allocated => row.allocated,
            SortOrder::Logical => row.logical,
        };
        rows.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.name.cmp(&b.name)));
        let total = rows.len() as u64;
        if rows.len() > limit {
            let rest = rows.split_off(limit - 1);
            let count = rest.len() as u64;
            let mut more = TreeRow {
                kind: TreeRowKind::More,
                node: None,
                name: counted(count, "more item"),
                path: None,
                logical: 0,
                allocated: 0,
                online_only: 0,
                files: 0,
                folders: 0,
                count,
                has_children: false,
                denied: false,
                error: None,
                modified: None,
            };
            for row in &rest {
                more.logical += row.logical;
                more.allocated += row.allocated;
                more.online_only += row.online_only;
                more.files += row.files;
                more.folders += row.folders + u64::from(row.kind == TreeRowKind::Folder);
            }
            rows.push(more);
        }
        Some(ChildrenPage {
            node: self.node_row(node),
            order,
            children: rows,
            total,
        })
    }

    /// The 200 largest files by `order`, largest first.
    pub fn largest_files(&self, order: SortOrder) -> Vec<TreeRow> {
        let list = match order {
            SortOrder::Allocated => &self.largest_allocated,
            SortOrder::Logical => &self.largest_logical,
        };
        list.iter()
            .map(|&i| self.file_row(&self.big[i as usize]))
            .collect()
    }

    /// Full path of folder `node`.
    pub fn path_of(&self, node: u32) -> Option<PathBuf> {
        let mut names = Vec::new();
        let mut at = node as usize;
        self.nodes.get(at)?;
        while at != 0 {
            let n = &self.nodes[at];
            names.push(&*n.name);
            at = n.parent as usize;
        }
        let mut path = self.root.clone();
        for name in names.iter().rev() {
            path.push(name);
        }
        Some(path)
    }

    /// Full path of a large file.
    pub(crate) fn file_path(&self, file: &BigFile) -> PathBuf {
        let mut path = self.path_of(file.node).unwrap_or_else(|| self.root.clone());
        path.push(&*file.name);
        path
    }

    /// The scanned folder.
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// The plan's notes, shown with the result.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn node_row(&self, index: u32) -> TreeRow {
        let node = &self.nodes[index as usize];
        let link = node.is_link();
        TreeRow {
            kind: if link {
                TreeRowKind::Link
            } else {
                TreeRowKind::Folder
            },
            node: Some(index),
            name: if index == 0 {
                self.root.display().to_string()
            } else {
                node.name.to_string()
            },
            path: self.path_of(index).map(|p| p.display().to_string()),
            logical: node.sub.logical,
            allocated: node.sub.allocated,
            online_only: node.sub.online_only,
            files: node.sub.files,
            folders: node.sub.folders,
            count: 0,
            has_children: !link
                && (node.children_len > 0 || node.files_len > 0 || node.small.count > 0),
            denied: node.flags & FLAG_DENIED != 0,
            error: node.error.as_deref().map(str::to_string),
            modified: None,
        }
    }

    fn file_row(&self, file: &BigFile) -> TreeRow {
        TreeRow {
            kind: TreeRowKind::File,
            node: None,
            name: file.name.to_string(),
            path: Some(self.file_path(file).display().to_string()),
            logical: file.logical,
            allocated: file.allocated,
            online_only: if file.online_only { file.logical } else { 0 },
            files: 1,
            folders: 0,
            count: 0,
            has_children: false,
            denied: false,
            error: None,
            modified: ticks_rfc3339(file.modified),
        }
    }

    /// The JSON a finished scan publishes: `{"kind": "scan", "job_id", "summary", "root",
    /// "largest_files" (by size on disk), "largest_files_by_size", "warnings"}`.
    pub fn result_json(&self, job_id: u64) -> serde_json::Value {
        json!({
            "kind": "scan",
            "job_id": job_id,
            "summary": self.summary,
            "root": self.root_row(),
            "largest_files": self.largest_files(SortOrder::Allocated),
            "largest_files_by_size": self.largest_files(SortOrder::Logical),
            "warnings": self.warnings,
        })
    }
}

// ───────────────────────────── Plan and start ─────────────────────────────

/// A plan with what its start needs.
struct Planned {
    plan: ScanPlan,
    root: PathBuf,
    volume_root: PathBuf,
}

/// `path` as a plain `X:\…` path without `.`/`..` parts; `None` for anything that is not an
/// absolute path on a lettered drive (UNC, device paths, relative paths).
///
/// A plain path names the folder Windows opens for it (`GetFullPathNameW`: a trailing dot
/// leaves each name, trailing dots and spaces the last one); a `\\?\` path keeps its names as
/// given. The scan then opens the result and everything below it by exact name.
pub(crate) fn local_path(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    let (letter, exact) = match parts.next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(l) => (char::from(l).to_ascii_uppercase(), false),
            Prefix::VerbatimDisk(l) => (char::from(l).to_ascii_uppercase(), true),
            _ => return None,
        },
        _ => return None,
    };
    if parts.next()? != Component::RootDir {
        return None;
    }
    let mut out = PathBuf::from(format!("{letter}:\\"));
    for part in parts {
        match part {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    if !exact {
        if let Ok(resolved) = std::path::absolute(&out) {
            out = resolved;
        }
    }
    Some(out)
}

/// The volume root holding `path` (`C:\`, or the folder a volume is mounted in).
fn volume_root(path: &Path) -> PathBuf {
    let wide = wide_path(path);
    let mut buf = [0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` is writable for its whole length.
    let found = unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut buf) };
    let fallback = || PathBuf::from(format!("{}\\", &path.to_string_lossy()[..2]));
    match found {
        Ok(()) => {
            let text = crate::win::from_wide_nul(&buf);
            let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
            if text.is_empty() {
                fallback()
            } else {
                PathBuf::from(text)
            }
        }
        Err(_) => fallback(),
    }
}

fn is_fixed(volume_root: &Path) -> bool {
    let mut text: Vec<u16> = volume_root
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .collect();
    if text.last() != Some(&u16::from(b'\\')) {
        text.push(u16::from(b'\\'));
    }
    text.push(0);
    // SAFETY: `text` is NUL-terminated.
    unsafe { GetDriveTypeW(PCWSTR(text.as_ptr())) == 3 }
}

fn media_of(letter: &str) -> MediaKind {
    match crate::win::storage::open_device(&format!(r"\\.\{letter}"))
        .ok()
        .and_then(|d| crate::win::storage::seek_penalty(&d))
    {
        Some(true) => MediaKind::Hdd,
        Some(false) => MediaKind::Ssd,
        None => MediaKind::Unknown,
    }
}

/// Worker threads for a scan: two on a hard disk, else up to eight.
pub(crate) fn threads_for(media: MediaKind) -> u32 {
    if media == MediaKind::Hdd {
        return 2;
    }
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    cores.clamp(2, 8) as u32
}

/// The root or a folder above it is a cloud placeholder, or it lies in a OneDrive folder.
fn in_cloud_folder(root: &Path) -> bool {
    for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(folder) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            if walk::inside(root, Path::new(&folder)) {
                return true;
            }
        }
    }
    root.ancestors().any(|folder| {
        open_exact(
            folder,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
        )
        .ok()
        .and_then(|f| tag_info(&f).ok())
        .is_some_and(|t| is_cloud_tag(t.tag))
    })
}

fn plan_scan(host: &JobHost, request: &ScanRequest) -> Planned {
    let elevated = crate::is_elevated();
    let mut plan = ScanPlan {
        root: request.path.display().to_string(),
        volume: String::new(),
        whole_volume: false,
        file_system: String::new(),
        media: MediaKind::Unknown,
        threads: 2,
        elevated,
        blocked_reason: None,
        notes: Vec::new(),
    };
    let Some(root) = local_path(&request.path) else {
        plan.blocked_reason = Some(NOT_LOCAL_TEXT.to_string());
        return Planned {
            plan,
            root: request.path.clone(),
            volume_root: request.path.clone(),
        };
    };
    let letter = root.to_string_lossy()[..2].to_string();
    let volume_root = volume_root(&root);
    plan.root = root.display().to_string();
    plan.volume = letter.clone();
    plan.whole_volume = crate::storage::files::same_path(&root, &volume_root)
        && crate::storage::speed::place::is_volume_root(&root);
    plan.file_system = file_system(&volume_root);
    plan.media = media_of(&letter);
    plan.threads = threads_for(plan.media);
    let blocked = if !is_fixed(&volume_root) {
        Some(format!("{letter} is not a fixed drive."))
    } else {
        match open_exact(
            &root,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
        )
        .and_then(|f| tag_info(&f))
        {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(3) => {
                Some(format!("{} doesn't exist", root.display()))
            }
            Ok(tag) if !tag.is_dir() => {
                Some(format!("{} is a file; choose a folder", root.display()))
            }
            Ok(tag) if tag.is_link() => Some(format!(
                "{} is a link to another folder; scan the folder it points to",
                root.display()
            )),
            _ => None,
        }
    };
    plan.blocked_reason = blocked.or_else(|| {
        host.running().map(|job| {
            format!(
                "{} is running; wait for it to finish or stop it.",
                job.title
            )
        })
    });
    if !elevated {
        plan.notes.push(NOT_ELEVATED_NOTE.to_string());
    }
    if plan.blocked_reason.is_none() && in_cloud_folder(&root) {
        plan.notes.push(CLOUD_NOTE.to_string());
    }
    if plan.whole_volume {
        plan.notes.push(MOUNTED_NOTE.to_string());
    }
    Planned {
        plan,
        root,
        volume_root,
    }
}

/// Title of the scan of `root`: "Scan of C:" for a whole volume, else "Scan of <folder>".
pub fn scan_title(plan: &ScanPlan) -> String {
    if plan.whole_volume {
        format!("Scan of {}", plan.volume)
    } else {
        format!("Scan of {}", plan.root)
    }
}

/// Plans a scan of `request.path` and, unless `dry_run`, starts it as a job of `host`.
/// Read-only: it opens no journal and writes no rows. A blocked plan is refused.
pub fn plan_or_start_scan(
    host: &JobHost,
    request: &ScanRequest,
    dry_run: bool,
) -> Result<(ScanPlan, Option<HostJobSnapshot>)> {
    plan_or_start_scan_with(host, request, dry_run, Arc::new(LiveSource))
}

/// Which folders a scan job reads.
pub(crate) trait SourceFactory: Send + Sync {
    fn source(&self, ids: bool) -> Box<dyn DirSource>;
}

#[derive(Debug)]
struct LiveSource;

impl SourceFactory for LiveSource {
    fn source(&self, ids: bool) -> Box<dyn DirSource> {
        Box::new(LiveDirs { ids })
    }
}

pub(crate) fn plan_or_start_scan_with(
    host: &JobHost,
    request: &ScanRequest,
    dry_run: bool,
    sources: Arc<dyn SourceFactory>,
) -> Result<(ScanPlan, Option<HostJobSnapshot>)> {
    let planned = plan_scan(host, request);
    if dry_run {
        return Ok((planned.plan, None));
    }
    if let Some(reason) = &planned.plan.blocked_reason {
        return Err(Error::Other(reason.clone()));
    }
    let plan = planned.plan.clone();
    let spec = JobSpec {
        kind: KIND_SCAN,
        title: scan_title(&plan),
        command_line: format!("optctl storage scan \"{}\"", plan.root),
        cancellable: true,
        audit: None,
        needs_journal: false,
        log: false,
    };
    let job = host.start(
        spec,
        || Err(Error::Other("a scan keeps no journal".to_string())),
        Box::new(move |ctx| run_scan(ctx, planned, sources.as_ref())),
    )?;
    Ok((plan, Some(job)))
}

fn run_scan(ctx: &JobContext, planned: Planned, sources: &dyn SourceFactory) -> WorkEnd {
    let _awake = AwakeGuard::new();
    let _mode = ErrorModeGuard::new();
    let started = std::time::Instant::now();
    let Planned {
        plan,
        root,
        volume_root,
    } = planned;
    let (volume_size, volume_used) = match volume_space(&volume_root) {
        Ok((total, free)) => (Some(total), Some(total.saturating_sub(free))),
        Err(_) => (None, None),
    };
    let fs = plan.file_system.to_ascii_uppercase();
    let source = sources.source(fs == "NTFS" || fs == "REFS");
    let windows_dir = crate::win::paths::windows_dir().ok();
    let used = volume_used.filter(|u| *u > 0 && plan.whole_volume);
    let label = if plan.whole_volume {
        plan.volume.clone()
    } else {
        plan.root.clone()
    };
    // The newest progress block; the phases after the walk keep showing it.
    let last_block: Mutex<Option<Value>> = Mutex::new(None);
    let report = |p: &ScanProgress| {
        let percent = used.map(|u| (p.allocated_bytes as f64 / u as f64 * 100.0).min(99.0));
        let line = format!(
            "Scanning {label}…  {} in {}  ·  {}",
            counted(p.files, "file"),
            counted(p.folders, "folder"),
            size_text(p.allocated_bytes)
        );
        ctx.progress(percent, Some(&line));
        let block = serde_json::to_value(p).ok();
        last_block.lock().clone_from(&block);
        ctx.set_detail(detail("scanning", None, block, None));
    };
    report(&ScanProgress {
        files: 0,
        folders: 0,
        logical_bytes: 0,
        allocated_bytes: 0,
        denied_folders: 0,
        current: root.display().to_string(),
    });
    let cancel: Arc<AtomicBool> = ctx.cancel_flag();
    let root_name = root.display().to_string();
    let walked = walk(
        source.as_ref(),
        &WalkSpec {
            root: &root,
            root_name: &root_name,
            threads: plan.threads,
            windows_dir: windows_dir.as_deref(),
            limits: Limits::default(),
        },
        &cancel,
        &report,
    );
    // The walk ends with a report of its final counts.
    let block = last_block.into_inner();
    ctx.set_detail(detail("summarizing", None, block.clone(), None));
    ctx.progress(used.map(|_| 99.0), Some("Summarizing…"));
    let result = ScanResult::finish(
        walked,
        ScanFacts {
            root: root.clone(),
            volume: plan.volume.clone(),
            whole_volume: plan.whole_volume,
            media: plan.media,
            threads: plan.threads,
            volume_size,
            volume_used,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            warnings: plan.notes.clone(),
        },
    );
    let summary = result.summary().clone();
    ctx.publish(result.result_json(ctx.id().0));
    ctx.publish_typed(Arc::new(result));
    ctx.set_detail(detail("done", None, block, None));
    let text = format!(
        "{label}  ·  {} on disk in {} and {}",
        size_text(summary.allocated_bytes),
        counted(summary.files, "file"),
        counted(summary.folders, "folder")
    );
    WorkEnd {
        state: if summary.completed {
            JobState::Succeeded
        } else {
            JobState::Cancelled
        },
        summary: text,
        hint: None,
        restart_required: false,
        audit_detail: None,
    }
}

#[cfg(test)]
mod tests;
