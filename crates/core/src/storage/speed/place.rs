//! The speed test's folder and file, and the folders of tests that did not finish.
//!
//! The folder is created new, with a random name, so a junction planted in advance is never
//! reused; on a volume that keeps ACLs it gets an administrators-only DACL, so no unelevated
//! process can plant, rename or relink anything in it while the test runs. Removing a
//! leftover deletes only an exact, single-link, non-reparse `cairn-speedtest.dat` reached
//! through a verified final path, and the folder only when it holds nothing else.

use std::ffi::c_void;
use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, FindClose, FindExInfoBasic, FindExSearchLimitToDirectories, FindFirstFileExW,
    FindNextFileW, GetFileAttributesW, FIND_FIRST_EX_LARGE_FETCH, INVALID_FILE_ATTRIBUTES,
    WIN32_FIND_DATAW,
};
use windows::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;
use windows::Win32::System::IO::DeviceIoControl;

use super::SpeedEnv;
use crate::safety::state_log::Journal;
use crate::storage::files::{
    self, final_path, mark_for_deletion, open, same_path, standard_info, tag_info, wide_path,
    ATTR_COMPRESSED, ATTR_DIRECTORY, ATTR_REPARSE, DELETE, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FLAG_BACKUP_SEMANTICS, FLAG_OPEN_REPARSE_POINT, GENERIC_READ, SHARE_ALL,
    SYNCHRONIZE,
};
use crate::storage::hash::{hex, random_fill};
use crate::storage::{drive_tests_forbidden, size_text};
use crate::win::from_wide_nul;
use crate::{Error, Result};

/// Name prefix of a test folder; eight lowercase hex digits follow.
pub const FOLDER_PREFIX: &str = "CairnSpeedTest-";
/// The test file inside the folder.
pub const FILE_NAME: &str = "cairn-speedtest.dat";
/// Owner Administrators; a protected DACL granting full access to SYSTEM and Administrators
/// only.
pub(crate) const FOLDER_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// ops_log name of a leftover removal.
pub const OP_LEFTOVER: &str = "remove_speed_test_file";
/// Why leftover removal at a volume root is refused in tests and guarded runs.
pub const ROOT_REMOVAL_FORBIDDEN: &str = "Leftover removal at a volume root is turned off in this \
     environment (OPTIMIZER_FORBID_DRIVE_TESTS)";
/// Name attempts before creating the test folder gives up.
const FOLDER_TRIES: usize = 5;

/// Test folders of the speed tests running in this process, registered before each folder is
/// created and released once its test has removed it (or failed to). They are not leftovers,
/// so `find_leftovers` leaves them out and a running test is never reported as another
/// process's.
static RUNNING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A test folder a speed test left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leftover {
    pub path: String,
    /// Size of the test file; `None` when it cannot be read (an admin-only folder read
    /// without administrator rights, or a file in use).
    pub bytes: Option<u64>,
    /// A speed test of another Cairn process is using it now.
    pub in_use: bool,
    /// Entries in the folder besides the test file.
    pub other_entries: u32,
}

/// What removing a leftover did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftoverRemoval {
    pub path: String,
    pub removed: bool,
    pub bytes: u64,
    pub detail: String,
}

/// `CairnSpeedTest-` followed by eight hex digits, ignoring case.
pub fn is_folder_name(name: &str) -> bool {
    let Some(prefix) = name.get(..FOLDER_PREFIX.len()) else {
        return false;
    };
    let rest = &name[FOLDER_PREFIX.len()..];
    prefix.eq_ignore_ascii_case(FOLDER_PREFIX)
        && rest.len() == 8
        && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checks that `folder` names a test folder: an absolute path whose last component matches
/// [`is_folder_name`].
pub fn check_folder_name(folder: &Path) -> Result<()> {
    let name = folder.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if folder.is_absolute() && is_folder_name(name) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "{} is not a speed-test folder",
            folder.display()
        )))
    }
}

/// `X:\` exactly.
pub(crate) fn is_volume_root(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(Component::Prefix(p)), Some(Component::RootDir), None)
            if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    )
}

/// Eight random lowercase hex digits.
pub(crate) fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 4];
    random_fill(&mut bytes)?;
    Ok(hex(&bytes))
}

// ───────────────────────────── Finding leftovers ─────────────────────────────

/// Closes a find handle on drop.
struct FindHandle(HANDLE);

impl Drop for FindHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from FindFirstFileExW and is closed once, here.
        unsafe {
            let _ = FindClose(self.0);
        }
    }
}

/// Test folders directly inside `root` (a volume root, or the place of a test). Reparse
/// points, names other than the pattern and the folders of tests running in this process are
/// left out. Read-only.
pub(crate) fn find_leftovers(root: &Path) -> Vec<Leftover> {
    let pattern = wide_path(&root.join(format!("{FOLDER_PREFIX}*")));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated and `data` is a WIN32_FIND_DATAW, which is what
    // FindExInfoBasic fills; no search filter is passed.
    let first = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut c_void,
            FindExSearchLimitToDirectories,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let Ok(handle) = first else {
        return Vec::new();
    };
    let handle = FindHandle(handle);
    let mut found = Vec::new();
    loop {
        let name = from_wide_nul(&data.cFileName);
        let attrs = data.dwFileAttributes;
        if attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE == 0 && is_folder_name(&name) {
            let folder = root.join(&name);
            if !is_running_folder(&folder) {
                found.push(probe(&folder));
            }
        }
        // SAFETY: the handle is open and `data` is writable.
        if unsafe { FindNextFileW(handle.0, &mut data) }.is_err() {
            break;
        }
    }
    drop(handle);
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// What a test folder holds, without changing it.
fn probe(folder: &Path) -> Leftover {
    let file = folder.join(FILE_NAME);
    let (bytes, in_use) = match open(&file, GENERIC_READ, SHARE_ALL, FLAG_OPEN_REPARSE_POINT) {
        Ok(handle) => (standard_info(&handle).ok().map(|i| i.size), false),
        Err(e) => match e.raw_os_error() {
            Some(
                files::ERROR_SHARING_VIOLATION
                | files::ERROR_LOCK_VIOLATION
                | files::ERROR_DELETE_PENDING,
            ) => (None, true),
            Some(files::ERROR_FILE_NOT_FOUND) => (Some(0), false),
            _ => (None, false),
        },
    };
    Leftover {
        path: folder.display().to_string(),
        bytes,
        in_use,
        other_entries: other_entries(folder).unwrap_or(0),
    }
}

/// Entries in `folder` besides the test file; `None` when it cannot be listed.
fn other_entries(folder: &Path) -> Option<u32> {
    let entries = fs::read_dir(folder).ok()?;
    let count = entries
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().eq_ignore_ascii_case(FILE_NAME))
        .count();
    Some(u32::try_from(count).unwrap_or(u32::MAX))
}

// ───────────────────────────── Creating the folder ─────────────────────────────

fn running_folders() -> MutexGuard<'static, Vec<PathBuf>> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `folder` is the test folder of a test running in this process.
fn is_running_folder(folder: &Path) -> bool {
    running_folders().iter().any(|p| same_path(p, folder))
}

/// The test folder of a speed test running in this process: `find_leftovers` leaves it out
/// until this value is dropped.
#[derive(Debug)]
pub(crate) struct RunningFolder(PathBuf);

impl RunningFolder {
    fn new(path: PathBuf) -> RunningFolder {
        running_folders().push(path.clone());
        RunningFolder(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RunningFolder {
    fn drop(&mut self) {
        // One entry per value: an attempt at a taken name never releases the folder of a
        // test that holds that name.
        let mut running = running_folders();
        if let Some(i) = running.iter().position(|p| p == &self.0) {
            running.swap_remove(i);
        }
    }
}

/// A security descriptor from an SDDL string, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn from_sddl(sddl: &str) -> Result<Descriptor> {
        let text = crate::win::wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is NUL-terminated and `sd` a valid out pointer; the descriptor is
        // freed with LocalFree when the value drops.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?
        };
        Ok(Descriptor(sd))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW and
            // freed exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

/// Creates a new test folder `<base>\CairnSpeedTest-<hex>`; an existing name is never
/// reused (up to five random names are tried). With `protect`, the folder gets
/// [`FOLDER_SDDL`]. A folder that inherited NTFS compression is set uncompressed. The folder
/// counts as a running test's, not a leftover, from before it exists until the returned value
/// is dropped.
pub(crate) fn create_test_folder(base: &Path, hex: &str, protect: bool) -> Result<RunningFolder> {
    let descriptor = if protect {
        Some(Descriptor::from_sddl(FOLDER_SDDL)?)
    } else {
        None
    };
    let attributes = descriptor.as_ref().map(|d| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: d.0 .0,
        bInheritHandle: false.into(),
    });
    let mut name = hex.to_string();
    for attempt in 0..FOLDER_TRIES {
        if attempt > 0 {
            name = random_hex()?;
        }
        let folder = RunningFolder::new(base.join(format!("{FOLDER_PREFIX}{name}")));
        let wide = wide_path(folder.path());
        // SAFETY: `wide` is NUL-terminated; the security attributes and the descriptor they
        // point to outlive the call.
        let created = unsafe {
            CreateDirectoryW(
                PCWSTR(wide.as_ptr()),
                attributes.as_ref().map(|a| a as *const SECURITY_ATTRIBUTES),
            )
        };
        match created {
            Ok(()) => {
                uncompress(folder.path());
                return Ok(folder);
            }
            Err(e)
                if crate::win::is_win32(&e, windows::Win32::Foundation::ERROR_ALREADY_EXISTS) =>
            {
                continue
            }
            Err(e) => {
                return Err(Error::Other(format!(
                    "the test folder can't be created in {}: {}",
                    base.display(),
                    e.message()
                )))
            }
        }
    }
    Err(Error::Other(format!(
        "no new test folder name was free in {}",
        base.display()
    )))
}

/// Turns off NTFS compression the folder inherited from its parent, so the test file is not
/// compressed; failures leave it as it is (the file check refuses a compressed file).
fn uncompress(folder: &Path) {
    let wide = wide_path(folder);
    // SAFETY: `wide` is NUL-terminated.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    if attrs == INVALID_FILE_ATTRIBUTES || attrs & ATTR_COMPRESSED == 0 {
        return;
    }
    let dir = match open(
        folder,
        files::FILE_READ_DATA | files::FILE_WRITE_DATA | SYNCHRONIZE,
        SHARE_ALL,
        FLAG_BACKUP_SEMANTICS,
    ) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(folder = %folder.display(), error = %e, "cannot open the test folder to uncompress it");
            return;
        }
    };
    let format: u16 = 0; // COMPRESSION_FORMAT_NONE
    let mut returned = 0u32;
    // SAFETY: the input is a u16 that outlives the synchronous call; no output is requested.
    let result = unsafe {
        DeviceIoControl(
            files::raw(&dir),
            FSCTL_SET_COMPRESSION,
            Some(&format as *const u16 as *const c_void),
            std::mem::size_of::<u16>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    if let Err(e) = result {
        tracing::warn!(folder = %folder.display(), error = %e, "cannot uncompress the test folder");
    }
}

// ───────────────────────────── Removing a leftover ─────────────────────────────

/// Removes a test folder a speed test left at a volume root: the test file, then the folder
/// when it holds nothing else. Needs administrator rights. Writes a "started" row before it
/// touches anything and one final row ("deleted", "not_found" or "failed").
pub fn remove_leftover(
    env: &SpeedEnv,
    journal: &Journal,
    folder: &Path,
) -> Result<LeftoverRemoval> {
    remove_leftover_in(env, journal, folder, None)
}

/// [`remove_leftover`] for a caller that has no journal open yet: `open_journal` is called
/// only once the removal is allowed, so a refused call (not elevated, not a test folder, not
/// at a volume root) opens and creates nothing.
pub fn remove_leftover_opening(
    env: &SpeedEnv,
    folder: &Path,
    open_journal: impl FnOnce() -> Result<Journal>,
) -> Result<LeftoverRemoval> {
    remove_leftover_opening_in(env, folder, None, open_journal)
}

/// [`remove_leftover_opening`] for a folder whose parent is `place` instead of a volume root.
pub(crate) fn remove_leftover_opening_in(
    env: &SpeedEnv,
    folder: &Path,
    place: Option<&Path>,
    open_journal: impl FnOnce() -> Result<Journal>,
) -> Result<LeftoverRemoval> {
    check_removal(env, folder, place)?;
    let journal = open_journal()?;
    remove_checked(&journal, folder, || {})
}

/// [`remove_leftover`] for a folder whose parent is `place` (a test's place) instead of a
/// volume root.
pub(crate) fn remove_leftover_in(
    env: &SpeedEnv,
    journal: &Journal,
    folder: &Path,
    place: Option<&Path>,
) -> Result<LeftoverRemoval> {
    check_removal(env, folder, place)?;
    remove_checked(journal, folder, || {})
}

/// Refuses a removal that is not allowed, before anything is opened or written: the process
/// must be elevated, and `folder` must be a test folder directly inside `place` (a volume
/// root without one; never a volume root while drive tests are forbidden).
fn check_removal(env: &SpeedEnv, folder: &Path, place: Option<&Path>) -> Result<()> {
    if !(env.elevated)() {
        return Err(Error::NotElevated);
    }
    check_folder_name(folder)?;
    let parent = folder.parent().unwrap_or(folder);
    let at_root = is_volume_root(parent);
    let allowed = match place {
        Some(place) => same_path(parent, place),
        None => at_root,
    };
    if !allowed {
        return Err(Error::Other(format!(
            "{} is not where Cairn puts its speed-test folders",
            folder.display()
        )));
    }
    if at_root && drive_tests_forbidden() {
        return Err(Error::Other(ROOT_REMOVAL_FORBIDDEN.to_string()));
    }
    Ok(())
}

/// Removes `folder`, which [`check_removal`] allowed: the "started" row, then the removal,
/// then the final row. `before_removal` runs between the "started" row and the removal.
fn remove_checked(
    journal: &Journal,
    folder: &Path,
    before_removal: impl FnOnce(),
) -> Result<LeftoverRemoval> {
    let path = folder.display().to_string();
    let size = open(
        &folder.join(FILE_NAME),
        FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FLAG_OPEN_REPARSE_POINT,
    )
    .ok()
    .and_then(|f| standard_info(&f).ok())
    .map(|i| i.size);
    let size_detail = size.map_or_else(|| "size unknown".to_string(), size_text);
    journal.log_op(None, OP_LEFTOVER, &path, "started", Some(&size_detail))?;

    before_removal();
    let (outcome, removed, bytes, detail) = remove_folder(folder);
    if let Err(e) = journal.log_op(None, OP_LEFTOVER, &path, outcome, Some(&detail)) {
        tracing::error!(folder = %path, error = %e, "cannot write the leftover removal's final row");
    }
    Ok(LeftoverRemoval {
        path,
        removed,
        bytes,
        detail,
    })
}

/// (outcome, removed, bytes freed, detail) of removing the test file and then the folder.
fn remove_folder(folder: &Path) -> (&'static str, bool, u64, String) {
    let dir = match open(
        folder,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
    ) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ("not_found", false, 0, "the folder is gone".to_string())
        }
        Err(e) => {
            return (
                "failed",
                false,
                0,
                format!("the folder can't be opened: {e}"),
            )
        }
    };
    match tag_info(&dir) {
        Ok(tag) if tag.attrs & ATTR_REPARSE != 0 => {
            return ("failed", false, 0, "it is a link; left alone".to_string())
        }
        Ok(tag) if !tag.is_dir() => {
            return (
                "failed",
                false,
                0,
                "it is not a folder; left alone".to_string(),
            )
        }
        Ok(_) => {}
        Err(e) => return ("failed", false, 0, format!("the folder can't be read: {e}")),
    }
    let real_folder = match final_path(&dir) {
        Ok(real) => real,
        Err(e) => {
            return (
                "failed",
                false,
                0,
                format!("the folder can't be resolved: {e}"),
            )
        }
    };
    drop(dir);

    let file_path = folder.join(FILE_NAME);
    let mut freed = 0u64;
    let mut file_removed = false;
    match open(
        &file_path,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FLAG_OPEN_REPARSE_POINT,
    ) {
        Ok(file) => {
            let refusal = match (tag_info(&file), standard_info(&file), final_path(&file)) {
                (Ok(tag), _, _) if tag.attrs & ATTR_REPARSE != 0 => {
                    Some("the test file is a link; left alone".to_string())
                }
                (Ok(tag), _, _) if tag.is_dir() => {
                    Some("the test file is a folder; left alone".to_string())
                }
                (_, Ok(info), _) if info.links != 1 => {
                    Some("the test file has other hard links; left alone".to_string())
                }
                (_, _, Ok(real)) if !same_path(&real, &real_folder.join(FILE_NAME)) => {
                    Some("the test file is not where it should be; left alone".to_string())
                }
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                    Some(format!("the test file can't be read: {e}"))
                }
                _ => None,
            };
            if let Some(reason) = refusal {
                return ("failed", false, 0, reason);
            }
            let size = standard_info(&file).map(|i| i.size).unwrap_or(0);
            if let Err(e) = mark_for_deletion(&file) {
                return (
                    "failed",
                    false,
                    0,
                    format!("the test file can't be deleted: {e}"),
                );
            }
            drop(file);
            freed = size;
            file_removed = true;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(files::ERROR_SHARING_VIOLATION | files::ERROR_LOCK_VIOLATION)
            ) =>
        {
            return (
                "failed",
                false,
                0,
                "the test file is in use by a speed test that is still running".to_string(),
            )
        }
        Err(e) => {
            return (
                "failed",
                false,
                0,
                format!("the test file can't be opened: {e}"),
            )
        }
    }

    let others = other_entries(folder);
    let folder_removed = others == Some(0) && fs::remove_dir(folder).is_ok();
    let what = if file_removed {
        format!("removed the {} test file", size_text(freed))
    } else {
        "no test file was left".to_string()
    };
    let detail = match (folder_removed, others) {
        (true, _) => format!("{what} and its folder"),
        (false, Some(n)) if n > 0 => {
            format!("{what}; the folder holds other files, so it was kept")
        }
        (false, _) => format!("{what}; the folder could not be removed"),
    };
    if !file_removed && !folder_removed {
        return ("not_found", false, 0, detail);
    }
    ("deleted", true, freed, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::state_log::OpLogEntry;
    use std::os::windows::fs::OpenOptionsExt;
    use std::process::Command;

    fn elevated() -> bool {
        true
    }
    fn not_elevated() -> bool {
        false
    }

    fn env() -> SpeedEnv {
        SpeedEnv {
            elevated,
            ..super::super::tests::TEST_ENV
        }
    }

    fn journal(dir: &Path) -> Journal {
        Journal::open(dir.join("journal.db")).unwrap()
    }

    fn rows(journal: &Journal) -> Vec<OpLogEntry> {
        let mut rows = journal.ops(100).unwrap();
        rows.reverse();
        rows
    }

    fn leftover(place: &Path, hex: &str, bytes: usize) -> PathBuf {
        let folder = place.join(format!("{FOLDER_PREFIX}{hex}"));
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(FILE_NAME), vec![0u8; bytes]).unwrap();
        folder
    }

    fn junction(link: &Path, target: &Path) {
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
    }

    #[test]
    fn folder_names_follow_the_pattern() {
        assert!(is_folder_name("CairnSpeedTest-0123abcd"));
        assert!(is_folder_name("cairnspeedtest-0123ABCD"));
        assert!(!is_folder_name("CairnSpeedTest-0123abc"));
        assert!(!is_folder_name("CairnSpeedTest-0123abcde"));
        assert!(!is_folder_name("CairnSpeedTest-0123abcg"));
        assert!(!is_folder_name("Other-0123abcd"));
        assert!(!is_folder_name(""));
        assert!(check_folder_name(Path::new(r"C:\CairnSpeedTest-0123abcd")).is_ok());
        assert!(check_folder_name(Path::new(r"CairnSpeedTest-0123abcd")).is_err());
        assert!(check_folder_name(Path::new(r"C:\Windows")).is_err());
        assert!(is_volume_root(Path::new(r"C:\")));
        assert!(is_volume_root(Path::new(r"\\?\D:\")));
        assert!(!is_volume_root(Path::new(r"C:\Temp")));
        assert!(!is_volume_root(Path::new(r"C:")));
        let hex = random_hex().unwrap();
        assert_eq!(hex.len(), 8);
        assert!(hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn leftovers_are_found_with_their_size_and_use() {
        let dir = tempfile::tempdir().unwrap();
        let a = leftover(dir.path(), "0123abcd", 4096);
        fs::write(a.join("note.txt"), b"x").unwrap();
        leftover(dir.path(), "0000ffff", 100);
        // Wrong names and files are ignored.
        fs::create_dir(dir.path().join("CairnSpeedTest-xyz")).unwrap();
        fs::write(dir.path().join("CairnSpeedTest-12345678"), b"file").unwrap();
        let found = find_leftovers(dir.path());
        assert_eq!(found.len(), 2, "{found:?}");
        let a_found = found.iter().find(|l| l.path.ends_with("0123abcd")).unwrap();
        assert_eq!(a_found.bytes, Some(4096));
        assert!(!a_found.in_use);
        assert_eq!(a_found.other_entries, 1);

        // A live test file (opened without sharing, deleted on close) is in use.
        let live = dir.path().join("CairnSpeedTest-aaaa1111");
        fs::create_dir(&live).unwrap();
        let handle = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(0x0400_0000) // FILE_FLAG_DELETE_ON_CLOSE
            .open(live.join(FILE_NAME))
            .unwrap();
        let found = find_leftovers(dir.path());
        let live_found = found.iter().find(|l| l.path.ends_with("aaaa1111")).unwrap();
        assert!(live_found.in_use);
        assert_eq!(live_found.bytes, None);
        drop(handle);
        assert!(!live.join(FILE_NAME).exists());
        let found = find_leftovers(dir.path());
        let emptied = found.iter().find(|l| l.path.ends_with("aaaa1111")).unwrap();
        assert!(!emptied.in_use);
        assert_eq!(emptied.bytes, Some(0));
    }

    #[test]
    fn a_leftover_is_removed_with_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let place = dir.path().join("place");
        fs::create_dir(&place).unwrap();
        let folder = leftover(&place, "0123abcd", 8192);
        let journal = journal(dir.path());
        let done = remove_leftover_in(&env(), &journal, &folder, Some(&place)).unwrap();
        assert!(done.removed, "{done:?}");
        assert_eq!(done.bytes, 8192);
        assert!(!folder.exists());
        let rows = rows(&journal);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].op, OP_LEFTOVER);
        assert_eq!(rows[0].outcome, "started");
        assert_eq!(rows[0].detail.as_deref(), Some("8 KB"));
        assert_eq!(rows[1].outcome, "deleted");
        assert_eq!(rows[0].target, folder.display().to_string());
        assert!(rows[0].session_id.is_none());
    }

    #[test]
    fn the_started_row_is_written_before_a_removal_that_fails() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        // Held open without sharing, as a test that is still running holds it.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(folder.join(FILE_NAME))
            .unwrap();
        let journal = journal(dir.path());
        let done = remove_leftover_in(&env(), &journal, &folder, Some(dir.path())).unwrap();
        assert!(!done.removed);
        assert!(done.detail.contains("in use"), "{}", done.detail);
        let rows = rows(&journal);
        let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
        assert_eq!(outcomes, ["started", "failed"]);
        drop(held);
        assert!(folder.join(FILE_NAME).exists());
    }

    #[test]
    fn the_started_row_exists_before_anything_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        let journal = journal(dir.path());
        check_removal(&env(), &folder, Some(dir.path())).unwrap();
        let mut seen = None;
        let done = remove_checked(&journal, &folder, || {
            seen = Some((rows(&journal), folder.join(FILE_NAME).exists()));
        })
        .unwrap();
        assert!(done.removed, "{done:?}");
        let (before, file_there) = seen.expect("the hook runs before the removal");
        assert!(
            file_there,
            "the test file is still there when the row is written"
        );
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].op, OP_LEFTOVER);
        assert_eq!(before[0].outcome, "started");
        assert_eq!(before[0].target, folder.display().to_string());
        let rows = rows(&journal);
        let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
        assert_eq!(outcomes, ["started", "deleted"]);
        assert!(!folder.exists());
    }

    fn never_open() -> Result<Journal> {
        panic!("the journal must not be opened")
    }

    #[test]
    fn a_refused_removal_opens_no_journal() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        let root_folder = Path::new(r"C:\CairnSpeedTest-0123abcd");
        let refused = SpeedEnv {
            elevated: not_elevated,
            ..env()
        };
        assert!(matches!(
            remove_leftover_opening_in(&refused, &folder, Some(dir.path()), never_open),
            Err(Error::NotElevated)
        ));
        assert!(matches!(
            remove_leftover_opening(&refused, root_folder, never_open),
            Err(Error::NotElevated)
        ));
        // Wrong name, wrong place, and a volume root while drive tests are forbidden.
        assert!(
            remove_leftover_opening_in(&env(), dir.path(), Some(dir.path()), never_open).is_err()
        );
        let elsewhere = dir.path().join("sub");
        assert!(remove_leftover_opening_in(&env(), &folder, Some(&elsewhere), never_open).is_err());
        let err = remove_leftover_opening(&env(), root_folder, never_open).unwrap_err();
        assert_eq!(err.to_string(), ROOT_REMOVAL_FORBIDDEN);
        assert!(folder.join(FILE_NAME).exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        // An allowed removal opens the journal and writes its rows there.
        let path = dir.path().join("journal.db");
        let done =
            remove_leftover_opening_in(&env(), &folder, Some(dir.path()), || Journal::open(&path))
                .unwrap();
        assert!(done.removed, "{done:?}");
        assert!(!folder.exists());
        let rows = rows(&Journal::open(&path).unwrap());
        let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
        assert_eq!(outcomes, ["started", "deleted"]);
    }

    #[test]
    fn other_files_and_the_folder_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        fs::write(folder.join("mine.txt"), b"keep").unwrap();
        let journal = journal(dir.path());
        let done = remove_leftover_in(&env(), &journal, &folder, Some(dir.path())).unwrap();
        assert!(done.removed);
        assert!(!folder.join(FILE_NAME).exists());
        assert!(folder.join("mine.txt").exists());
        assert!(done.detail.contains("holds other files"), "{}", done.detail);
    }

    #[test]
    fn a_junction_is_left_alone_and_its_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join(FILE_NAME), b"precious").unwrap();
        let link = dir.path().join("CairnSpeedTest-1111abcd");
        junction(&link, &target);
        // Never listed as a leftover.
        assert!(find_leftovers(dir.path()).is_empty());
        let journal = journal(dir.path());
        let done = remove_leftover_in(&env(), &journal, &link, Some(dir.path())).unwrap();
        assert!(!done.removed);
        assert_eq!(done.detail, "it is a link; left alone");
        assert_eq!(fs::read(target.join(FILE_NAME)).unwrap(), b"precious");
        let rows = rows(&journal);
        assert_eq!(rows.last().unwrap().outcome, "failed");
    }

    #[test]
    fn a_hard_linked_test_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        let other = dir.path().join("elsewhere.dat");
        fs::hard_link(folder.join(FILE_NAME), &other).unwrap();
        let journal = journal(dir.path());
        let done = remove_leftover_in(&env(), &journal, &folder, Some(dir.path())).unwrap();
        assert!(!done.removed);
        assert!(done.detail.contains("hard links"), "{}", done.detail);
        assert!(folder.join(FILE_NAME).exists() && other.exists());
    }

    #[test]
    fn refusals_come_before_any_row() {
        let dir = tempfile::tempdir().unwrap();
        let folder = leftover(dir.path(), "0123abcd", 100);
        let journal = journal(dir.path());
        let refused = SpeedEnv {
            elevated: not_elevated,
            ..env()
        };
        assert!(matches!(
            remove_leftover_in(&refused, &journal, &folder, Some(dir.path())),
            Err(Error::NotElevated)
        ));
        // Wrong name, wrong place, and a volume root while drive tests are forbidden.
        assert!(remove_leftover_in(&env(), &journal, dir.path(), Some(dir.path())).is_err());
        let elsewhere = dir.path().join("sub");
        assert!(remove_leftover_in(&env(), &journal, &folder, Some(&elsewhere)).is_err());
        let err = remove_leftover(&env(), &journal, Path::new(r"C:\CairnSpeedTest-0123abcd"))
            .unwrap_err();
        assert_eq!(err.to_string(), ROOT_REMOVAL_FORBIDDEN);
        assert!(rows(&journal).is_empty());
        assert!(folder.join(FILE_NAME).exists());
    }

    #[test]
    fn a_created_folder_is_new_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let first = create_test_folder(dir.path(), "0123abcd", false).unwrap();
        assert!(first.path().ends_with("CairnSpeedTest-0123abcd"));
        let second = create_test_folder(dir.path(), "0123abcd", false).unwrap();
        assert_ne!(first.path(), second.path());
        assert!(second.path().is_dir());
    }

    #[test]
    fn the_folder_of_a_running_test_is_no_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let first = create_test_folder(dir.path(), "0123abcd", false).unwrap();
        // Held as a running test holds its file: without sharing, deleted on close.
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(0x0400_0000) // FILE_FLAG_DELETE_ON_CLOSE
            .open(first.path().join(FILE_NAME))
            .unwrap();
        // The second folder's first attempt meets the first folder's name; that attempt must
        // not release the first folder.
        let second = create_test_folder(dir.path(), "0123abcd", false).unwrap();
        let second_path = second.path().display().to_string();
        let found = find_leftovers(dir.path());
        assert!(found.is_empty(), "both tests are running: {found:?}");

        drop(second);
        let found = find_leftovers(dir.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, second_path);
        assert!(!found[0].in_use);

        // A folder its test could not remove is a leftover once the test ended.
        drop(held);
        drop(first);
        let found = find_leftovers(dir.path());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|l| !l.in_use && l.bytes == Some(0)));
    }

    #[test]
    fn the_folder_sddl_is_a_protected_admin_only_dacl() {
        use windows::Win32::Security::{GetSecurityDescriptorControl, SE_DACL_PROTECTED};
        let info = crate::win::acl::sddl_security(FOLDER_SDDL).unwrap();
        assert_eq!(info.owner.as_deref(), Some("S-1-5-32-544"));
        let dacl = info.dacl.unwrap();
        assert_eq!(dacl.len(), 2);
        let sids: Vec<&str> = dacl.iter().map(|a| a.sid.as_str()).collect();
        assert_eq!(sids, ["S-1-5-18", "S-1-5-32-544"]);
        assert!(dacl.iter().all(|a| a.allow && a.mask == 0x1F_01FF));
        let descriptor = Descriptor::from_sddl(FOLDER_SDDL).unwrap();
        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: the descriptor is valid while `descriptor` lives; both outputs are valid.
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }.unwrap();
        assert_ne!(control & SE_DACL_PROTECTED.0, 0);
    }

    /// Run by exact name from an elevated shell: creates the protected folder inside a
    /// temporary folder (never at a volume root), reads its security back and removes it.
    #[test]
    #[ignore = "needs an elevated process"]
    fn speed_test_folder_is_admin_only() {
        assert!(crate::is_elevated(), "run from an elevated shell");
        let dir = tempfile::tempdir().unwrap();
        let running = create_test_folder(dir.path(), &random_hex().unwrap(), true).unwrap();
        let folder = running.path();
        let info = crate::win::acl::file_security(folder).unwrap();
        assert_eq!(info.owner.as_deref(), Some("S-1-5-32-544"));
        let dacl = info.dacl.unwrap();
        assert!(dacl
            .iter()
            .all(|a| a.sid == "S-1-5-18" || a.sid == "S-1-5-32-544"));
        assert_eq!(
            crate::win::acl::untrusted_writer(
                &crate::win::acl::file_security(folder).unwrap(),
                crate::win::acl::AclRole::Folder
            ),
            None
        );
        fs::remove_dir(folder).unwrap();
    }
}
