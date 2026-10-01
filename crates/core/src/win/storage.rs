//! Storage device queries through `DeviceIoControl` on handles opened with no access rights,
//! which a standard user may open and which allow only `FILE_ANY_ACCESS` control codes.
//!
//! Buffers the driver fills are parsed as bytes, never read as structs, because several of
//! the descriptors carry one-byte `BOOLEAN` fields that are not guaranteed to be 0 or 1.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    StorageDeviceSeekPenaltyProperty, StorageDeviceTrimProperty, IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::handle::OwnedHandle;
use super::wide;
use crate::Result;

/// Whether a drive is a solid-state drive or a spinning hard disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Ssd,
    Hdd,
    Unknown,
}

/// `PropertyStandardQuery` of `STORAGE_QUERY_TYPE`.
const PROPERTY_STANDARD_QUERY: i32 = 0;
/// Offset of the one-byte flag in `DEVICE_SEEK_PENALTY_DESCRIPTOR` and
/// `DEVICE_TRIM_DESCRIPTOR`, after the `Version` and `Size` DWORDs.
const DESCRIPTOR_FLAG_OFFSET: usize = 8;

/// Opens a device path (`\\.\C:`, `\\.\PhysicalDrive0`) with no access rights
/// (FILE_ANY_ACCESS IOCTLs only).
pub fn open_device(path: &str) -> Result<OwnedHandle> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes or
    // template handle are passed.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }?;
    Ok(OwnedHandle::new(handle))
}

/// DeviceIoControl with byte buffers; returns the number of bytes written to `output`.
pub fn device_io_control(
    device: &OwnedHandle,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> Result<usize> {
    let mut returned = 0u32;
    let input_ptr = (!input.is_empty()).then(|| input.as_ptr().cast::<c_void>());
    let output_ptr = (!output.is_empty()).then(|| output.as_mut_ptr().cast::<c_void>());
    // SAFETY: each buffer is valid for the length passed with it for the whole
    // (synchronous) call; `returned` is a valid out pointer.
    unsafe {
        DeviceIoControl(
            device.raw(),
            code,
            input_ptr,
            input.len() as u32,
            output_ptr,
            output.len() as u32,
            Some(&mut returned),
            None,
        )
    }?;
    Ok((returned as usize).min(output.len()))
}

/// IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, for a STORAGE_PROPERTY_ID value
/// (`.0`). The 12-byte query is `PropertyId`, `QueryType` and an empty
/// `AdditionalParameters`.
pub fn query_storage_property(
    device: &OwnedHandle,
    property_id: i32,
    output: &mut [u8],
) -> Result<usize> {
    let mut query = [0u8; 12];
    query[0..4].copy_from_slice(&property_id.to_le_bytes());
    query[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    device_io_control(device, IOCTL_STORAGE_QUERY_PROPERTY, &query, output)
}

fn descriptor_flag(buf: &[u8]) -> Option<bool> {
    buf.get(DESCRIPTOR_FLAG_OFFSET).map(|&b| b != 0)
}

/// `IncursSeekPenalty` of a `DEVICE_SEEK_PENALTY_DESCRIPTOR`; `None` when the buffer is
/// too short to hold it.
pub fn parse_seek_penalty(buf: &[u8]) -> Option<bool> {
    descriptor_flag(buf)
}

/// `TrimEnabled` of a `DEVICE_TRIM_DESCRIPTOR`; `None` when the buffer is too short to
/// hold it.
pub fn parse_trim(buf: &[u8]) -> Option<bool> {
    descriptor_flag(buf)
}

/// Whether the device incurs a seek penalty (a spinning disk); `None` when the driver does
/// not report it.
pub fn seek_penalty(device: &OwnedHandle) -> Option<bool> {
    let mut buf = [0u8; 16];
    let n = query_storage_property(device, StorageDeviceSeekPenaltyProperty.0, &mut buf).ok()?;
    parse_seek_penalty(&buf[..n])
}

/// Whether the device and its driver support TRIM; `None` when the driver does not report
/// it.
pub fn trim_enabled(device: &OwnedHandle) -> Option<bool> {
    let mut buf = [0u8; 16];
    let n = query_storage_property(device, StorageDeviceTrimProperty.0, &mut buf).ok()?;
    parse_trim(&buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::paths::windows_dir;

    #[test]
    fn descriptor_flags_are_parsed_from_bytes() {
        let mut buf = [0u8; 12];
        assert_eq!(parse_seek_penalty(&buf[..8]), None);
        assert_eq!(parse_trim(&buf[..8]), None);
        assert_eq!(parse_seek_penalty(&[]), None);
        assert_eq!(parse_seek_penalty(&buf), Some(false));
        assert_eq!(parse_trim(&buf[..9]), Some(false));
        buf[8] = 1;
        assert_eq!(parse_seek_penalty(&buf), Some(true));
        assert_eq!(parse_trim(&buf[..9]), Some(true));
    }

    #[test]
    fn media_kind_serializes_in_snake_case() {
        assert_eq!(serde_json::to_string(&MediaKind::Ssd).unwrap(), "\"ssd\"");
        assert_eq!(
            serde_json::from_str::<MediaKind>("\"unknown\"").unwrap(),
            MediaKind::Unknown
        );
    }

    #[test]
    fn system_volume_can_be_queried_without_access_rights() {
        let windows = windows_dir().unwrap();
        let letter = windows.to_string_lossy().chars().next().unwrap();
        let device = open_device(&format!(r"\\.\{letter}:")).unwrap();
        // Either answer is valid; the query must only succeed or report nothing.
        let _ = seek_penalty(&device);
        let _ = trim_enabled(&device);
        assert!(!device.raw().is_invalid());
    }
}
