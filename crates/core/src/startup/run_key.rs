//! String values of a Run key, enumerated with `RegEnumValueW` through the 64-bit view.

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, HKEY, KEY_READ, KEY_WOW64_64KEY,
};

use crate::win::registry::{Hive, RegValue};
use crate::win::{check, wide};
use crate::Result;

/// Longest value name the registry allows, in UTF-16 units, plus the terminating NUL.
const MAX_VALUE_NAME: usize = 16_384;
/// Retries of one index when a value grows between the size probe and the read.
const MAX_RETRIES: u32 = 8;

struct OpenKey(HKEY);

impl Drop for OpenKey {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// `(name, data)` of every non-empty-named REG_SZ / REG_EXPAND_SZ value under `path`, in
/// registry order. Empty when the key does not exist. Values of other types are skipped
/// because Explorer does not run them.
pub(super) fn string_values(hive: Hive, path: &str) -> Result<Vec<(String, String)>> {
    let Some(key) = open(hive, path)? else {
        return Ok(Vec::new());
    };

    let mut max_name = 0u32;
    let mut max_data = 0u32;
    // SAFETY: only the two size counters are requested.
    check(unsafe {
        RegQueryInfoKeyW(
            key.0,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&mut max_name),
            Some(&mut max_data),
            None,
            None,
        )
    })?;

    let mut name_buf = vec![0u16; max_name as usize + 1];
    let mut data_buf = vec![0u8; max_data as usize];
    let mut out = Vec::new();
    let mut index = 0u32;
    let mut retries = 0u32;
    loop {
        let mut name_len = name_buf.len() as u32;
        let mut data_len = data_buf.len() as u32;
        let mut kind = 0u32;
        // SAFETY: both buffers are writable for the lengths passed alongside them.
        let err = unsafe {
            RegEnumValueW(
                key.0,
                index,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data_buf.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if err == ERROR_NO_MORE_ITEMS {
            break;
        }
        if err == ERROR_MORE_DATA && retries < MAX_RETRIES {
            retries += 1;
            name_buf.resize(MAX_VALUE_NAME, 0);
            let wanted = (data_len as usize).max(data_buf.len() * 2).max(64);
            data_buf.resize(wanted, 0);
            continue;
        }
        check(err)?;
        retries = 0;
        index += 1;

        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        if name.is_empty() {
            continue;
        }
        match RegValue::from_raw(kind, &data_buf[..data_len as usize]) {
            RegValue::Sz(command) | RegValue::ExpandSz(command) => out.push((name, command)),
            _ => {}
        }
    }
    Ok(out)
}

/// Opens `path` for reading through the 64-bit view; `None` when it does not exist.
fn open(hive: Hive, path: &str) -> Result<Option<OpenKey>> {
    let w = wide(path);
    let mut h = HKEY::default();
    // SAFETY: `w` is NUL-terminated and `h` is a valid out pointer.
    let err = unsafe {
        RegOpenKeyExW(
            hive.raw(),
            PCWSTR(w.as_ptr()),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut h,
        )
    };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(err)?;
    Ok(Some(OpenKey(h)))
}
