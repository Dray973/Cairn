//! The program the maintenance task runs, and the checks of the folders an unattended run
//! writes to.
//!
//! A task with administrator rights must only start a program that no standard user can
//! replace, so the program, the DLLs it loads from its folder, its folder and every folder
//! above it must pass [`install_location_problem`]. An unattended run writes its journal
//! rows and transcripts into the account's data folder, which the account can change without
//! elevation, so [`check_data_dir`] refuses links and extra hard links there before anything
//! is opened.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, SetPriorityClass, PROCESS_MODE_BACKGROUND_BEGIN,
};

use super::{PROGRAM_MISSING, PROGRAM_UNSAFE};
use crate::app::{self, MAINTENANCE_DLLS, MAINTENANCE_PROGRAM};
use crate::win::acl::install_location_problem;
use crate::{Error, Result};

/// Folder of the run transcripts inside the data folder.
pub(crate) const MAINTENANCE_DIR: &str = "maintenance";
/// Folder of the sfc and DISM transcripts inside [`MAINTENANCE_DIR`].
pub(crate) const TOOLS_DIR: &str = "tools";

/// `cairn-maintenance.exe` in the folder of the running code ([`app::app_root`]); fails
/// with [`PROGRAM_MISSING`] when it is not there.
pub(crate) fn runner_program() -> Result<PathBuf> {
    program_in(&app::app_root()?)
}

/// `cairn-maintenance.exe` in `root`, which must exist.
pub(crate) fn program_in(root: &Path) -> Result<PathBuf> {
    let program = root.join(MAINTENANCE_PROGRAM);
    if program.is_file() {
        Ok(program)
    } else {
        Err(Error::Other(
            PROGRAM_MISSING
                .replace("{file}", MAINTENANCE_PROGRAM)
                .replace("{folder}", &root.display().to_string()),
        ))
    }
}

/// Why a task with administrator rights must not run `program`: [`PROGRAM_UNSAFE`] with the
/// reason, or `None` when the program, its DLLs, its folder and every folder above it can be
/// changed only by SYSTEM, Administrators and TrustedInstaller.
pub(crate) fn program_problem(program: &Path) -> Option<String> {
    let why = match install_location_problem(program, MAINTENANCE_DLLS, &[]) {
        Ok(None) => return None,
        Ok(Some(why)) => why,
        Err(e) => format!("its permissions could not be read: {e}"),
    };
    Some(
        PROGRAM_UNSAFE
            .replace("{path}", &program.display().to_string())
            .replace("{why}", &why),
    )
}

/// Lowers this process's CPU, I/O and memory priority for an unattended run. Best effort.
pub(crate) fn enter_background_mode() {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; the call only
    // changes this process's scheduling priority.
    if let Err(e) = unsafe { SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN) }
    {
        tracing::debug!(error = %e, "cannot enter background mode");
    }
}

/// The data folder of a journal: the folder that holds it.
pub(crate) fn data_dir_of(journal: &Path) -> Result<PathBuf> {
    match journal.parent() {
        Some(dir) if journal.is_absolute() && !dir.as_os_str().is_empty() => Ok(dir.to_path_buf()),
        _ => Err(Error::Other(format!(
            "the journal path {} has no folder",
            journal.display()
        ))),
    }
}

/// Transcript folder of the runs of the journal at `journal`.
pub(crate) fn transcript_dir(journal: &Path) -> Result<PathBuf> {
    Ok(data_dir_of(journal)?.join(MAINTENANCE_DIR))
}

fn is_reparse_point(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

/// Link facts of the file itself (a reparse point is not followed): whether it is a
/// reparse point and its number of hard links.
fn file_links(path: &Path) -> io::Result<(bool, u32)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(0x7)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `info` is a valid out
    // pointer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    Ok((
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        info.nNumberOfLinks,
    ))
}

/// Refuses a data folder an unattended run must not write to: the journal's folder, the
/// folder above it, `maintenance` and `maintenance\tools` must not be links or mount points,
/// and the journal and its `-wal` and `-shm` files, where they exist, must be plain files
/// with a single name. Reads only; missing folders and files pass.
pub(crate) fn check_data_dir(journal: &Path) -> Result<()> {
    let dir = data_dir_of(journal)?;
    let maintenance = dir.join(MAINTENANCE_DIR);
    let folders = [
        Some(dir.clone()),
        dir.parent().map(Path::to_path_buf),
        Some(maintenance.clone()),
        Some(maintenance.join(TOOLS_DIR)),
    ];
    for folder in folders.into_iter().flatten() {
        match is_reparse_point(&folder) {
            Ok(false) => {}
            Ok(true) => {
                return Err(Error::Other(format!(
                    "{} is a link or mount point, so scheduled maintenance does not write there",
                    folder.display()
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let name = journal
        .file_name()
        .ok_or_else(|| Error::Other(format!("{} names no file", journal.display())))?;
    for suffix in ["", "-wal", "-shm"] {
        let mut file_name = name.to_os_string();
        file_name.push(suffix);
        let file = dir.join(file_name);
        match file_links(&file) {
            Ok((false, 1)) => {}
            Ok((true, _)) => {
                return Err(Error::Other(format!(
                    "{} is a link, so scheduled maintenance does not open it",
                    file.display()
                )))
            }
            Ok((false, links)) => {
                return Err(Error::Other(format!(
                    "{} has {links} names, so scheduled maintenance does not open it",
                    file.display()
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Whether `name` is a run transcript's file name: `YYYYMMDD-HHMMSS-maintenance.log`, with
/// `-2` to `-9` before `.log` for runs started in the same second.
fn is_transcript_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() < 27 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !digits(0..8) || bytes[8] != b'-' || !digits(9..15) || bytes[15] != b'-' {
        return false;
    }
    match &stem[16..] {
        "maintenance" => true,
        tail => tail
            .strip_prefix("maintenance-")
            .is_some_and(|n| n.len() == 1 && (b'2'..=b'9').contains(&n.as_bytes()[0])),
    }
}

/// The run transcript `log_path` when it is a transcript inside the maintenance folder of
/// the journal at `journal`; an error otherwise.
pub(crate) fn transcript_to_open(journal: &Path, log_path: &str) -> Result<PathBuf> {
    let dir = transcript_dir(journal)?;
    let path = PathBuf::from(log_path);
    let refused = || {
        Error::Other(format!(
            "{log_path} is not a maintenance log of this journal, so it is not opened"
        ))
    };
    let parent_matches = path.parent().is_some_and(|p| {
        p.to_string_lossy()
            .eq_ignore_ascii_case(&dir.to_string_lossy())
    });
    let name_matches = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_transcript_name);
    if !parent_matches || !name_matches {
        return Err(refused());
    }
    match is_reparse_point(&path) {
        Ok(false) => Ok(path),
        Ok(true) => Err(refused()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_names_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let err = program_in(dir.path()).unwrap_err().to_string();
        assert!(err.contains(MAINTENANCE_PROGRAM), "{err}");
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        std::fs::write(dir.path().join(MAINTENANCE_PROGRAM), b"").unwrap();
        assert_eq!(
            program_in(dir.path()).unwrap(),
            dir.path().join(MAINTENANCE_PROGRAM)
        );
    }

    #[test]
    fn a_program_in_a_user_folder_is_refused_and_system32_passes() {
        // Read-only: reads security descriptors.
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join(MAINTENANCE_PROGRAM);
        std::fs::write(&program, b"").unwrap();
        let problem = program_problem(&program).unwrap();
        assert!(
            problem.starts_with("Scheduled maintenance needs Cairn installed"),
            "{problem}"
        );
        assert!(
            problem.contains(&program.display().to_string()),
            "{problem}"
        );
        let cmd = crate::win::paths::system_dir().unwrap().join("cmd.exe");
        assert_eq!(program_problem(&cmd), None);
    }

    #[test]
    fn the_data_folder_is_the_journals_folder() {
        let journal = Path::new(r"C:\Users\Test\AppData\Local\PCOptimizer\journal.db");
        assert_eq!(
            data_dir_of(journal).unwrap(),
            Path::new(r"C:\Users\Test\AppData\Local\PCOptimizer")
        );
        assert_eq!(
            transcript_dir(journal).unwrap(),
            Path::new(r"C:\Users\Test\AppData\Local\PCOptimizer\maintenance")
        );
        assert!(data_dir_of(Path::new("journal.db")).is_err());
    }

    #[test]
    fn a_plain_data_folder_passes() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("data").join("journal.db");
        check_data_dir(&journal).unwrap();
        std::fs::create_dir_all(
            journal
                .parent()
                .unwrap()
                .join(MAINTENANCE_DIR)
                .join(TOOLS_DIR),
        )
        .unwrap();
        std::fs::write(&journal, b"x").unwrap();
        std::fs::write(journal.with_file_name("journal.db-wal"), b"x").unwrap();
        check_data_dir(&journal).unwrap();
    }

    #[test]
    fn a_hard_linked_journal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.db");
        std::fs::write(&journal, b"x").unwrap();
        std::fs::hard_link(&journal, dir.path().join("elsewhere.db")).unwrap();
        let err = check_data_dir(&journal).unwrap_err().to_string();
        assert!(err.contains("2 names"), "{err}");
        let wal = tempfile::tempdir().unwrap();
        let journal = wal.path().join("journal.db");
        std::fs::write(journal.with_file_name("journal.db-shm"), b"x").unwrap();
        std::fs::hard_link(
            journal.with_file_name("journal.db-shm"),
            wal.path().join("other"),
        )
        .unwrap();
        assert!(check_data_dir(&journal).is_err());
    }

    #[test]
    fn a_junction_in_the_data_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let link = data.join(MAINTENANCE_DIR);
        // A junction inside the temporary folder, made with the cmd built-in.
        let status =
            std::process::Command::new(crate::win::paths::system_dir().unwrap().join("cmd.exe"))
                .args(["/c", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
        assert!(status.success());
        let err = check_data_dir(&data.join("journal.db"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("link or mount point"), "{err}");
        let _ = std::fs::remove_dir(&link);
    }

    #[test]
    fn only_transcripts_of_this_journal_are_opened() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.db");
        let folder = dir.path().join(MAINTENANCE_DIR);
        std::fs::create_dir(&folder).unwrap();
        let log = folder.join("20260927-120301-maintenance.log");
        std::fs::write(&log, b"x").unwrap();
        let text = log.display().to_string();
        assert_eq!(transcript_to_open(&journal, &text).unwrap(), log);
        let upper = format!(
            r"{}\20260927-120301-maintenance.log",
            folder.display().to_string().to_uppercase()
        );
        assert_eq!(
            transcript_to_open(&journal, &upper).unwrap(),
            PathBuf::from(&upper)
        );
        for bad in [
            folder.join("20260927-120301-sfc_verify.log"),
            folder.join("notes.txt"),
            folder.join("20260927-120301-maintenance.raw"),
            folder.join("20260927-120301-maintenance-10.log"),
            dir.path().join("20260927-120301-maintenance.log"),
            folder
                .join(TOOLS_DIR)
                .join("20260927-120301-maintenance.log"),
        ] {
            assert!(
                transcript_to_open(&journal, &bad.display().to_string()).is_err(),
                "{}",
                bad.display()
            );
        }
        assert!(is_transcript_name("20260927-120301-maintenance-2.log"));
        assert!(!is_transcript_name("20260927-120301-maintenance-1.log"));
    }
}
