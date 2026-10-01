//! The parallel folder walk behind the space scan.
//!
//! Workers take folders from a shared stack, list each through a [`DirSource`] (live:
//! `GetFileInformationByHandleEx` with 64 KiB buffers on a handle that never follows reparse
//! points) and commit a folder's totals, its large files and its subfolders under one lock.
//! Links to other folders are recorded and never entered; cloud placeholders (OneDrive) are
//! entered and never hydrated. A file's extra hard links count once.

use std::collections::HashSet;
use std::ffi::{c_void, OsString};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_NO_MORE_FILES, NO_ERROR};
use windows::Win32::Storage::FileSystem::{
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdExtdDirectoryInfo,
    FileIdExtdDirectoryRestartInfo, GetCompressedFileSizeW, GetFileInformationByHandleEx,
    FILE_FULL_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS, INVALID_FILE_SIZE,
};

use super::{
    BigFile, Node, ScanProgress, Small, Totals, BIG_FILE_MIN, FLAG_DENIED, FLAG_ERROR,
    FLAG_IN_RECYCLE, FLAG_IN_WINDOWS, FLAG_LINK, MAX_BIG_FILES, MAX_FILE_IDS, MAX_NODES,
};
use crate::storage::files::{
    id_hash, is_name_surrogate, is_online_only, open_exact, raw, same_path, tag_info, wide_path,
    ATTR_COMPRESSED, ATTR_DIRECTORY, ATTR_REPARSE, ATTR_SPARSE, ATTR_SYSTEM, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FLAG_BACKUP_SEMANTICS, FLAG_OPEN_REPARSE_POINT, SHARE_ALL, SYNCHRONIZE,
    TAG_WOF,
};

/// One directory entry as the file system keeps it in the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    pub name: OsString,
    pub attrs: u32,
    /// The reparse tag when `attrs` has FILE_ATTRIBUTE_REPARSE_POINT, else 0.
    pub reparse_tag: u32,
    pub logical: u64,
    pub allocated: u64,
    /// The 128-bit file id (NTFS, ReFS); `None` where the file system has none.
    pub file_id: Option<[u8; 16]>,
    /// Last write, FILETIME ticks; 0 when unknown.
    pub modified: i64,
}

/// Why a folder could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListError {
    Denied,
    Gone,
    /// It became a link or something other than a folder.
    NotPlain,
    Other(String),
}

/// Lists folders for the walk.
pub(crate) trait DirSource: Sync {
    /// Streams `path`'s entries ("." and ".." excluded) after checking through the opened
    /// handle that it is a folder and not a link. `visit` returns false to stop.
    fn list(
        &self,
        path: &Path,
        visit: &mut dyn FnMut(RawEntry) -> bool,
    ) -> std::result::Result<(), ListError>;
    /// On-disk size of a compressed, sparse or WOF file; `None` when unreadable.
    fn compressed_size(&self, path: &Path) -> Option<u64>;
}

/// This PC's folders. With `ids`, entries carry file ids (NTFS, ReFS); folders whose file
/// system refuses the id listing are listed without them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveDirs {
    pub ids: bool,
}

/// Size of the listing buffer in 8-byte words (64 KiB); records are 8-byte aligned.
const LIST_BUFFER_WORDS: usize = 8 * 1024;

impl DirSource for LiveDirs {
    fn list(
        &self,
        path: &Path,
        visit: &mut dyn FnMut(RawEntry) -> bool,
    ) -> std::result::Result<(), ListError> {
        // The folder's path is built from listed names, which may end in a dot or a space.
        let dir = open_exact(
            path,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
        )
        .map_err(list_error)?;
        let tag = tag_info(&dir).map_err(list_error)?;
        if !tag.is_dir() || tag.is_link() {
            return Err(ListError::NotPlain);
        }
        let mut ids = self.ids;
        let mut buf = vec![0u64; LIST_BUFFER_WORDS];
        let len = LIST_BUFFER_WORDS * size_of::<u64>();
        let mut restart = true;
        loop {
            let class: FILE_INFO_BY_HANDLE_CLASS = match (ids, restart) {
                (true, true) => FileIdExtdDirectoryRestartInfo,
                (true, false) => FileIdExtdDirectoryInfo,
                (false, true) => FileFullDirectoryRestartInfo,
                (false, false) => FileFullDirectoryInfo,
            };
            // SAFETY: `buf` is writable for `len` bytes and outlives the call.
            let filled = unsafe {
                GetFileInformationByHandleEx(
                    raw(&dir),
                    class,
                    buf.as_mut_ptr().cast::<c_void>(),
                    len as u32,
                )
            };
            match filled {
                Ok(()) => {}
                Err(e) if crate::win::is_win32(&e, ERROR_NO_MORE_FILES) => return Ok(()),
                Err(e)
                    if ids
                        && restart
                        && matches!(e.code().0 as u32 & 0xFFFF, 1 | 50 | 87 | 124) =>
                {
                    // The file system has no id listing: list this folder without ids.
                    ids = false;
                    continue;
                }
                Err(e) => return Err(list_error(crate::storage::files::to_io(e))),
            }
            restart = false;
            let keep_going = if ids {
                parse_extd(&buf, len, visit)
            } else {
                parse_full(&buf, len, visit)
            }
            .map_err(ListError::Other)?;
            if !keep_going {
                return Ok(());
            }
        }
    }

    fn compressed_size(&self, path: &Path) -> Option<u64> {
        let wide = wide_path(path);
        let mut high = 0u32;
        // SAFETY: `wide` is NUL-terminated and `high` is a valid out pointer.
        let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high)) };
        // SAFETY: reads the calling thread's last error.
        if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != NO_ERROR {
            return None;
        }
        Some((u64::from(high) << 32) | u64::from(low))
    }
}

fn list_error(e: std::io::Error) -> ListError {
    match e.raw_os_error() {
        Some(5) => ListError::Denied,
        Some(2 | 3) => ListError::Gone,
        _ if e.kind() == std::io::ErrorKind::NotFound => ListError::Gone,
        _ => ListError::Other(e.to_string()),
    }
}

/// Walks the records of a `FILE_ID_EXTD_DIR_INFO` buffer; false when `visit` asked to stop.
fn parse_extd(
    buf: &[u64],
    len: usize,
    visit: &mut dyn FnMut(RawEntry) -> bool,
) -> std::result::Result<bool, String> {
    let base = buf.as_ptr().cast::<u8>();
    let name_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
    let mut offset = 0usize;
    loop {
        if offset + size_of::<FILE_ID_EXTD_DIR_INFO>() > len {
            return Err("malformed directory listing".to_string());
        }
        // SAFETY: the record header lies inside `buf` (checked above); it is copied out
        // unaligned, so no reference into the buffer is formed.
        let rec: FILE_ID_EXTD_DIR_INFO =
            unsafe { std::ptr::read_unaligned(base.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>()) };
        let units = rec.FileNameLength as usize / 2;
        if offset + name_at + units * 2 > len {
            return Err("malformed directory listing".to_string());
        }
        // SAFETY: the name lies inside `buf` (checked above) and starts at an even offset of
        // an 8-byte aligned buffer, so it is an aligned run of `units` UTF-16 units.
        let name =
            unsafe { std::slice::from_raw_parts(base.add(offset + name_at).cast::<u16>(), units) };
        if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
            let entry = RawEntry {
                name: OsString::from_wide(name),
                attrs: rec.FileAttributes,
                reparse_tag: if rec.FileAttributes & ATTR_REPARSE != 0 {
                    rec.ReparsePointTag
                } else {
                    0
                },
                logical: u64::try_from(rec.EndOfFile).unwrap_or(0),
                allocated: u64::try_from(rec.AllocationSize).unwrap_or(0),
                file_id: Some(rec.FileId.Identifier),
                modified: rec.LastWriteTime,
            };
            if !visit(entry) {
                return Ok(false);
            }
        }
        if rec.NextEntryOffset == 0 {
            return Ok(true);
        }
        offset += rec.NextEntryOffset as usize;
    }
}

/// Walks the records of a `FILE_FULL_DIR_INFO` buffer, where the reparse tag is kept in
/// `EaSize`; false when `visit` asked to stop.
fn parse_full(
    buf: &[u64],
    len: usize,
    visit: &mut dyn FnMut(RawEntry) -> bool,
) -> std::result::Result<bool, String> {
    let base = buf.as_ptr().cast::<u8>();
    let name_at = offset_of!(FILE_FULL_DIR_INFO, FileName);
    let mut offset = 0usize;
    loop {
        if offset + size_of::<FILE_FULL_DIR_INFO>() > len {
            return Err("malformed directory listing".to_string());
        }
        // SAFETY: as in `parse_extd`.
        let rec: FILE_FULL_DIR_INFO =
            unsafe { std::ptr::read_unaligned(base.add(offset).cast::<FILE_FULL_DIR_INFO>()) };
        let units = rec.FileNameLength as usize / 2;
        if offset + name_at + units * 2 > len {
            return Err("malformed directory listing".to_string());
        }
        // SAFETY: as in `parse_extd`.
        let name =
            unsafe { std::slice::from_raw_parts(base.add(offset + name_at).cast::<u16>(), units) };
        if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
            let entry = RawEntry {
                name: OsString::from_wide(name),
                attrs: rec.FileAttributes,
                reparse_tag: if rec.FileAttributes & ATTR_REPARSE != 0 {
                    rec.EaSize
                } else {
                    0
                },
                logical: u64::try_from(rec.EndOfFile).unwrap_or(0),
                allocated: u64::try_from(rec.AllocationSize).unwrap_or(0),
                file_id: None,
                modified: rec.LastWriteTime,
            };
            if !visit(entry) {
                return Ok(false);
            }
        }
        if rec.NextEntryOffset == 0 {
            return Ok(true);
        }
        offset += rec.NextEntryOffset as usize;
    }
}

// ───────────────────────────── Walk ─────────────────────────────

/// Caps that end a walk's growth with a note instead of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_nodes: usize,
    pub max_big_files: usize,
    pub max_file_ids: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_nodes: MAX_NODES,
            max_big_files: MAX_BIG_FILES,
            max_file_ids: MAX_FILE_IDS,
        }
    }
}

/// What the walk collected, before the totals are summed.
#[derive(Debug, Default)]
pub(crate) struct Walked {
    pub nodes: Vec<Node>,
    pub big: Vec<BigFile>,
    pub completed: bool,
    pub hard_links_counted_once: u64,
    pub links_skipped: u64,
    pub denied_folders: u64,
    pub unreadable_folders: u64,
    pub online_only_files: u64,
    pub id_limit_reached: bool,
    pub node_limit_reached: bool,
    pub big_file_limit_reached: bool,
}

#[derive(Debug, Default)]
struct Arena {
    nodes: Vec<Node>,
    big: Vec<BigFile>,
    node_limit_reached: bool,
    big_file_limit_reached: bool,
}

/// Shards of the set of file ids seen, so workers rarely wait for each other.
const ID_SHARDS: usize = 16;
/// Entries between two looks at the stop flag within one folder.
const CANCEL_EVERY: u64 = 4096;
/// Least time between two progress reports.
const REPORT_EVERY: Duration = Duration::from_millis(100);

struct Shared<'a> {
    src: &'a dyn DirSource,
    root_is_volume: bool,
    windows_dir: Option<&'a Path>,
    limits: Limits,
    cancel: &'a AtomicBool,
    progress: &'a (dyn Fn(&ScanProgress) + Sync),
    arena: Mutex<Arena>,
    stack: Mutex<Vec<(u32, PathBuf, u8)>>,
    wake: Condvar,
    /// Folders queued or being listed.
    pending: AtomicUsize,
    stopped: AtomicBool,
    ids: Vec<Mutex<HashSet<u64>>>,
    id_count: AtomicUsize,
    id_limit_reached: AtomicBool,
    files: AtomicU64,
    folders: AtomicU64,
    logical: AtomicU64,
    allocated: AtomicU64,
    denied: AtomicU64,
    unreadable: AtomicU64,
    links: AtomicU64,
    hard_links: AtomicU64,
    online_files: AtomicU64,
    started: Instant,
    last_report: AtomicU64,
    current: Mutex<String>,
}

/// A folder's entries, sorted into what the arena keeps.
#[derive(Default)]
struct Listed {
    own: Totals,
    small: Small,
    big: Vec<BigFile>,
    dirs: Vec<(Box<str>, bool)>,
}

impl Shared<'_> {
    /// True the first time a file id is seen (and when ids are no longer tracked).
    fn first_link(&self, id: [u8; 16]) -> bool {
        let hash = id_hash(id);
        let shard = &self.ids[(hash % ID_SHARDS as u64) as usize];
        let mut set = shard.lock();
        if set.contains(&hash) {
            return false;
        }
        if self.id_count.load(Ordering::Relaxed) >= self.limits.max_file_ids {
            self.id_limit_reached.store(true, Ordering::Relaxed);
            return true;
        }
        set.insert(hash);
        self.id_count.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn report(&self, path: &Path, force: bool) {
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_report.load(Ordering::Relaxed);
        if !force && now < last + REPORT_EVERY.as_millis() as u64 {
            return;
        }
        if !force
            && self
                .last_report
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let current = {
            let mut current = self.current.lock();
            *current = path.display().to_string();
            current.clone()
        };
        (self.progress)(&self.snapshot(current));
    }

    fn snapshot(&self, current: String) -> ScanProgress {
        ScanProgress {
            files: self.files.load(Ordering::Relaxed),
            folders: self.folders.load(Ordering::Relaxed),
            logical_bytes: self.logical.load(Ordering::Relaxed),
            allocated_bytes: self.allocated.load(Ordering::Relaxed),
            denied_folders: self.denied.load(Ordering::Relaxed),
            current,
        }
    }

    fn worker(&self) {
        loop {
            let (node, path, flags) = {
                let mut stack = self.stack.lock();
                loop {
                    if self.cancel.load(Ordering::SeqCst) {
                        self.stopped.store(true, Ordering::SeqCst);
                        return;
                    }
                    if let Some(item) = stack.pop() {
                        break item;
                    }
                    if self.pending.load(Ordering::SeqCst) == 0 {
                        return;
                    }
                    self.wake.wait_for(&mut stack, Duration::from_millis(50));
                }
            };
            self.visit(node, &path, flags);
            if self.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.wake.notify_all();
            }
        }
    }

    /// Lists one folder and commits it.
    fn visit(&self, node: u32, path: &Path, flags: u8) {
        let mut listed = Listed::default();
        let mut seen = 0u64;
        let mut stopped = false;
        let result = self.src.list(path, &mut |entry| {
            seen += 1;
            if seen % CANCEL_EVERY == 0 && self.cancel.load(Ordering::SeqCst) {
                stopped = true;
                return false;
            }
            self.take(node, path, flags, entry, &mut listed);
            true
        });
        if stopped {
            self.stopped.store(true, Ordering::SeqCst);
        }
        let (mut error_flags, mut error_text) = (0u8, None);
        match result {
            Ok(()) => {}
            Err(ListError::Denied) => {
                error_flags |= FLAG_DENIED;
                self.denied.fetch_add(1, Ordering::Relaxed);
            }
            Err(ListError::NotPlain) => {
                error_flags |= FLAG_LINK;
                self.links.fetch_add(1, Ordering::Relaxed);
            }
            Err(ListError::Gone) => {
                error_flags |= FLAG_ERROR;
                error_text = Some("it was removed while Cairn scanned".into());
                self.unreadable.fetch_add(1, Ordering::Relaxed);
            }
            Err(ListError::Other(text)) => {
                error_flags |= FLAG_ERROR;
                error_text = Some(text.into_boxed_str());
                self.unreadable.fetch_add(1, Ordering::Relaxed);
            }
        }

        let children = self.commit(node, path, flags, (error_flags, error_text), listed);
        if !children.is_empty() {
            self.pending.fetch_add(children.len(), Ordering::SeqCst);
            let mut stack = self.stack.lock();
            for (index, name, child_flags) in children {
                stack.push((index, path.join(&*name), child_flags));
            }
            drop(stack);
            self.wake.notify_all();
        }
        self.report(path, false);
    }

    /// Sorts one entry of folder `node` into `listed`.
    fn take(&self, node: u32, path: &Path, flags: u8, entry: RawEntry, listed: &mut Listed) {
        let name = entry.name.to_string_lossy().into_owned().into_boxed_str();
        let surrogate = entry.attrs & ATTR_REPARSE != 0 && is_name_surrogate(entry.reparse_tag);
        if entry.attrs & ATTR_DIRECTORY != 0 {
            listed.dirs.push((name, surrogate));
            return;
        }
        if surrogate {
            // A file link counts as a file of no size.
            listed.own.files += 1;
            listed.small.count += 1;
            self.files.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(id) = entry.file_id {
            if !self.first_link(id) {
                self.hard_links.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        let online = is_online_only(entry.attrs);
        let mut allocated = entry.allocated;
        let packed =
            entry.attrs & (ATTR_COMPRESSED | ATTR_SPARSE) != 0 || entry.reparse_tag == TAG_WOF;
        if packed && !online {
            allocated = self
                .src
                .compressed_size(&path.join(&*name))
                .unwrap_or(allocated);
        }
        listed.own.files += 1;
        listed.own.logical += entry.logical;
        listed.own.allocated += allocated;
        if online {
            listed.own.online_only += entry.logical;
            self.online_files.fetch_add(1, Ordering::Relaxed);
        }
        self.files.fetch_add(1, Ordering::Relaxed);
        self.logical.fetch_add(entry.logical, Ordering::Relaxed);
        self.allocated.fetch_add(allocated, Ordering::Relaxed);
        if entry.logical >= BIG_FILE_MIN {
            let candidate = !online
                && entry.attrs & ATTR_SYSTEM == 0
                && flags & (FLAG_IN_WINDOWS | FLAG_IN_RECYCLE) == 0;
            listed.big.push(BigFile {
                node,
                name,
                logical: entry.logical,
                allocated,
                online_only: online,
                modified: entry.modified,
                candidate,
                id_hash: entry.file_id.map(id_hash),
            });
        } else {
            listed.small.count += 1;
            listed.small.logical += entry.logical;
            listed.small.allocated += allocated;
            listed.small.online_only += if online { entry.logical } else { 0 };
        }
    }

    /// Commits a listed folder under one lock; returns the subfolders to enter.
    fn commit(
        &self,
        node: u32,
        path: &Path,
        flags: u8,
        error: (u8, Option<Box<str>>),
        mut listed: Listed,
    ) -> Vec<(u32, Box<str>, u8)> {
        let (error_flags, error_text) = error;
        let mut enter = Vec::new();
        let mut arena = self.arena.lock();
        let room = self.limits.max_big_files.saturating_sub(arena.big.len());
        if listed.big.len() > room {
            arena.big_file_limit_reached = true;
            for file in listed.big.drain(room..) {
                listed.small.count += 1;
                listed.small.logical += file.logical;
                listed.small.allocated += file.allocated;
                if file.online_only {
                    listed.small.online_only += file.logical;
                }
            }
        }
        let files_start = arena.big.len() as u32;
        let files_len = listed.big.len() as u32;
        arena.big.append(&mut listed.big);

        let children_start = arena.nodes.len() as u32;
        let mut children_len = 0u32;
        for (name, link) in listed.dirs {
            if arena.nodes.len() >= self.limits.max_nodes {
                arena.node_limit_reached = true;
                break;
            }
            let mut child_flags = flags & (FLAG_IN_WINDOWS | FLAG_IN_RECYCLE);
            if node == 0 && self.root_is_volume && name.eq_ignore_ascii_case("$Recycle.Bin") {
                child_flags |= FLAG_IN_RECYCLE;
            }
            if self.is_windows_dir(path, &name) {
                child_flags |= FLAG_IN_WINDOWS;
            }
            if link {
                child_flags |= FLAG_LINK;
                self.links.fetch_add(1, Ordering::Relaxed);
            } else {
                self.folders.fetch_add(1, Ordering::Relaxed);
            }
            let index = arena.nodes.len() as u32;
            arena.nodes.push(Node {
                parent: node,
                name: name.clone(),
                flags: child_flags,
                ..Node::default()
            });
            children_len += 1;
            if !link {
                enter.push((index, name, child_flags));
            }
        }
        let entry = &mut arena.nodes[node as usize];
        entry.flags |= error_flags;
        entry.error = error_text;
        entry.own = listed.own;
        entry.small = listed.small;
        entry.files_start = files_start;
        entry.files_len = files_len;
        entry.children_start = children_start;
        entry.children_len = children_len;
        enter
    }

    /// Whether the subfolder `name` of `parent` is the Windows folder.
    fn is_windows_dir(&self, parent: &Path, name: &str) -> bool {
        let Some(windows) = self.windows_dir else {
            return false;
        };
        windows
            .file_name()
            .is_some_and(|w| w.eq_ignore_ascii_case(name))
            && same_path(&parent.join(name), windows)
    }
}

/// Where and how to walk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkSpec<'a> {
    pub root: &'a Path,
    /// Name of the root row ("C:\" or the folder's name).
    pub root_name: &'a str,
    pub threads: u32,
    /// Files in it are never duplicate candidates.
    pub windows_dir: Option<&'a Path>,
    pub limits: Limits,
}

/// Walks `spec.root` with `spec.threads` workers and returns what it collected. Stops early
/// (with `completed` false) when `cancel` is set.
pub(crate) fn walk(
    src: &dyn DirSource,
    spec: &WalkSpec<'_>,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&ScanProgress) + Sync),
) -> Walked {
    let WalkSpec {
        root,
        root_name,
        threads,
        windows_dir,
        limits,
    } = *spec;
    let root_is_volume = crate::storage::speed::place::is_volume_root(root);
    let mut root_flags = 0u8;
    if windows_dir.is_some_and(|w| inside(root, w)) {
        root_flags |= FLAG_IN_WINDOWS;
    }
    if root
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("$Recycle.Bin"))
    {
        root_flags |= FLAG_IN_RECYCLE;
    }
    let shared = Shared {
        src,
        root_is_volume,
        windows_dir,
        limits,
        cancel,
        progress,
        arena: Mutex::new(Arena {
            nodes: vec![Node {
                parent: 0,
                name: root_name.into(),
                flags: root_flags,
                ..Node::default()
            }],
            ..Arena::default()
        }),
        stack: Mutex::new(vec![(0, root.to_path_buf(), root_flags)]),
        wake: Condvar::new(),
        pending: AtomicUsize::new(1),
        stopped: AtomicBool::new(false),
        ids: (0..ID_SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
        id_count: AtomicUsize::new(0),
        id_limit_reached: AtomicBool::new(false),
        files: AtomicU64::new(0),
        folders: AtomicU64::new(0),
        logical: AtomicU64::new(0),
        allocated: AtomicU64::new(0),
        denied: AtomicU64::new(0),
        unreadable: AtomicU64::new(0),
        links: AtomicU64::new(0),
        hard_links: AtomicU64::new(0),
        online_files: AtomicU64::new(0),
        started: Instant::now(),
        last_report: AtomicU64::new(0),
        current: Mutex::new(String::new()),
    };
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| shared.worker());
        }
    });
    shared.report(root, true);
    let arena = shared.arena.into_inner();
    Walked {
        completed: !shared.stopped.load(Ordering::SeqCst) && !cancel.load(Ordering::SeqCst),
        hard_links_counted_once: shared.hard_links.load(Ordering::Relaxed),
        links_skipped: shared.links.load(Ordering::Relaxed),
        denied_folders: shared.denied.load(Ordering::Relaxed),
        unreadable_folders: shared.unreadable.load(Ordering::Relaxed),
        online_only_files: shared.online_files.load(Ordering::Relaxed),
        id_limit_reached: shared.id_limit_reached.load(Ordering::Relaxed),
        node_limit_reached: arena.node_limit_reached,
        big_file_limit_reached: arena.big_file_limit_reached,
        nodes: arena.nodes,
        big: arena.big,
    }
}

/// `path` is `folder` or inside it (ASCII case ignored).
pub(crate) fn inside(path: &Path, folder: &Path) -> bool {
    if same_path(path, folder) {
        return true;
    }
    let path = path.to_string_lossy().to_lowercase();
    let folder = folder.to_string_lossy().to_lowercase();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let folder = folder.strip_prefix(r"\\?\").unwrap_or(&folder);
    let folder = folder.trim_end_matches('\\');
    path.starts_with(folder) && path[folder.len()..].starts_with('\\')
}
