//! System Restore checkpoints via `SRSetRestorePointW`.
//!
//! Windows throttles restore-point creation to one per 24 hours by default
//! (`SystemRestorePointCreationFrequency`). [`create_restore_point`] can lift that
//! throttle for the duration of the call so a checkpoint is always taken.
//!
//! Unit tests, and any process started with `OPTIMIZER_FORBID_RESTORE_POINT=1` (cargo sets
//! it for every test and `cargo run`), never create a restore point or change System
//! Protection: see [`forbidden`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use windows::core::HRESULT;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SERVICE_DISABLED, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Restore::{
    SRSetRestorePointW, BEGIN_SYSTEM_CHANGE, END_SYSTEM_CHANGE, MAX_DESC_W, MODIFY_SETTINGS,
    RESTOREPOINTINFOW, RESTOREPOINTINFO_EVENT_TYPE, STATEMGRSTATUS,
};

use crate::win::powershell;
use crate::win::registry::{read_value, Hive, Key, RawValue, RegValue};
use crate::{is_elevated, Error, Result};

const SR_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\SystemRestore";
const SR_POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows NT\SystemRestore";
const FREQUENCY_VALUE: &str = "SystemRestorePointCreationFrequency";

pub const DEFAULT_DESCRIPTION: &str = "Cairn checkpoint";

/// Environment variable that, set to `1`, makes [`create_restore_point`] and
/// [`enable_system_restore`] fail before they change anything.
pub const FORBID_ENV: &str = "OPTIMIZER_FORBID_RESTORE_POINT";

/// True in unit tests and when OPTIMIZER_FORBID_RESTORE_POINT=1.
pub fn forbidden() -> bool {
    cfg!(test) || std::env::var(FORBID_ENV).map(|v| v == "1").unwrap_or(false)
}

fn forbidden_error() -> Error {
    Error::Other(
        "restore points are disabled in this environment (OPTIMIZER_FORBID_RESTORE_POINT)".into(),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePoint {
    pub sequence: i64,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

/// True when System Protection is enabled and not blocked by policy.
pub fn is_system_restore_enabled() -> Result<bool> {
    if let Some(RegValue::Dword(1)) = read_value(Hive::LocalMachine, SR_POLICY_KEY, "DisableSR")? {
        return Ok(false);
    }
    Ok(matches!(
        read_value(Hive::LocalMachine, SR_KEY, "RPSessionInterval")?,
        Some(RegValue::Dword(n)) if n > 0
    ))
}

/// Enables System Protection for `drive` (for example `C:\`) via `Enable-ComputerRestore`.
/// Fails without changing anything when [`forbidden`].
pub fn enable_system_restore(drive: &str) -> Result<()> {
    if forbidden() {
        return Err(forbidden_error());
    }
    if !is_elevated() {
        return Err(Error::NotElevated);
    }
    powershell::run(&enable_script(drive)?)?;
    Ok(())
}

/// The PowerShell script that turns on System Protection for `drive` (`C:` or `C:\`): it
/// imports the Management module from System32 and calls the cmdlet module-qualified.
fn enable_script(drive: &str) -> Result<String> {
    let mut drive = drive.trim().to_string();
    if !drive.ends_with('\\') {
        drive.push('\\');
    }
    let bytes = drive.as_bytes();
    if bytes.len() != 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err(Error::Other(format!("not a drive root: {drive}")));
    }
    Ok(format!(
        r"{}Microsoft.PowerShell.Management\Enable-ComputerRestore -Drive '{drive}'",
        powershell::import_system_module("Microsoft.PowerShell.Management")?
    ))
}

/// Creates a restore point of type `MODIFY_SETTINGS`. With `force`, the 24-hour
/// creation throttle is suspended for the call and restored afterwards. Fails before the
/// throttle or System Restore is touched when [`forbidden`].
pub fn create_restore_point(description: &str, force: bool) -> Result<RestorePoint> {
    if forbidden() {
        return Err(forbidden_error());
    }
    if !is_elevated() {
        return Err(Error::NotElevated);
    }
    let _throttle = if force {
        Some(ThrottleOverride::apply()?)
    } else {
        None
    };

    let begin = restore_point_info(BEGIN_SYSTEM_CHANGE, 0, description);
    let mut status = STATEMGRSTATUS::default();
    // SAFETY: both pointers reference stack-owned, fully initialised structs.
    let ok = unsafe { SRSetRestorePointW(&begin, &mut status) };
    // The structs are packed; copy fields out before comparing.
    let n_status = status.nStatus;
    let sequence = status.llSequenceNumber;
    if !ok.as_bool() || n_status != ERROR_SUCCESS {
        return Err(map_status(n_status));
    }

    let end = restore_point_info(END_SYSTEM_CHANGE, sequence, description);
    let mut end_status = STATEMGRSTATUS::default();
    // SAFETY: as above. The end marker is advisory; its result does not affect the checkpoint.
    let _ = unsafe { SRSetRestorePointW(&end, &mut end_status) };

    Ok(RestorePoint {
        sequence,
        description: description.to_string(),
        created_at: Utc::now(),
    })
}

fn restore_point_info(
    event: RESTOREPOINTINFO_EVENT_TYPE,
    sequence: i64,
    description: &str,
) -> RESTOREPOINTINFOW {
    let mut desc = [0u16; MAX_DESC_W as usize];
    let max = MAX_DESC_W as usize - 1;
    for (dst, src) in desc.iter_mut().zip(description.encode_utf16().take(max)) {
        *dst = src;
    }
    RESTOREPOINTINFOW {
        dwEventType: event,
        dwRestorePtType: MODIFY_SETTINGS,
        llSequenceNumber: sequence,
        szDescription: desc,
    }
}

fn map_status(status: WIN32_ERROR) -> Error {
    let message = match status {
        ERROR_SERVICE_DISABLED => {
            "System Protection is turned off for the system drive".to_string()
        }
        ERROR_ACCESS_DENIED => "access denied; the process must be elevated".to_string(),
        other => windows::core::Error::from_hresult(HRESULT::from_win32(other.0)).message(),
    };
    Error::RestorePoint {
        code: status.0,
        message,
    }
}

/// RAII override of `SystemRestorePointCreationFrequency`. Restores the previous
/// value (or removes the value if it did not exist) on drop.
struct ThrottleOverride {
    previous: Option<RawValue>,
}

impl ThrottleOverride {
    fn apply() -> Result<Self> {
        let (key, _) = Key::create(Hive::LocalMachine, SR_KEY)?;
        let previous = key.query_raw(FREQUENCY_VALUE)?;
        key.set(FREQUENCY_VALUE, &RegValue::Dword(0))?;
        Ok(Self { previous })
    }
}

impl Drop for ThrottleOverride {
    fn drop(&mut self) {
        if let Ok(Some(key)) = Key::open(Hive::LocalMachine, SR_KEY, true) {
            let _ = match &self.previous {
                Some(raw) => key.set_raw(FREQUENCY_VALUE, raw.kind, &raw.data),
                None => key.delete_value(FREQUENCY_VALUE).map(|_| ()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_points_are_forbidden_in_unit_tests() {
        assert!(forbidden());
        let before = read_value(Hive::LocalMachine, SR_KEY, FREQUENCY_VALUE).unwrap();
        let err = create_restore_point("x", true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "restore points are disabled in this environment (OPTIMIZER_FORBID_RESTORE_POINT)"
        );
        let err = enable_system_restore("C:").unwrap_err();
        assert!(err.to_string().contains(FORBID_ENV), "{err}");
        let after = read_value(Hive::LocalMachine, SR_KEY, FREQUENCY_VALUE).unwrap();
        assert_eq!(before, after, "the creation throttle was not touched");
    }

    #[test]
    fn enable_script_imports_its_module_and_qualifies_the_cmdlet() {
        let import = powershell::import_system_module("Microsoft.PowerShell.Management").unwrap();
        for (drive, root) in [("C:", r"C:\"), (r"d:\", r"d:\"), (" E: ", r"E:\")] {
            let script = enable_script(drive).unwrap();
            assert_eq!(
                script,
                format!(
                    r"{import}Microsoft.PowerShell.Management\Enable-ComputerRestore -Drive '{root}'"
                )
            );
        }
        for bad in ["", "C", "CD:", r"C:\Windows", "1:", "C:'; x"] {
            assert!(enable_script(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn default_description_names_the_app() {
        assert_eq!(DEFAULT_DESCRIPTION, "Cairn checkpoint");
    }
}
