//! System directories from `GetSystemDirectoryW` and `GetWindowsDirectoryW`, and shell known
//! folders from `SHGetKnownFolderPath`; none of them depends on environment variables.

use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::core::GUID;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};

use crate::{Error, Result};

/// Initial buffer size, in UTF-16 units; `MAX_PATH` covers every standard installation.
const INITIAL_UNITS: usize = 260;
/// Attempts of a read that reports a larger buffer each time.
const MAX_ATTEMPTS: usize = 4;

/// `C:\Windows\System32` (the 64-bit system directory of this installation).
pub fn system_dir() -> Result<PathBuf> {
    // SAFETY: `buf` is a writable slice whose length is passed alongside it.
    read_dir(|buf| unsafe { GetSystemDirectoryW(Some(buf)) })
}

/// `C:\Windows` (the Windows directory of this installation).
pub fn windows_dir() -> Result<PathBuf> {
    // SAFETY: as above.
    read_dir(|buf| unsafe { GetWindowsDirectoryW(Some(buf)) })
}

/// Path of a shell known folder (for example `FOLDERID_ProgramFiles`) of the account this
/// process runs as, read from the shell rather than from environment variables, whether or
/// not the folder exists.
pub fn known_folder(id: &GUID) -> Result<PathBuf> {
    // SAFETY: `id` is a valid GUID; the returned buffer is freed below with CoTaskMemFree.
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None) }?;
    // SAFETY: on success `raw` points to a NUL-terminated string owned by this call; it is
    // copied before the buffer is freed exactly once.
    let path = unsafe {
        let path = PathBuf::from(OsString::from_wide(raw.as_wide()));
        CoTaskMemFree(Some(raw.0 as *const c_void));
        path
    };
    if path.as_os_str().is_empty() {
        return Err(Error::Other(format!("the shell folder {id:?} has no path")));
    }
    Ok(path)
}

/// Runs a `GetXxxDirectoryW`-style call: the return value is the length copied without
/// the NUL, the size needed including the NUL when the buffer is too small, or 0 on error.
fn read_dir(mut call: impl FnMut(&mut [u16]) -> u32) -> Result<PathBuf> {
    let mut buf = vec![0u16; INITIAL_UNITS];
    for _ in 0..MAX_ATTEMPTS {
        let n = call(&mut buf) as usize;
        if n == 0 {
            return Err(Error::Win32(windows::core::Error::from_win32()));
        }
        if n < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
    Err(Error::Other(
        "the directory path kept growing while it was read".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_directories_are_absolute_and_exist() {
        let system = system_dir().unwrap();
        let windows = windows_dir().unwrap();
        assert!(system.is_absolute(), "{}", system.display());
        assert!(windows.is_absolute(), "{}", windows.display());
        assert!(system.is_dir(), "{}", system.display());
        assert!(windows.is_dir(), "{}", windows.display());
        assert!(system.join("cmd.exe").is_file());
    }

    #[test]
    fn known_folders_are_absolute() {
        use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_ProgramFiles};
        let program_files = known_folder(&FOLDERID_ProgramFiles).unwrap();
        assert!(program_files.is_absolute(), "{}", program_files.display());
        assert!(program_files.is_dir(), "{}", program_files.display());
        let local = known_folder(&FOLDERID_LocalAppData).unwrap();
        assert!(local.is_absolute(), "{}", local.display());
        assert!(known_folder(&GUID::zeroed()).is_err());
    }

    #[test]
    fn a_short_buffer_is_grown_to_the_reported_size() {
        let text: Vec<u16> = r"C:\Some\Long\Directory".encode_utf16().collect();
        let mut calls = 0;
        let path = read_dir(|buf| {
            calls += 1;
            if buf.len() <= text.len() {
                return text.len() as u32 + 1;
            }
            buf[..text.len()].copy_from_slice(&text);
            buf[text.len()] = 0;
            text.len() as u32
        })
        .unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Some\Long\Directory"));
        assert_eq!(calls, 1, "the initial buffer already fits");

        let long: Vec<u16> = "x".repeat(300).encode_utf16().collect();
        let mut sizes = Vec::new();
        let path = read_dir(|buf| {
            sizes.push(buf.len());
            if buf.len() <= long.len() {
                return long.len() as u32 + 1;
            }
            buf[..long.len()].copy_from_slice(&long);
            long.len() as u32
        })
        .unwrap();
        assert_eq!(path.as_os_str().len(), 300);
        assert_eq!(sizes, vec![INITIAL_UNITS, 302]);
    }
}
