//! The fixed volumes the speed test and the space scan work on. Read-only.

use std::path::Path;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};
use windows::Win32::System::SystemServices::{FILE_PERSISTENT_ACLS, FILE_READ_ONLY_VOLUME};

use super::files::wide_path;
use super::speed::place::find_leftovers;
use super::speed::Leftover;
use crate::sysinfo::{self, DiskInfo, DriveKind, VolumeInfo};
use crate::win::storage::MediaKind;
use crate::Result;

/// A fixed volume with a drive letter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageVolume {
    /// "C:".
    pub letter: String,
    pub label: String,
    pub file_system: String,
    pub size_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub media: MediaKind,
    /// "NVMe", "SATA", … of the disk the volume starts on.
    pub bus: Option<String>,
    pub model: Option<String>,
    /// Holds the Windows folder.
    pub system: bool,
    pub read_only: bool,
    /// The file system keeps ACLs (NTFS, ReFS).
    pub persistent_acls: bool,
    pub not_responding: bool,
    pub error: Option<String>,
    /// Why a speed test cannot run on this volume at all; `None` when it can.
    pub speed_test_blocked: Option<String>,
    /// Why this volume cannot be scanned; `None` when it can.
    pub scan_blocked: Option<String>,
    /// Test folders a speed test left at the volume's root.
    pub leftovers: Vec<Leftover>,
}

/// Fixed, ready volumes with a letter, the Windows volume first. Read-only; the device reads
/// share the system information's 5 s deadline, so a hung drive is listed as not responding.
pub fn volumes() -> Result<Vec<StorageVolume>> {
    let (disks, volumes) = sysinfo::storage_devices()?;
    let mut out: Vec<StorageVolume> = volumes
        .iter()
        .filter(|v| v.kind == DriveKind::Fixed && v.ready)
        .map(|v| {
            let flags = if v.not_responding || v.error.is_some() {
                None
            } else {
                volume_flags(&root_of(&v.letter)).ok()
            };
            let leftovers = if flags.is_some() {
                find_leftovers(&root_of(&v.letter))
            } else {
                Vec::new()
            };
            build(v, &disks, flags, leftovers)
        })
        .collect();
    out.sort_by(|a, b| {
        b.system
            .cmp(&a.system)
            .then_with(|| a.letter.cmp(&b.letter))
    });
    Ok(out)
}

/// `X:\` of a letter such as "C:".
pub(crate) fn root_of(letter: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}\\", letter.trim_end_matches('\\')))
}

/// Pure: the storage view of one sysinfo volume.
pub(crate) fn build(
    v: &VolumeInfo,
    disks: &[DiskInfo],
    flags: Option<u32>,
    leftovers: Vec<Leftover>,
) -> StorageVolume {
    let disk = v
        .disk_numbers
        .first()
        .and_then(|n| disks.iter().find(|d| d.number == *n));
    let read_only = flags.is_some_and(|f| f & FILE_READ_ONLY_VOLUME != 0);
    let persistent_acls = flags.is_some_and(|f| f & FILE_PERSISTENT_ACLS != 0);
    let speed_test_blocked = if let Some(error) = &v.error {
        Some(error.clone())
    } else if v.not_responding {
        Some(format!("{} isn't responding", v.letter))
    } else if read_only {
        Some(format!("{} is read-only", v.letter))
    } else {
        None
    };
    let scan_blocked = if let Some(error) = &v.error {
        Some(error.clone())
    } else if v.not_responding {
        Some(format!("{} isn't responding", v.letter))
    } else {
        None
    };
    StorageVolume {
        letter: v.letter.clone(),
        label: v.label.clone(),
        file_system: v.file_system.clone(),
        size_bytes: v.size_bytes,
        free_bytes: v.free_bytes,
        media: disk.map_or(MediaKind::Unknown, |d| d.media),
        bus: disk.map(|d| d.bus.clone()).filter(|b| !b.is_empty()),
        model: disk.map(|d| d.model.clone()).filter(|m| !m.is_empty()),
        system: v.system,
        read_only,
        persistent_acls,
        not_responding: v.not_responding,
        error: v.error.clone(),
        speed_test_blocked,
        scan_blocked,
        leftovers,
    }
}

/// `lpFileSystemFlags` of the volume whose root is `root` (`C:\`).
pub(crate) fn volume_flags(root: &Path) -> Result<u32> {
    let wide = wide_path(root);
    let mut flags = 0u32;
    // SAFETY: `wide` is NUL-terminated; only the flags are requested.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            None,
            None,
            Some(&mut flags),
            None,
        )
    }?;
    Ok(flags)
}

/// File system name of the volume whose root is `root`; empty when unreadable.
pub(crate) fn file_system(root: &Path) -> String {
    let wide = wide_path(root);
    let mut name = [0u16; 261];
    // SAFETY: `wide` is NUL-terminated and `name` is writable for its whole length.
    let read = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut name),
        )
    };
    match read {
        Ok(()) => crate::win::from_wide_nul(&name),
        Err(_) => String::new(),
    }
}

/// Bytes free on the volume of `path` for this process's user.
pub(crate) fn free_bytes(path: &Path) -> Result<u64> {
    let wide = wide_path(path);
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and `free` is a valid out pointer.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }?;
    Ok(free)
}

/// (total, free for anyone) bytes of the volume of `path`.
pub(crate) fn volume_space(path: &Path) -> Result<(u64, u64)> {
    let wide = wide_path(path);
    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and both outputs are valid u64 locations.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut total),
            Some(&mut free),
        )
    }?;
    Ok((total, free))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(letter: &str) -> VolumeInfo {
        VolumeInfo {
            letter: letter.to_string(),
            label: "Windows".to_string(),
            file_system: "NTFS".to_string(),
            kind: DriveKind::Fixed,
            size_bytes: Some(952 << 30),
            free_bytes: Some(611 << 30),
            disk_numbers: vec![1],
            system: true,
            ready: true,
            not_responding: false,
            error: None,
        }
    }

    fn disk() -> DiskInfo {
        DiskInfo {
            number: 1,
            model: "Test NVMe SSD".to_string(),
            firmware: None,
            bus: "NVMe".to_string(),
            media: MediaKind::Ssd,
            size_bytes: Some(1 << 40),
            removable: false,
            system: true,
        }
    }

    #[test]
    fn a_volume_takes_its_disk_and_flags() {
        let built = build(
            &volume("C:"),
            &[disk()],
            Some(FILE_PERSISTENT_ACLS),
            Vec::new(),
        );
        assert_eq!(built.model.as_deref(), Some("Test NVMe SSD"));
        assert_eq!(built.bus.as_deref(), Some("NVMe"));
        assert_eq!(built.media, MediaKind::Ssd);
        assert!(built.persistent_acls && !built.read_only);
        assert_eq!(built.speed_test_blocked, None);
        assert_eq!(built.scan_blocked, None);
    }

    #[test]
    fn blocked_volumes_say_why() {
        let read_only = build(&volume("D:"), &[], Some(FILE_READ_ONLY_VOLUME), Vec::new());
        assert_eq!(
            read_only.speed_test_blocked.as_deref(),
            Some("D: is read-only")
        );
        assert_eq!(read_only.scan_blocked, None);
        assert_eq!(read_only.media, MediaKind::Unknown);
        let mut hung = volume("E:");
        hung.not_responding = true;
        let hung = build(&hung, &[], None, Vec::new());
        assert_eq!(
            hung.speed_test_blocked.as_deref(),
            Some("E: isn't responding")
        );
        assert_eq!(hung.scan_blocked.as_deref(), Some("E: isn't responding"));
        let mut locked = volume("F:");
        locked.error = Some("cannot read the drive: locked".to_string());
        let locked = build(&locked, &[], None, Vec::new());
        assert_eq!(locked.speed_test_blocked, locked.error);
        assert_eq!(locked.scan_blocked, locked.error);
    }

    #[test]
    fn the_windows_volume_is_listed_first() {
        let list = volumes().unwrap();
        let windows = crate::win::paths::windows_dir().unwrap();
        let letter = windows.to_string_lossy()[..2].to_ascii_uppercase();
        let first = list.first().expect("the Windows volume");
        assert_eq!(first.letter, letter);
        assert!(first.system);
        assert!(first.persistent_acls, "{first:?}");
        let root = root_of(&first.letter);
        assert!(free_bytes(&root).unwrap() > 0);
        let (total, free) = volume_space(&root).unwrap();
        assert!(total >= free && total > 0);
        assert!(!file_system(&root).is_empty());
        let json = serde_json::to_value(first).unwrap();
        assert!(json["leftovers"].is_array());
    }
}
