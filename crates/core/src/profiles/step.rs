//! One setting of a profile: its planned change (a step) and the result of applying it, plus
//! the profile sections that other areas plan and apply (scheduled maintenance, Windows
//! Update).

use serde::{Deserialize, Serialize};

use crate::maintenance::config::ScheduleDay;

/// What applying a profile does to one setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Change,
    Already,
    Skipped,
}

/// Why a step is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepReason {
    Unsupported,
    Unreadable,
    CannotChange,
    OtherAccount,
    Edition,
    NotOnThisPc,
    UnknownId,
}

/// One planned row of a profile preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingStep {
    pub key: String,
    pub title: String,
    pub status: StepStatus,
    pub detail: String,
    pub reason: Option<StepReason>,
    /// Set when the row needs the user's explicit choice; such a row starts unselected.
    pub caution: Option<String>,
}

/// What applying one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Applied,
    AlreadySet,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub key: String,
    pub outcome: StepOutcome,
    pub details: Vec<String>,
}

/// Profile section "maintenance". enabled=false turns maintenance off; then no other field
/// may be set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceChoice {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<ScheduleDay>,
    /// "HH:MM".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clean: Vec<String>,
    #[serde(default)]
    pub sfc_verify: bool,
    #[serde(default)]
    pub dism_check: bool,
}

/// Profile section "windows_update". Pause is never part of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WindowsUpdateChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_hours: Option<ActiveHoursChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_notify: Option<bool>,
    /// true excludes drivers from quality updates; false leaves the setting alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_drivers: bool,
    /// 1..=365.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_feature_days: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHoursChoice {
    #[serde(default)]
    pub automatic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_serialize_only_what_is_set_and_refuse_unknown_fields() {
        let wu = WindowsUpdateChoice::default();
        assert_eq!(serde_json::to_string(&wu).unwrap(), "{}");
        let wu = WindowsUpdateChoice {
            active_hours: Some(ActiveHoursChoice {
                automatic: false,
                start: Some(8),
                end: Some(23),
            }),
            restart_notify: Some(true),
            exclude_drivers: true,
            defer_feature_days: Some(180),
        };
        let json = serde_json::to_value(&wu).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "active_hours": {"automatic": false, "start": 8, "end": 23},
                "restart_notify": true,
                "exclude_drivers": true,
                "defer_feature_days": 180
            })
        );
        assert_eq!(
            serde_json::from_value::<WindowsUpdateChoice>(json).unwrap(),
            wu
        );
        assert!(serde_json::from_str::<WindowsUpdateChoice>(r#"{"pause": 7}"#).is_err());

        let off: MaintenanceChoice = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert_eq!(
            serde_json::to_value(&off).unwrap()["day"],
            serde_json::Value::Null
        );
        let on: MaintenanceChoice = serde_json::from_str(
            r#"{"enabled": true, "day": "sunday", "time": "12:00", "clean": ["user_temp"],
                "sfc_verify": true}"#,
        )
        .unwrap();
        assert_eq!(on.day, Some(ScheduleDay::Sunday));
        assert!(on.sfc_verify && !on.dism_check);
        assert!(
            serde_json::from_str::<MaintenanceChoice>(r#"{"enabled": true, "command": "x"}"#)
                .is_err()
        );
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(StepReason::NotOnThisPc).unwrap(),
            "not_on_this_pc"
        );
        assert_eq!(
            serde_json::to_value(StepOutcome::AlreadySet).unwrap(),
            "already_set"
        );
        assert_eq!(serde_json::to_value(StepStatus::Change).unwrap(), "change");
    }
}
