//! Running-process lookup and Recycle Bin access used by the cleanup targets.

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHEmptyRecycleBinW, SHQueryRecycleBinW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI,
    SHERB_NOSOUND, SHQUERYRBINFO,
};

pub(crate) use crate::win::process::running_process_names;
use crate::Result;

/// Contents of the current user's Recycle Bin across all drives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecycleBin {
    pub(crate) bytes: u64,
    pub(crate) items: u64,
}

pub(crate) fn query_recycle_bin() -> Result<RecycleBin> {
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a null root queries every drive; `info` is initialised with its size.
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info)? };
    let (bytes, items) = (info.i64Size, info.i64NumItems);
    Ok(RecycleBin {
        bytes: bytes.max(0) as u64,
        items: items.max(0) as u64,
    })
}

/// Empties the Recycle Bin on every drive without confirmation, progress UI or sound.
/// The deleted items cannot be recovered.
pub(crate) fn empty_recycle_bin() -> Result<()> {
    // SAFETY: no owner window; a null root empties every drive.
    unsafe {
        SHEmptyRecycleBinW(
            None,
            PCWSTR::null(),
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )?
    };
    Ok(())
}
