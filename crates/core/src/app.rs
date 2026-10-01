//! The installed copy of Cairn as its installer registered it, and machine state removed at
//! uninstall.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

use crate::win::registry::{Hive, Key, RegValue};
use crate::{Error, Result};

/// Product name shown to the user.
pub const APP_NAME: &str = "Cairn";

/// Files beside Cairn.exe that the elevated launcher loads code from; checked by
/// `win::acl::install_location_problem` before the launcher elevates.
pub const LAUNCHER_DLLS: &[&str] = &[
    "python312.dll",
    "python3.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    r"app\optimizer\native\optimizer_engine.pyd",
    r"app\optimizer\native\optimizer_telemetry.dll",
];

/// Folders beside Cairn.exe that the elevated launcher loads code from; checked (each folder
/// itself, not recursively) by `win::acl::install_location_problem` before the launcher elevates.
pub const LAUNCHER_FOLDERS: &[&str] = &[
    "app",
    r"app\optimizer",
    r"app\optimizer\native",
    "Lib",
    "DLLs",
    "tcl",
];

/// DLLs beside cairn-maintenance.exe that it loads (the VC runtime; api-ms-win-crt-* resolve to
/// System32).
pub const MAINTENANCE_DLLS: &[&str] = &["vcruntime140.dll", "vcruntime140_1.dll"];

/// Inno Setup AppId; upgrades and the uninstaller rely on it, so it never changes.
pub const INSTALLER_APP_ID: &str = "{616A27F7-C15A-4244-ACD1-B7F536649EA0}";

/// HKLM uninstall key the installer writes for [`INSTALLER_APP_ID`].
pub const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{616A27F7-C15A-4244-ACD1-B7F536649EA0}_is1";

/// File name of the launcher in the install folder.
pub const LAUNCHER_PROGRAM: &str = "Cairn.exe";

/// File name of the command-line tool in the install folder.
pub const CLI_PROGRAM: &str = "optctl.exe";

/// File name of the scheduled maintenance runner in the install folder.
pub const MAINTENANCE_PROGRAM: &str = "cairn-maintenance.exe";

/// Longest module path Windows returns, in UTF-16 units.
const MAX_MODULE_PATH: usize = 32_768;

/// The installed copy as its installer registered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallInfo {
    /// Install folder (the folder that holds Cairn.exe).
    pub dir: PathBuf,
    /// Version the installer registered.
    pub version: String,
    pub launcher: PathBuf,
    pub cli: PathBuf,
}

/// The installed copy, read from the installer's uninstall entry (HKLM, 64-bit view).
/// `Ok(None)` when Cairn is not installed on this PC, or when the entry does not name a local
/// folder that holds Cairn.exe and optctl.exe. Read-only.
pub fn installed() -> Result<Option<InstallInfo>> {
    let key = Key::open(Hive::LocalMachine, UNINSTALL_KEY, false)?;
    installed_with(&RegistryEntry(key), &|path: &Path| path.is_file())
}

/// Values of the installer's uninstall entry.
pub(crate) trait UninstallValues {
    /// The value `name`; `Ok(None)` when it or the whole entry is missing.
    fn value(&self, name: &str) -> Result<Option<RegValue>>;
}

/// The uninstall entry in the registry; `None` when the key does not exist.
#[derive(Debug)]
struct RegistryEntry(Option<Key>);

impl UninstallValues for RegistryEntry {
    fn value(&self, name: &str) -> Result<Option<RegValue>> {
        match &self.0 {
            Some(key) => key.query(name),
            None => Ok(None),
        }
    }
}

/// [`installed`] over any source of uninstall values and file checks:
///
/// - no entry or no `InstallLocation` string (REG_SZ or REG_EXPAND_SZ, never expanded): None;
/// - trailing backslashes are removed; the rest must be a drive path `X:\…` (not UNC, not
///   `\\?\`, not relative), else None with a warning;
/// - `Cairn.exe` and `optctl.exe` must exist in it, else None;
/// - the version is `DisplayVersion`, or "" without one.
///
/// A value that cannot be read is an error.
pub(crate) fn installed_with(
    values: &dyn UninstallValues,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<Option<InstallInfo>> {
    let Some(location) = string_value(values, "InstallLocation")? else {
        return Ok(None);
    };
    let location = location.trim_end_matches('\\');
    if !is_drive_path(location) {
        tracing::warn!(
            location,
            "the uninstall entry's install location is not a local folder"
        );
        return Ok(None);
    }
    let dir = PathBuf::from(location);
    let launcher = dir.join(LAUNCHER_PROGRAM);
    let cli = dir.join(CLI_PROGRAM);
    if !is_file(&launcher) || !is_file(&cli) {
        return Ok(None);
    }
    let version = string_value(values, "DisplayVersion")?.unwrap_or_default();
    Ok(Some(InstallInfo {
        dir,
        version,
        launcher,
        cli,
    }))
}

/// A REG_SZ or REG_EXPAND_SZ value as written, without its terminating NULs; None for a
/// missing value or another type.
fn string_value(values: &dyn UninstallValues, name: &str) -> Result<Option<String>> {
    Ok(match values.value(name)? {
        Some(RegValue::Sz(text)) | Some(RegValue::ExpandSz(text)) => {
            Some(text.trim_end_matches('\0').to_string())
        }
        _ => None,
    })
}

/// `X:\` followed by at least one more character: a folder on a drive letter.
fn is_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// Folder that holds Cairn.exe, optctl.exe and cairn-maintenance.exe for the running code:
/// [`root_for_module`] of the module (exe or DLL) this function is compiled into.
pub fn app_root() -> Result<PathBuf> {
    Ok(root_for_module(&this_module_path()?))
}

/// Pure: the module's folder; when that folder ends with the components
/// `app\optimizer\native` (ASCII case ignored), the folder three levels above, where the
/// installed engine module lives below the install root.
pub fn root_for_module(module: &Path) -> PathBuf {
    let dir = module.parent().unwrap_or(module);
    let tail: Vec<&std::ffi::OsStr> = dir
        .components()
        .rev()
        .take(3)
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let installed_engine = tail.len() == 3
        && tail
            .iter()
            .rev()
            .zip(["app", "optimizer", "native"])
            .all(|(name, want)| name.to_str().is_some_and(|n| n.eq_ignore_ascii_case(want)));
    if installed_engine {
        if let Some(root) = dir.ancestors().nth(3) {
            return root.to_path_buf();
        }
    }
    dir.to_path_buf()
}

/// Full path of the module that contains this code (optctl.exe, cairn-maintenance.exe,
/// Cairn.exe or optimizer_engine.pyd).
fn this_module_path() -> Result<PathBuf> {
    static MARKER: u8 = 0;
    let mut module = HMODULE::default();
    // SAFETY: with FROM_ADDRESS the name argument is an address inside this module (a static
    // of this crate); UNCHANGED_REFCOUNT leaves the module's reference count alone, so the
    // handle needs no FreeLibrary. `module` is a valid out pointer.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(std::ptr::addr_of!(MARKER).cast::<u16>()),
            &mut module,
        )
    }?;
    let mut buf = vec![0u16; 260];
    loop {
        // SAFETY: `module` is a loaded module of this process; `buf` is a writable slice
        // whose length is passed with it.
        let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
        if n == 0 {
            return Err(Error::Win32(windows::core::Error::from_win32()));
        }
        if n < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        if buf.len() >= MAX_MODULE_PATH {
            return Err(Error::Other("the module path is too long".into()));
        }
        buf.resize((buf.len() * 2).min(MAX_MODULE_PATH), 0);
    }
}

/// One piece of machine state the uninstaller removes.
#[derive(Debug)]
pub struct UninstallStep {
    /// What the step removes, for the uninstaller's log.
    pub name: &'static str,
    /// Removes it; the text says what was done.
    pub run: fn() -> Result<String>,
}

/// Every step the uninstaller runs, in order.
pub fn uninstall_steps() -> Vec<UninstallStep> {
    vec![UninstallStep {
        name: "scheduled maintenance tasks",
        run: crate::maintenance::remove_all_for_uninstall,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_for_module_steps_out_of_the_installed_engine_folder() {
        assert_eq!(
            root_for_module(Path::new(
                r"C:\Program Files\Cairn\app\optimizer\native\optimizer_engine.pyd"
            )),
            PathBuf::from(r"C:\Program Files\Cairn")
        );
        assert_eq!(
            root_for_module(Path::new(
                r"C:\Program Files\Cairn\APP\Optimizer\NATIVE\optimizer_engine.pyd"
            )),
            PathBuf::from(r"C:\Program Files\Cairn"),
            "ASCII case is ignored"
        );
        assert_eq!(
            root_for_module(Path::new(r"C:\Program Files\Cairn\optctl.exe")),
            PathBuf::from(r"C:\Program Files\Cairn")
        );
        assert_eq!(
            root_for_module(Path::new(
                r"X:\src\ui\optimizer\native\optimizer_engine.pyd"
            )),
            PathBuf::from(r"X:\src\ui\optimizer\native"),
            "a dev tree keeps the module's folder"
        );
        assert_eq!(
            root_for_module(Path::new(r"X:\optimizer\native\optimizer_engine.pyd")),
            PathBuf::from(r"X:\optimizer\native")
        );
    }

    #[test]
    fn app_root_is_absolute_and_exists() {
        let root = app_root().unwrap();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(root.is_dir(), "{}", root.display());
        assert!(this_module_path().unwrap().is_file());
    }

    #[test]
    fn installer_constants_agree() {
        assert!(UNINSTALL_KEY.ends_with(&format!(r"\{INSTALLER_APP_ID}_is1")));
        assert_eq!(LAUNCHER_PROGRAM, format!("{APP_NAME}.exe"));
        assert!(MAINTENANCE_DLLS
            .iter()
            .all(|dll| LAUNCHER_DLLS.contains(dll)));
    }

    /// Uninstall values of a fake entry; `None` stands for a missing entry.
    struct FakeEntry(Option<Vec<(&'static str, RegValue)>>);

    impl UninstallValues for FakeEntry {
        fn value(&self, name: &str) -> Result<Option<RegValue>> {
            Ok(self.0.as_ref().and_then(|values| {
                values
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            }))
        }
    }

    struct FailingEntry;

    impl UninstallValues for FailingEntry {
        fn value(&self, _name: &str) -> Result<Option<RegValue>> {
            Err(Error::Other("access denied".into()))
        }
    }

    fn entry(location: RegValue) -> FakeEntry {
        FakeEntry(Some(vec![
            ("InstallLocation", location),
            ("DisplayVersion", RegValue::Sz("0.2.0".into())),
        ]))
    }

    fn sz(text: &str) -> RegValue {
        RegValue::Sz(text.into())
    }

    fn every_file(_: &Path) -> bool {
        true
    }

    #[test]
    fn installed_reads_the_entry_and_checks_the_programs() {
        let found = installed_with(&entry(sz(r"C:\Program Files\Cairn\")), &every_file)
            .unwrap()
            .unwrap();
        assert_eq!(
            found,
            InstallInfo {
                dir: PathBuf::from(r"C:\Program Files\Cairn"),
                version: "0.2.0".into(),
                launcher: PathBuf::from(r"C:\Program Files\Cairn\Cairn.exe"),
                cli: PathBuf::from(r"C:\Program Files\Cairn\optctl.exe"),
            }
        );
        let checked = std::cell::RefCell::new(Vec::new());
        let is_file = |path: &Path| {
            checked.borrow_mut().push(path.to_path_buf());
            true
        };
        installed_with(&entry(sz(r"D:\Apps\Cairn")), &is_file).unwrap();
        assert_eq!(
            checked.into_inner(),
            [
                PathBuf::from(r"D:\Apps\Cairn\Cairn.exe"),
                PathBuf::from(r"D:\Apps\Cairn\optctl.exe")
            ]
        );
    }

    #[test]
    fn installed_is_none_without_an_entry_or_location() {
        assert_eq!(installed_with(&FakeEntry(None), &every_file).unwrap(), None);
        let no_location = FakeEntry(Some(vec![("DisplayVersion", sz("0.2.0"))]));
        assert_eq!(installed_with(&no_location, &every_file).unwrap(), None);
        let not_a_string = entry(RegValue::Dword(1));
        assert_eq!(installed_with(&not_a_string, &every_file).unwrap(), None);
    }

    #[test]
    fn installed_accepts_expand_sz_without_expanding_it() {
        let found = installed_with(&entry(RegValue::ExpandSz(r"C:\Cairn".into())), &every_file)
            .unwrap()
            .unwrap();
        assert_eq!(found.dir, PathBuf::from(r"C:\Cairn"));
        let unexpanded = entry(RegValue::ExpandSz(r"%ProgramFiles%\Cairn".into()));
        assert_eq!(installed_with(&unexpanded, &every_file).unwrap(), None);
    }

    #[test]
    fn installed_refuses_paths_that_are_not_local_folders() {
        for location in [
            r"\\server\share\Cairn",
            r"\\?\C:\Program Files\Cairn",
            r"Program Files\Cairn",
            r"C:Cairn",
            r"C:\",
            "",
        ] {
            assert_eq!(
                installed_with(&entry(sz(location)), &every_file).unwrap(),
                None,
                "{location:?}"
            );
        }
    }

    #[test]
    fn installed_needs_both_programs_and_tolerates_a_missing_version() {
        let only_launcher = |path: &Path| path.ends_with(LAUNCHER_PROGRAM);
        assert_eq!(
            installed_with(&entry(sz(r"C:\Cairn")), &only_launcher).unwrap(),
            None
        );
        let no_version = FakeEntry(Some(vec![("InstallLocation", sz(r"C:\Cairn\\"))]));
        let found = installed_with(&no_version, &every_file).unwrap().unwrap();
        assert_eq!(found.version, "");
        assert_eq!(found.dir, PathBuf::from(r"C:\Cairn"));
    }

    #[test]
    fn installed_reports_read_errors() {
        assert!(installed_with(&FailingEntry, &every_file).is_err());
    }

    #[test]
    fn installed_reads_the_live_entry() {
        // Read-only; Cairn may or may not be installed on the machine running the tests.
        if let Some(info) = installed().unwrap() {
            assert!(info.launcher.is_file() && info.cli.is_file());
        }
    }

    #[test]
    fn uninstall_removes_the_maintenance_tasks() {
        let steps = uninstall_steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "scheduled maintenance tasks");
    }

    #[test]
    fn user_texts_use_the_app_name() {
        let texts = [
            ("tools::runner::CLOSING", crate::tools::runner::CLOSING),
            (
                "tools::runner::STORE_NOT_READ",
                crate::tools::runner::STORE_NOT_READ,
            ),
            (
                "tools::runner::UNKNOWN_MEDIA_NOTE",
                crate::tools::runner::UNKNOWN_MEDIA_NOTE,
            ),
            (
                "tools::runner::RUNS_TO_COMPLETION_NOTE",
                crate::tools::runner::RUNS_TO_COMPLETION_NOTE,
            ),
            (
                "tools::launch::DETACH_REFUSED",
                crate::tools::launch::DETACH_REFUSED,
            ),
            (
                "tools::MANUAL_RESTORE_POINT_DESCRIPTION",
                crate::tools::MANUAL_RESTORE_POINT_DESCRIPTION,
            ),
            (
                "safety::restore_point::DEFAULT_DESCRIPTION",
                crate::safety::restore_point::DEFAULT_DESCRIPTION,
            ),
            (
                "debloat::power::APP_SCHEME_NAME",
                crate::debloat::power::APP_SCHEME_NAME,
            ),
        ];
        // Built from parts so that a scan of the sources for the old name skips this file.
        let old_name = ["PC", "Optimizer"].join(" ");
        for (name, text) in texts {
            assert!(
                text.contains(APP_NAME),
                "{name} does not name {APP_NAME}: {text}"
            );
            assert!(
                !text.contains(&old_name),
                "{name} still names {old_name}: {text}"
            );
        }
        assert_eq!(crate::APP_NAME, APP_NAME);
    }
}
