//! CompanyName from a file's version resource.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};

use crate::win::wide;

/// Language/code-page blocks tried after the file's own first translation:
/// U.S. English with Unicode, then with Windows-1252.
const FALLBACK_TRANSLATIONS: [&str; 2] = ["040904b0", "040904e4"];

/// CompanyName of `path`; empty when the file has no version resource or no such string.
pub(super) fn company_name(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let file = wide(path);
    // SAFETY: `file` is NUL-terminated.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(file.as_ptr()), None) };
    if size == 0 {
        return String::new();
    }
    // u64 storage keeps the block aligned for the u16/u32 reads below.
    let mut block = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `block` provides at least `size` writable bytes.
    let loaded = unsafe {
        GetFileVersionInfoW(
            PCWSTR(file.as_ptr()),
            None,
            size,
            block.as_mut_ptr().cast::<c_void>(),
        )
    };
    if loaded.is_err() {
        return String::new();
    }
    let block = block.as_ptr().cast::<c_void>();

    let mut translations = Vec::with_capacity(3);
    // SAFETY: `block` holds the version resource loaded above and outlives every query.
    if let Some((ptr, len)) = unsafe { query(block, r"\VarFileInfo\Translation") } {
        if len >= 4 {
            let ptr = ptr.cast::<u16>();
            // SAFETY: at least four bytes (one LANGANDCODEPAGE pair) are available.
            let (lang, code_page) = unsafe { (ptr.read_unaligned(), ptr.add(1).read_unaligned()) };
            translations.push(format!("{lang:04x}{code_page:04x}"));
        }
    }
    translations.extend(FALLBACK_TRANSLATIONS.iter().map(|t| t.to_string()));

    for translation in translations {
        let sub = format!(r"\StringFileInfo\{translation}\CompanyName");
        // SAFETY: as above.
        let Some((ptr, len)) = (unsafe { query(block, &sub) }) else {
            continue;
        };
        let ptr = ptr.cast::<u16>();
        // SAFETY: VerQueryValueW reports the string length in UTF-16 units.
        let units: Vec<u16> = (0..len as usize)
            .map(|i| unsafe { ptr.add(i).read_unaligned() })
            .take_while(|&c| c != 0)
            .collect();
        let name = String::from_utf16_lossy(&units).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    String::new()
}

/// Pointer to and length of one item of a version-resource block.
///
/// # Safety
/// `block` must point to a block filled by `GetFileVersionInfoW` that stays alive while the
/// returned pointer is used.
unsafe fn query(block: *const c_void, sub_block: &str) -> Option<(*const c_void, u32)> {
    let sub = wide(sub_block);
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: guaranteed by the caller; `sub` is NUL-terminated and the out pointers are valid.
    let found = unsafe { VerQueryValueW(block, PCWSTR(sub.as_ptr()), &mut ptr, &mut len) };
    (found.as_bool() && !ptr.is_null() && len > 0).then_some((ptr.cast_const(), len))
}
