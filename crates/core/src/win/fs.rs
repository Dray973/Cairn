//! Administrator-only folders and bounded reads of regular files.
//!
//! An elevated process that writes a file where a standard user can create entries, or reads
//! one a standard user can replace with a link, can be pointed at another file. A folder made
//! with [`create_private_dir`] grants access to SYSTEM and Administrators only, so nobody else
//! can plant files in it; [`read_small_regular_file`] opens the file itself (never a link's
//! target) and refuses reparse points and files with more than one name.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};

use super::wide;
use crate::{Error, Result};

/// Protected DACL: full access for SYSTEM and Administrators, inherited by files and
/// subfolders; nobody else has any access.
pub const PRIVATE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// A security descriptor the system allocated with LocalAlloc, freed on drop.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: the descriptor was allocated by the API that returned it and is freed
            // exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

/// Creates the folder `path` with [`PRIVATE_DIR_SDDL`] as its security. Fails when `path`
/// already exists (as a folder, a file or a link) or its parent is missing, so an existing
/// folder with other permissions is never taken for a private one.
pub fn create_private_dir(path: &Path) -> Result<()> {
    let sddl = wide(PRIVATE_DIR_SDDL);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated and `sd` a valid out pointer; the descriptor is freed
    // with LocalFree when `descriptor` drops.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?
    };
    let descriptor = LocalDescriptor(sd);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        bInheritHandle: false.into(),
    };
    let name = wide(&path.to_string_lossy());
    // SAFETY: `name` is NUL-terminated; `attributes` and the descriptor it points to stay
    // alive for the call.
    unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&attributes))? };
    drop(descriptor);
    Ok(())
}

/// The contents of the regular file `path`, at most `max` bytes. `Ok(None)` when the file
/// (or its folder) does not exist. The file is opened without following a link; a reparse
/// point (symbolic link, junction and the like), a folder, a file with more than one name
/// (hard links) and a file larger than `max` are refused with an error.
pub fn read_small_regular_file(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let info = file_information(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(Error::Other(format!(
            "{} is a link or mount point; it was not read",
            path.display()
        )));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Err(Error::Other(format!(
            "{} is a folder, not a file",
            path.display()
        )));
    }
    if info.nNumberOfLinks > 1 {
        return Err(Error::Other(format!(
            "{} has {} names (hard links); it was not read",
            path.display(),
            info.nNumberOfLinks
        )));
    }
    let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
    if size > max {
        return Err(Error::Other(format!(
            "{} is {size} bytes, more than the {max} bytes read",
            path.display()
        )));
    }
    let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    // One byte more than allowed tells a file that grew after the check.
    (&mut file).take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(Error::Other(format!(
            "{} grew beyond {max} bytes while it was read",
            path.display()
        )));
    }
    Ok(Some(data))
}

fn file_information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `info` is a valid out
    // pointer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)? };
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::acl::file_security;

    #[test]
    fn a_private_dir_grants_only_system_and_administrators() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("steps");
        create_private_dir(&private).unwrap();
        assert!(private.is_dir());
        let info = file_security(&private).unwrap();
        let aces = info.dacl.expect("a DACL");
        assert!(!aces.is_empty());
        for ace in &aces {
            assert!(ace.allow);
            assert!(
                ace.sid == "S-1-5-18" || ace.sid == "S-1-5-32-544",
                "unexpected trustee {}",
                ace.sid
            );
        }
        // An existing folder is never taken for a private one.
        assert!(create_private_dir(&private).is_err());
        // Nor is a missing parent created.
        assert!(create_private_dir(&dir.path().join("missing").join("steps")).is_err());
        if !crate::is_elevated() {
            // The creator is not in the DACL: an unelevated process cannot plant a file.
            assert!(File::create(private.join("planted.txt")).is_err());
        }
    }

    #[test]
    fn small_regular_files_are_read_and_missing_ones_are_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.json");
        std::fs::write(&path, b"{\"version\": 1}").unwrap();
        assert_eq!(
            read_small_regular_file(&path, 64).unwrap().as_deref(),
            Some(&b"{\"version\": 1}"[..])
        );
        assert_eq!(
            read_small_regular_file(&dir.path().join("none.json"), 64).unwrap(),
            None
        );
        assert_eq!(
            read_small_regular_file(&dir.path().join("no").join("dir.json"), 64).unwrap(),
            None
        );
    }

    #[test]
    fn oversized_files_folders_and_hard_links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![b'x'; 65]).unwrap();
        assert!(read_small_regular_file(&path, 64).is_err());
        assert_eq!(
            read_small_regular_file(&path, 65).unwrap().map(|d| d.len()),
            Some(65)
        );

        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(read_small_regular_file(&folder, 64).is_err());

        let linked = dir.path().join("linked.bin");
        std::fs::hard_link(&path, &linked).unwrap();
        let err = read_small_regular_file(&path, 1024).unwrap_err();
        assert!(err.to_string().contains("hard links"), "{err}");
        assert!(read_small_regular_file(&linked, 1024).is_err());
    }

    #[test]
    fn a_symbolic_link_is_refused_when_one_can_be_made() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        let link = dir.path().join("link.json");
        // Creating a symbolic link needs Developer Mode or an elevated process.
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        let err = read_small_regular_file(&link, 64).unwrap_err();
        assert!(err.to_string().contains("link"), "{err}");
        assert!(read_small_regular_file(&target, 64).unwrap().is_some());
    }
}
