//! Active power scheme, through the native power management API (powrprof).
//!
//! Ultimate Performance is a hidden scheme that must be duplicated before it can be
//! activated. The duplicate is created under the fixed, application-owned
//! [`APP_SCHEME_GUID`], so the engine can recognise it on later scans and delete it on
//! rollback without touching any scheme the user created.

use std::ptr;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use windows::core::GUID;
use windows::Win32::Foundation::{LocalFree, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HLOCAL};
use windows::Win32::System::Power::{
    PowerDeleteScheme, PowerDuplicateScheme, PowerEnumerate, PowerGetActiveScheme,
    PowerReadFriendlyName, PowerSetActiveScheme, PowerWriteFriendlyName, ACCESS_SCHEME,
};

use super::catalog::PowerPlan;
use super::{ActionState, ActionStatus};
use crate::safety::state_log::{NewPowerRecord, PowerRecord};
use crate::safety::{MutationOutcome, Safety};
use crate::win::{check, wide};
use crate::{Error, Result};

/// Application-owned duplicate of Ultimate Performance.
pub const APP_SCHEME_GUID: &str = "5c1e0d6a-2b3f-4c7e-9a41-0f7a3c8e9b21";
pub const ULTIMATE_PERFORMANCE_GUID: &str = "e9a42b02-d5df-448d-aa00-03f14749eb61";
pub const HIGH_PERFORMANCE_GUID: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
pub const BALANCED_GUID: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";

/// Friendly name written to the application-owned duplicate.
pub const APP_SCHEME_NAME: &str = "Ultimate Performance (Cairn)";

// Binary forms of the string constants above (kept in sync by the unit tests).
const APP_SCHEME: GUID = GUID::from_u128(0x5c1e0d6a_2b3f_4c7e_9a41_0f7a3c8e9b21);
const ULTIMATE_PERFORMANCE: GUID = GUID::from_u128(0xe9a42b02_d5df_448d_aa00_03f14749eb61);
const HIGH_PERFORMANCE: GUID = GUID::from_u128(0x8c5e7fda_e8bf_4a96_9a85_a6e23a8c635c);
const BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);

/// Retries for `PowerReadFriendlyName` when the name grows between the size probe and
/// the read.
const FRIENDLY_NAME_ATTEMPTS: usize = 3;

/// Shown in place of a scheme name that cannot be read.
const UNNAMED_SCHEME: &str = "(unnamed)";

const NO_PERFORMANCE_PLAN: &str =
    "this system offers no high-performance power plan (common on Modern Standby laptops)";

const OP_SET_SCHEME: &str = "set_power_scheme";
const OP_DUPLICATE_SCHEME: &str = "duplicate_power_scheme";

/// A power scheme. `guid` is lowercase without braces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerScheme {
    pub guid: String,
    pub name: String,
}

pub fn describe(plan: PowerPlan) -> String {
    match plan {
        PowerPlan::UltimatePerformance => "power plan Ultimate Performance".to_string(),
    }
}

/// The active scheme (PowerGetActiveScheme + PowerReadFriendlyName).
pub fn active_scheme() -> Result<PowerScheme> {
    let guid = active_scheme_guid()?;
    Ok(scheme_info(&guid))
}

/// Whether a scheme with this GUID exists (PowerEnumerate over ACCESS_SCHEME).
pub fn scheme_exists(guid: &str) -> Result<bool> {
    match parse_guid(guid) {
        Some(g) => scheme_exists_guid(&g),
        None => Ok(false),
    }
}

/// True when the system reports a battery (GetSystemPowerStatus, BatteryFlag != 128).
/// False when the status cannot be read.
pub fn has_system_battery() -> bool {
    crate::win::power::power_source().has_battery
}

/// Applied when the active scheme is the application-owned duplicate, Ultimate
/// Performance itself or High Performance. Read-only; never creates a scheme.
pub fn status(plan: PowerPlan) -> Result<ActionStatus> {
    let guid = active_scheme_guid()?;
    let scheme = scheme_info(&guid);
    let state = if satisfies(plan, &guid) {
        ActionState::Applied
    } else {
        ActionState::NotApplied
    };
    Ok(ActionStatus::new(
        state,
        format!("active power plan: {} ({})", scheme.name, scheme.guid),
    ))
}

/// Journals the active scheme through `safety.record_power`, then activates the target:
/// the application-owned duplicate (created from Ultimate Performance and named
/// [`APP_SCHEME_NAME`] if missing), else High Performance, else skipped because this
/// system offers neither (common on Modern Standby laptops). The journal record is written
/// before the duplicate is created.
pub fn apply(safety: &Safety, plan: PowerPlan) -> Result<MutationOutcome> {
    safety.ensure_elevated()?;

    let active = active_scheme_guid()?;
    let active_text = format_guid(&active);
    if satisfies(plan, &active) {
        safety.log_op(
            OP_SET_SCHEME,
            &active_text,
            "already_in_desired_state",
            None,
        )?;
        return Ok(MutationOutcome::AlreadyInDesiredState);
    }

    let Some(target) = choose_target(plan)? else {
        safety.log_op(
            OP_SET_SCHEME,
            &describe(plan),
            "skipped",
            Some(NO_PERFORMANCE_PLAN),
        )?;
        return Ok(MutationOutcome::Skipped(NO_PERFORMANCE_PLAN.to_string()));
    };

    let planned = match target {
        Target::Existing(g) => g,
        Target::DuplicateUltimate => APP_SCHEME,
    };
    safety.record_power(&NewPowerRecord {
        previous_scheme: active_text.clone(),
        target_scheme: format_guid(&planned),
    })?;

    let scheme = match target {
        Target::Existing(g) => g,
        Target::DuplicateUltimate => match create_app_scheme() {
            Ok(()) => {
                safety.log_op(
                    OP_DUPLICATE_SCHEME,
                    APP_SCHEME_GUID,
                    "applied",
                    Some(&format!("from {ULTIMATE_PERFORMANCE_GUID}")),
                )?;
                info!(scheme = APP_SCHEME_GUID, "Ultimate Performance duplicated");
                APP_SCHEME
            }
            Err(e) => {
                warn!(error = %e, "could not duplicate Ultimate Performance");
                safety.log_op(
                    OP_DUPLICATE_SCHEME,
                    APP_SCHEME_GUID,
                    "failed",
                    Some(&e.to_string()),
                )?;
                if scheme_exists_guid(&HIGH_PERFORMANCE)? {
                    HIGH_PERFORMANCE
                } else {
                    let reason = format!(
                        "Ultimate Performance could not be duplicated ({e}) and {NO_PERFORMANCE_PLAN}"
                    );
                    safety.log_op(OP_SET_SCHEME, &describe(plan), "skipped", Some(&reason))?;
                    return Ok(MutationOutcome::Skipped(reason));
                }
            }
        },
    };

    let scheme_text = format_guid(&scheme);
    if let Err(e) = set_active_scheme(&scheme) {
        safety.log_op(OP_SET_SCHEME, &scheme_text, "failed", Some(&e.to_string()))?;
        return Err(e);
    }
    safety.log_op(
        OP_SET_SCHEME,
        &scheme_text,
        "applied",
        Some(&format!("previous {active_text}")),
    )?;
    info!(previous = %active_text, scheme = %scheme_text, "power scheme activated");
    Ok(MutationOutcome::Applied)
}

/// Reactivates `rec.previous_scheme` (Balanced if it no longer exists), then deletes the
/// application-owned duplicate if it exists and is not active.
pub fn restore(rec: &PowerRecord) -> Result<()> {
    let target = match parse_guid(&rec.previous_scheme) {
        Some(g) if scheme_exists_guid(&g)? => g,
        _ => {
            warn!(
                previous = %rec.previous_scheme,
                "recorded power scheme no longer exists; restoring Balanced"
            );
            BALANCED
        }
    };

    if active_scheme_guid().ok() != Some(target) {
        set_active_scheme(&target)?;
        info!(scheme = %format_guid(&target), "power scheme restored");
    }

    // A scheme cannot be deleted while it is active, so this runs after reactivation.
    if scheme_exists_guid(&APP_SCHEME)? {
        if active_scheme_guid()? == APP_SCHEME {
            warn!(
                scheme = APP_SCHEME_GUID,
                "application power scheme is active; not deleting it"
            );
        } else {
            // SAFETY: APP_SCHEME is a valid GUID for the duration of the call.
            check(unsafe { PowerDeleteScheme(None, &APP_SCHEME) })?;
            info!(scheme = APP_SCHEME_GUID, "application power scheme deleted");
        }
    }
    Ok(())
}

// ───────────────────────────── helpers ─────────────────────────────

/// What `apply` activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A scheme that already exists.
    Existing(GUID),
    /// A new duplicate of Ultimate Performance under [`APP_SCHEME`].
    DuplicateUltimate,
}

/// Picks the scheme `apply` activates, or `None` when this system offers none.
fn choose_target(plan: PowerPlan) -> Result<Option<Target>> {
    match plan {
        PowerPlan::UltimatePerformance => {
            if scheme_exists_guid(&APP_SCHEME)? {
                Ok(Some(Target::Existing(APP_SCHEME)))
            } else if ultimate_is_available() {
                Ok(Some(Target::DuplicateUltimate))
            } else if scheme_exists_guid(&HIGH_PERFORMANCE)? {
                Ok(Some(Target::Existing(HIGH_PERFORMANCE)))
            } else {
                Ok(None)
            }
        }
    }
}

/// Whether `active` already meets `plan`.
fn satisfies(plan: PowerPlan, active: &GUID) -> bool {
    match plan {
        PowerPlan::UltimatePerformance => {
            [APP_SCHEME, ULTIMATE_PERFORMANCE, HIGH_PERFORMANCE].contains(active)
        }
    }
}

/// Ultimate Performance is normally hidden from enumeration, so its presence is probed by
/// reading its friendly name, which succeeds for hidden default schemes as well.
fn ultimate_is_available() -> bool {
    match read_friendly_name(&ULTIMATE_PERFORMANCE) {
        Ok(_) => true,
        Err(e) => {
            debug!(error = %e, "Ultimate Performance scheme is not available");
            false
        }
    }
}

/// Frees memory returned by the power API with `LocalFree` exactly once. A null pointer
/// owns nothing.
#[derive(Debug)]
struct LocalGuid(*mut GUID);

impl LocalGuid {
    fn get(&self) -> Option<GUID> {
        if self.0.is_null() {
            None
        } else {
            // SAFETY: a non-null pointer was returned by the power API and points at a GUID
            // that stays allocated until `drop`.
            Some(unsafe { *self.0 })
        }
    }
}

impl Drop for LocalGuid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was allocated by the power API with LocalAlloc and is
            // released only here.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.cast())));
            }
        }
    }
}

fn active_scheme_guid() -> Result<GUID> {
    let mut out: *mut GUID = ptr::null_mut();
    // SAFETY: `out` is a valid location for the API to store its allocated GUID pointer.
    let rc = unsafe { PowerGetActiveScheme(None, &mut out) };
    let owned = LocalGuid(out);
    check(rc)?;
    owned
        .get()
        .ok_or_else(|| Error::Other("PowerGetActiveScheme returned no scheme".to_string()))
}

fn scheme_info(guid: &GUID) -> PowerScheme {
    let name = match read_friendly_name(guid) {
        Ok(name) if !name.trim().is_empty() => name,
        Ok(_) => UNNAMED_SCHEME.to_string(),
        Err(e) => {
            debug!(error = %e, scheme = %format_guid(guid), "cannot read power scheme name");
            UNNAMED_SCHEME.to_string()
        }
    };
    PowerScheme {
        guid: format_guid(guid),
        name,
    }
}

fn scheme_exists_guid(target: &GUID) -> Result<bool> {
    for index in 0..u32::MAX {
        let mut guid = GUID::zeroed();
        let mut size = size_of::<GUID>() as u32;
        // SAFETY: the buffer is a GUID-sized, GUID-aligned local and `size` states its
        // length in bytes.
        let rc = unsafe {
            PowerEnumerate(
                None,
                None,
                None,
                ACCESS_SCHEME,
                index,
                Some(ptr::from_mut(&mut guid).cast::<u8>()),
                &mut size,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        check(rc)?;
        if guid == *target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Friendly name of a scheme; resolves indirect (MUI) names to the display language.
fn read_friendly_name(scheme: &GUID) -> Result<String> {
    let scheme_ptr = ptr::from_ref(scheme);
    let mut size: u32 = 0;
    // SAFETY: a null buffer asks only for the required size, written to `size`.
    check(unsafe { PowerReadFriendlyName(None, Some(scheme_ptr), None, None, None, &mut size) })?;

    for _ in 0..FRIENDLY_NAME_ATTEMPTS {
        if size == 0 {
            return Ok(String::new());
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let mut written = size;
        // SAFETY: `buf` holds at least `written` bytes and outlives the call.
        let rc = unsafe {
            PowerReadFriendlyName(
                None,
                Some(scheme_ptr),
                None,
                None,
                Some(buf.as_mut_ptr().cast::<u8>()),
                &mut written,
            )
        };
        if rc == ERROR_MORE_DATA {
            size = written.max(size.saturating_mul(2));
            continue;
        }
        check(rc)?;
        let units = (written as usize / 2).min(buf.len());
        return Ok(decode_wide(&buf[..units]));
    }
    Err(Error::Other(format!(
        "friendly name of power scheme {} changed size while being read",
        format_guid(scheme)
    )))
}

fn set_active_scheme(scheme: &GUID) -> Result<()> {
    // SAFETY: `scheme` is a valid GUID for the duration of the call.
    check(unsafe { PowerSetActiveScheme(None, Some(ptr::from_ref(scheme))) })
}

/// Duplicates Ultimate Performance under [`APP_SCHEME`] and names the copy
/// [`APP_SCHEME_NAME`]. A failure to write the name is logged and otherwise ignored:
/// the copy keeps the source name and is still recognised by its GUID.
fn create_app_scheme() -> Result<()> {
    let mut destination = APP_SCHEME;
    let requested: *mut GUID = ptr::from_mut(&mut destination);
    let mut out = requested;
    // A non-null destination pointer makes the API create the copy under that GUID
    // (ERROR_ALREADY_EXISTS if it is taken). It allocates a GUID for the caller to free
    // only when the pointer it receives is null; anything it returns in place of the
    // requested pointer is freed here.
    // SAFETY: both GUID pointers are valid for the duration of the call and `out` is a
    // writable pointer slot.
    let rc = unsafe { PowerDuplicateScheme(None, &ULTIMATE_PERFORMANCE, &mut out) };
    let allocated = LocalGuid(if out == requested {
        ptr::null_mut()
    } else {
        out
    });
    check(rc)?;

    if let Some(created) = allocated.get() {
        if created != APP_SCHEME {
            // SAFETY: `created` is a valid GUID for the duration of the call.
            let _ = unsafe { PowerDeleteScheme(None, &created) };
            return Err(Error::Other(format!(
                "PowerDuplicateScheme created {} instead of {APP_SCHEME_GUID}",
                format_guid(&created)
            )));
        }
    }

    let name = friendly_name_bytes(APP_SCHEME_NAME);
    // SAFETY: APP_SCHEME is a valid GUID and `name` is a NUL-terminated UTF-16 buffer
    // whose length is passed by the wrapper.
    let rc = unsafe { PowerWriteFriendlyName(None, &APP_SCHEME, None, None, &name) };
    if let Err(e) = check(rc) {
        warn!(error = %e, scheme = APP_SCHEME_GUID, "could not name the duplicated power scheme");
    }
    Ok(())
}

/// UTF-16LE bytes of `name` including the terminating NUL.
fn friendly_name_bytes(name: &str) -> Vec<u8> {
    wide(name).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// UTF-16 up to the first NUL.
fn decode_wide(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// Parses `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, case-insensitive, optionally wrapped
/// in braces.
fn parse_guid(text: &str) -> Option<GUID> {
    let text = text.trim();
    let text = text
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(text);
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let mut value: u128 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return None;
            }
        } else {
            let digit = char::from(b).to_digit(16)?;
            value = (value << 4) | u128::from(digit);
        }
    }
    Some(GUID::from_u128(value))
}

/// Lowercase, without braces.
fn format_guid(guid: &GUID) -> String {
    let v = guid.to_u128();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        v & 0xffff_ffff_ffff,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSTANTS: [(&str, GUID); 4] = [
        (APP_SCHEME_GUID, APP_SCHEME),
        (ULTIMATE_PERFORMANCE_GUID, ULTIMATE_PERFORMANCE),
        (HIGH_PERFORMANCE_GUID, HIGH_PERFORMANCE),
        (BALANCED_GUID, BALANCED),
    ];

    #[test]
    fn constants_round_trip() {
        for (text, guid) in CONSTANTS {
            assert_eq!(parse_guid(text), Some(guid), "{text}");
            assert_eq!(format_guid(&guid), text);
            assert_eq!(text, text.to_ascii_lowercase());
        }
    }

    #[test]
    fn constants_match_field_layout() {
        assert_eq!(
            APP_SCHEME,
            GUID::from_values(
                0x5c1e0d6a,
                0x2b3f,
                0x4c7e,
                [0x9a, 0x41, 0x0f, 0x7a, 0x3c, 0x8e, 0x9b, 0x21]
            )
        );
        assert_eq!(
            BALANCED,
            GUID::from_values(
                0x381b4222,
                0xf694,
                0x41f0,
                [0x96, 0x85, 0xff, 0x5b, 0xb2, 0x60, 0xdf, 0x2e]
            )
        );
    }

    #[test]
    fn constants_are_distinct() {
        for (i, (_, a)) in CONSTANTS.iter().enumerate() {
            for (_, b) in &CONSTANTS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn parse_accepts_case_and_braces() {
        let upper = "{E9A42B02-D5DF-448D-AA00-03F14749EB61}";
        assert_eq!(parse_guid(upper), Some(ULTIMATE_PERFORMANCE));
        assert_eq!(
            parse_guid(&upper.to_ascii_uppercase()[1..37]),
            Some(ULTIMATE_PERFORMANCE)
        );
        assert_eq!(
            parse_guid(" 8C5E7FDA-e8bf-4a96-9a85-a6e23a8c635c "),
            Some(HIGH_PERFORMANCE)
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "not-a-guid",
            "{381b4222-f694-41f0-9685-ff5bb260df2e",
            "381b4222-f694-41f0-9685-ff5bb260df2e}",
            "381b4222f69441f09685ff5bb260df2e",
            "381b4222-f694-41f0-9685-ff5bb260df2",
            "381b4222-f694-41f0-9685-ff5bb260df2e0",
            "381b4222-f694-41f0-9685ff5bb260df2e0",
            "381b422g-f694-41f0-9685-ff5bb260df2e",
            "+81b4222-f694-41f0-9685-ff5bb260df2e",
            "381b4222-f694-41f0-9685-ff5bb260d\u{e9}e",
        ] {
            assert_eq!(parse_guid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn format_pads_with_zeros() {
        assert_eq!(
            format_guid(&GUID::zeroed()),
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            format_guid(&GUID::from_u128(1)),
            "00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(
            format_guid(&GUID::from_u128(u128::MAX)),
            "ffffffff-ffff-ffff-ffff-ffffffffffff"
        );
    }

    #[test]
    fn arbitrary_guids_round_trip() {
        for v in [
            0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128,
            0x8000_0000_0000_0000_0000_0000_0000_0001,
        ] {
            let g = GUID::from_u128(v);
            assert_eq!(parse_guid(&format_guid(&g)), Some(g));
        }
    }

    #[test]
    fn satisfies_only_performance_schemes() {
        let plan = PowerPlan::UltimatePerformance;
        assert!(satisfies(plan, &APP_SCHEME));
        assert!(satisfies(plan, &ULTIMATE_PERFORMANCE));
        assert!(satisfies(plan, &HIGH_PERFORMANCE));
        assert!(!satisfies(plan, &BALANCED));
        assert!(!satisfies(plan, &GUID::zeroed()));
    }

    #[test]
    fn friendly_name_is_nul_terminated_utf16() {
        let bytes = friendly_name_bytes(APP_SCHEME_NAME);
        assert_eq!(
            bytes.len(),
            (APP_SCHEME_NAME.encode_utf16().count() + 1) * 2
        );
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
        assert_eq!(&bytes[..2], &[b'U', 0]);
    }

    #[test]
    fn decode_wide_stops_at_nul() {
        let units: Vec<u16> = "Balanced\0junk".encode_utf16().collect();
        assert_eq!(decode_wide(&units), "Balanced");
        assert_eq!(decode_wide(&wide("Power saver")), "Power saver");
        assert_eq!(decode_wide(&[]), "");
    }

    // Read-only queries against the live power configuration.

    #[test]
    fn active_scheme_is_enumerated() {
        let active = active_scheme().expect("active scheme");
        assert_eq!(active.guid, active.guid.to_ascii_lowercase());
        assert!(parse_guid(&active.guid).is_some());
        assert!(!active.name.is_empty());
        assert!(scheme_exists(&active.guid).expect("enumerate schemes"));
        assert!(scheme_exists(&active.guid.to_ascii_uppercase()).expect("enumerate schemes"));
    }

    #[test]
    fn unknown_schemes_do_not_exist() {
        assert!(!scheme_exists("00000000-0000-0000-0000-000000000000").expect("enumerate"));
        assert!(!scheme_exists("not-a-guid").expect("invalid text"));
    }

    #[test]
    fn status_names_active_scheme() {
        let active = active_scheme().expect("active scheme");
        let st = status(PowerPlan::UltimatePerformance).expect("status");
        assert!(
            st.detail.starts_with("active power plan: "),
            "{}",
            st.detail
        );
        assert!(st.detail.contains(&active.guid), "{}", st.detail);
        let guid = parse_guid(&active.guid).expect("guid");
        let expected = if satisfies(PowerPlan::UltimatePerformance, &guid) {
            ActionState::Applied
        } else {
            ActionState::NotApplied
        };
        assert_eq!(st.state, expected);
    }

    #[test]
    fn read_only_probes_do_not_fail() {
        let _ = has_system_battery();
        let _ = ultimate_is_available();
        let _ = choose_target(PowerPlan::UltimatePerformance).expect("choose target");
    }
}
