//! Where winget is: `winget.exe` in the App Installer package folder under
//! `Program Files\WindowsApps`, found through the package API for the account Cairn runs as.
//!
//! The `winget` command users type is an alias in `%LOCALAPPDATA%\Microsoft\WindowsApps`,
//! which the user can replace without elevation, so an elevated Cairn never starts it. The
//! package folder is owned by TrustedInstaller; the file there is checked to be a regular
//! file (no reparse point) whose final path is the expected one.

use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES,
};
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
use windows::Win32::UI::Shell::FOLDERID_ProgramFiles;

use crate::win::package::installed_packages;
use crate::win::paths::known_folder;
use crate::{Error, Result};

/// Package family of App Installer, which ships winget.
pub const APP_INSTALLER_FAMILY: &str = "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe";
const FAMILY_NAME: &str = "Microsoft.DesktopAppInstaller";
const PUBLISHER_ID: &str = "8wekyb3d8bbwe";
const PROGRAM: &str = "winget.exe";

fn path_string<S: Serializer>(path: &Path, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.display().to_string())
}

/// winget of the App Installer package installed for this account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WingetLocation {
    #[serde(serialize_with = "path_string")]
    pub path: PathBuf,
    pub package_full_name: String,
    /// The package version, such as "1.29.380.0".
    pub package_version: String,
}

/// `<Program Files>\WindowsApps`, from the known folder (never `%ProgramFiles%`, which the
/// environment can change).
pub fn windows_apps_root() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("WindowsApps"))
}

/// Architecture name of this machine as package names write it ("x64", "arm64", "x86").
fn native_arch() -> &'static str {
    let mut process = IMAGE_FILE_MACHINE(0);
    let mut native = IMAGE_FILE_MACHINE(0);
    // SAFETY: both out pointers are valid locals; the pseudo handle needs no closing.
    let read = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, Some(&mut native)) };
    match (read, native) {
        (Ok(()), IMAGE_FILE_MACHINE_ARM64) => "arm64",
        (Ok(()), IMAGE_FILE_MACHINE_AMD64) => "x64",
        (Ok(()), IMAGE_FILE_MACHINE_I386) => "x86",
        _ if cfg!(target_arch = "aarch64") => "arm64",
        _ => "x64",
    }
}

/// `(name, version, architecture)` of a package full name
/// `Name_Version_Architecture_ResourceId_PublisherId`.
fn name_parts(full_name: &str) -> Option<(&str, &str, &str, &str)> {
    let parts: Vec<&str> = full_name.split('_').collect();
    match parts.as_slice() {
        [name, version, arch, _resource, publisher] => Some((name, version, arch, publisher)),
        _ => None,
    }
}

/// A 4-part numeric version ("1.29.380.0"); missing parts count as 0.
fn version_key(version: &str) -> Option<[u64; 4]> {
    let mut key = [0u64; 4];
    let parts: Vec<&str> = version.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    for (slot, part) in key.iter_mut().zip(parts) {
        *slot = part.parse().ok()?;
    }
    Some(key)
}

/// The package to run winget from: of the App Installer packages whose install folder is
/// known and whose architecture is `native_arch` or "neutral", the one with the highest
/// version. Returns its full name and install folder.
pub(crate) fn choose_package(
    packages: &[(String, Option<PathBuf>)],
    native_arch: &str,
) -> Option<(String, PathBuf)> {
    packages
        .iter()
        .filter_map(|(full_name, dir)| {
            let dir = dir.as_ref()?;
            let (name, version, arch, publisher) = name_parts(full_name)?;
            let fits = name.eq_ignore_ascii_case(FAMILY_NAME)
                && publisher.eq_ignore_ascii_case(PUBLISHER_ID)
                && (arch.eq_ignore_ascii_case(native_arch) || arch.eq_ignore_ascii_case("neutral"));
            fits.then(|| (version_key(version), full_name, dir))
        })
        .filter_map(|(key, full_name, dir)| key.map(|k| (k, full_name, dir)))
        .max_by_key(|(key, _, _)| *key)
        .map(|(_, full_name, dir)| (full_name.clone(), dir.clone()))
}

fn eq_ignore_case(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// Whether `path` is `winget.exe` directly in an App Installer package folder directly in
/// `windows_apps`. Pure: names are compared ignoring ASCII case, and both paths must be
/// absolute.
pub(crate) fn winget_path_ok(path: &Path, windows_apps: &Path) -> bool {
    if !path.is_absolute() || !windows_apps.is_absolute() {
        return false;
    }
    let file_ok = path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(PROGRAM));
    let Some(folder) = path.parent() else {
        return false;
    };
    let folder_name = folder
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let folder_ok = folder_name.starts_with(&format!("{}_", FAMILY_NAME.to_ascii_lowercase()))
        && folder_name.ends_with(&format!("__{PUBLISHER_ID}"));
    let root_ok = folder
        .parent()
        .is_some_and(|root| eq_ignore_case(root, windows_apps));
    file_ok && folder_ok && root_ok
}

fn unexpected_place(path: &Path) -> Error {
    Error::Other(format!(
        "winget was found in an unexpected place ({}); Cairn won't run it.",
        path.display()
    ))
}

/// The final path of the open file, without the `\\?\` prefix.
fn final_path(handle: HANDLE) -> Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `handle` is an open file handle owned by the caller; the buffer is
        // writable for its whole length.
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, FILE_NAME_NORMALIZED) }
            as usize;
        if len == 0 {
            return Err(Error::Win32(windows::core::Error::from_win32()));
        }
        if len < buffer.len() {
            return Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len])));
        }
        buffer.resize(len + 1, 0);
    }
}

/// Checks that `path` is a regular file (not a reparse point) whose final path is `path`.
/// `Ok(false)` when it does not exist.
fn verify_file(path: &Path) -> Result<bool> {
    let file = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call.
    unsafe { GetFileInformationByHandle(handle, &mut info)? };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(unexpected_place(path));
    }
    let resolved = final_path(handle)?;
    let expected = PathBuf::from(format!(r"\\?\{}", path.display()));
    if !eq_ignore_case(&resolved, &expected) {
        return Err(unexpected_place(path));
    }
    Ok(true)
}

/// winget of the App Installer package installed for this account. `Ok(None)` when App
/// Installer is not installed for it (or has no package for this machine's architecture);
/// an error when the package list cannot be read or winget is not where it must be.
pub fn locate() -> Result<Option<WingetLocation>> {
    let packages = installed_packages(APP_INSTALLER_FAMILY).ok_or_else(|| {
        Error::Other("the App Installer packages of this account could not be read".into())
    })?;
    let packages: Vec<(String, Option<PathBuf>)> = packages
        .into_iter()
        .map(|p| (p.full_name, p.install_dir))
        .collect();
    let Some((full_name, dir)) = choose_package(&packages, native_arch()) else {
        return Ok(None);
    };
    let path = dir.join(PROGRAM);
    if !winget_path_ok(&path, &windows_apps_root()?) {
        return Err(unexpected_place(&path));
    }
    if !verify_file(&path)? {
        return Ok(None);
    }
    let package_version = name_parts(&full_name)
        .map(|(_, version, _, _)| version.to_string())
        .unwrap_or_default();
    Ok(Some(WingetLocation {
        path,
        package_full_name: full_name,
        package_version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r"C:\Program Files\WindowsApps";

    fn entry(full_name: &str, dir: bool) -> (String, Option<PathBuf>) {
        (
            full_name.to_string(),
            dir.then(|| PathBuf::from(ROOT).join(full_name)),
        )
    }

    #[test]
    fn the_newest_package_for_this_architecture_is_chosen() {
        let packages = vec![
            entry(
                "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe",
                true,
            ),
            entry(
                "Microsoft.DesktopAppInstaller_1.30.1.0_arm64__8wekyb3d8bbwe",
                true,
            ),
            entry(
                "Microsoft.DesktopAppInstaller_1.28.9.0_x64__8wekyb3d8bbwe",
                true,
            ),
            entry(
                "Microsoft.DesktopAppInstaller_1.31.0.0_x64__8wekyb3d8bbwe",
                false,
            ),
        ];
        let (name, dir) = choose_package(&packages, "x64").unwrap();
        assert_eq!(
            name,
            "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe"
        );
        assert_eq!(dir, PathBuf::from(ROOT).join(&name));
        let (name, _) = choose_package(&packages, "arm64").unwrap();
        assert_eq!(
            name,
            "Microsoft.DesktopAppInstaller_1.30.1.0_arm64__8wekyb3d8bbwe"
        );

        let neutral = vec![
            entry(
                "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe",
                true,
            ),
            entry(
                "Microsoft.DesktopAppInstaller_1.29.1000.0_neutral__8wekyb3d8bbwe",
                true,
            ),
        ];
        let (name, _) = choose_package(&neutral, "x64").unwrap();
        assert!(name.contains("_neutral_"), "{name}");
        assert_eq!(choose_package(&neutral, "x86").unwrap().0, neutral[1].0);

        assert_eq!(choose_package(&[], "x64"), None);
        assert_eq!(
            choose_package(
                &[entry(
                    "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe",
                    false
                )],
                "x64"
            ),
            None
        );
        assert_eq!(
            choose_package(
                &[entry("Contoso.Other_1.0.0.0_x64__8wekyb3d8bbwe", true)],
                "x64"
            ),
            None
        );
        assert_eq!(
            choose_package(&[entry("not-a-full-name", true)], "x64"),
            None
        );
    }

    #[test]
    fn only_the_package_folder_path_is_accepted() {
        let root = Path::new(ROOT);
        let good =
            root.join(r"Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\winget.exe");
        assert!(winget_path_ok(&good, root));
        let upper = PathBuf::from(
            r"C:\PROGRAM FILES\WINDOWSAPPS\MICROSOFT.DESKTOPAPPINSTALLER_1.29.380.0_X64__8WEKYB3D8BBWE\WINGET.EXE",
        );
        assert!(winget_path_ok(&upper, root));

        let alias = Path::new(r"C:\Users\Test\AppData\Local\Microsoft\WindowsApps\winget.exe");
        assert!(!winget_path_ok(alias, root));
        let family = root.join(r"Contoso.Tools_1.0.0.0_x64__8wekyb3d8bbwe\winget.exe");
        assert!(!winget_path_ok(&family, root));
        let publisher =
            root.join(r"Microsoft.DesktopAppInstaller_1.29.380.0_x64__aaaaaaaaaaaaa\winget.exe");
        assert!(!winget_path_ok(&publisher, root));
        let grandparent = Path::new(
            r"D:\Other\Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\winget.exe",
        );
        assert!(!winget_path_ok(grandparent, root));
        let nested = root
            .join(r"Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\sub\winget.exe");
        assert!(!winget_path_ok(&nested, root));
        let other_program = root
            .join(r"Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\AppInstaller.exe");
        assert!(!winget_path_ok(&other_program, root));
        assert!(!winget_path_ok(
            Path::new(r"Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\winget.exe"),
            root
        ));
    }

    #[test]
    fn locate_reads_this_accounts_package_without_starting_anything() {
        // Read-only: the package list and the file's attributes.
        match locate() {
            Ok(Some(location)) => {
                assert!(winget_path_ok(
                    &location.path,
                    &windows_apps_root().unwrap()
                ));
                assert!(!location.package_version.is_empty());
            }
            Ok(None) => {}
            Err(e) => panic!("{e}"),
        }
        assert!(windows_apps_root().unwrap().ends_with("WindowsApps"));
    }
}
