//! Registry reads behind the permissions guide: Windows' own usage records of the desktop
//! programs that used a device, kept in the per-user capability consent store. Read-only.

use super::Capability;
use crate::win::filetime;
use crate::win::registry::{Hive, Key, RegValue};
use crate::Result;

/// Per-user consent store, with one subkey per capability (`webcam`, `microphone`,
/// `location`).
pub(crate) const USER_STORE: &str =
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore";
/// Subkey of a capability whose own subkeys are the desktop programs that used the device,
/// named after the program's path with `#` for `\`.
pub(crate) const NON_PACKAGED: &str = "NonPackaged";
/// REG_QWORD FILETIMEs Windows writes when a program starts and stops using the device.
const LAST_USED_START: &str = "LastUsedTimeStart";
const LAST_USED_STOP: &str = "LastUsedTimeStop";

/// Where the per-user consent store lives. [`SYSTEM`] is the real one; tests use a key in the
/// self-test sandbox.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout<'a> {
    pub user: (Hive, &'a str),
}

pub(crate) const SYSTEM: Layout<'static> = Layout {
    user: (Hive::CurrentUser, USER_STORE),
};

impl Layout<'_> {
    /// The key whose subkeys are the desktop programs that used `cap`.
    pub(crate) fn desktop_apps(&self, cap: Capability) -> (Hive, String) {
        (
            self.user.0,
            format!(r"{}\{}\{NON_PACKAGED}", self.user.1, cap.store_key()),
        )
    }
}

/// A REG_QWORD value of an open key; 0 when it is missing or has another type.
fn qword(key: &Key, name: &str) -> Result<u64> {
    Ok(match key.query(name)? {
        Some(RegValue::Qword(v)) => v,
        _ => 0,
    })
}

/// `LastUsedTimeStart` and `LastUsedTimeStop` of an open key; 0 for a value that is missing.
pub(crate) fn usage_times(key: &Key) -> Result<(u64, u64)> {
    Ok((qword(key, LAST_USED_START)?, qword(key, LAST_USED_STOP)?))
}

/// `(last used, in use)` of the two usage stamps: the RFC 3339 time of the newer one (`None`
/// when there is none) and whether the device is in use now (started and not stopped).
pub(crate) fn usage(start: u64, stop: u64) -> (Option<String>, bool) {
    (
        filetime::to_rfc3339(start.max(stop)),
        start != 0 && stop == 0,
    )
}
