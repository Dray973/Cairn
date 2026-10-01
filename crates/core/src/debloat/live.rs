//! Settings Windows reads only at sign-in, pushed to the running session after their
//! registry values were written (apply) or restored (rollback).

use super::catalog::{Action, Tweak, MOUSE_KEY};
use crate::win::registry::{read_value, Hive, RegValue};
use crate::win::{input, session};
use crate::{Error, Result};

/// A setting of the signed-in session that Windows loads from the registry only at sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LiveSetting {
    /// Pointer acceleration: HKCU\Control Panel\Mouse MouseSpeed, MouseThreshold1,
    /// MouseThreshold2.
    Mouse,
}

impl LiveSetting {
    /// Audit target in ops_log.
    pub fn target(self) -> &'static str {
        match self {
            LiveSetting::Mouse => r"HKCU\Control Panel\Mouse (pointer acceleration)",
        }
    }

    /// Name used in warnings ("Mouse settings could not be updated…").
    pub fn label(self) -> &'static str {
        match self {
            LiveSetting::Mouse => "Mouse settings",
        }
    }
}

/// ops_log operation of a live refresh.
pub const OP_REFRESH: &str = "refresh_live_setting";

/// Registry values whose change needs a live push. Hive exact; path and name compared
/// ignoring ASCII case.
static LIVE_VALUES: &[(Hive, &str, &str, LiveSetting)] = &[
    (
        Hive::CurrentUser,
        MOUSE_KEY,
        "MouseSpeed",
        LiveSetting::Mouse,
    ),
    (
        Hive::CurrentUser,
        MOUSE_KEY,
        "MouseThreshold1",
        LiveSetting::Mouse,
    ),
    (
        Hive::CurrentUser,
        MOUSE_KEY,
        "MouseThreshold2",
        LiveSetting::Mouse,
    ),
];

/// The live setting the registry value feeds, if any.
pub fn setting_for(hive: Hive, key_path: &str, value_name: &str) -> Option<LiveSetting> {
    LIVE_VALUES
        .iter()
        .find(|(h, path, name, _)| {
            *h == hive
                && path.eq_ignore_ascii_case(key_path)
                && name.eq_ignore_ascii_case(value_name)
        })
        .map(|&(_, _, _, setting)| setting)
}

/// Live settings a tweak's registry actions feed, without repeats, in action order.
pub fn settings_of(t: &Tweak) -> Vec<LiveSetting> {
    let mut out = Vec::new();
    for action in t.actions {
        if let Action::Registry(r) = action {
            if let Some(setting) = setting_for(r.hive, r.path, r.name) {
                if !out.contains(&setting) {
                    out.push(setting);
                }
            }
        }
    }
    out
}

/// Pushes the stored values of `setting` to this session. Refused, with nothing changed,
/// when this process runs outside a signed-in desktop session (session 0) or as another
/// account than the signed-in user (it would push that account's values onto the signed-in
/// user's desktop), and when the stored values cannot be read.
pub fn refresh(setting: LiveSetting) -> Result<()> {
    if session::current_session_id()? == 0 {
        return Err(Error::Other(
            "this process does not run in a signed-in desktop session".into(),
        ));
    }
    // An error from the comparison propagates, so an unconfirmed account is refused too.
    if session::elevated_as_other_user()? {
        return Err(Error::Other(
            "this window runs as a different account than the signed-in user".into(),
        ));
    }
    match setting {
        LiveSetting::Mouse => {
            let read = |name: &str| read_value(Hive::CurrentUser, MOUSE_KEY, name);
            let params = mouse_params(
                read("MouseSpeed")?,
                read("MouseThreshold1")?,
                read("MouseThreshold2")?,
            )?;
            input::set_mouse_acceleration(params)
        }
    }
}

/// Highest accepted MouseSpeed (acceleration level) and MouseThreshold1/2 (pixels).
const MAX_MOUSE_SPEED: i32 = 2;
const MAX_MOUSE_THRESHOLD: i32 = 1000;

/// `[MouseThreshold1, MouseThreshold2, MouseSpeed]` from the stored values: decimal REG_SZ
/// (surrounding blanks ignored) or DWORD. Speed must be 0 to 2 and the thresholds 0 to 1000;
/// a missing or other value is an error.
fn mouse_params(
    speed: Option<RegValue>,
    threshold1: Option<RegValue>,
    threshold2: Option<RegValue>,
) -> Result<[i32; 3]> {
    let number = |name: &str, value: Option<RegValue>, max: i32| -> Result<i32> {
        let Some(value) = value else {
            return Err(Error::Other(format!(
                r"{name} is not set in HKCU\{MOUSE_KEY}"
            )));
        };
        let parsed = match &value {
            RegValue::Sz(s) => s.trim().parse::<i32>().ok(),
            RegValue::Dword(v) => i32::try_from(*v).ok(),
            _ => None,
        };
        parsed.filter(|n| (0..=max).contains(n)).ok_or_else(|| {
            Error::Other(format!(
                r"{name} in HKCU\{MOUSE_KEY} is {}; expected a number from 0 to {max}",
                value.display()
            ))
        })
    };
    Ok([
        number("MouseThreshold1", threshold1, MAX_MOUSE_THRESHOLD)?,
        number("MouseThreshold2", threshold2, MAX_MOUSE_THRESHOLD)?,
        number("MouseSpeed", speed, MAX_MOUSE_SPEED)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debloat::catalog::TWEAKS;

    fn sz(s: &str) -> Option<RegValue> {
        Some(RegValue::Sz(s.to_string()))
    }

    #[test]
    fn mouse_params_parse() {
        assert_eq!(
            mouse_params(sz(" 1 "), sz("6"), Some(RegValue::Dword(10))).unwrap(),
            [6, 10, 1]
        );
        assert_eq!(mouse_params(sz("0"), sz("0"), sz("0")).unwrap(), [0, 0, 0]);
        assert_eq!(
            mouse_params(sz("2"), sz("1000"), sz("0")).unwrap(),
            [1000, 0, 2]
        );
        let missing = mouse_params(None, sz("6"), sz("10"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("MouseSpeed is not set"), "{missing}");
        let text = mouse_params(sz("1"), sz("x"), sz("10"))
            .unwrap_err()
            .to_string();
        assert!(text.contains("MouseThreshold1"), "{text}");
        assert!(mouse_params(sz("3"), sz("6"), sz("10")).is_err());
        assert!(mouse_params(sz("-1"), sz("6"), sz("10")).is_err());
        assert!(mouse_params(sz("1"), sz("6"), sz("1001")).is_err());
        assert!(mouse_params(sz("1"), sz("6"), Some(RegValue::Binary(vec![1]))).is_err());
        assert!(mouse_params(sz("1"), Some(RegValue::Dword(u32::MAX)), sz("10")).is_err());
    }

    #[test]
    fn session_mouse_acceleration_is_readable() {
        // SPI_GETMOUSE only; the live push is never called from a test.
        let [threshold1, threshold2, speed] = input::mouse_acceleration().unwrap();
        assert!((0..=MAX_MOUSE_SPEED).contains(&speed), "{speed}");
        assert!(
            (0..=MAX_MOUSE_THRESHOLD).contains(&threshold1),
            "{threshold1}"
        );
        assert!(
            (0..=MAX_MOUSE_THRESHOLD).contains(&threshold2),
            "{threshold2}"
        );
    }

    #[test]
    fn setting_for_matches_mouse_values_ignoring_case() {
        assert_eq!(
            setting_for(Hive::CurrentUser, r"control panel\MOUSE", "mousespeed"),
            Some(LiveSetting::Mouse)
        );
        assert_eq!(
            setting_for(Hive::CurrentUser, r"Control Panel\Mouse", "MouseThreshold2"),
            Some(LiveSetting::Mouse)
        );
        assert_eq!(
            setting_for(Hive::LocalMachine, r"Control Panel\Mouse", "MouseSpeed"),
            None
        );
        assert_eq!(
            setting_for(
                Hive::CurrentUser,
                r"Control Panel\Mouse",
                "MouseSensitivity"
            ),
            None
        );
    }

    #[test]
    fn settings_of_the_catalog() {
        for t in TWEAKS {
            let expected = if t.id == "gaming.mouse_acceleration" {
                vec![LiveSetting::Mouse]
            } else {
                Vec::new()
            };
            assert_eq!(settings_of(t), expected, "{}", t.id);
        }
    }

    #[test]
    fn targets_and_labels() {
        assert_eq!(
            LiveSetting::Mouse.target(),
            r"HKCU\Control Panel\Mouse (pointer acceleration)"
        );
        assert_eq!(LiveSetting::Mouse.label(), "Mouse settings");
        assert_eq!(OP_REFRESH, "refresh_live_setting");
    }
}
