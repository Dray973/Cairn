//! winget app updates and Windows Update settings.
//!
//! - [`wu`]: Windows Update settings (pause, active hours, drivers, feature update delay,
//!   restart notifications). They are journaled registry values: the baseline is recorded
//!   before each write, and History, Revert All and `revert_targets` undo them.
//! - [`winget`]: app update checks, upgrades and installs through the winget command line,
//!   run as background jobs of the `winget` lane. Upgrades and installs are irreversible:
//!   they are only audited (ops_log ops `app_upgrade` and `app_install`, a "started" row
//!   before each app and one final row after it) and never journaled. The check is
//!   read-only and writes no rows.
//! - [`apps`]: the list of apps the Install apps view offers, a small file in the data
//!   folder; reading it changes nothing.

pub mod apps;
pub mod winget;
pub mod wu;

pub use apps::{app_list, save_app_list, AppCategory, AppEntry, AppList, DEFAULT_APPS};
pub use winget::{
    plan_or_start, winget_status, Availability, BatchResult, ItemResult, ItemState, ScanError,
    ScanResult, StartOutcome, UpdateItem, UpdatesKind, UpdatesPlan, UpdatesRequest, UpgradeRow,
    WingetEnv, WingetLocation, WingetStatus, WingetVersion,
};
pub use wu::{
    plan_or_apply_wu, wu_catalog, wu_state, Edition, ServiceState, WuCatalogEntry, WuChange, WuEnv,
    WuLayout, WuReport, WuSetting, WuSettingId, WuState, WuValue, WuWrite,
};

use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

/// `bytes` random bytes as lowercase hex, for names of temporary files nobody can guess.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    // SAFETY: the buffer is writable for its whole length, which is passed alongside it.
    let status = unsafe { BCryptGenRandom(None, &mut buffer, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.is_err() {
        // Unpredictable enough for a file name: std's hasher keys are random per process.
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        for chunk in buffer.chunks_mut(8) {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            let value = hasher.finish().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_has_the_asked_length_and_differs() {
        let a = random_hex(8);
        let b = random_hex(8);
        assert_eq!(a.len(), 16);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
        assert_eq!(random_hex(4).len(), 8);
    }
}
