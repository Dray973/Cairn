//! Thin, typed, RAII wrappers over the raw Win32 surfaces used by the engine.
//! Nothing here journals state; that is the job of [`crate::safety`].
//!
//! - [`registry`]        registry keys and values through the 64-bit view
//! - [`scm`]             Service Control Manager
//! - [`powershell`]      headless PowerShell for cmdlet-only operations
//! - [`session`]         the signed-in user versus the account this process runs as
//! - [`shell`]           shell folders and File Explorer
//! - [`paths`]           System32 and Windows directories, shell known folders
//! - [`process`]         hardened child processes and the running-process list
//! - [`console_text`]    decoding console output in the OEM and other code pages
//! - [`handle`]          owned kernel handles
//! - [`storage`]         storage device queries through `DeviceIoControl`
//! - [`task_scheduler`]  Task Scheduler 2.0
//! - [`volume`]          fixed volumes with their media type
//! - [`filetime`]        FILETIME values: the current time, UTC and RFC 3339 conversion
//! - [`power`]           power source: AC or battery, and the battery charge
//! - [`mutex`]           named mutexes, with an owner check for administrator-only locks
//! - [`acl`]             security descriptor checks for program files, folders and tasks
//! - [`fs`]              administrator-only folders and bounded reads of regular files
//! - [`gpu`]             GPU hardware scheduling support through D3DKMT
//! - [`input`]           live mouse settings of the running session
//! - [`wmi`]             read-only WMI queries
//! - [`security_center`] Windows Security Center products and provider health
//! - [`firewall`]        read-only Windows Firewall state
//! - [`update_agent`]    read-only Windows Update Agent status, history and search
//! - [`event_log`]       event log queries and rendering
//! - [`accounts`]        local accounts, the Administrators group and domain membership
//! - [`deadline`]        independent reads run in parallel under one deadline
//! - `com`               COM apartment scope for the calling thread
//! - `error_mode`        thread error mode without critical-error dialogs
//! - `package`           installed packaged apps and their install folders

pub mod accounts;
pub mod acl;
pub(crate) mod com;
pub mod console_text;
pub mod deadline;
pub(crate) mod error_mode;
pub mod event_log;
pub mod filetime;
pub mod firewall;
pub mod fs;
pub mod gpu;
pub mod handle;
pub mod input;
pub mod mutex;
pub(crate) mod package;
pub mod paths;
pub mod power;
pub mod powershell;
pub mod process;
pub mod registry;
pub mod scm;
pub mod security_center;
pub mod session;
pub mod shell;
pub mod storage;
pub mod task_scheduler;
pub mod update_agent;
pub mod volume;
pub mod wmi;

/// UTF-16, NUL-terminated buffer for `PCWSTR` parameters.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Text of a UTF-16 buffer up to its first NUL (the whole buffer when it has none).
/// Unpaired surrogates become U+FFFD.
pub fn from_wide_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Maps a `WIN32_ERROR` return value to `Result`.
pub(crate) fn check(err: windows::Win32::Foundation::WIN32_ERROR) -> crate::Result<()> {
    if err.is_ok() {
        Ok(())
    } else {
        Err(crate::Error::Win32(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(err.0),
        )))
    }
}

/// True when a `windows::core::Error` carries the given Win32 code.
pub(crate) fn is_win32(
    e: &windows::core::Error,
    code: windows::Win32::Foundation::WIN32_ERROR,
) -> bool {
    e.code() == windows::core::HRESULT::from_win32(code.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_wide_nul_stops_at_the_first_nul() {
        let buf: Vec<u16> = "abc\0def".encode_utf16().collect();
        assert_eq!(from_wide_nul(&buf), "abc");
        let buf: Vec<u16> = "no terminator".encode_utf16().collect();
        assert_eq!(from_wide_nul(&buf), "no terminator");
        assert_eq!(from_wide_nul(&[]), "");
        assert_eq!(from_wide_nul(&[0, 65]), "");
    }
}
