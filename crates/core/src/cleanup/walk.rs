//! Measuring and deleting the contents of one validated folder.
//!
//! A [`SafeRoot`] is resolved once from a well-known location. It is refused when the path
//! is not an absolute path on a fixed local drive letter (decided before anything is
//! opened, so a share is never contacted), is a reparse point, does not resolve to a fixed
//! local volume, sits directly on a drive root, is one of the protected system and profile
//! folders or contains one of them, or lies inside a user profile but outside that
//! profile's `AppData\Local` folder. Protected folders are compared both by path and by file
//! identity (volume serial number and file ID), so another spelling of the same folder is
//! refused as well. [`measure`] and [`purge`] then walk the root's contents without
//! following reparse points (junctions, symbolic links, mount points, cloud placeholders):
//! such entries are neither entered nor deleted.
//!
//! Deletion never trusts a path alone. Each file or directory is opened itself
//! (`FILE_FLAG_OPEN_REPARSE_POINT`), the open handle is checked to be an ordinary entry
//! whose final path lies strictly inside the root, and the entry is deleted through that
//! same handle. A folder swapped for a link after the walk listed it therefore cannot
//! redirect a deletion outside the root. Directories are only removed when empty, never
//! recursively, and the root itself is never removed. Files that a pending reboot-time
//! rename will move into place (`PendingFileRenameOperations`) are never deleted.
//!
//! With an age limit ([`Filter::older_than`]) a file is eligible only when its creation,
//! last-write and change times are all older than the limit, and a subfolder created within
//! the limit is neither entered nor removed. The change time is the NTFS metadata time: every
//! write, rename or attribute change moves it forward and `SetFileTime` cannot set it, so a
//! file that an installer extracted recently and stamped with its original build date still
//! counts as recent.

use std::collections::HashSet;
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_DIR_NOT_EMPTY, ERROR_LOCK_VIOLATION, ERROR_NO_MORE_FILES, ERROR_SHARING_VIOLATION,
    FILETIME, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    FileBasicInfo, FileDispositionInfo, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    FileIdInfo, GetDriveTypeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, GetVolumePathNameW, SetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, GETFINALPATHNAMEBYHANDLE_FLAGS,
    VOLUME_NAME_DOS,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Favorites,
    FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesCommon,
    FOLDERID_ProgramFilesX86, FOLDERID_Public, FOLDERID_PublicDesktop, FOLDERID_PublicDocuments,
    FOLDERID_RoamingAppData, FOLDERID_SavedGames, FOLDERID_SkyDrive, FOLDERID_System,
    FOLDERID_SystemX86, FOLDERID_UserProfiles, FOLDERID_UserProgramFiles, FOLDERID_Videos,
    FOLDERID_Windows,
};

use crate::win::registry::{Hive, Key};

/// Most error messages kept per target.
pub(crate) const MAX_ERRORS: usize = 5;

const REPARSE: u32 = FILE_ATTRIBUTE_REPARSE_POINT.0;
const DIRECTORY: u32 = FILE_ATTRIBUTE_DIRECTORY.0;
const READONLY: u32 = FILE_ATTRIBUTE_READONLY.0;
/// Attributes that `FILE_BASIC_INFO` may set on an existing entry.
const SETTABLE: u32 = FILE_ATTRIBUTE_READONLY.0
    | FILE_ATTRIBUTE_HIDDEN.0
    | FILE_ATTRIBUTE_SYSTEM.0
    | FILE_ATTRIBUTE_ARCHIVE.0
    | FILE_ATTRIBUTE_TEMPORARY.0
    | FILE_ATTRIBUTE_OFFLINE.0
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.0;
const SHARE_ALL: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

const SHARING_VIOLATION: i32 = ERROR_SHARING_VIOLATION.0 as i32;
const LOCK_VIOLATION: i32 = ERROR_LOCK_VIOLATION.0 as i32;
const DIR_NOT_EMPTY: i32 = ERROR_DIR_NOT_EMPTY.0 as i32;

/// `GetDriveTypeW` result for a fixed local disk.
const DRIVE_FIXED: u32 = 3;

/// 100 ns intervals between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 116_444_736_000_000_000;

/// Registry key holding the reboot-time rename and delete queue.
const SESSION_MANAGER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";
const PENDING_RENAME_VALUES: [&str; 2] = [
    "PendingFileRenameOperations",
    "PendingFileRenameOperations2",
];

// ───────────────────────────── Locations ─────────────────────────────

/// Value of an environment variable as a path; `None` when unset or empty.
pub(crate) fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Path of a shell known folder (for example `FOLDERID_LocalAppData`), read from the shell
/// rather than from environment variables, whether or not the folder exists.
pub(crate) fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    crate::win::paths::known_folder(id)
        .map_err(|e| format!("the shell folder {id:?} cannot be determined: {e}"))
}

#[link(name = "kernel32")]
extern "system" {
    fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
}

/// The Windows folder as reported by `GetWindowsDirectoryW`, independent of `%SystemRoot%`.
pub(crate) fn windows_dir() -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 260];
    loop {
        // SAFETY: `buf` is writable for `buf.len()` UTF-16 units.
        let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return Err(format!(
                "the Windows folder cannot be determined: {}",
                io::Error::last_os_error()
            ));
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(n + 1, 0);
    }
}

// ───────────────────────────── Paths ─────────────────────────────

/// Path for messages: the verbatim `\\?\` prefix removed.
pub(crate) fn display(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

/// Case-insensitive comparison key: no verbatim prefix, backslashes, no trailing separator.
pub(crate) fn key(path: &Path) -> String {
    display(path)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Number of named components below the drive or share root.
fn depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

/// Drive letter of `C:\...` or `\\?\C:\...`; `None` for UNC shares, device paths and
/// relative paths.
fn drive_letter(path: &Path) -> Option<u8> {
    match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
            _ => None,
        },
        _ => None,
    }
}

/// True for an absolute path without `.` or `..` components on a drive letter that is a
/// fixed local disk. Decided from the spelling and the drive letter's type alone, so a
/// share or a mapped network drive is never contacted.
pub(crate) fn on_local_disk(path: &Path) -> bool {
    let Some(letter) = drive_letter(path).filter(|_| is_plain_absolute(path)) else {
        return false;
    };
    let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: `root` is a NUL-terminated drive root that outlives the call.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

/// `fs::canonicalize` of a path on a fixed local disk. `None` when it cannot be resolved or
/// is not on one; such a path is never opened.
pub(crate) fn canonical_local(path: &Path) -> Option<PathBuf> {
    if on_local_disk(path) {
        fs::canonicalize(path).ok()
    } else {
        None
    }
}

/// True when the entry at `path` itself (not what it points to) is a reparse point: a
/// junction, symbolic link or mount point.
pub(crate) fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & REPARSE != 0)
}

/// True for a canonical `\\?\C:\...` path, the form a local folder resolves to. A mapped
/// network drive or a share resolves to `\\?\UNC\...` instead.
fn verbatim_disk(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::VerbatimDisk(_))
    )
}

fn wide_z(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// True when the volume holding `path` is a fixed local disk (not removable, network,
/// optical or RAM disk). The volume is found through its mount point, so a volume mounted
/// in a folder is judged by itself rather than by the drive that holds the folder.
fn on_fixed_volume(path: &Path) -> bool {
    let name = wide_z(path);
    let mut volume = vec![0u16; name.len().max(64) + 1];
    // SAFETY: `name` is NUL-terminated and `volume` is writable for its length.
    if unsafe { GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut volume) }.is_err() {
        return false;
    }
    let end = volume.iter().position(|&c| c == 0).unwrap_or(volume.len());
    volume.truncate(end);
    volume.push(0);
    // SAFETY: `volume` is a NUL-terminated volume mount point.
    unsafe { GetDriveTypeW(PCWSTR(volume.as_ptr())) == DRIVE_FIXED }
}

/// Ordinary subfolders of `dir` (never reparse points) whose name passes `keep`, sorted.
/// An unreadable or missing `dir` yields nothing.
pub(crate) fn plain_subfolders(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(items) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = items
        .flatten()
        .filter(|item| {
            item.metadata()
                .is_ok_and(|m| m.is_dir() && m.file_attributes() & REPARSE == 0)
        })
        .filter(|item| keep(&item.file_name().to_string_lossy()))
        .map(|item| item.path())
        .collect();
    out.sort();
    out
}

fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

// ───────────────────────────── Guards ─────────────────────────────

/// Identity of a file or folder: volume serial number and 128-bit file ID. Two paths with
/// the same identity name the same object, however they are spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileId {
    volume: u64,
    file: [u8; 16],
}

fn file_id(file: &File) -> io::Result<FileId> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `info` is a FILE_ID_INFO that outlives the call; the size matches it.
    unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut c_void,
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(to_io)?;
    Ok(FileId {
        volume: info.VolumeSerialNumber,
        file: info.FileId.Identifier,
    })
}

/// Identity of the folder `path` names, following links.
fn folder_id(path: &Path) -> Option<FileId> {
    let dir = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .ok()?;
    file_id(&dir).ok()
}

/// Environment variables naming folders that are never a cleanup root.
const PROTECTED_VARS: [&str; 11] = [
    "SystemRoot",
    "windir",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
    "ALLUSERSPROFILE",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "PUBLIC",
];

/// Shell folders that are never a cleanup root.
const PROTECTED_FOLDERS: [GUID; 25] = [
    FOLDERID_Windows,
    FOLDERID_System,
    FOLDERID_SystemX86,
    FOLDERID_ProgramFiles,
    FOLDERID_ProgramFilesX86,
    FOLDERID_ProgramFilesCommon,
    FOLDERID_ProgramData,
    FOLDERID_UserProfiles,
    FOLDERID_Public,
    FOLDERID_PublicDesktop,
    FOLDERID_PublicDocuments,
    FOLDERID_Profile,
    FOLDERID_LocalAppData,
    FOLDERID_LocalAppDataLow,
    FOLDERID_RoamingAppData,
    FOLDERID_UserProgramFiles,
    FOLDERID_Desktop,
    FOLDERID_Documents,
    FOLDERID_Downloads,
    FOLDERID_Pictures,
    FOLDERID_Music,
    FOLDERID_Videos,
    FOLDERID_Favorites,
    FOLDERID_SavedGames,
    FOLDERID_SkyDrive,
];

/// Folders that must never be used as a cleanup root, and files that must never be deleted.
#[derive(Debug, Clone, Default)]
pub(crate) struct Guards {
    /// Comparison keys of the protected folders, as written and as resolved.
    keys: Vec<String>,
    /// Identities of the protected folders and of every folder above them.
    ids: HashSet<FileId>,
    /// Keys of user profile folders: inside one, only its `AppData\Local` subtree is allowed.
    profiles: Vec<String>,
    /// Keys of folders holding user profiles (`C:\Users`): inside one, only
    /// `<profile>\AppData\Local\...` is allowed.
    profile_parents: Vec<String>,
    /// Keys of files that a pending reboot-time rename will move into place.
    pinned: HashSet<String>,
}

impl Guards {
    /// The protected system and profile folders of this machine and user, taken from the
    /// shell's known folders, `GetWindowsDirectoryW` and the environment (both as written
    /// and as resolved on disk), plus the sources of pending reboot-time renames.
    pub(crate) fn current() -> Guards {
        let mut protected: Vec<PathBuf> =
            PROTECTED_VARS.iter().filter_map(|v| env_path(v)).collect();
        protected.extend(
            PROTECTED_FOLDERS
                .iter()
                .filter_map(|id| known_folder(id).ok()),
        );
        let windows_dirs = env_path("SystemRoot").into_iter().chain(windows_dir().ok());
        for system in windows_dirs.collect::<Vec<_>>() {
            for sub in ["System32", "SysWOW64", "WinSxS"] {
                protected.push(system.join(sub));
            }
            protected.push(system);
        }
        let profiles: Vec<PathBuf> = known_folder(&FOLDERID_Profile)
            .ok()
            .into_iter()
            .chain(env_path("USERPROFILE"))
            .collect();
        let mut parents: Vec<PathBuf> = known_folder(&FOLDERID_UserProfiles)
            .ok()
            .into_iter()
            .collect();
        for profile in &profiles {
            protected.push(profile.join("AppData"));
            protected.push(profile.join(r"AppData\Local"));
            parents.extend(profile.parent().map(Path::to_path_buf));
        }
        let mut guards = Guards::build(&protected, &profiles, &parents);
        guards.pinned = pending_moves();
        guards
    }

    /// Guards over the given folders. `profiles` and `profile_parents` are protected
    /// themselves and also restrict roots inside them to `AppData\Local`.
    fn build(protected: &[PathBuf], profiles: &[PathBuf], profile_parents: &[PathBuf]) -> Guards {
        let mut keys = Vec::new();
        let mut ids = HashSet::new();
        let mut visited = HashSet::new();
        for dir in protected.iter().chain(profiles).chain(profile_parents) {
            keys.push(key(dir));
            // Only folders on a fixed local disk are resolved and identified: every root
            // lies on one, and opening a folder on a share (a Documents folder redirected
            // to a server, say) can block until the connection times out.
            if !on_local_disk(dir) {
                continue;
            }
            let resolved = canonical_local(dir);
            if let Some(resolved) = &resolved {
                keys.push(key(resolved));
            }
            let chain = resolved.as_deref().unwrap_or(dir);
            if !on_local_disk(chain) {
                continue;
            }
            // Every folder above a protected folder is refused too; each chain is walked
            // once, since a visited folder's own ancestors were visited with it.
            for folder in chain.ancestors() {
                if !visited.insert(key(folder)) {
                    break;
                }
                ids.extend(folder_id(folder));
            }
        }
        let with_resolved = |dirs: &[PathBuf]| {
            let mut out: Vec<String> = dirs
                .iter()
                .flat_map(|d| [Some(key(d)), canonical_local(d).map(|r| key(&r))])
                .flatten()
                .collect();
            out.sort();
            out.dedup();
            out
        };
        keys.sort();
        keys.dedup();
        Guards {
            keys,
            ids,
            profiles: with_resolved(profiles),
            profile_parents: with_resolved(profile_parents),
            pinned: HashSet::new(),
        }
    }

    /// True when `k` (a comparison key) is inside a user profile but not strictly inside
    /// that profile's `AppData\Local` folder.
    fn outside_profile_cache(&self, k: &str) -> bool {
        for profile in &self.profiles {
            if k == profile || k.starts_with(&format!("{profile}\\")) {
                return !k.starts_with(&format!("{profile}\\appdata\\local\\"));
            }
        }
        for parent in &self.profile_parents {
            if let Some(rest) = k.strip_prefix(&format!("{parent}\\")) {
                let parts: Vec<&str> = rest.split('\\').collect();
                return !(parts.len() >= 4 && parts[1] == "appdata" && parts[2] == "local");
            }
        }
        false
    }

    /// Why a folder cannot be a cleanup root, checking the given path, the path it resolves
    /// to and its identity; `None` when it may be one.
    fn refusal(&self, given: &Path, canonical: &Path, id: Option<FileId>) -> Option<String> {
        let shown = display(given);
        for k in [key(given), key(canonical)] {
            if self.keys.contains(&k) {
                return Some(format!("{shown} is a protected folder"));
            }
            let inside = format!("{k}\\");
            if self.keys.iter().any(|g| g.starts_with(&inside)) {
                return Some(format!("{shown} contains a protected folder"));
            }
            if self.outside_profile_cache(&k) {
                return Some(format!(
                    "{shown} is in a user profile but not in its AppData\\Local folder"
                ));
            }
        }
        if id.is_some_and(|id| self.ids.contains(&id)) {
            return Some(format!("{shown} is or contains a protected folder"));
        }
        None
    }

    /// True when a pending reboot-time rename will move the file at `real` into place.
    fn pins(&self, real: &Path) -> bool {
        !self.pinned.is_empty() && self.pinned.contains(&key(real))
    }
}

/// Keys of the files that `PendingFileRenameOperations` will move at the next boot.
fn pending_moves() -> HashSet<String> {
    let Ok(Some(session_manager)) = Key::open(Hive::LocalMachine, SESSION_MANAGER, false) else {
        return HashSet::new();
    };
    PENDING_RENAME_VALUES
        .iter()
        .filter_map(|name| session_manager.query_raw(name).ok().flatten())
        .flat_map(|value| pending_sources(&value.data))
        .map(|source| match canonical_local(&source) {
            Some(resolved) => key(&resolved),
            None => key(&source),
        })
        .collect()
}

/// Sources of the (source, destination) pairs in a `PendingFileRenameOperations` value
/// whose destination is not empty. An empty destination means delete at boot, which a
/// cleanup may do early; a named one means the file is waiting to be moved into place.
fn pending_sources(data: &[u8]) -> Vec<PathBuf> {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // An empty destination is stored as an empty string, so the value is split on every
    // NUL rather than read as an ordinary REG_MULTI_SZ list.
    let parts: Vec<String> = units
        .split(|&u| u == 0)
        .map(String::from_utf16_lossy)
        .collect();
    parts
        .chunks_exact(2)
        .filter(|pair| !pair[0].is_empty() && !pair[1].is_empty())
        .map(|pair| PathBuf::from(nt_to_win32(&pair[0])))
        .collect()
}

/// `\??\C:\x` (with an optional `*<n>` or `!` marker) as `C:\x`.
fn nt_to_win32(path: &str) -> &str {
    let path = path.trim_start_matches(|c: char| c == '*' || c == '!' || c.is_ascii_digit());
    path.strip_prefix(r"\??\")
        .or_else(|| path.strip_prefix(r"\\?\"))
        .unwrap_or(path)
}

// ───────────────────────────── Roots ─────────────────────────────

/// Outcome of validating a location.
#[derive(Debug)]
pub(crate) enum Resolved<T> {
    Ready(T),
    /// Nothing exists there; the location counts as empty.
    Missing,
    /// The file system refused access (typically needs an elevated process).
    Denied,
    /// The location failed a safety check and is never touched.
    Refused(String),
}

fn resolve_error<T>(path: &Path, e: io::Error) -> Resolved<T> {
    match e.kind() {
        io::ErrorKind::NotFound => Resolved::Missing,
        io::ErrorKind::PermissionDenied => Resolved::Denied,
        _ => Resolved::Refused(format!("cannot inspect {}: {e}", display(path))),
    }
}

/// A validated folder whose contents may be measured and deleted. Holds the resolved
/// (final, verbatim) path; the folder itself is never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafeRoot {
    path: PathBuf,
}

impl SafeRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Opens an entry itself (never a link's target) to inspect it.
fn open_to_inspect(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

/// Validates a cleanup folder: an absolute path on a fixed local drive letter to an
/// ordinary folder (not a reparse point) that resolves to a fixed local volume, at least two
/// levels below the drive root, that is not a protected folder, does not contain one and,
/// inside a user profile, lies within its `AppData\Local` folder. Any other drive or path
/// form is refused before it is opened. The folder is opened once and its type, final path
/// and identity are all read from that handle.
pub(crate) fn resolve_root(path: &Path, guards: &Guards) -> Resolved<SafeRoot> {
    if !is_plain_absolute(path) {
        return Resolved::Refused(format!("{} is not an absolute path", display(path)));
    }
    if !on_local_disk(path) {
        return Resolved::Refused(format!("{} is not on a fixed local drive", display(path)));
    }
    let dir = match open_to_inspect(path) {
        Ok(dir) => dir,
        Err(e) => return resolve_error(path, e),
    };
    let info = match handle_info(&dir) {
        Ok(info) => info,
        Err(e) => return resolve_error(path, e),
    };
    if info.attrs & REPARSE != 0 {
        return Resolved::Refused(format!("{} is a link or mount point", display(path)));
    }
    if info.attrs & DIRECTORY == 0 {
        return Resolved::Refused(format!("{} is not a folder", display(path)));
    }
    let canonical = match final_path(&dir) {
        Ok(c) => c,
        Err(e) => return resolve_error(path, e),
    };
    if !verbatim_disk(&canonical) || !on_fixed_volume(&canonical) {
        return Resolved::Refused(format!(
            "{} is not on a fixed local drive (it resolves to {})",
            display(path),
            display(&canonical)
        ));
    }
    if depth(path) < 2 || depth(&canonical) < 2 {
        return Resolved::Refused(format!("{} is too close to a drive root", display(path)));
    }
    if let Some(reason) = guards.refusal(path, &canonical, file_id(&dir).ok()) {
        return Resolved::Refused(reason);
    }
    Resolved::Ready(SafeRoot { path: canonical })
}

/// A validated single file (for example `C:\Windows\MEMORY.DMP`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafeFile {
    path: PathBuf,
    size: u64,
}

impl SafeFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }
}

/// Validates one file: an absolute path on a fixed local drive letter to an ordinary file
/// (not a reparse point) whose resolved location is the same file name inside the resolved
/// parent folder, on a fixed local volume.
pub(crate) fn resolve_file(path: &Path) -> Resolved<SafeFile> {
    if !is_plain_absolute(path) {
        return Resolved::Refused(format!("{} is not an absolute path", display(path)));
    }
    if !on_local_disk(path) {
        return Resolved::Refused(format!("{} is not on a fixed local drive", display(path)));
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Resolved::Refused(format!("{} has no parent folder", display(path)));
    };
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return resolve_error(path, e),
    };
    if meta.file_attributes() & REPARSE != 0 {
        return Resolved::Refused(format!("{} is a link", display(path)));
    }
    if !meta.is_file() {
        return Resolved::Refused(format!("{} is not a file", display(path)));
    }
    let (canonical, canonical_parent) = match (fs::canonicalize(path), fs::canonicalize(parent)) {
        (Ok(c), Ok(p)) => (c, p),
        (Err(e), _) | (_, Err(e)) => return resolve_error(path, e),
    };
    let same_place = canonical.parent() == Some(canonical_parent.as_path())
        && canonical.file_name().is_some_and(|n| {
            n.to_string_lossy().to_lowercase() == name.to_string_lossy().to_lowercase()
        });
    if !same_place || depth(&canonical_parent) < 1 {
        return Resolved::Refused(format!("{} resolves elsewhere", display(path)));
    }
    if !verbatim_disk(&canonical) || !on_fixed_volume(&canonical) {
        return Resolved::Refused(format!("{} is not on a fixed local drive", display(path)));
    }
    Resolved::Ready(SafeFile {
        path: canonical,
        size: meta.len(),
    })
}

// ───────────────────────────── Filters ─────────────────────────────

/// Which files under a root are eligible for deletion.
#[derive(Debug, Clone)]
pub(crate) struct Filter {
    min_age: Option<Duration>,
    prefix: &'static str,
    suffix: &'static str,
    recursive: bool,
}

impl Filter {
    /// Every file in the folder and its subfolders.
    pub(crate) fn everything() -> Filter {
        Filter {
            min_age: None,
            prefix: "",
            suffix: "",
            recursive: true,
        }
    }

    /// Only files whose creation, last-write and change times are all more than `age` ago,
    /// in subfolders that were themselves created more than `age` ago. The change time is
    /// re-read through the file's own handle right before deletion; `SetFileTime` cannot
    /// move it back, so a file extracted recently with a backdated creation and last-write
    /// time still counts as recent, and so does everything in a freshly created folder.
    pub(crate) fn older_than(mut self, age: Duration) -> Filter {
        self.min_age = Some(age);
        self
    }

    /// Only files named `<prefix>*<suffix>`; both parts are lowercase ASCII and compared
    /// case-insensitively.
    pub(crate) fn named(mut self, prefix: &'static str, suffix: &'static str) -> Filter {
        self.prefix = prefix;
        self.suffix = suffix;
        self
    }

    /// Only files directly inside the root; subfolders are not entered.
    pub(crate) fn top_level(mut self) -> Filter {
        self.recursive = false;
        self
    }

    /// True when recently changed files are left in place.
    #[cfg(test)]
    pub(crate) fn keeps_recent(&self) -> bool {
        self.min_age.is_some()
    }

    fn has_pattern(&self) -> bool {
        !self.prefix.is_empty() || !self.suffix.is_empty()
    }

    fn matches(&self, name: &OsStr) -> bool {
        if !self.has_pattern() {
            return true;
        }
        let name = name.to_string_lossy().to_ascii_lowercase();
        name.len() >= self.prefix.len() + self.suffix.len()
            && name.starts_with(self.prefix)
            && name.ends_with(self.suffix)
    }

    /// Latest time an eligible file may carry; `None` when age does not matter.
    fn cutoff(&self, now: SystemTime) -> Option<SystemTime> {
        self.min_age
            .map(|age| now.checked_sub(age).unwrap_or(UNIX_EPOCH))
    }
}

/// Creation, last-write and change (metadata) times of a file or folder. `None` where the
/// file system does not keep that time (FAT volumes have no change time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Times {
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    changed: Option<SystemTime>,
}

/// True when every known time is at or before the cutoff (always, without a cutoff).
fn old_enough(cutoff: Option<SystemTime>, times: &Times) -> bool {
    match cutoff {
        None => true,
        Some(limit) => {
            let before = |t: Option<SystemTime>| t.map_or(true, |t| t <= limit);
            times.modified.is_some_and(|m| m <= limit)
                && before(times.created)
                && before(times.changed)
        }
    }
}

/// True when a folder was created at or before the cutoff (always, without a cutoff).
fn created_before(cutoff: Option<SystemTime>, times: &Times) -> bool {
    cutoff.map_or(true, |limit| times.created.map_or(true, |c| c <= limit))
}

// ───────────────────────────── Walking ─────────────────────────────

/// Size of the eligible files under a root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Measure {
    pub(crate) bytes: u64,
    pub(crate) files: u64,
    /// Listing the root itself was refused.
    pub(crate) denied: bool,
}

impl Measure {
    pub(crate) fn add(&mut self, other: Measure) {
        self.bytes += other.bytes;
        self.files += other.files;
        self.denied |= other.denied;
    }
}

/// Result of deleting the eligible files under a root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Purge {
    pub(crate) freed_bytes: u64,
    pub(crate) deleted_files: u64,
    /// Eligible-looking entries left in place: too recent (including freshly created
    /// folders), in use, links, pending a reboot-time move, not deletable.
    pub(crate) skipped_files: u64,
    pub(crate) removed_dirs: u64,
    /// Listing the root itself was refused.
    pub(crate) denied: bool,
    /// Up to [`MAX_ERRORS`] representative error messages.
    pub(crate) errors: Vec<String>,
    /// Every error noted, including those beyond the stored messages.
    pub(crate) error_count: u64,
}

impl Purge {
    fn note(&mut self, message: String) {
        self.error_count += 1;
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(message);
        }
    }

    fn note_failure(&mut self, path: &Path, failure: Failure) {
        match failure {
            Failure::Gone
            | Failure::InUse
            | Failure::Changed
            | Failure::Recent
            | Failure::Pinned
            | Failure::NotEmpty => {}
            Failure::Outside(real) => self.note(format!(
                "{} left in place: it resolves to {} outside the cleanup folder",
                display(path),
                display(&real)
            )),
            Failure::Io(e) => self.note(format!("{}: {e}", display(path))),
        }
    }
}

/// One directory entry, with the metadata the file system keeps in the directory itself.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    name: OsString,
    attrs: u32,
    len: u64,
    times: Times,
}

impl Entry {
    fn is_reparse(&self) -> bool {
        self.attrs & REPARSE != 0
    }

    fn is_dir(&self) -> bool {
        self.attrs & DIRECTORY != 0
    }
}

/// Opens a folder for listing after checking through the same handle that it is an
/// ordinary directory. `Ok(None)` when it is a reparse point or not a directory.
fn open_plain_dir(path: &Path) -> io::Result<Option<(File, HandleInfo)>> {
    let dir = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let info = handle_info(&dir)?;
    if info.attrs & REPARSE != 0 || info.attrs & DIRECTORY == 0 {
        return Ok(None);
    }
    Ok(Some((dir, info)))
}

/// Size of the listing buffer in 8-byte words (64 KiB); directory records are 8-byte aligned.
const LIST_BUFFER_WORDS: usize = 8 * 1024;

/// Every entry of an open directory (`.` and `..` excluded), read with
/// `FileFullDirectoryInfo` so that each entry carries its creation, last-write and change
/// times. Entries are never opened, so reparse points are never followed.
fn read_entries(dir: &File, path: &Path) -> io::Result<Vec<Entry>> {
    let mut buf = vec![0u64; LIST_BUFFER_WORDS];
    let len = LIST_BUFFER_WORDS * size_of::<u64>();
    let name_at = offset_of!(FILE_FULL_DIR_INFO, FileName);
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed directory listing");
    let mut class = FileFullDirectoryRestartInfo;
    let mut out = Vec::new();
    loop {
        // SAFETY: `buf` is writable for `len` bytes and outlives the call.
        let filled = unsafe {
            GetFileInformationByHandleEx(
                raw(dir),
                class,
                buf.as_mut_ptr().cast::<c_void>(),
                len as u32,
            )
        };
        match filled {
            Ok(()) => {}
            Err(e) if crate::win::is_win32(&e, ERROR_NO_MORE_FILES) => break,
            Err(e) => return Err(to_io(e)),
        }
        class = FileFullDirectoryInfo;
        let base = buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            if offset + size_of::<FILE_FULL_DIR_INFO>() > len {
                return Err(malformed());
            }
            // SAFETY: the record header lies inside `buf` (checked above); it is copied out
            // unaligned, so no reference into the buffer is formed.
            let rec: FILE_FULL_DIR_INFO =
                unsafe { std::ptr::read_unaligned(base.add(offset).cast::<FILE_FULL_DIR_INFO>()) };
            let name_units = rec.FileNameLength as usize / 2;
            if offset + name_at + name_units * 2 > len {
                return Err(malformed());
            }
            // SAFETY: the name lies inside `buf` (checked above) and starts at an even
            // offset, so it is a properly aligned run of `name_units` UTF-16 units.
            let name = unsafe {
                std::slice::from_raw_parts(base.add(offset + name_at).cast::<u16>(), name_units)
            };
            let name = OsString::from_wide(name);
            if name != "." && name != ".." {
                out.push(Entry {
                    path: path.join(&name),
                    name,
                    attrs: rec.FileAttributes,
                    len: u64::try_from(rec.EndOfFile).unwrap_or(0),
                    times: Times {
                        created: ticks_time(rec.CreationTime),
                        modified: ticks_time(rec.LastWriteTime),
                        changed: ticks_time(rec.ChangeTime),
                    },
                });
            }
            if rec.NextEntryOffset == 0 {
                break;
            }
            offset += rec.NextEntryOffset as usize;
        }
    }
    Ok(out)
}

/// Why a subfolder was not entered.
enum Skip {
    /// It became a reparse point or something other than a directory.
    NotPlain,
    /// It was created within the filter's age limit.
    TooNew,
}

/// Opens and lists a folder after confirming through its handle that it is an ordinary
/// directory and, with a cutoff, that it was created at or before it. Also returns the
/// folder's own times.
fn enter(path: &Path, cutoff: Option<SystemTime>) -> io::Result<Result<(Vec<Entry>, Times), Skip>> {
    let Some((dir, info)) = open_plain_dir(path)? else {
        return Ok(Err(Skip::NotPlain));
    };
    if !created_before(cutoff, &info.times) {
        return Ok(Err(Skip::TooNew));
    }
    Ok(Ok((read_entries(&dir, path)?, info.times)))
}

/// Read-only: sizes the eligible files under `root`. Reparse points are neither counted
/// nor entered, and with an age limit neither are subfolders created within it;
/// unreadable subfolders are skipped.
pub(crate) fn measure(root: &SafeRoot, filter: &Filter) -> Measure {
    let cutoff = filter.cutoff(SystemTime::now());
    let mut out = Measure::default();
    let mut pending = vec![root.path.clone()];
    while let Some(dir) = pending.pop() {
        // The root's own age does not matter; only folders below it must be old enough.
        let dir_cutoff = if dir == root.path { None } else { cutoff };
        let entries = match enter(&dir, dir_cutoff) {
            Ok(Ok((entries, _))) => entries,
            Ok(Err(_)) => continue,
            Err(e) => {
                if dir == root.path && e.kind() == io::ErrorKind::PermissionDenied {
                    out.denied = true;
                }
                continue;
            }
        };
        for entry in entries {
            if entry.is_reparse() {
                continue;
            }
            if entry.is_dir() {
                if filter.recursive {
                    pending.push(entry.path);
                }
                continue;
            }
            if filter.matches(&entry.name) && old_enough(cutoff, &entry.times) {
                out.bytes += entry.len;
                out.files += 1;
            }
        }
    }
    out
}

struct Frame {
    path: PathBuf,
    entries: std::vec::IntoIter<Entry>,
    /// Something beneath this directory was deleted during this purge.
    deleted_any: bool,
    /// Already empty before the purge and old enough to go.
    remove_if_empty: bool,
}

/// Deletes the eligible files under `root` (never the root itself) and then removes
/// subdirectories that this purge emptied, deepest first. Subdirectories that were already
/// empty are removed too when the filter has no name pattern and they pass its age rule.
/// With an age limit, a subdirectory created within it is left alone entirely. Files that
/// cannot be deleted are counted as skipped and not retried.
pub(crate) fn purge(root: &SafeRoot, filter: &Filter, guards: &Guards) -> Purge {
    let cutoff = filter.cutoff(SystemTime::now());
    let mut out = Purge::default();
    let entries = match enter(&root.path, None) {
        Ok(Ok((entries, _))) => entries,
        Ok(Err(_)) => {
            out.note(format!(
                "{} is no longer an ordinary folder",
                display(&root.path)
            ));
            return out;
        }
        Err(e) => {
            out.denied = e.kind() == io::ErrorKind::PermissionDenied;
            if e.kind() != io::ErrorKind::NotFound {
                out.note(format!("cannot list {}: {e}", display(&root.path)));
            }
            return out;
        }
    };
    let mut stack = vec![Frame {
        path: root.path.clone(),
        entries: entries.into_iter(),
        deleted_any: false,
        remove_if_empty: false,
    }];

    while let Some(top) = stack.last_mut() {
        let Some(entry) = top.entries.next() else {
            let Some(frame) = stack.pop() else { break };
            // The root frame has no parent and is never removed.
            let Some(parent) = stack.last_mut() else {
                break;
            };
            if frame.deleted_any || frame.remove_if_empty {
                let untouched = !frame.deleted_any;
                match remove_empty_dir(&frame.path, &root.path, cutoff, untouched) {
                    Ok(()) => {
                        out.removed_dirs += 1;
                        parent.deleted_any = true;
                    }
                    Err(failure) => out.note_failure(&frame.path, failure),
                }
            }
            continue;
        };

        if entry.is_reparse() {
            out.skipped_files += 1;
            continue;
        }
        if entry.is_dir() {
            if !filter.recursive {
                continue;
            }
            match enter(&entry.path, cutoff) {
                Ok(Ok((children, times))) => {
                    let remove_if_empty =
                        children.is_empty() && !filter.has_pattern() && old_enough(cutoff, &times);
                    stack.push(Frame {
                        path: entry.path,
                        entries: children.into_iter(),
                        deleted_any: false,
                        remove_if_empty,
                    });
                }
                Ok(Err(Skip::NotPlain | Skip::TooNew)) => out.skipped_files += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => out.note(format!("cannot list {}: {e}", display(&entry.path))),
            }
            continue;
        }
        if !filter.matches(&entry.name) {
            continue;
        }
        if !old_enough(cutoff, &entry.times) {
            out.skipped_files += 1;
            continue;
        }
        match delete_file(&entry.path, Scope::Inside(&root.path), cutoff, guards) {
            Ok(size) => {
                out.freed_bytes += size;
                out.deleted_files += 1;
                if let Some(top) = stack.last_mut() {
                    top.deleted_any = true;
                }
            }
            Err(Failure::Gone) => {}
            Err(failure) => {
                out.skipped_files += 1;
                out.note_failure(&entry.path, failure);
            }
        }
    }
    out
}

/// Deletes one validated file.
pub(crate) fn purge_file(file: &SafeFile, guards: &Guards) -> Purge {
    let mut out = Purge::default();
    match delete_file(&file.path, Scope::Exactly(&file.path), None, guards) {
        Ok(size) => {
            out.freed_bytes = size;
            out.deleted_files = 1;
        }
        Err(Failure::Gone) => {}
        Err(failure) => {
            out.skipped_files = 1;
            out.note_failure(&file.path, failure);
        }
    }
    out
}

// ───────────────────────────── Deletion ─────────────────────────────

/// Why an entry was not deleted.
#[derive(Debug)]
enum Failure {
    /// It vanished before it could be deleted.
    Gone,
    /// Open elsewhere without delete sharing, or locked.
    InUse,
    /// No longer eligible: became a link or another kind of entry, or has several hard
    /// links.
    Changed,
    /// Created, written or changed within the age limit.
    Recent,
    /// A pending reboot-time rename will move it into place.
    Pinned,
    /// Its final path is not inside the permitted scope (reached through a link).
    Outside(PathBuf),
    /// A directory that still has entries.
    NotEmpty,
    Io(io::Error),
}

fn classify(e: io::Error) -> Failure {
    match e.raw_os_error() {
        Some(SHARING_VIOLATION | LOCK_VIOLATION) => Failure::InUse,
        Some(DIR_NOT_EMPTY) => Failure::NotEmpty,
        _ if e.kind() == io::ErrorKind::NotFound => Failure::Gone,
        _ => Failure::Io(e),
    }
}

fn to_io(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

/// Where a deleted entry's final path must lie.
#[derive(Debug, Clone, Copy)]
enum Scope<'a> {
    /// Strictly below this resolved folder.
    Inside(&'a Path),
    /// Exactly this resolved path.
    Exactly(&'a Path),
}

impl Scope<'_> {
    fn allows(self, real: &Path) -> bool {
        match self {
            Scope::Inside(root) => real != root && real.starts_with(root),
            Scope::Exactly(path) => real == path,
        }
    }
}

struct HandleInfo {
    attrs: u32,
    size: u64,
    links: u32,
    times: Times,
}

fn raw(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

/// A FILETIME tick count (100 ns since 1601) as a time; `None` for zero, which file
/// systems report for a time they do not keep.
fn ticks_time(ticks: i64) -> Option<SystemTime> {
    let ticks = u64::try_from(ticks).ok().filter(|&t| t != 0)?;
    let to_duration =
        |t: u64| Duration::from_secs(t / 10_000_000) + Duration::from_nanos((t % 10_000_000) * 100);
    if ticks >= FILETIME_UNIX_OFFSET {
        UNIX_EPOCH.checked_add(to_duration(ticks - FILETIME_UNIX_OFFSET))
    } else {
        UNIX_EPOCH.checked_sub(to_duration(FILETIME_UNIX_OFFSET - ticks))
    }
}

fn filetime(ft: FILETIME) -> Option<SystemTime> {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    ticks_time(i64::try_from(ticks).ok()?)
}

/// Opens the entry at `path` itself (a reparse point is opened as the link, not its
/// target) with delete and attribute access, sharing everything so that only entries open
/// elsewhere without delete sharing fail.
fn open_entry(path: &Path, directory: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT.0;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS.0;
    }
    OpenOptions::new()
        .access_mode(DELETE.0 | FILE_READ_ATTRIBUTES.0 | FILE_WRITE_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(flags)
        .open(path)
}

/// Attributes, size, link count and times of an open entry. The change time comes from
/// `FILE_BASIC_INFO`, which only the handle-based query reports.
fn handle_info(file: &File) -> io::Result<HandleInfo> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` is an open handle and `info` is a valid out-pointer for the call.
    unsafe { GetFileInformationByHandle(raw(file), &mut info) }.map_err(to_io)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: `basic` is a FILE_BASIC_INFO that outlives the call; the size matches it.
    unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileBasicInfo,
            &mut basic as *mut FILE_BASIC_INFO as *mut c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .map_err(to_io)?;
    Ok(HandleInfo {
        attrs: info.dwFileAttributes,
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        links: info.nNumberOfLinks,
        times: Times {
            created: filetime(info.ftCreationTime),
            modified: filetime(info.ftLastWriteTime),
            changed: ticks_time(basic.ChangeTime),
        },
    })
}

/// Normalized `\\?\` path of the open entry, the same form `fs::canonicalize` returns.
fn final_path(file: &File) -> io::Result<PathBuf> {
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `file` is an open handle; the slice length bounds the write.
        let n = unsafe { GetFinalPathNameByHandleW(raw(file), &mut buf, flags) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(n, 0);
    }
}

fn set_attributes(file: &File, attrs: u32) -> io::Result<()> {
    let info = FILE_BASIC_INFO {
        FileAttributes: if attrs == 0 {
            FILE_ATTRIBUTE_NORMAL.0
        } else {
            attrs
        },
        // Zero times leave the timestamps unchanged.
        ..Default::default()
    };
    // SAFETY: `info` is a FILE_BASIC_INFO that outlives the call; the size matches it.
    unsafe {
        SetFileInformationByHandle(
            raw(file),
            FileBasicInfo,
            &info as *const FILE_BASIC_INFO as *const c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .map_err(to_io)
}

fn mark_for_deletion(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `info` is a FILE_DISPOSITION_INFO that outlives the call; the size matches it.
    unsafe {
        SetFileInformationByHandle(
            raw(file),
            FileDispositionInfo,
            &info as *const FILE_DISPOSITION_INFO as *const c_void,
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(to_io)
}

/// Marks the open entry for deletion, clearing a read-only attribute first and putting it
/// back if the deletion is refused. The entry disappears when the handle closes.
fn delete_through(file: &File, attrs: u32) -> Result<(), Failure> {
    let readonly = attrs & READONLY != 0;
    if readonly {
        set_attributes(file, attrs & SETTABLE & !READONLY).map_err(classify)?;
    }
    if let Err(e) = mark_for_deletion(file) {
        if readonly {
            let _ = set_attributes(file, attrs & SETTABLE);
        }
        return Err(classify(e));
    }
    Ok(())
}

/// Deletes one file after re-checking it through its own handle: still an ordinary file
/// with a single link, still old enough (creation, last-write and change time), its final
/// path inside `scope` and not waiting for a reboot-time move. Returns the size that was
/// freed.
fn delete_file(
    path: &Path,
    scope: Scope<'_>,
    cutoff: Option<SystemTime>,
    guards: &Guards,
) -> Result<u64, Failure> {
    let file = open_entry(path, false).map_err(classify)?;
    let info = handle_info(&file).map_err(classify)?;
    if info.attrs & (REPARSE | DIRECTORY) != 0 || info.links > 1 {
        return Err(Failure::Changed);
    }
    if !old_enough(cutoff, &info.times) {
        return Err(Failure::Recent);
    }
    let real = final_path(&file).map_err(classify)?;
    if !scope.allows(&real) {
        return Err(Failure::Outside(real));
    }
    if guards.pins(&real) {
        return Err(Failure::Pinned);
    }
    delete_through(&file, info.attrs)?;
    Ok(info.size)
}

/// Removes one directory if it is empty, after checking through its own handle that it is
/// an ordinary directory strictly inside `root` that passes the age rule: created at or
/// before `cutoff`, and when `untouched` (nothing in it was deleted by this purge) also
/// last written and changed at or before it.
fn remove_empty_dir(
    path: &Path,
    root: &Path,
    cutoff: Option<SystemTime>,
    untouched: bool,
) -> Result<(), Failure> {
    let dir = open_entry(path, true).map_err(classify)?;
    let info = handle_info(&dir).map_err(classify)?;
    if info.attrs & REPARSE != 0 || info.attrs & DIRECTORY == 0 {
        return Err(Failure::Changed);
    }
    let old = if untouched {
        old_enough(cutoff, &info.times)
    } else {
        created_before(cutoff, &info.times)
    };
    if !old {
        return Err(Failure::Recent);
    }
    let real = final_path(&dir).map_err(classify)?;
    if !Scope::Inside(root).allows(&real) {
        return Err(Failure::Outside(real));
    }
    delete_through(&dir, info.attrs)
}

/// Sandbox helpers shared by the cleanup tests.
#[cfg(test)]
pub(super) mod testing {
    use super::*;
    use std::process::Command;

    pub(crate) fn ticks(t: SystemTime) -> i64 {
        let since = t.duration_since(UNIX_EPOCH).unwrap();
        (since.as_nanos() / 100) as i64 + FILETIME_UNIX_OFFSET as i64
    }

    /// Moves the creation, last-write and change times of a file or folder back by `by`,
    /// the way a file that has not been touched for that long looks.
    pub(crate) fn age(path: &Path, by: Duration) {
        let t = ticks(SystemTime::now() - by);
        let entry = OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES.0)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
            .unwrap();
        let info = FILE_BASIC_INFO {
            CreationTime: t,
            LastAccessTime: 0,
            LastWriteTime: t,
            ChangeTime: t,
            // Zero leaves the attributes unchanged.
            FileAttributes: 0,
        };
        // SAFETY: `info` is a FILE_BASIC_INFO that outlives the call; the size matches it.
        unsafe {
            SetFileInformationByHandle(
                raw(&entry),
                FileBasicInfo,
                &info as *const FILE_BASIC_INFO as *const c_void,
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        }
        .unwrap();
    }

    /// Makes `link` a directory junction pointing to `target`.
    pub(crate) fn junction(link: &Path, target: &Path) {
        let status = Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run mklink");
        assert!(status.success(), "mklink /J failed");
        let attrs = fs::symlink_metadata(link).unwrap().file_attributes();
        assert_ne!(attrs & REPARSE, 0, "junction is not a reparse point");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{age, junction};
    use super::*;
    use std::fs::FileTimes;
    use std::os::windows::fs::FileTimesExt;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn sandbox() -> tempfile::TempDir {
        tempfile::tempdir().expect("create sandbox")
    }

    fn guards() -> Guards {
        Guards::current()
    }

    fn ready(path: &Path) -> SafeRoot {
        match resolve_root(path, &guards()) {
            Resolved::Ready(root) => root,
            other => panic!("sandbox root not accepted: {other:?}"),
        }
    }

    fn refused(path: &Path, guards: &Guards) -> bool {
        matches!(resolve_root(path, guards), Resolved::Refused(_))
    }

    fn write(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn cleanup_age_rule_keeps_recent_files() {
        let dir = sandbox();
        let old = dir.path().join("old.tmp");
        let new = dir.path().join("new.tmp");
        let old_modified_only = dir.path().join("extracted.tmp");
        write(&old, 100);
        write(&new, 50);
        write(&old_modified_only, 25);
        age(&old, 2 * DAY);
        // An old last-write time on a file created just now still counts as recent.
        OpenOptions::new()
            .write(true)
            .open(&old_modified_only)
            .unwrap()
            .set_modified(SystemTime::now() - 2 * DAY)
            .unwrap();

        let root = ready(dir.path());
        let filter = Filter::everything().older_than(DAY);
        let m = measure(&root, &filter);
        assert_eq!((m.files, m.bytes, m.denied), (1, 100, false));

        let p = purge(&root, &filter, &Guards::default());
        assert_eq!(p.deleted_files, 1);
        assert_eq!(p.freed_bytes, 100);
        assert_eq!(p.skipped_files, 2);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(!old.exists());
        assert!(new.exists());
        assert!(old_modified_only.exists());
    }

    #[test]
    fn cleanup_change_time_keeps_backdated_extracted_files() {
        // Installers stamp extracted files with the package's build date through
        // SetFileTime, which moves the creation and last-write times but not the change
        // time.
        let dir = sandbox();
        let extracted = dir.path().join("payload.dll");
        write(&extracted, 64);
        let old = SystemTime::now() - 30 * DAY;
        OpenOptions::new()
            .write(true)
            .open(&extracted)
            .unwrap()
            .set_times(FileTimes::new().set_created(old).set_modified(old))
            .unwrap();
        let listed =
            read_entries(&open_plain_dir(dir.path()).unwrap().unwrap().0, dir.path()).unwrap();
        let times = listed
            .iter()
            .find(|e| e.name == "payload.dll")
            .unwrap()
            .times;
        assert!(times.created.unwrap() <= old + Duration::from_secs(1));
        assert!(times.changed.unwrap() > SystemTime::now() - DAY);

        let root = ready(dir.path());
        let filter = Filter::everything().older_than(DAY);
        assert_eq!(measure(&root, &filter).files, 0);
        let p = purge(&root, &filter, &Guards::default());
        assert_eq!((p.deleted_files, p.skipped_files), (0, 1));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(extracted.exists());

        // The handle re-check alone refuses it too.
        let cutoff = filter.cutoff(SystemTime::now());
        let result = delete_file(
            &extracted,
            Scope::Inside(root.path()),
            cutoff,
            &Guards::default(),
        );
        assert!(matches!(result, Err(Failure::Recent)), "{result:?}");
        assert!(extracted.exists());
    }

    #[test]
    fn cleanup_age_rule_skips_fresh_folders() {
        // A folder an installer just created is left alone even when every file in it
        // looks old.
        let dir = sandbox();
        let fresh = dir.path().join("nsA1B2.tmp");
        let payload = fresh.join(r"sub\setup.exe");
        write(&payload, 32);
        age(&payload, 5 * DAY);
        age(&fresh.join("sub"), 5 * DAY);

        let root = ready(dir.path());
        let filter = Filter::everything().older_than(DAY);
        assert_eq!(measure(&root, &filter).files, 0);
        let p = purge(&root, &filter, &Guards::default());
        assert_eq!(
            (p.deleted_files, p.skipped_files, p.removed_dirs),
            (0, 1, 0)
        );
        assert!(payload.exists());

        // Once the folder itself is old, its old contents are cleaned.
        age(&fresh, 5 * DAY);
        assert_eq!(measure(&root, &filter).files, 1);
        let p = purge(&root, &filter, &Guards::default());
        assert_eq!(p.deleted_files, 1);
        assert!(!fresh.exists(), "emptied old folders are removed");
    }

    #[test]
    fn cleanup_lists_large_folders_completely() {
        // More entries than one listing buffer holds.
        let dir = sandbox();
        for i in 0..1500 {
            write(
                &dir.path()
                    .join(format!("entry-with-a-fairly-long-name-{i:05}.bin")),
                1,
            );
        }
        let m = measure(&ready(dir.path()), &Filter::everything());
        assert_eq!((m.files, m.bytes), (1500, 1500));
    }

    #[test]
    fn cleanup_clears_read_only_before_deleting() {
        let dir = sandbox();
        let file = dir.path().join("cache").join("locked.bin");
        write(&file, 10);
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file, perms).unwrap();

        let p = purge(
            &ready(dir.path()),
            &Filter::everything(),
            &Guards::default(),
        );
        assert_eq!((p.deleted_files, p.freed_bytes), (1, 10));
        assert!(!file.exists());
    }

    #[test]
    fn cleanup_removes_emptied_dirs_but_keeps_root() {
        let dir = sandbox();
        write(&dir.path().join(r"a\b\c\deep.bin"), 7);
        fs::create_dir_all(dir.path().join(r"e\f\g")).unwrap();

        let p = purge(
            &ready(dir.path()),
            &Filter::everything(),
            &Guards::default(),
        );
        assert_eq!(p.deleted_files, 1);
        assert_eq!(p.removed_dirs, 6);
        assert!(dir.path().is_dir(), "root must survive");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn cleanup_keeps_fresh_empty_dirs_under_age_rule() {
        let dir = sandbox();
        fs::create_dir_all(dir.path().join(r"fresh\nested")).unwrap();
        write(&dir.path().join(r"used\old.tmp"), 3);
        age(&dir.path().join(r"used\old.tmp"), 3 * DAY);
        age(&dir.path().join("used"), 3 * DAY);

        let p = purge(
            &ready(dir.path()),
            &Filter::everything().older_than(DAY),
            &Guards::default(),
        );
        assert_eq!(p.deleted_files, 1);
        assert!(dir.path().join(r"fresh\nested").is_dir());
        assert!(
            !dir.path().join("used").exists(),
            "emptied folder is removed"
        );
    }

    #[test]
    fn cleanup_never_follows_or_deletes_junctions() {
        let dir = sandbox();
        let outside = sandbox();
        let canary = outside.path().join("canary.txt");
        write(&canary, 42);
        age(&canary, 5 * DAY);
        write(&dir.path().join("junk.tmp"), 5);
        let link = dir.path().join(r"sub\link");
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        junction(&link, outside.path());

        let root = ready(dir.path());
        let m = measure(&root, &Filter::everything());
        assert_eq!(
            (m.files, m.bytes),
            (1, 5),
            "junction target must not be counted"
        );

        let p = purge(&root, &Filter::everything(), &Guards::default());
        assert_eq!(p.deleted_files, 1);
        assert_eq!(p.skipped_files, 1, "the junction is skipped");
        assert!(canary.exists(), "canary behind the junction was deleted");
        assert_eq!(fs::read(&canary).unwrap().len(), 42);
        let attrs = fs::symlink_metadata(&link).unwrap().file_attributes();
        assert_ne!(attrs & REPARSE, 0, "junction itself must survive");
        assert!(
            dir.path().join("sub").is_dir(),
            "parent of the junction is not empty"
        );

        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn cleanup_refuses_a_path_reached_through_a_junction() {
        let dir = sandbox();
        let outside = sandbox();
        let canary = outside.path().join("canary.txt");
        write(&canary, 1);
        let link = dir.path().join("link");
        junction(&link, outside.path());
        let root = ready(dir.path());

        let result = delete_file(
            &link.join("canary.txt"),
            Scope::Inside(root.path()),
            None,
            &Guards::default(),
        );
        assert!(matches!(result, Err(Failure::Outside(_))), "{result:?}");
        assert!(canary.exists());
        let result = remove_empty_dir(&link, root.path(), None, true);
        assert!(matches!(result, Err(Failure::Changed)), "{result:?}");
        assert!(link.exists());

        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn cleanup_refuses_junction_as_root() {
        let dir = sandbox();
        let outside = sandbox();
        let link = dir.path().join("root_link");
        junction(&link, outside.path());
        assert!(refused(&link, &guards()));
        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn cleanup_skips_files_in_use() {
        let dir = sandbox();
        let busy = dir.path().join("busy.log");
        let handle = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&busy)
            .unwrap();

        let p = purge(
            &ready(dir.path()),
            &Filter::everything(),
            &Guards::default(),
        );
        assert_eq!((p.deleted_files, p.skipped_files), (0, 1));
        assert!(
            p.errors.is_empty(),
            "in-use files are not errors: {:?}",
            p.errors
        );
        drop(handle);
        assert!(busy.exists());
    }

    #[test]
    fn cleanup_skips_hard_linked_files() {
        let dir = sandbox();
        let outside = sandbox();
        let original = outside.path().join("original.bin");
        write(&original, 9);
        fs::hard_link(&original, dir.path().join("alias.bin")).unwrap();

        let p = purge(
            &ready(dir.path()),
            &Filter::everything(),
            &Guards::default(),
        );
        assert_eq!((p.deleted_files, p.skipped_files), (0, 1));
        assert!(original.exists());
        assert!(dir.path().join("alias.bin").exists());
    }

    #[test]
    fn cleanup_keeps_files_waiting_for_a_reboot_time_move() {
        let dir = sandbox();
        let staged = dir.path().join("staged.dll");
        let other = dir.path().join("other.tmp");
        write(&staged, 6);
        write(&other, 4);
        let mut guards = Guards::default();
        guards
            .pinned
            .insert(key(&fs::canonicalize(&staged).unwrap()));

        let p = purge(&ready(dir.path()), &Filter::everything(), &guards);
        assert_eq!((p.deleted_files, p.skipped_files), (1, 1));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(staged.exists());
        assert!(!other.exists());
    }

    #[test]
    fn cleanup_pending_rename_sources_with_a_destination() {
        let value: Vec<u8> = [
            r"\??\C:\Windows\Temp\a.tmp",
            "",
            r"\??\C:\Staging\new.dll",
            r"!\??\C:\Program Files\App\app.dll",
            r"*1\??\C:\Staging\other.dll",
            r"\??\C:\Program Files\App\other.dll",
            "",
            "",
        ]
        .iter()
        .flat_map(|s| s.encode_utf16().chain(std::iter::once(0)))
        .flat_map(u16::to_le_bytes)
        .collect();
        assert_eq!(
            pending_sources(&value),
            [
                PathBuf::from(r"C:\Staging\new.dll"),
                PathBuf::from(r"C:\Staging\other.dll")
            ]
        );
        assert!(pending_sources(&[]).is_empty());
        // Reading the live queue is read-only and must not fail.
        let _ = pending_moves();
    }

    #[test]
    fn cleanup_name_pattern_and_top_level_only() {
        let dir = sandbox();
        write(&dir.path().join("thumbcache_256.db"), 11);
        write(&dir.path().join("THUMBCACHE_IDX.DB"), 13);
        write(&dir.path().join("iconcache_256.db"), 17);
        write(&dir.path().join(r"sub\thumbcache_32.db"), 19);
        let filter = Filter::everything().named("thumbcache_", ".db").top_level();
        let root = ready(dir.path());

        let m = measure(&root, &filter);
        assert_eq!((m.files, m.bytes), (2, 24));
        let p = purge(&root, &filter, &Guards::default());
        assert_eq!((p.deleted_files, p.freed_bytes), (2, 24));
        assert!(dir.path().join("iconcache_256.db").exists());
        assert!(dir.path().join(r"sub\thumbcache_32.db").exists());
    }

    #[test]
    fn cleanup_pattern_filter_only_removes_dirs_it_emptied() {
        let dir = sandbox();
        write(&dir.path().join(r"reports\a.dmp"), 4);
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        write(&dir.path().join(r"mixed\b.dmp"), 4);
        write(&dir.path().join(r"mixed\notes.txt"), 4);

        let p = purge(
            &ready(dir.path()),
            &Filter::everything().named("", ".dmp"),
            &Guards::default(),
        );
        assert_eq!(p.deleted_files, 2);
        assert!(!dir.path().join("reports").exists());
        assert!(dir.path().join("empty").is_dir());
        assert!(dir.path().join(r"mixed\notes.txt").exists());
    }

    #[test]
    fn cleanup_single_file() {
        let dir = sandbox();
        let dump = dir.path().join("MEMORY.DMP");
        let neighbour = dir.path().join("other.dmp");
        write(&dump, 64);
        write(&neighbour, 8);
        let Resolved::Ready(file) = resolve_file(&dump) else {
            panic!("file not accepted")
        };
        assert_eq!(file.size(), 64);
        let p = purge_file(&file, &Guards::default());
        assert_eq!((p.deleted_files, p.freed_bytes), (1, 64));
        assert!(!dump.exists());
        assert!(neighbour.exists());
        assert!(matches!(resolve_file(&dump), Resolved::Missing));
    }

    #[test]
    fn cleanup_refuses_protected_and_shallow_roots() {
        let guards = guards();
        for var in [
            "SystemRoot",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramData",
        ] {
            let path = env_path(var).expect("variable is set");
            assert!(refused(&path, &guards), "%{var}% accepted");
        }
        let drive = env_path("SystemDrive").unwrap_or_else(|| PathBuf::from("C:"));
        let drive_root = PathBuf::from(format!("{}\\", drive.display()));
        assert!(refused(&drive_root, &guards));
        assert!(refused(Path::new(r"relative\temp"), &guards));
        assert!(refused(Path::new(r"C:\Windows\..\Windows\Temp"), &guards));
        let dir = sandbox();
        assert!(matches!(
            resolve_root(&dir.path().join("absent"), &guards),
            Resolved::Missing
        ));
    }

    #[test]
    fn cleanup_refuses_network_and_device_paths() {
        // Refused from the path alone, before any network or device access.
        let guards = Guards::default();
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        for path in [
            format!(r"\\localhost\C$\Users\{user}"),
            format!(r"\\localhost\C$\Users\{user}\AppData\Local\Temp"),
            format!(r"\\?\UNC\localhost\C$\Users\{user}\AppData\Local\Temp"),
            r"\\.\C:\Windows\Temp".to_string(),
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Windows\Temp".to_string(),
        ] {
            let result = resolve_root(Path::new(&path), &guards);
            assert!(
                matches!(&result, Resolved::Refused(reason) if reason.contains("local drive")),
                "{path}: {result:?}"
            );
        }
        assert!(matches!(
            resolve_file(Path::new(r"\\localhost\C$\Windows\MEMORY.DMP")),
            Resolved::Refused(_)
        ));
    }

    #[test]
    fn cleanup_sandbox_is_on_a_fixed_drive() {
        let dir = sandbox();
        let canonical = fs::canonicalize(dir.path()).unwrap();
        assert!(verbatim_disk(&canonical));
        assert!(on_fixed_volume(&canonical));
        assert!(!verbatim_disk(Path::new(r"\\?\UNC\localhost\C$\Temp")));
        assert_eq!(drive_letter(Path::new(r"\\localhost\C$\Temp")), None);
        assert_eq!(drive_letter(Path::new(r"C:\Temp")), Some(b'C'));
        assert!(on_local_disk(dir.path()));
        assert!(on_local_disk(&canonical));
        for path in [
            r"\\localhost\C$\Temp",
            r"\\?\UNC\localhost\C$\Temp",
            r"\\.\C:\Temp",
            r"relative\Temp",
            r"C:relative",
            r"C:\Windows\..\Temp",
        ] {
            assert!(!on_local_disk(Path::new(path)), "{path}");
            assert_eq!(canonical_local(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn cleanup_guards_skip_folders_not_on_a_fixed_local_disk() {
        // A protected folder on a share keeps its path key but is never opened, so an
        // unreachable server cannot stall building the guards.
        let dir = sandbox();
        let local = dir.path().join("guarded");
        fs::create_dir_all(&local).unwrap();
        let share = PathBuf::from(r"\\192.0.2.1\home\Documents");
        let started = std::time::Instant::now();
        let guards = Guards::build(&[share.clone(), local.clone()], &[], &[]);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(guards.keys.contains(&key(&share)));
        assert!(guards.keys.contains(&key(&local)));
        assert!(guards.ids.contains(&folder_id(&local).unwrap()));
    }

    #[test]
    fn cleanup_refuses_ancestors_of_protected_folders() {
        let dir = sandbox();
        let guarded = dir.path().join(r"outer\middle\guarded");
        let sibling = dir.path().join(r"outer\sibling");
        let inside = guarded.join("cache");
        for d in [&guarded, &sibling, &inside] {
            fs::create_dir_all(d).unwrap();
        }
        let guards = Guards::build(std::slice::from_ref(&guarded), &[], &[]);

        assert!(refused(&guarded, &guards), "the protected folder itself");
        assert!(
            refused(&dir.path().join(r"outer\middle"), &guards),
            "its parent"
        );
        assert!(
            refused(&dir.path().join("outer"), &guards),
            "its grandparent"
        );
        assert!(matches!(
            resolve_root(&sibling, &guards),
            Resolved::Ready(_)
        ));
        assert!(matches!(resolve_root(&inside, &guards), Resolved::Ready(_)));
    }

    #[test]
    fn cleanup_compares_protected_folders_by_identity() {
        let dir = sandbox();
        let guarded = dir.path().join(r"outer\guarded");
        fs::create_dir_all(&guarded).unwrap();
        let mut guards = Guards::build(std::slice::from_ref(&guarded), &[], &[]);
        // With the path keys gone, the identities alone still refuse the folder and the
        // folders above it.
        guards.keys.clear();
        assert!(refused(&guarded, &guards));
        assert!(refused(&dir.path().join("outer"), &guards));
        fs::create_dir_all(dir.path().join(r"outer\other")).unwrap();
        assert!(matches!(
            resolve_root(&dir.path().join(r"outer\other"), &guards),
            Resolved::Ready(_)
        ));
    }

    #[test]
    fn cleanup_profile_roots_must_be_under_appdata_local() {
        let dir = sandbox();
        let users = dir.path().join("Users");
        let me = users.join("me");
        let other = users.join("other");
        let folders = [
            me.join("Documents"),
            me.join(r"OneDrive\Desktop"),
            me.join(r"AppData\LocalLow\Vendor"),
            me.join(r"AppData\Roaming\Vendor"),
            me.join(r"AppData\Local"),
            me.join(r"AppData\Local\Temp"),
            me.join(r"AppData\Local\Google\Chrome\User Data\Default\Cache"),
            other.join("Pictures"),
            other.join(r"AppData\Local\Temp"),
        ];
        for d in &folders {
            fs::create_dir_all(d).unwrap();
        }
        let guards = Guards::build(&[], std::slice::from_ref(&me), std::slice::from_ref(&users));

        for blocked in &folders[..5] {
            assert!(refused(blocked, &guards), "{} accepted", blocked.display());
        }
        assert!(refused(&other.join("Pictures"), &guards));
        for allowed in [&folders[5], &folders[6], &folders[8]] {
            assert!(
                matches!(resolve_root(allowed, &guards), Resolved::Ready(_)),
                "{} refused",
                allowed.display()
            );
        }
    }

    #[test]
    fn cleanup_refuses_real_profile_folders_outside_appdata_local() {
        // Read-only: only resolves the folders, never lists or deletes anything.
        let guards = guards();
        for id in [
            FOLDERID_Documents,
            FOLDERID_Desktop,
            FOLDERID_Downloads,
            FOLDERID_LocalAppDataLow,
            FOLDERID_RoamingAppData,
        ] {
            let Ok(path) = known_folder(&id) else {
                continue;
            };
            if path.is_dir() {
                assert!(refused(&path, &guards), "{} accepted", path.display());
            }
        }
        let profile = known_folder(&FOLDERID_Profile).unwrap();
        assert!(refused(&profile.join("AppData"), &guards));
        let local = known_folder(&FOLDERID_LocalAppData).unwrap();
        assert!(refused(&local, &guards));
        assert!(!guards.ids.is_empty());
    }

    #[test]
    fn cleanup_known_folders_and_windows_dir() {
        let windows = windows_dir().unwrap();
        assert!(windows.join("System32").is_dir());
        let local = known_folder(&FOLDERID_LocalAppData).unwrap();
        assert!(local.ends_with(r"AppData\Local"), "{}", local.display());
    }

    #[test]
    fn cleanup_scope_checks() {
        let root = Path::new(r"\\?\C:\Users\Test\AppData\Local\Temp");
        assert!(Scope::Inside(root).allows(Path::new(r"\\?\C:\Users\Test\AppData\Local\Temp\a")));
        assert!(!Scope::Inside(root).allows(root));
        assert!(!Scope::Inside(root).allows(Path::new(r"\\?\C:\Users\Test\AppData\Local\Temp2\a")));
        assert!(!Scope::Inside(root).allows(Path::new(r"\\?\C:\Windows\System32\a")));
    }

    #[test]
    fn cleanup_filetime_conversion() {
        let unix = FILETIME {
            dwLowDateTime: (FILETIME_UNIX_OFFSET & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (FILETIME_UNIX_OFFSET >> 32) as u32,
        };
        assert_eq!(filetime(unix), Some(UNIX_EPOCH));
        assert_eq!(filetime(FILETIME::default()), None);
        assert_eq!(ticks_time(0), None);
        assert_eq!(ticks_time(-5), None);
        assert_eq!(ticks_time(FILETIME_UNIX_OFFSET as i64), Some(UNIX_EPOCH));
    }

    #[test]
    fn cleanup_age_rule_uses_every_time() {
        let now = SystemTime::now();
        let cutoff = Some(now - DAY);
        let old = Some(now - 2 * DAY);
        let new = Some(now);
        let all_old = Times {
            created: old,
            modified: old,
            changed: old,
        };
        assert!(old_enough(cutoff, &all_old));
        assert!(old_enough(None, &Times::default()));
        assert!(!old_enough(
            cutoff,
            &Times {
                changed: new,
                ..all_old
            }
        ));
        assert!(!old_enough(
            cutoff,
            &Times {
                created: new,
                ..all_old
            }
        ));
        assert!(!old_enough(
            cutoff,
            &Times {
                modified: new,
                ..all_old
            }
        ));
        assert!(!old_enough(
            cutoff,
            &Times {
                modified: None,
                ..all_old
            }
        ));
        // A file system without a change time (FAT) is judged by the other two.
        assert!(old_enough(
            cutoff,
            &Times {
                changed: None,
                ..all_old
            }
        ));
        assert!(created_before(
            cutoff,
            &Times {
                modified: new,
                changed: new,
                ..all_old
            }
        ));
        assert!(!created_before(
            cutoff,
            &Times {
                created: new,
                ..all_old
            }
        ));
    }
}
