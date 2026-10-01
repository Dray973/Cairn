//! Registry actions: policy and setting values written through the safety layer.

use super::catalog::{RegData, RegistryAction};
use super::{ActionState, ActionStatus};
use crate::safety::rollback::RegistryTarget;
use crate::safety::{MutationOutcome, Safety};
use crate::win::registry::{read_value, RegValue};
use crate::Result;

/// The value the action writes when the value currently holds `current` (`None` when it
/// is absent). Fixed values ignore `current`. A flags action keeps every bit of the
/// current number except the ones it clears, keeping the current registry type, and
/// starts from its default when the value is absent. `None` when the current value
/// cannot be read as a number, so there is nothing safe to write.
pub fn desired_value(action: &RegistryAction, current: Option<&RegValue>) -> Option<RegValue> {
    match action.data {
        RegData::Dword(v) => Some(RegValue::Dword(v)),
        RegData::Sz(s) => Some(RegValue::Sz(s.to_string())),
        RegData::FlagsSzClear { clear, default } => match current {
            None => Some(RegValue::Sz((default & !clear).to_string())),
            Some(RegValue::Dword(v)) => Some(RegValue::Dword(v & !clear)),
            Some(v) => flags_of(v).map(|n| RegValue::Sz((n & !clear).to_string())),
        },
    }
}

/// Number held by a flags value: a decimal REG_SZ (surrounding blanks ignored) or a DWORD.
fn flags_of(value: &RegValue) -> Option<u32> {
    match value {
        RegValue::Sz(s) => s.trim().parse().ok(),
        RegValue::Dword(v) => Some(*v),
        _ => None,
    }
}

/// `HKLM\Path\Name` form used in reports and logs.
pub fn describe(action: &RegistryAction) -> String {
    let name = if action.name.is_empty() {
        "(Default)"
    } else {
        action.name
    };
    format!("{}\\{}\\{}", action.hive.short(), action.path, name)
}

pub fn target(action: &RegistryAction) -> RegistryTarget {
    RegistryTarget {
        hive: action.hive,
        key_path: action.path.to_string(),
        value_name: action.name.to_string(),
    }
}

/// Reads the current value and compares it with the target. A value of the right data
/// but a different registry type counts as not applied, because apply rewrites the type.
/// A flags action is applied when none of the bits it clears is set; an absent value
/// stands for the action's default.
pub fn status(action: &RegistryAction) -> Result<ActionStatus> {
    let current = read_value(action.hive, action.path, action.name)?;
    Ok(status_of(action, current.as_ref()))
}

fn status_of(action: &RegistryAction, current: Option<&RegValue>) -> ActionStatus {
    if let RegData::FlagsSzClear { clear, default } = action.data {
        return flags_status(action, current, clear, default);
    }
    let desired = desired_value(action, current).expect("fixed values always have a target");
    match current {
        Some(v) if *v == desired => ActionStatus::new(
            ActionState::Applied,
            format!("{} = {}", describe(action), v.display()),
        ),
        Some(v) => ActionStatus::new(
            ActionState::NotApplied,
            format!(
                "{} = {}, target {}",
                describe(action),
                v.display(),
                desired.display()
            ),
        ),
        None => ActionStatus::new(
            ActionState::NotApplied,
            format!("{} not set, target {}", describe(action), desired.display()),
        ),
    }
}

fn flags_status(
    action: &RegistryAction,
    current: Option<&RegValue>,
    clear: u32,
    default: u32,
) -> ActionStatus {
    let name = describe(action);
    let Some(value) = current else {
        let state = if default & clear == 0 {
            ActionState::Applied
        } else {
            ActionState::NotApplied
        };
        return ActionStatus::new(
            state,
            format!(
                "{name} not set (Windows uses {default}), target {}",
                default & !clear
            ),
        );
    };
    match flags_of(value) {
        Some(n) if n & clear == 0 => ActionStatus::new(
            ActionState::Applied,
            format!("{name} = {}, bits 0x{clear:x} clear", value.display()),
        ),
        Some(n) => ActionStatus::new(
            ActionState::NotApplied,
            format!("{name} = {}, target {}", value.display(), n & !clear),
        ),
        None => ActionStatus::new(
            ActionState::NotApplied,
            format!(
                "{name} = {}, which is not a decimal flag value",
                value.display()
            ),
        ),
    }
}

/// Journals the current value, then writes the target value. A flags action whose value
/// cannot be read as a number is skipped without writing anything.
pub fn apply(safety: &Safety, action: &RegistryAction) -> Result<MutationOutcome> {
    let current = match action.data {
        RegData::FlagsSzClear { .. } => read_value(action.hive, action.path, action.name)?,
        RegData::Dword(_) | RegData::Sz(_) => None,
    };
    let Some(desired) = desired_value(action, current.as_ref()) else {
        return Ok(MutationOutcome::Skipped(format!(
            "{} is not a decimal flag value",
            describe(action)
        )));
    };
    safety.set_registry_value(action.hive, action.path, action.name, &desired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::registry::Hive;

    static STICKY: RegistryAction = RegistryAction {
        hive: Hive::CurrentUser,
        path: r"Control Panel\Accessibility\StickyKeys",
        name: "Flags",
        data: RegData::FlagsSzClear {
            clear: 0x4,
            default: 510,
        },
    };

    fn sz(s: &str) -> RegValue {
        RegValue::Sz(s.to_string())
    }

    fn desired(current: Option<RegValue>) -> Option<RegValue> {
        desired_value(&STICKY, current.as_ref())
    }

    fn state(current: Option<RegValue>) -> ActionState {
        status_of(&STICKY, current.as_ref()).state
    }

    #[test]
    fn flags_clear_only_the_hotkey_bit() {
        // Sticky Keys on (0x1) stays on; only the shortcut bit goes.
        assert_eq!(desired(Some(sz("511"))), Some(sz("507")));
        assert_eq!(desired(Some(sz("510"))), Some(sz("506")));
        // Filter Keys on.
        assert_eq!(desired(Some(sz("127"))), Some(sz("123")));
        // Already clear: the same number comes back, so nothing is written.
        assert_eq!(desired(Some(sz("498"))), Some(sz("498")));
        assert_eq!(desired(Some(sz(" 506 "))), Some(sz("506")));
        assert_eq!(
            desired(Some(RegValue::Dword(511))),
            Some(RegValue::Dword(507))
        );
        // Absent: Windows' default without the shortcut bit.
        assert_eq!(desired(None), Some(sz("506")));
        // Unreadable values are left alone.
        assert_eq!(desired(Some(sz("garbage"))), None);
        assert_eq!(desired(Some(sz("-1"))), None);
        assert_eq!(desired(Some(RegValue::Binary(vec![1]))), None);
    }

    #[test]
    fn flags_status_looks_at_the_hotkey_bit_only() {
        assert_eq!(state(Some(sz("498"))), ActionState::Applied);
        assert_eq!(state(Some(sz("50"))), ActionState::Applied);
        assert_eq!(state(Some(sz("506"))), ActionState::Applied);
        assert_eq!(state(Some(sz("507"))), ActionState::Applied);
        assert_eq!(state(Some(sz("510"))), ActionState::NotApplied);
        assert_eq!(state(Some(sz("511"))), ActionState::NotApplied);
        assert_eq!(state(Some(RegValue::Dword(2))), ActionState::Applied);
        assert_eq!(
            state(None),
            ActionState::NotApplied,
            "Windows default has it on"
        );
        let odd = status_of(&STICKY, Some(&sz("garbage")));
        assert_eq!(odd.state, ActionState::NotApplied);
        assert!(
            odd.detail.contains("not a decimal flag value"),
            "{}",
            odd.detail
        );
    }

    #[test]
    fn fixed_values_ignore_the_current_value() {
        static DWORD: RegistryAction = RegistryAction {
            hive: Hive::CurrentUser,
            path: r"Software\PCOptimizer\SelfTest",
            name: "Probe",
            data: RegData::Dword(1),
        };
        assert_eq!(
            desired_value(&DWORD, Some(&RegValue::Dword(9))),
            Some(RegValue::Dword(1))
        );
        assert_eq!(desired_value(&DWORD, None), Some(RegValue::Dword(1)));
        assert_eq!(
            status_of(&DWORD, Some(&RegValue::Dword(1))).state,
            ActionState::Applied
        );
        assert_eq!(
            status_of(&DWORD, Some(&RegValue::Sz("1".into()))).state,
            ActionState::NotApplied
        );
        assert_eq!(status_of(&DWORD, None).state, ActionState::NotApplied);
    }
}
