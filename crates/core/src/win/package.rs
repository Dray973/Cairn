//! Installed packaged apps and their install folders.

use std::path::PathBuf;

use windows::core::{PCWSTR, PWSTR};

use crate::win::wide;

const ERROR_SUCCESS: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// Attempts of a size probe followed by a read, for data that may grow in between.
const MAX_ATTEMPTS: u32 = 4;

// kernel32 exports (appmodel.h). windows-rs places them behind the
// Win32_Storage_Packaging_Appx feature, which this crate does not enable.
#[link(name = "kernel32")]
extern "system" {
    fn GetPackagesByPackageFamily(
        package_family_name: PCWSTR,
        count: *mut u32,
        package_full_names: *mut PWSTR,
        buffer_length: *mut u32,
        buffer: *mut u16,
    ) -> u32;
    fn GetPackagePathByFullName(
        package_full_name: PCWSTR,
        path_length: *mut u32,
        path: *mut u16,
    ) -> u32;
}

/// One installed package of a family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Package {
    pub full_name: String,
    /// Root folder of the installed package; `None` when it cannot be determined.
    pub install_dir: Option<PathBuf>,
}

/// Packages of `family` installed for the account this process runs as. `Some(empty)` when
/// none is installed; `None` when the lookup itself failed.
pub(crate) fn installed_packages(family: &str) -> Option<Vec<Package>> {
    let family_w = wide(family);
    let mut count = 0u32;
    let mut length = 0u32;
    for _ in 0..MAX_ATTEMPTS {
        let mut names = vec![PWSTR::null(); count as usize];
        let mut buffer = vec![0u16; length as usize];
        let names_ptr = if names.is_empty() {
            std::ptr::null_mut()
        } else {
            names.as_mut_ptr()
        };
        let buffer_ptr = if buffer.is_empty() {
            std::ptr::null_mut()
        } else {
            buffer.as_mut_ptr()
        };
        // SAFETY: `family_w` is NUL-terminated; `names` holds `count` pointers and `buffer`
        // holds `length` UTF-16 units, or both are null with zero counts for the size probe.
        let err = unsafe {
            GetPackagesByPackageFamily(
                PCWSTR(family_w.as_ptr()),
                &mut count,
                names_ptr,
                &mut length,
                buffer_ptr,
            )
        };
        match err {
            ERROR_SUCCESS => {
                let used = (count as usize).min(names.len());
                return Some(
                    names[..used]
                        .iter()
                        .filter(|name| !name.is_null())
                        // SAFETY: each pointer refers to a NUL-terminated name inside
                        // `buffer`, which is still alive.
                        .filter_map(|name| unsafe { name.to_string() }.ok())
                        .map(|full_name| Package {
                            install_dir: package_path(&full_name),
                            full_name,
                        })
                        .collect(),
                );
            }
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return None,
        }
    }
    None
}

/// Install folder of an installed package.
pub(crate) fn package_path(full_name: &str) -> Option<PathBuf> {
    let name_w = wide(full_name);
    let mut length = 0u32;
    for _ in 0..MAX_ATTEMPTS {
        let mut buffer = vec![0u16; length as usize];
        let buffer_ptr = if buffer.is_empty() {
            std::ptr::null_mut()
        } else {
            buffer.as_mut_ptr()
        };
        // SAFETY: `name_w` is NUL-terminated; `buffer` holds `length` UTF-16 units, or is
        // null with a zero length for the size probe.
        let err =
            unsafe { GetPackagePathByFullName(PCWSTR(name_w.as_ptr()), &mut length, buffer_ptr) };
        match err {
            ERROR_SUCCESS => {
                let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                return Some(PathBuf::from(String::from_utf16_lossy(&buffer[..end])));
            }
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_family_has_no_installed_packages() {
        assert_eq!(
            installed_packages("PCOptimizer.SelfTest.Missing_0000000000000"),
            Some(Vec::new())
        );
    }

    #[test]
    fn missing_package_has_no_path() {
        assert_eq!(
            package_path("PCOptimizer.SelfTest.Missing_1.0.0.0_x64__0000000000000"),
            None
        );
    }
}
