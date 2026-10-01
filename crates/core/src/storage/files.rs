//! File handle helpers shared by the speed test, the scanner and the duplicate finder: opening
//! with explicit access and flags, reading what a handle refers to, and the attribute tests.

use std::ffi::{c_void, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, FileDispositionInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SetFileInformationByHandle,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO, FILE_ID_INFO, FILE_NAME_NORMALIZED,
    GETFINALPATHNAMEBYHANDLE_FLAGS, VOLUME_NAME_DOS,
};

pub(crate) const ATTR_SYSTEM: u32 = 0x4;
pub(crate) const ATTR_DIRECTORY: u32 = 0x10;
pub(crate) const ATTR_SPARSE: u32 = 0x200;
pub(crate) const ATTR_REPARSE: u32 = 0x400;
pub(crate) const ATTR_COMPRESSED: u32 = 0x800;
pub(crate) const ATTR_OFFLINE: u32 = 0x1000;
pub(crate) const ATTR_ENCRYPTED: u32 = 0x4000;
pub(crate) const ATTR_INTEGRITY_STREAM: u32 = 0x8000;
pub(crate) const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
pub(crate) const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
/// Any of these makes a file online-only: its data is not stored on this PC.
pub(crate) const ONLINE_ONLY: u32 = ATTR_RECALL_ON_DATA_ACCESS | ATTR_RECALL_ON_OPEN | ATTR_OFFLINE;

/// Reparse tags with this bit make the entry stand for another entry (junctions, symbolic
/// links); windows-rs has no constant for it.
pub(crate) const NAME_SURROGATE: u32 = 0x2000_0000;
/// `IO_REPARSE_TAG_WOF`: files compressed by the Windows Overlay Filter.
pub(crate) const TAG_WOF: u32 = 0x8000_0017;
/// `IO_REPARSE_TAG_CLOUD` with the subtype bits cleared.
pub(crate) const TAG_CLOUD: u32 = 0x9000_001A;
const CLOUD_SUBTYPE_MASK: u32 = 0xFFFF_0FFF;

pub(crate) const FILE_READ_DATA: u32 = 0x1;
pub(crate) const FILE_WRITE_DATA: u32 = 0x2;
pub(crate) const FILE_LIST_DIRECTORY: u32 = 0x1;
pub(crate) const FILE_READ_ATTRIBUTES: u32 = 0x80;
pub(crate) const DELETE: u32 = 0x1_0000;
pub(crate) const SYNCHRONIZE: u32 = 0x10_0000;
pub(crate) const GENERIC_READ: u32 = 0x8000_0000;
pub(crate) const FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub(crate) const FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub(crate) const FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
/// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
pub(crate) const SHARE_ALL: u32 = 0x7;

pub(crate) const ERROR_FILE_NOT_FOUND: i32 = 2;
pub(crate) const ERROR_PATH_NOT_FOUND: i32 = 3;
pub(crate) const ERROR_ACCESS_DENIED: i32 = 5;
pub(crate) const ERROR_SHARING_VIOLATION: i32 = 32;
pub(crate) const ERROR_LOCK_VIOLATION: i32 = 33;
pub(crate) const ERROR_DELETE_PENDING: i32 = 303;

/// The reparse tag makes the entry a link to another entry (junction, mount point, symbolic
/// link), which is never followed.
pub(crate) fn is_name_surrogate(tag: u32) -> bool {
    tag & NAME_SURROGATE != 0
}

/// A cloud files placeholder (OneDrive), whatever its subtype.
pub(crate) fn is_cloud_tag(tag: u32) -> bool {
    tag & CLOUD_SUBTYPE_MASK == TAG_CLOUD
}

pub(crate) fn is_online_only(attrs: u32) -> bool {
    attrs & ONLINE_ONLY != 0
}

pub(crate) fn raw(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

/// A Windows error as an `io::Error` with its OS code when it has one.
pub(crate) fn to_io(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

/// Opens `path` itself with exactly `access`, `share` and `flags` (no access is added).
pub(crate) fn open(path: &Path, access: u32, share: u32, flags: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(flags)
        .open(path)
}

/// `path` with the `\\?\` prefix when it is an absolute path on a lettered drive (`C:\…`), so
/// that Windows uses every name in it exactly as given; any other path unchanged. Win32 path
/// normalization removes a trailing dot from each name of a plain path and trailing dots and
/// spaces from its last name, so an entry listed as `data.` or `data ` would open as `data`.
/// The path must hold no `.` or `..` parts.
pub(crate) fn verbatim(path: &Path) -> PathBuf {
    let mut parts = path.components();
    let on_drive = matches!(
        parts.next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_))
    ) && parts.next() == Some(Component::RootDir);
    if !on_drive {
        return path.to_path_buf();
    }
    let mut text = OsString::from(r"\\?\");
    text.push(path.as_os_str());
    PathBuf::from(text)
}

/// [`open`] of an entry whose path is built from the names a folder listing returned, through
/// its [`verbatim`] form: the entry opened is the one listed, whatever its name ends in.
pub(crate) fn open_exact(path: &Path, access: u32, share: u32, flags: u32) -> io::Result<File> {
    open(&verbatim(path), access, share, flags)
}

/// `path` as a NUL-terminated UTF-16 string for Win32 calls, with the `\\?\` prefix on a
/// plain drive path so that long paths work. The path must be absolute and normalized.
pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    let plain_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_))
    );
    let mut out: Vec<u16> = Vec::new();
    if plain_drive {
        out.extend(r"\\?\".encode_utf16());
    }
    out.extend(path.as_os_str().encode_wide());
    out.push(0);
    out
}

/// Attributes and reparse tag of the open entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TagInfo {
    pub attrs: u32,
    pub tag: u32,
}

impl TagInfo {
    pub(crate) fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }

    /// A reparse point that stands for another entry.
    pub(crate) fn is_link(&self) -> bool {
        self.attrs & ATTR_REPARSE != 0 && is_name_surrogate(self.tag)
    }
}

pub(crate) fn tag_info(file: &File) -> io::Result<TagInfo> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is a FILE_ATTRIBUTE_TAG_INFO that outlives the call; the size matches it.
    unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileAttributeTagInfo,
            &mut info as *mut FILE_ATTRIBUTE_TAG_INFO as *mut c_void,
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(to_io)?;
    Ok(TagInfo {
        attrs: info.FileAttributes,
        tag: if info.FileAttributes & ATTR_REPARSE != 0 {
            info.ReparseTag
        } else {
            0
        },
    })
}

/// `FILE_STANDARD_INFO` with its two BOOLEAN fields read as bytes.
#[repr(C)]
#[derive(Default)]
struct RawStandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
}

/// Size, link count and state of the open entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StandardInfo {
    pub allocated: u64,
    pub size: u64,
    pub links: u32,
    pub delete_pending: bool,
    pub directory: bool,
}

pub(crate) fn standard_info(file: &File) -> io::Result<StandardInfo> {
    let mut info = RawStandardInfo::default();
    // SAFETY: `info` has the layout of FILE_STANDARD_INFO (the BOOLEANs as bytes), outlives
    // the call, and the size passed matches it.
    unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileStandardInfo,
            &mut info as *mut RawStandardInfo as *mut c_void,
            size_of::<RawStandardInfo>() as u32,
        )
    }
    .map_err(to_io)?;
    Ok(StandardInfo {
        allocated: u64::try_from(info.allocation_size).unwrap_or(0),
        size: u64::try_from(info.end_of_file).unwrap_or(0),
        links: info.number_of_links,
        delete_pending: info.delete_pending != 0,
        directory: info.directory != 0,
    })
}

/// A 64-bit digest of a 128-bit file id (splitmix64 of its halves); ids equal within a
/// volume give equal digests.
pub(crate) fn id_hash(id: [u8; 16]) -> u64 {
    let lo = u64::from_le_bytes(id[..8].try_into().unwrap_or([0; 8]));
    let hi = u64::from_le_bytes(id[8..].try_into().unwrap_or([0; 8]));
    mix64(lo ^ hi.rotate_left(29))
}

/// splitmix64 finalizer.
pub(crate) fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// [`id_hash`] of the open entry's 128-bit file id.
pub(crate) fn file_id_hash(file: &File) -> io::Result<u64> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `info` is a FILE_ID_INFO that outlives the call; the size matches it.
    unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut c_void,
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(to_io)?;
    Ok(id_hash(info.FileId.Identifier))
}

/// Normalized `\\?\` path of the open entry.
pub(crate) fn final_path(file: &File) -> io::Result<PathBuf> {
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `file` is an open handle; the slice length bounds the write.
        let n = unsafe { GetFinalPathNameByHandleW(raw(file), &mut buf, flags) } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(n + 1, 0);
    }
}

/// Marks the open entry for deletion; it disappears when its last handle closes.
pub(crate) fn mark_for_deletion(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `info` is a FILE_DISPOSITION_INFO that outlives the call; the size matches it.
    unsafe {
        SetFileInformationByHandle(
            raw(file),
            FileDispositionInfo,
            &info as *const FILE_DISPOSITION_INFO as *const c_void,
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(to_io)
}

/// Case-insensitive equality of two paths as Windows compares names on NTFS.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().to_lowercase();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
        text.trim_end_matches('\\').to_string()
    };
    norm(a) == norm(b)
}

/// RFC 3339 UTC text of a FILETIME tick count; `None` for 0 and times before 1970.
pub(crate) fn ticks_rfc3339(ticks: i64) -> Option<String> {
    crate::win::filetime::to_rfc3339(u64::try_from(ticks).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_tests() {
        assert!(is_name_surrogate(0xA000_0003));
        assert!(is_name_surrogate(0xA000_000C));
        assert!(!is_name_surrogate(0x9000_601A));
        assert!(is_cloud_tag(0x9000_601A));
        assert!(is_cloud_tag(TAG_CLOUD));
        assert!(!is_cloud_tag(TAG_WOF));
        assert!(is_online_only(0x40_1620));
        assert!(!is_online_only(ATTR_REPARSE | ATTR_SPARSE));
        let junction = TagInfo {
            attrs: ATTR_DIRECTORY | ATTR_REPARSE,
            tag: 0xA000_0003,
        };
        assert!(junction.is_dir() && junction.is_link());
        let cloud = TagInfo {
            attrs: ATTR_DIRECTORY | ATTR_REPARSE,
            tag: 0x9000_601A,
        };
        assert!(!cloud.is_link());
    }

    #[test]
    fn wide_paths_get_the_long_path_prefix() {
        let text = |p: &str| String::from_utf16_lossy(&wide_path(Path::new(p)));
        assert_eq!(text(r"C:\Temp\x"), "\\\\?\\C:\\Temp\\x\0");
        assert_eq!(text(r"\\?\C:\Temp"), "\\\\?\\C:\\Temp\0");
    }

    #[test]
    fn only_plain_drive_paths_become_verbatim() {
        let exact = |p: &str| verbatim(Path::new(p)).display().to_string();
        assert_eq!(
            exact(r"C:\Temp\trail.\space "),
            r"\\?\C:\Temp\trail.\space "
        );
        assert_eq!(exact(r"C:\"), r"\\?\C:\");
        assert_eq!(exact(r"\\?\C:\Temp"), r"\\?\C:\Temp");
        assert_eq!(exact(r"\\server\share\x"), r"\\server\share\x");
        assert_eq!(exact(r"C:relative"), "C:relative");
        assert_eq!(exact(r"relative\x"), r"relative\x");
    }

    #[test]
    fn names_ending_in_a_dot_or_a_space_open_as_listed() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("trail.");
        let file = folder.join("space ");
        std::fs::create_dir(verbatim(&folder)).unwrap();
        std::fs::write(verbatim(&file), b"exact").unwrap();
        let read = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
        // A plain path names "trail\space", which does not exist.
        let plain = open(&file, read, SHARE_ALL, 0).unwrap_err();
        assert!(matches!(plain.raw_os_error(), Some(2 | 3)), "{plain}");
        let opened = open_exact(&file, read, SHARE_ALL, 0).unwrap();
        assert_eq!(standard_info(&opened).unwrap().size, 5);
        let listed = open_exact(&folder, read, SHARE_ALL, FLAG_BACKUP_SEMANTICS).unwrap();
        assert!(tag_info(&listed).unwrap().is_dir());
        // A missing entry is still "not found".
        let missing = open_exact(&folder.join("missing"), read, SHARE_ALL, 0).unwrap_err();
        assert!(matches!(missing.raw_os_error(), Some(2 | 3)), "{missing}");
    }

    #[test]
    fn id_hash_is_stable_and_spreads() {
        let a = id_hash([1; 16]);
        assert_eq!(a, id_hash([1; 16]));
        assert_ne!(a, id_hash([2; 16]));
        let mut other = [1u8; 16];
        other[15] = 9;
        assert_ne!(a, id_hash(other));
    }

    #[test]
    fn same_path_ignores_case_prefix_and_trailing_separator() {
        assert!(same_path(
            Path::new(r"C:\Windows"),
            Path::new(r"c:\WINDOWS\")
        ));
        assert!(same_path(Path::new(r"\\?\C:\A\b"), Path::new(r"C:\a\B")));
        assert!(!same_path(Path::new(r"C:\A"), Path::new(r"C:\AB")));
    }

    #[test]
    fn handle_reads_describe_a_file_and_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, vec![7u8; 5000]).unwrap();
        let file = open(&path, FILE_READ_ATTRIBUTES | SYNCHRONIZE, SHARE_ALL, 0).unwrap();
        let tag = tag_info(&file).unwrap();
        assert!(!tag.is_dir() && tag.tag == 0);
        let info = standard_info(&file).unwrap();
        assert_eq!(info.size, 5000);
        assert_eq!(info.links, 1);
        assert!(!info.directory && !info.delete_pending);
        assert_eq!(file_id_hash(&file).unwrap(), file_id_hash(&file).unwrap());
        let real = final_path(&file).unwrap();
        assert!(same_path(&real, &std::fs::canonicalize(&path).unwrap()));

        let folder = open(
            dir.path(),
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_ALL,
            FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
        )
        .unwrap();
        assert!(tag_info(&folder).unwrap().is_dir());
        assert!(standard_info(&folder).unwrap().directory);
    }

    #[test]
    fn ticks_convert_to_rfc3339() {
        assert_eq!(
            ticks_rfc3339(116_444_736_000_000_000).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(ticks_rfc3339(0), None);
        assert_eq!(ticks_rfc3339(-5), None);
    }
}
