//! Fixed volumes with media type for the drive tools.
//!
//! Only drive letters on fixed disks are listed. Every query is read-only; the media type
//! comes from storage IOCTLs on a volume handle opened with no access rights.

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;

use super::paths::windows_dir;
use super::storage::{self, MediaKind};
use super::{from_wide_nul, wide};
use crate::Result;

/// A volume with a drive letter on a fixed disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedVolume {
    /// "C:".
    pub letter: String,
    pub label: String,
    /// "NTFS", "ReFS", "FAT32", "exFAT"; empty when unreadable.
    pub file_system: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub media: MediaKind,
    /// Whether the device reports TRIM support; `None` when it does not say.
    pub trim: Option<bool>,
    /// Windows is installed on it.
    pub system: bool,
    /// Why its details could not be read (for example a locked BitLocker drive).
    pub error: Option<String>,
}

/// Length of the label and file system name buffers (`MAX_PATH + 1`).
const NAME_UNITS: usize = 261;

/// Fixed volumes with a drive letter: the Windows volume first, then by letter. A volume
/// whose details cannot be read is listed with `error` set.
pub fn fixed_volumes() -> Result<Vec<FixedVolume>> {
    let system_letter = windows_dir()?
        .to_string_lossy()
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase());
    // SAFETY: GetLogicalDrives takes no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut volumes: Vec<FixedVolume> = (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
        .filter(|&letter| is_fixed(letter))
        .map(|letter| read_volume(letter, Some(letter) == system_letter))
        .collect();
    volumes.sort_by(|a, b| {
        b.system
            .cmp(&a.system)
            .then_with(|| a.letter.cmp(&b.letter))
    });
    Ok(volumes)
}

fn is_fixed(letter: char) -> bool {
    let root = wide(&format!("{letter}:\\"));
    // SAFETY: `root` is NUL-terminated and outlives the call.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

fn read_volume(letter: char, system: bool) -> FixedVolume {
    let root = wide(&format!("{letter}:\\"));
    let mut volume = FixedVolume {
        letter: format!("{letter}:"),
        label: String::new(),
        file_system: String::new(),
        size_bytes: 0,
        free_bytes: 0,
        media: MediaKind::Unknown,
        trim: None,
        system,
        error: None,
    };

    let mut label = [0u16; NAME_UNITS];
    let mut file_system = [0u16; NAME_UNITS];
    // SAFETY: `root` is NUL-terminated; both buffers are writable for their whole length,
    // which the wrapper passes alongside them; the other outputs are not requested.
    let info = unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut file_system),
        )
    };
    match info {
        Ok(()) => {
            volume.label = from_wide_nul(&label);
            volume.file_system = from_wide_nul(&file_system);
        }
        Err(e) => volume.error = Some(format!("cannot read the drive: {}", e.message())),
    }

    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: `root` is NUL-terminated; both outputs are valid u64 locations.
    let space = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root.as_ptr()),
            None,
            Some(&mut total),
            Some(&mut free),
        )
    };
    match space {
        Ok(()) => {
            volume.size_bytes = total;
            volume.free_bytes = free;
        }
        Err(e) => {
            volume
                .error
                .get_or_insert_with(|| format!("cannot read the drive's size: {}", e.message()));
        }
    }

    if let Ok(device) = storage::open_device(&format!(r"\\.\{letter}:")) {
        volume.media = match storage::seek_penalty(&device) {
            Some(true) => MediaKind::Hdd,
            Some(false) => MediaKind::Ssd,
            None => MediaKind::Unknown,
        };
        volume.trim = storage::trim_enabled(&device);
    }
    volume
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_volumes_include_the_system_drive() {
        let volumes = fixed_volumes().unwrap();
        let windows = windows_dir().unwrap();
        let letter = windows.to_string_lossy()[..2].to_ascii_uppercase();
        let first = volumes.first().expect("at least the Windows volume");
        assert_eq!(first.letter, letter);
        assert!(first.system);
        assert!(first.error.is_none(), "{first:?}");
        assert!(first.size_bytes > 0 && first.free_bytes <= first.size_bytes);
        assert!(!first.file_system.is_empty());
        assert_eq!(volumes.iter().filter(|v| v.system).count(), 1);
        for v in &volumes {
            let bytes = v.letter.as_bytes();
            assert!(bytes.len() == 2 && bytes[0].is_ascii_uppercase() && bytes[1] == b':');
        }
        let rest: Vec<&str> = volumes[1..].iter().map(|v| v.letter.as_str()).collect();
        let mut sorted = rest.clone();
        sorted.sort_unstable();
        assert_eq!(rest, sorted);
        let json = serde_json::to_value(first).unwrap();
        assert!(json["media"].is_string());
    }
}
