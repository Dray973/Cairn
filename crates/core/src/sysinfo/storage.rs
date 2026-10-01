//! Physical disks and volumes.
//!
//! Volumes come from `GetLogicalDrives`; disks are opened as `\\.\PhysicalDriveN` with no
//! access rights and queried with `FILE_ANY_ACCESS` IOCTLs only. Every query that can wait
//! on a drive's medium runs on its own thread, and all of them share one deadline, so a
//! drive that does not answer (a failing card reader, a sleeping external disk) cannot stall
//! the snapshot:
//!
//! - each volume query; a volume that misses the deadline is reported as not responding;
//! - the seek penalty and size queries of each disk, together on one thread per disk. The
//!   first seek penalty query after the device starts can send an INQUIRY for the Block
//!   Device Characteristics VPD page, and the size (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`)
//!   reads the capacity of removable media. A disk that misses the deadline is listed
//!   without its size, and its media kind comes from its bus alone.
//!
//! A disk's device descriptor is answered by the port driver from the INQUIRY data it
//! cached when the device started, so it is read on the calling thread. A thread that
//! misses the deadline is left to finish on its own and holds the handle it uses until
//! then. Every descriptor the driver fills is parsed from bytes with bounds checks.
//! Critical-error dialogs ("insert a disk into drive E:") are suppressed on every thread
//! that queries a drive.

use std::collections::BTreeSet;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NOT_READY};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows::Win32::System::Ioctl::{StorageDeviceProperty, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX};
use windows::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE, DRIVE_UNKNOWN,
};

use super::{DiskInfo, DriveKind, VolumeInfo};
use crate::win::error_mode::ErrorModeGuard;
use crate::win::handle::OwnedHandle;
use crate::win::paths::windows_dir;
use crate::win::storage::{
    device_io_control, open_device, query_storage_property, seek_penalty, MediaKind,
};
use crate::win::{from_wide_nul, is_win32, wide};
use crate::Result;

/// Time the volume and disk queries of one read may take together.
const DEADLINE: Duration = Duration::from_secs(5);
/// Time at least given to the size queries of disks that only a volume's extents name;
/// those disks are found once the volumes have answered.
const LATE_DISK_TIME: Duration = Duration::from_secs(1);
/// `\\.\PhysicalDriveN` numbers probed besides those the volumes name.
const PROBED_DISKS: u32 = 64;
/// Buffer for `STORAGE_DEVICE_DESCRIPTOR` with its strings.
const DESCRIPTOR_BUFFER: usize = 4096;
/// Buffer for `DISK_GEOMETRY_EX`, which ends in variable partition and detection data.
const GEOMETRY_BUFFER: usize = 256;
/// Characters of a volume label or file system name, with the NUL.
const NAME_UNITS: usize = 261;

/// `STORAGE_DEVICE_DESCRIPTOR` offsets.
const DESCRIPTOR_REMOVABLE: usize = 10;
const DESCRIPTOR_VENDOR: usize = 12;
const DESCRIPTOR_PRODUCT: usize = 16;
const DESCRIPTOR_REVISION: usize = 20;
const DESCRIPTOR_BUS_TYPE: usize = 28;
/// Size of the fixed part; string offsets point past it.
const DESCRIPTOR_FIXED: usize = 36;
/// `DISK_GEOMETRY_EX.DiskSize`, after the 24-byte `DISK_GEOMETRY`.
const GEOMETRY_DISK_SIZE: usize = 24;
/// `VOLUME_DISK_EXTENTS`: the count, padding, then 24-byte `DISK_EXTENT`s.
const EXTENTS_FIRST: usize = 8;
const EXTENT_SIZE: usize = 24;
/// Upper bound on extents of one volume, far above any real spanned volume.
const MAX_EXTENTS: usize = 1024;

/// `STORAGE_BUS_TYPE` values with special media rules.
const BUS_USB: u32 = 7;
const BUS_SD: u32 = 12;
const BUS_MMC: u32 = 13;
const BUS_NVME: u32 = 17;
const BUS_SCM: u32 = 18;

pub(super) fn read() -> Result<(Vec<DiskInfo>, Vec<VolumeInfo>)> {
    let _errors = ErrorModeGuard::new();
    let system_letter = windows_dir().ok().and_then(|dir| drive_letter(&dir));
    Ok(read_with(
        &LiveDrives,
        list_volumes(),
        system_letter,
        DEADLINE,
    ))
}

/// The queries storage is read with: [`LiveDrives`] on this PC, stand-ins in tests.
trait Drives: Clone + Send + 'static {
    /// An opened disk, shared with the thread that queries it.
    type Disk: Clone + Send + 'static;

    /// Label, file system, size and disk extents of one volume; may wait on the medium.
    fn query_volume(&self, letter: char) -> VolumeQuery;

    /// Opens physical disk `number` and reads its device descriptor, which the port driver
    /// answers without sending a command to the device; `None` when the disk does not exist
    /// or cannot be opened.
    fn open_disk(&self, number: u32) -> Option<(DeviceDescriptor, Self::Disk)>;

    /// Seek penalty and size of an opened disk; may wait on the device.
    fn query_disk(&self, disk: &Self::Disk) -> DiskQuery;
}

/// What the queries of an opened disk that may wait on the device found.
#[derive(Debug, Clone, Default, PartialEq)]
struct DiskQuery {
    seek_penalty: Option<bool>,
    size: Option<u64>,
}

/// The drives of this PC.
#[derive(Debug, Clone, Copy)]
struct LiveDrives;

impl Drives for LiveDrives {
    type Disk = Arc<OwnedHandle>;

    fn query_volume(&self, letter: char) -> VolumeQuery {
        query_volume(letter)
    }

    fn open_disk(&self, number: u32) -> Option<(DeviceDescriptor, Arc<OwnedHandle>)> {
        let device = open_device(&format!(r"\\.\PhysicalDrive{number}")).ok()?;
        let mut buf = vec![0u8; DESCRIPTOR_BUFFER];
        let descriptor = query_storage_property(&device, StorageDeviceProperty.0, &mut buf)
            .ok()
            .and_then(|n| parse_device_descriptor(&buf[..n]))
            .unwrap_or_default();
        Some((descriptor, Arc::new(device)))
    }

    fn query_disk(&self, device: &Arc<OwnedHandle>) -> DiskQuery {
        let seek_penalty = seek_penalty(device);
        let mut geometry = [0u8; GEOMETRY_BUFFER];
        let size = device_io_control(device, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry)
            .ok()
            .and_then(|n| parse_disk_size(&geometry[..n]));
        DiskQuery { seek_penalty, size }
    }
}

/// Reads the volumes of `listed` and every disk `drives` can open. The volume queries and
/// the disk queries run at the same time, each on its own thread, until one shared
/// `deadline`; see the module docs.
fn read_with<D: Drives>(
    drives: &D,
    listed: Vec<(char, DriveKind)>,
    system_letter: Option<char>,
    deadline: Duration,
) -> (Vec<DiskInfo>, Vec<VolumeInfo>) {
    let end = Instant::now() + deadline;
    // Optical drives are not queried: an empty tray can take seconds to answer.
    let queried: Vec<char> = listed
        .iter()
        .filter(|(_, kind)| *kind != DriveKind::Optical)
        .map(|&(letter, _)| letter)
        .collect();
    let volume_drives = drives.clone();
    let volume_queries = start_all(
        &queried,
        |letter| format!("sysinfo-volume-{letter}"),
        move |letter| guarded_query(|| volume_drives.query_volume(letter)),
    );
    let (mut disks, disk_queries) = probe_disks(drives, 0..PROBED_DISKS);

    let answers = volume_queries.collect(end);
    let volumes: Vec<VolumeInfo> = listed
        .into_iter()
        .map(|(letter, kind)| {
            let answer = queried
                .iter()
                .position(|&l| l == letter)
                .and_then(|i| answers[i].clone());
            volume_info(letter, kind, answer, system_letter == Some(letter))
        })
        .collect();

    // Disks past the probed numbers are found through the volumes' extents.
    let late: BTreeSet<u32> = volumes
        .iter()
        .flat_map(|v| v.disk_numbers.iter().copied())
        .filter(|&number| number >= PROBED_DISKS)
        .collect();
    let (late_disks, late_disk_queries) = probe_disks(drives, late);
    let mut disk_answers = disk_queries.collect(end);
    let late_end = end.max(Instant::now() + LATE_DISK_TIME.min(deadline));
    disk_answers.extend(late_disk_queries.collect(late_end));
    disks.extend(late_disks);

    let system_disks: Vec<u32> = volumes
        .iter()
        .filter(|v| v.system)
        .flat_map(|v| v.disk_numbers.iter().copied())
        .collect();
    let disks = disks
        .into_iter()
        .zip(disk_answers)
        .map(|((number, descriptor), answer)| {
            disk_info(
                number,
                descriptor,
                answer.flatten(),
                system_disks.contains(&number),
            )
        })
        .collect();
    (disks, volumes)
}

// ───────────────────────────── Threads ─────────────────────────────

/// Queries running on their own threads, collected until a deadline.
struct Running<T> {
    answers: Vec<Option<T>>,
    receiver: mpsc::Receiver<(usize, T)>,
    pending: usize,
}

/// Runs `query` for every item, each on its own thread named by `name`, with critical-error
/// dialogs suppressed there. When a thread cannot be started the item is queried on the
/// calling thread.
fn start_all<I, T, F>(items: &[I], name: impl Fn(&I) -> String, query: F) -> Running<T>
where
    I: Clone + Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> T + Clone + Send + 'static,
{
    let mut answers: Vec<Option<T>> = items.iter().map(|_| None).collect();
    let (sender, receiver) = mpsc::channel::<(usize, T)>();
    let mut pending = 0usize;
    for (index, item) in items.iter().enumerate() {
        let sender = sender.clone();
        let threaded = query.clone();
        let owned = item.clone();
        let spawned = thread::Builder::new().name(name(item)).spawn(move || {
            let _errors = ErrorModeGuard::new();
            let _ = sender.send((index, threaded(owned)));
        });
        match spawned {
            Ok(_) => pending += 1,
            Err(_) => answers[index] = Some(query(item.clone())),
        }
    }
    Running {
        answers,
        receiver,
        pending,
    }
}

impl<T> Running<T> {
    /// The answers in item order: `None` for a query that had not answered by `end`, whose
    /// thread is left to finish on its own. A query whose thread ended without an answer
    /// (it panicked) is `None` too and does not hold the collection up.
    fn collect(mut self, end: Instant) -> Vec<Option<T>> {
        while self.pending > 0 {
            let left = end.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(left) {
                Ok((index, answer)) => {
                    self.answers[index] = Some(answer);
                    self.pending -= 1;
                }
                Err(_) => break,
            }
        }
        self.answers
    }
}

// ───────────────────────────── Volumes ─────────────────────────────

/// Letter of a path such as `C:\Windows`.
fn drive_letter(path: &Path) -> Option<char> {
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then_some(letter)
}

/// Letters whose bit is set in a `GetLogicalDrives` mask.
pub(super) fn letters(mask: u32) -> Vec<char> {
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
        .collect()
}

/// Kind of a `GetDriveTypeW` result; `None` for network drives and letters without a
/// root directory, which are not listed.
pub(super) fn drive_kind(drive_type: u32) -> Option<DriveKind> {
    match drive_type {
        DRIVE_FIXED => Some(DriveKind::Fixed),
        DRIVE_REMOVABLE => Some(DriveKind::Removable),
        DRIVE_CDROM => Some(DriveKind::Optical),
        DRIVE_RAMDISK => Some(DriveKind::RamDisk),
        DRIVE_UNKNOWN => Some(DriveKind::Unknown),
        _ => None,
    }
}

/// What one volume query found.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct VolumeQuery {
    pub label: String,
    pub file_system: String,
    pub size_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub disk_numbers: Vec<u32>,
    pub ready: bool,
    pub error: Option<String>,
}

/// Letters of the drives that are listed, with their kind: every letter with a root
/// directory except network drives. Nothing here waits on a medium.
fn list_volumes() -> Vec<(char, DriveKind)> {
    // SAFETY: GetLogicalDrives takes no arguments.
    let mask = unsafe { GetLogicalDrives() };
    letters(mask)
        .into_iter()
        .filter_map(|letter| {
            let root = wide(&format!("{letter}:\\"));
            // SAFETY: `root` is NUL-terminated and outlives the call.
            let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
            drive_kind(drive_type).map(|kind| (letter, kind))
        })
        .collect()
}

/// A panicking query reports an internal error for its volume only.
fn guarded_query(query: impl FnOnce() -> VolumeQuery) -> VolumeQuery {
    panic::catch_unwind(AssertUnwindSafe(query)).unwrap_or_else(|_| VolumeQuery {
        ready: true,
        error: Some("internal error while reading the volume".to_string()),
        ..VolumeQuery::default()
    })
}

/// Combines the listing of a letter with its query answer: `None` for an optical drive
/// (never queried) or a query that missed the deadline.
pub(super) fn volume_info(
    letter: char,
    kind: DriveKind,
    answer: Option<VolumeQuery>,
    system: bool,
) -> VolumeInfo {
    let not_responding = answer.is_none() && kind != DriveKind::Optical;
    let answer = answer.unwrap_or_default();
    VolumeInfo {
        letter: format!("{letter}:"),
        label: answer.label,
        file_system: answer.file_system,
        kind,
        size_bytes: answer.size_bytes,
        free_bytes: answer.free_bytes,
        disk_numbers: answer.disk_numbers,
        system,
        ready: answer.ready,
        not_responding,
        error: answer.error,
    }
}

/// Label, file system, size and disk extents of one volume. Read-only.
fn query_volume(letter: char) -> VolumeQuery {
    let root = wide(&format!("{letter}:\\"));
    let mut label = [0u16; NAME_UNITS];
    let mut file_system = [0u16; NAME_UNITS];
    // SAFETY: `root` is NUL-terminated; both name buffers are writable slices whose lengths
    // the wrapper passes; the other out parameters are omitted.
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
    let info_error = match info {
        Ok(()) => None,
        Err(e) if is_win32(&e, ERROR_NOT_READY) => return VolumeQuery::default(),
        Err(e) => Some(e),
    };

    let mut available = 0u64;
    let mut total = 0u64;
    // SAFETY: `root` is NUL-terminated; both counters are valid out pointers.
    let space = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            None,
        )
    };
    let (size_bytes, free_bytes, error) = match (space, info_error) {
        (Ok(()), _) => (Some(total), Some(available.min(total)), None),
        (Err(e), _) if is_win32(&e, ERROR_NOT_READY) => return VolumeQuery::default(),
        (Err(_), Some(e)) | (Err(e), None) => (None, None, Some(e.message())),
    };
    VolumeQuery {
        label: from_wide_nul(&label).trim().to_string(),
        file_system: from_wide_nul(&file_system).trim().to_string(),
        size_bytes,
        free_bytes,
        disk_numbers: disk_extents(letter),
        ready: true,
        error,
    }
}

/// Numbers of the physical disks a volume spans, the disk holding its start first.
fn disk_extents(letter: char) -> Vec<u32> {
    let Ok(device) = open_device(&format!(r"\\.\{letter}:")) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; EXTENTS_FIRST + 4 * EXTENT_SIZE];
    for _ in 0..3 {
        match device_io_control(&device, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut buf) {
            Ok(n) => return parse_disk_extents(&buf[..n]),
            Err(e) if e.win32_code() == Some(ERROR_MORE_DATA.0) => {
                // The header holds the full count even when the extents did not fit.
                let count = u32_at(&buf, 0).unwrap_or(0) as usize;
                if count == 0 || count > MAX_EXTENTS {
                    return Vec::new();
                }
                buf = vec![0u8; EXTENTS_FIRST + count * EXTENT_SIZE];
            }
            Err(_) => return Vec::new(),
        }
    }
    Vec::new()
}

// ───────────────────────────── Disks ─────────────────────────────

/// Opens the disks among `numbers` that exist, in order, reads their device descriptors,
/// and starts the seek penalty and size queries of each on its own thread. Opening a disk
/// number that does not exist fails at once.
fn probe_disks<D: Drives>(
    drives: &D,
    numbers: impl IntoIterator<Item = u32>,
) -> (Vec<(u32, DeviceDescriptor)>, Running<Option<DiskQuery>>) {
    let mut found = Vec::new();
    let mut opened = Vec::new();
    for number in numbers {
        if let Some((descriptor, disk)) = drives.open_disk(number) {
            found.push((number, descriptor));
            opened.push((number, disk));
        }
    }
    let drives = drives.clone();
    let queries = start_all(
        &opened,
        |(number, _)| format!("sysinfo-disk-{number}"),
        // A panicking query leaves that disk's seek penalty and size unknown.
        move |(_, disk)| panic::catch_unwind(AssertUnwindSafe(|| drives.query_disk(&disk))).ok(),
    );
    (found, queries)
}

/// One physical disk: its device descriptor, the answer of its queries when that arrived
/// in time, and whether it holds the Windows volume. Without an answer the size is unknown
/// and the media kind comes from the bus alone.
fn disk_info(
    number: u32,
    descriptor: DeviceDescriptor,
    answer: Option<DiskQuery>,
    system: bool,
) -> DiskInfo {
    let seek_penalty = answer.as_ref().and_then(|a| a.seek_penalty);
    DiskInfo {
        number,
        model: model(descriptor.vendor.as_deref(), descriptor.product.as_deref()),
        firmware: descriptor.revision,
        bus: bus_label(descriptor.bus_type).to_string(),
        media: media(descriptor.bus_type, seek_penalty),
        size_bytes: answer.and_then(|a| a.size),
        removable: descriptor.removable,
        system,
    }
}

fn u32_at(buf: &[u8], offset: usize) -> Option<u32> {
    let b = buf.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The parts of a `STORAGE_DEVICE_DESCRIPTOR` the report uses. The serial number is never
/// read.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct DeviceDescriptor {
    pub removable: bool,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub bus_type: u32,
}

/// Parses a `STORAGE_DEVICE_DESCRIPTOR`: `RemovableMedia` at 10, the vendor, product and
/// revision string offsets at 12, 16 and 20, and `BusType` at 28. The serial number offset
/// at 24 is never read. `None` when the buffer is shorter than the fixed fields.
pub(super) fn parse_device_descriptor(buf: &[u8]) -> Option<DeviceDescriptor> {
    let bus_type = u32_at(buf, DESCRIPTOR_BUS_TYPE)?;
    Some(DeviceDescriptor {
        removable: buf.get(DESCRIPTOR_REMOVABLE).is_some_and(|&b| b != 0),
        vendor: descriptor_string(buf, DESCRIPTOR_VENDOR),
        product: descriptor_string(buf, DESCRIPTOR_PRODUCT),
        revision: descriptor_string(buf, DESCRIPTOR_REVISION),
        bus_type,
    })
}

/// The NUL-terminated ASCII string whose offset is the DWORD at `field`; `None` for an
/// offset of 0, one inside the fixed fields or past the buffer, a string without its NUL,
/// or an empty string.
fn descriptor_string(buf: &[u8], field: usize) -> Option<String> {
    let offset = u32_at(buf, field)? as usize;
    if offset < DESCRIPTOR_FIXED {
        return None;
    }
    let rest = buf.get(offset..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    let text = String::from_utf8_lossy(&rest[..len]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `DiskSize` of a `DISK_GEOMETRY_EX`; `None` for a short buffer or a negative size.
pub(super) fn parse_disk_size(buf: &[u8]) -> Option<u64> {
    let b = buf.get(GEOMETRY_DISK_SIZE..GEOMETRY_DISK_SIZE + 8)?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(b);
    u64::try_from(i64::from_le_bytes(bytes)).ok()
}

/// Disk numbers of a `VOLUME_DISK_EXTENTS`, in extent order; extents past the buffer are
/// left out.
pub(super) fn parse_disk_extents(buf: &[u8]) -> Vec<u32> {
    let count = u32_at(buf, 0).unwrap_or(0) as usize;
    (0..count.min(MAX_EXTENTS))
        .map_while(|i| u32_at(buf, EXTENTS_FIRST + i * EXTENT_SIZE))
        .collect()
}

/// Vendor names that say nothing about the drive.
const GENERIC_VENDORS: &[&str] = &["ATA", "NVMe", "(Standard disk drives)"];

/// Model line of a disk: the product, preceded by the vendor unless the vendor is generic,
/// empty, or already starts the product name.
pub(super) fn model(vendor: Option<&str>, product: Option<&str>) -> String {
    let product = product.map(str::trim).filter(|p| !p.is_empty());
    let vendor = vendor
        .map(str::trim)
        .filter(|v| !v.is_empty() && !GENERIC_VENDORS.iter().any(|g| g.eq_ignore_ascii_case(v)))
        .filter(|v| match product {
            Some(p) => !p.to_ascii_lowercase().starts_with(&v.to_ascii_lowercase()),
            None => true,
        });
    match (vendor, product) {
        (Some(v), Some(p)) => format!("{v} {p}"),
        (Some(v), None) => v.to_string(),
        (None, Some(p)) => p.to_string(),
        (None, None) => "Unknown disk".to_string(),
    }
}

/// Name of a `STORAGE_BUS_TYPE`.
pub(super) fn bus_label(bus_type: u32) -> &'static str {
    match bus_type {
        1 => "SCSI",
        3 => "ATA",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD card",
        13 => "MMC",
        14 | 15 => "Virtual disk",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "Storage-class memory",
        19 => "UFS",
        _ => "Other",
    }
}

/// SSD or hard disk from the seek penalty the driver reports. USB, SD and MMC bridges often
/// report no penalty for spinning disks, so there it proves nothing; without a report, NVMe
/// and storage-class memory are solid state.
pub(super) fn media(bus_type: u32, seek_penalty: Option<bool>) -> MediaKind {
    match seek_penalty {
        Some(true) => MediaKind::Hdd,
        Some(false) if matches!(bus_type, BUS_USB | BUS_SD | BUS_MMC) => MediaKind::Unknown,
        Some(false) => MediaKind::Ssd,
        None if matches!(bus_type, BUS_NVME | BUS_SCM) => MediaKind::Ssd,
        None => MediaKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A `STORAGE_DEVICE_DESCRIPTOR` with `strings` placed after the fixed fields; each
    /// entry is (offset field, text), and a text of `None` leaves the offset at 0.
    fn descriptor(removable: bool, bus_type: u32, strings: &[(usize, Option<&str>)]) -> Vec<u8> {
        let mut buf = vec![0u8; 40];
        buf[0..4].copy_from_slice(&40u32.to_le_bytes());
        buf[DESCRIPTOR_REMOVABLE] = u8::from(removable);
        buf[11] = 1;
        buf[DESCRIPTOR_BUS_TYPE..DESCRIPTOR_BUS_TYPE + 4].copy_from_slice(&bus_type.to_le_bytes());
        for &(field, text) in strings {
            if let Some(text) = text {
                let offset = buf.len() as u32;
                buf[field..field + 4].copy_from_slice(&offset.to_le_bytes());
                buf.extend(text.bytes());
                buf.push(0);
            }
        }
        let size = buf.len() as u32;
        buf[4..8].copy_from_slice(&size.to_le_bytes());
        buf
    }

    #[test]
    fn device_descriptor_of_an_nvme_disk() {
        // An NVMe disk: no vendor string, the product, the firmware revision and a serial
        // number (two EUI-64 values from the RFC 7042 documentation range) that must never
        // be read.
        let buf = descriptor(
            false,
            17,
            &[
                (DESCRIPTOR_VENDOR, None),
                (DESCRIPTOR_PRODUCT, Some("Northwind NV1000 1TB")),
                (DESCRIPTOR_REVISION, Some("FW100201")),
                (24, Some("0000_5EEF_1000_0001_0000_5EEF_1000_0002.")),
            ],
        );
        let d = parse_device_descriptor(&buf).unwrap();
        assert_eq!(d.vendor, None);
        assert_eq!(d.product.as_deref(), Some("Northwind NV1000 1TB"));
        assert_eq!(d.revision.as_deref(), Some("FW100201"));
        assert_eq!(d.bus_type, 17);
        assert!(!d.removable);
        assert!(
            !format!("{d:?}").contains("5EEF"),
            "the serial number must never be read"
        );
        assert_eq!(
            model(d.vendor.as_deref(), d.product.as_deref()),
            "Northwind NV1000 1TB"
        );
        assert_eq!(bus_label(d.bus_type), "NVMe");
        assert_eq!(media(d.bus_type, Some(false)), MediaKind::Ssd);

        let usb = descriptor(
            true,
            7,
            &[
                (DESCRIPTOR_VENDOR, Some("SanDisk ")),
                (DESCRIPTOR_PRODUCT, Some("Ultra Fit       ")),
                (DESCRIPTOR_REVISION, Some("1.00")),
            ],
        );
        let d = parse_device_descriptor(&usb).unwrap();
        assert!(d.removable);
        assert_eq!(d.vendor.as_deref(), Some("SanDisk"));
        assert_eq!(
            model(d.vendor.as_deref(), d.product.as_deref()),
            "SanDisk Ultra Fit"
        );
    }

    #[test]
    fn descriptor_offsets_out_of_range_are_none() {
        let mut buf = descriptor(
            false,
            11,
            &[(DESCRIPTOR_PRODUCT, Some("Samsung SSD 870 EVO 1TB"))],
        );
        // Offsets past the buffer, inside the fixed fields and to a string without its NUL.
        buf[DESCRIPTOR_VENDOR..DESCRIPTOR_VENDOR + 4].copy_from_slice(&5000u32.to_le_bytes());
        buf[DESCRIPTOR_REVISION..DESCRIPTOR_REVISION + 4].copy_from_slice(&8u32.to_le_bytes());
        let d = parse_device_descriptor(&buf).unwrap();
        assert_eq!(d.vendor, None);
        assert_eq!(d.revision, None);
        assert_eq!(d.product.as_deref(), Some("Samsung SSD 870 EVO 1TB"));

        let without_nul = &buf[..buf.len() - 1];
        assert_eq!(parse_device_descriptor(without_nul).unwrap().product, None);

        // Too short for the bus type: nothing is parsed.
        assert_eq!(parse_device_descriptor(&buf[..31]), None);
        assert_eq!(parse_device_descriptor(&[]), None);
        let d = parse_device_descriptor(&buf[..32]).unwrap();
        assert_eq!(d.bus_type, 11);
        assert_eq!(d.product, None, "the string lies past a truncated buffer");
    }

    #[test]
    fn model_drops_generic_or_repeated_vendor() {
        assert_eq!(
            model(Some("ATA"), Some("Samsung SSD 870 EVO 1TB")),
            "Samsung SSD 870 EVO 1TB"
        );
        assert_eq!(
            model(Some("NVMe"), Some("Northwind NV1000")),
            "Northwind NV1000"
        );
        assert_eq!(
            model(Some("nvme"), Some("Northwind NV1000")),
            "Northwind NV1000"
        );
        assert_eq!(
            model(Some("(Standard disk drives)"), Some("KINGSTON SA400")),
            "KINGSTON SA400"
        );
        assert_eq!(
            model(Some("Samsung"), Some("Samsung SSD 990 PRO")),
            "Samsung SSD 990 PRO"
        );
        assert_eq!(
            model(Some("Seagate"), Some("Expansion HDD")),
            "Seagate Expansion HDD"
        );
        assert_eq!(model(Some("  "), Some(" ST2000DM008 ")), "ST2000DM008");
        assert_eq!(model(Some("Generic"), None), "Generic");
        assert_eq!(model(Some("ATA"), Some("  ")), "Unknown disk");
        assert_eq!(model(None, None), "Unknown disk");
    }

    #[test]
    fn media_rules_per_bus() {
        assert_eq!(media(11, Some(true)), MediaKind::Hdd);
        assert_eq!(media(7, Some(true)), MediaKind::Hdd);
        assert_eq!(media(11, Some(false)), MediaKind::Ssd);
        assert_eq!(media(17, Some(false)), MediaKind::Ssd);
        assert_eq!(media(7, Some(false)), MediaKind::Unknown, "USB bridges");
        assert_eq!(media(12, Some(false)), MediaKind::Unknown);
        assert_eq!(media(13, Some(false)), MediaKind::Unknown);
        assert_eq!(media(17, None), MediaKind::Ssd);
        assert_eq!(media(18, None), MediaKind::Ssd);
        assert_eq!(media(11, None), MediaKind::Unknown);
        assert_eq!(media(7, None), MediaKind::Unknown);
    }

    fn extents(disks: &[u32], count: u32) -> Vec<u8> {
        let mut buf = vec![0u8; EXTENTS_FIRST];
        buf[0..4].copy_from_slice(&count.to_le_bytes());
        for (i, &disk) in disks.iter().enumerate() {
            let mut extent = vec![0u8; EXTENT_SIZE];
            extent[0..4].copy_from_slice(&disk.to_le_bytes());
            extent[8..16].copy_from_slice(&(1_048_576i64 * (i as i64 + 1)).to_le_bytes());
            extent[16..24].copy_from_slice(&(1_000_000_000i64).to_le_bytes());
            buf.extend(extent);
        }
        buf
    }

    #[test]
    fn disk_extents_parse_multiple() {
        assert_eq!(parse_disk_extents(&extents(&[0], 1)), vec![0]);
        assert_eq!(parse_disk_extents(&extents(&[2, 0, 1], 3)), vec![2, 0, 1]);
        // A count larger than the extents present stops at the end of the buffer.
        assert_eq!(parse_disk_extents(&extents(&[3], 4)), vec![3]);
        let partial = extents(&[1, 2], 2);
        assert_eq!(
            parse_disk_extents(&partial[..EXTENTS_FIRST + EXTENT_SIZE + 2]),
            vec![1]
        );
        assert_eq!(parse_disk_extents(&extents(&[], 0)), Vec::<u32>::new());
        assert_eq!(parse_disk_extents(&[1, 0]), Vec::<u32>::new());
    }

    #[test]
    fn disk_size_parse() {
        let mut buf = vec![0u8; 40];
        buf[GEOMETRY_DISK_SIZE..GEOMETRY_DISK_SIZE + 8]
            .copy_from_slice(&1_024_209_543_168i64.to_le_bytes());
        assert_eq!(parse_disk_size(&buf), Some(1_024_209_543_168));
        assert_eq!(parse_disk_size(&buf[..32]), Some(1_024_209_543_168));
        assert_eq!(parse_disk_size(&buf[..31]), None);
        buf[GEOMETRY_DISK_SIZE..GEOMETRY_DISK_SIZE + 8].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(parse_disk_size(&buf), None);
    }

    #[test]
    fn bus_labels() {
        let cases: &[(u32, &str)] = &[
            (17, "NVMe"),
            (11, "SATA"),
            (7, "USB"),
            (10, "SAS"),
            (8, "RAID"),
            (16, "Storage Spaces"),
            (12, "SD card"),
            (13, "MMC"),
            (14, "Virtual disk"),
            (15, "Virtual disk"),
            (1, "SCSI"),
            (3, "ATA"),
            (9, "iSCSI"),
            (19, "UFS"),
            (18, "Storage-class memory"),
            (0, "Other"),
            (2, "Other"),
            (99, "Other"),
        ];
        for &(bus, label) in cases {
            assert_eq!(bus_label(bus), label, "{bus}");
        }
    }

    #[test]
    fn volume_deadline_marks_slow_queries_not_responding() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&started);
        let query = move |letter: char| {
            seen.lock().unwrap().push(letter);
            if letter == 'E' {
                thread::sleep(Duration::from_secs(3));
            }
            VolumeQuery {
                label: format!("Volume {letter}"),
                file_system: "NTFS".into(),
                size_bytes: Some(100),
                free_bytes: Some(50),
                disk_numbers: vec![0],
                ready: true,
                error: None,
            }
        };
        let begin = Instant::now();
        let answers = start_all(&['C', 'D', 'E'], |l| format!("test-volume-{l}"), query)
            .collect(begin + Duration::from_millis(400));
        assert!(
            begin.elapsed() < Duration::from_secs(2),
            "{:?}",
            begin.elapsed()
        );
        assert_eq!(answers[0].as_ref().unwrap().label, "Volume C");
        assert_eq!(answers[1].as_ref().unwrap().label, "Volume D");
        assert_eq!(answers[2], None);
        assert_eq!(
            started.lock().unwrap().len(),
            3,
            "every letter is queried in parallel"
        );

        let slow = volume_info('E', DriveKind::Removable, answers[2].clone(), false);
        assert!(slow.not_responding);
        assert!(!slow.ready);
        assert_eq!(slow.letter, "E:");
        let fast = volume_info('C', DriveKind::Fixed, answers[0].clone(), true);
        assert!(!fast.not_responding);
        assert!(fast.ready && fast.system);
        assert_eq!(fast.size_bytes, Some(100));
        let optical = volume_info('F', DriveKind::Optical, None, false);
        assert!(!optical.not_responding && !optical.ready);
    }

    /// Drives that answer from a table; `slow` queries sleep for `delay`, `broken` ones
    /// panic, and every query records when it started. Disk 0 is on NVMe, `spinning` disks
    /// are hard disks on SATA, and the others are on USB.
    #[derive(Debug, Clone)]
    struct FakeDrives {
        /// Letter and the disk numbers its extents name.
        volumes: Vec<(char, Vec<u32>)>,
        disks: Vec<u32>,
        spinning: Vec<u32>,
        slow: Vec<String>,
        broken: Vec<String>,
        delay: Duration,
        started: Arc<Mutex<Vec<(String, Instant)>>>,
    }

    impl FakeDrives {
        fn new(volumes: &[(char, &[u32])], disks: &[u32]) -> FakeDrives {
            FakeDrives {
                volumes: volumes.iter().map(|&(l, d)| (l, d.to_vec())).collect(),
                disks: disks.to_vec(),
                spinning: Vec::new(),
                slow: Vec::new(),
                broken: Vec::new(),
                delay: Duration::from_secs(3),
                started: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn enter(&self, query: String) {
            self.started
                .lock()
                .unwrap()
                .push((query.clone(), Instant::now()));
            if self.broken.contains(&query) {
                panic!("broken driver");
            }
            if self.slow.contains(&query) {
                thread::sleep(self.delay);
            }
        }

        fn started_at(&self, query: &str) -> Option<Instant> {
            let started = self.started.lock().unwrap();
            started.iter().find(|(q, _)| q == query).map(|&(_, at)| at)
        }
    }

    impl Drives for FakeDrives {
        type Disk = u32;

        fn query_volume(&self, letter: char) -> VolumeQuery {
            self.enter(format!("volume {letter}"));
            let disks = self
                .volumes
                .iter()
                .find(|(l, _)| *l == letter)
                .map(|(_, d)| d.clone())
                .unwrap_or_default();
            VolumeQuery {
                label: format!("Volume {letter}"),
                file_system: "NTFS".into(),
                size_bytes: Some(100),
                free_bytes: Some(50),
                disk_numbers: disks,
                ready: true,
                error: None,
            }
        }

        fn open_disk(&self, number: u32) -> Option<(DeviceDescriptor, u32)> {
            self.started
                .lock()
                .unwrap()
                .push((format!("open {number}"), Instant::now()));
            let spinning = self.spinning.contains(&number);
            let removable = number != 0 && !spinning;
            let bus_type = match (number, spinning) {
                (0, _) => BUS_NVME,
                (_, true) => BUS_SATA,
                _ => BUS_USB,
            };
            self.disks.contains(&number).then(|| {
                let descriptor = DeviceDescriptor {
                    removable,
                    vendor: None,
                    product: Some(format!("Test disk {number}")),
                    revision: Some("1.0".into()),
                    bus_type,
                };
                (descriptor, number)
            })
        }

        fn query_disk(&self, &number: &u32) -> DiskQuery {
            self.enter(format!("disk {number}"));
            DiskQuery {
                seek_penalty: Some(self.spinning.contains(&number)),
                size: Some(1_000_000_000 * (u64::from(number) + 1)),
            }
        }
    }

    const BUS_SATA: u32 = 11;

    fn fixed(letters: &str) -> Vec<(char, DriveKind)> {
        letters.chars().map(|l| (l, DriveKind::Fixed)).collect()
    }

    #[test]
    fn a_disk_whose_size_hangs_is_listed_without_it_within_the_deadline() {
        // A failing card reader as E: on disk 1: the volume and the disk's queries both hang.
        let mut drives = FakeDrives::new(&[('C', &[0]), ('E', &[1])], &[0, 1]);
        drives.slow = vec!["volume E".into(), "disk 1".into()];
        let deadline = Duration::from_secs(1);
        let begin = Instant::now();
        let (disks, volumes) = read_with(&drives, fixed("CE"), Some('C'), deadline);
        let elapsed = begin.elapsed();
        assert!(
            elapsed < deadline + Duration::from_millis(800),
            "the volume and disk queries share one deadline: {elapsed:?}"
        );
        // The disk queries start with the volume queries, not after their deadline.
        let disk_started = drives.started_at("disk 1").unwrap() - begin;
        assert!(disk_started < deadline / 2, "{disk_started:?}");

        let letters: Vec<(&str, bool)> = volumes
            .iter()
            .map(|v| (v.letter.as_str(), v.not_responding))
            .collect();
        assert_eq!(letters, [("C:", false), ("E:", true)]);
        assert_eq!(disks.len(), 2);
        let (system, card) = (&disks[0], &disks[1]);
        assert_eq!((system.number, system.system), (0, true));
        assert_eq!(system.size_bytes, Some(1_000_000_000));
        assert_eq!(system.bus, "NVMe");
        assert_eq!(system.media, MediaKind::Ssd);
        // The card is still listed with its device descriptor, only without its size.
        assert_eq!(card.number, 1);
        assert_eq!(card.model, "Test disk 1");
        assert_eq!((card.bus.as_str(), card.removable), ("USB", true));
        assert_eq!(card.firmware.as_deref(), Some("1.0"));
        assert_eq!(card.size_bytes, None);
        assert_eq!(card.media, MediaKind::Unknown);
        assert!(!card.system);
    }

    #[test]
    fn a_disk_whose_seek_penalty_hangs_gets_its_media_from_the_bus() {
        // Two hard disks on SATA and an NVMe disk; the seek penalty of disks 2 and 0 hangs.
        let mut drives = FakeDrives::new(&[('C', &[0]), ('D', &[1]), ('E', &[2])], &[0, 1, 2]);
        drives.spinning = vec![1, 2];
        drives.slow = vec!["disk 0".into(), "disk 2".into()];
        let deadline = Duration::from_millis(600);
        let begin = Instant::now();
        let (disks, volumes) = read_with(&drives, fixed("CDE"), Some('C'), deadline);
        assert!(
            begin.elapsed() < deadline + Duration::from_millis(800),
            "the seek penalty is not read on the calling thread: {:?}",
            begin.elapsed()
        );
        assert!(volumes.iter().all(|v| !v.not_responding));
        let media: Vec<(u32, &str, MediaKind, Option<u64>)> = disks
            .iter()
            .map(|d| (d.number, d.bus.as_str(), d.media, d.size_bytes))
            .collect();
        assert_eq!(
            media,
            [
                (0, "NVMe", MediaKind::Ssd, None),
                (1, "SATA", MediaKind::Hdd, Some(2_000_000_000)),
                (2, "SATA", MediaKind::Unknown, None),
            ]
        );
    }

    #[test]
    fn disks_named_only_by_extents_are_probed_after_the_volumes() {
        let drives = FakeDrives::new(&[('C', &[70, 0]), ('D', &[3])], &[0, 3, 70]);
        let (disks, volumes) = read_with(&drives, fixed("CD"), Some('C'), Duration::from_secs(2));
        assert_eq!(volumes.len(), 2);
        let numbers: Vec<(u32, bool, Option<u64>)> = disks
            .iter()
            .map(|d| (d.number, d.system, d.size_bytes))
            .collect();
        assert_eq!(
            numbers,
            [
                (0, true, Some(1_000_000_000)),
                (3, false, Some(4_000_000_000)),
                (70, true, Some(71_000_000_000)),
            ]
        );
        // The first 64 numbers are probed, then the extent number past them, once.
        let started = drives.started.lock().unwrap();
        let opened: Vec<&str> = started
            .iter()
            .filter(|(q, _)| q.starts_with("open "))
            .map(|(q, _)| q.as_str())
            .collect();
        assert_eq!(opened.len(), PROBED_DISKS as usize + 1);
        assert_eq!(opened.last(), Some(&"open 70"));
    }

    #[test]
    fn late_disks_get_time_after_a_volume_used_the_whole_deadline() {
        let mut drives = FakeDrives::new(&[('C', &[80]), ('E', &[])], &[80]);
        drives.slow = vec!["volume E".into()];
        let (disks, volumes) =
            read_with(&drives, fixed("CE"), Some('C'), Duration::from_millis(500));
        assert!(volumes[1].not_responding);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].size_bytes, Some(81_000_000_000), "{disks:?}");
    }

    #[test]
    fn a_panicking_query_affects_only_its_own_drive() {
        let mut drives = FakeDrives::new(&[('C', &[0]), ('D', &[1])], &[0, 1]);
        drives.broken = vec!["volume D".into(), "disk 1".into()];
        let begin = Instant::now();
        let (disks, volumes) = read_with(&drives, fixed("CD"), Some('C'), Duration::from_secs(5));
        assert!(
            begin.elapsed() < Duration::from_secs(2),
            "a panic does not wait for the deadline"
        );
        assert_eq!(volumes[0].error, None);
        assert!(!volumes[1].not_responding);
        let broken = volumes[1].error.as_deref().unwrap();
        assert!(broken.contains("internal error"), "{broken}");
        assert_eq!(disks[0].size_bytes, Some(1_000_000_000));
        assert_eq!(disks[1].size_bytes, None);
        assert_eq!(disks[1].model, "Test disk 1");
    }

    #[test]
    fn optical_drives_are_listed_without_a_query() {
        let drives = FakeDrives::new(&[('C', &[0])], &[0]);
        let listed = vec![('C', DriveKind::Fixed), ('F', DriveKind::Optical)];
        let (_, volumes) = read_with(&drives, listed, Some('C'), Duration::from_secs(2));
        assert!(drives.started_at("volume F").is_none());
        let optical = &volumes[1];
        assert_eq!(optical.letter, "F:");
        assert!(!optical.ready && !optical.not_responding);
        assert!(volumes[0].system && volumes[0].ready);
    }

    #[test]
    fn letters_and_drive_kinds() {
        assert_eq!(letters(0b1101), vec!['A', 'C', 'D']);
        assert_eq!(letters(1 << 25), vec!['Z']);
        assert_eq!(letters(u32::MAX).len(), 26);
        assert_eq!(drive_kind(DRIVE_FIXED), Some(DriveKind::Fixed));
        assert_eq!(drive_kind(DRIVE_REMOVABLE), Some(DriveKind::Removable));
        assert_eq!(drive_kind(DRIVE_CDROM), Some(DriveKind::Optical));
        assert_eq!(drive_kind(DRIVE_RAMDISK), Some(DriveKind::RamDisk));
        assert_eq!(drive_kind(DRIVE_UNKNOWN), Some(DriveKind::Unknown));
        assert_eq!(drive_kind(4), None, "network drives are not listed");
        assert_eq!(drive_kind(1), None, "no root directory");
        assert_eq!(drive_letter(Path::new(r"C:\Windows")), Some('C'));
        assert_eq!(drive_letter(Path::new(r"d:\Windows")), Some('D'));
        assert_eq!(drive_letter(Path::new(r"\\server\share")), None);
    }
}
