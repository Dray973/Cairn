//! Read-backs after Cairn writes or restores a target, and their comparison with what it
//! wrote.
//!
//! A read-back shows only that a target holds a value at the moment it is read, never that
//! Windows uses the value. Values are compared by registry type and data, except startup
//! switches, which compare by the on or off state they encode (a StartupApproved value also
//! holds the time it was last switched, which changes whoever switches it).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::state_log::{DnsRecord, Written};
use crate::debloat::power;
use crate::startup;
use crate::win::registry::{path_within, Hive, Key, RawValue, RegValue};
use crate::win::scm::{Scm, StartType, READ_ACCESS};
use crate::Result;

/// A target whose current value is not what Cairn set (after an apply, in a drift check)
/// or restored (after an undo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// The journal's target text: `HKLM\…\AllowTelemetry`, `service SysMain`,
    /// `scheduled task \…\Proxy`, `power scheme …`, `DNS … (IPv4)`.
    pub target: String,
    /// Short name for one-line texts: the value name, "service SysMain", "task Proxy",
    /// "the power plan", "DNS servers of Wi-Fi (IPv4)".
    pub label: String,
    /// What Cairn set (apply, drift) or restored (undo): "0", "Manual", "disabled",
    /// "not set", "1.1.1.1, 1.0.0.1".
    pub expected: String,
    /// What the target holds now, in the same form.
    pub found: String,
    /// The value before Cairn's first change, when the journal holds it.
    #[serde(default)]
    pub original: Option<String>,
    /// `found` is the original: the target is back to what it was before Cairn changed it.
    #[serde(default)]
    pub back_to_original: bool,
}

/// How long the engine waits after its writes before it reads them back.
pub const SETTLE: Duration = Duration::from_millis(500);

/// Text of a value that does not exist.
pub(crate) const NOT_SET: &str = "not set";

/// Found text of a service, scheduled task or network adapter that no longer exists.
pub(crate) const NOT_ON_THIS_PC: &str = "not on this PC";

/// Label of the active power plan in one-line texts.
pub(crate) const POWER_PLAN_LABEL: &str = "the power plan";

/// The capability consent store; the same path under HKCU and HKLM.
const CONSENT_STORE: &str =
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore";

/// Value Windows rewrites in the consent store whenever a permission changes.
const LAST_SET_TIME: &str = "LastSetTime";

/// The Power saver scheme Windows ships.
const POWER_SAVER_GUID: &str = "a1841308-3541-4fab-bc81-f71556f20b4a";

/// Targets whose value Windows itself changes over time, so a read-back or drift check
/// would report a change nobody made:
/// - the six Windows Update pause values (`updates::wu::pause_targets()`);
/// - `LastSetTime` under `…\CapabilityAccessManager\ConsentStore\…` (records of earlier
///   builds).
pub(crate) fn unverified(hive: Hive, key_path: &str, value_name: &str) -> bool {
    if value_name.eq_ignore_ascii_case(LAST_SET_TIME) && path_within(key_path, CONSENT_STORE) {
        return true;
    }
    crate::updates::wu::pause_targets().iter().any(|t| {
        t.hive == hive
            && t.key_path.eq_ignore_ascii_case(key_path)
            && t.value_name.eq_ignore_ascii_case(value_name)
    })
}

/// A registry value as reports show it:
/// - `RegValue::display`;
/// - "not set" for an absent value;
/// - "on" or "off" for StartupApproved and packaged-task `State` values
///   (`startup::describe_approved`).
pub(crate) fn value_text(
    hive: Hive,
    key_path: &str,
    value_name: &str,
    value: Option<&RegValue>,
) -> String {
    let Some(value) = value else {
        return NOT_SET.to_string();
    };
    match startup::describe_approved(hive, key_path, value_name, value) {
        Some(state) => state.to_string(),
        None => value.display(),
    }
}

/// [`value_text`] of a raw value.
pub(crate) fn raw_text(
    hive: Hive,
    key_path: &str,
    value_name: &str,
    value: Option<&RawValue>,
) -> String {
    let decoded = value.map(RawValue::decode);
    value_text(hive, key_path, value_name, decoded.as_ref())
}

/// [`value_text`] of what Cairn last wrote to a target.
pub(crate) fn written_text(
    hive: Hive,
    key_path: &str,
    value_name: &str,
    written: &Written,
) -> String {
    raw_text(hive, key_path, value_name, written_value(written))
}

/// Current raw value of a registry target, `None` when absent.
pub(crate) fn read_raw(hive: Hive, key_path: &str, value_name: &str) -> Result<Option<RawValue>> {
    match Key::open(hive, key_path, false)? {
        Some(key) => key.query_raw(value_name),
        None => Ok(None),
    }
}

/// Whether two values of one registry target (`None`: absent) are the same setting. Startup
/// switches compare by the state they encode (`startup::switch_state`); every other value by
/// registry type and data, an absent value matching only an absent one.
pub(crate) fn same_setting(
    hive: Hive,
    key_path: &str,
    value_name: &str,
    a: Option<&RawValue>,
    b: Option<&RawValue>,
) -> bool {
    if startup::is_switch(hive, key_path, value_name) {
        let state = |raw: Option<&RawValue>| {
            let value = raw.map(RawValue::decode);
            startup::switch_state(hive, key_path, value_name, value.as_ref())
        };
        return state(a) == state(b);
    }
    a == b
}

/// Whether a target still holds what Cairn last wrote to it (`current`: `None` when absent).
pub(crate) fn holds_written(
    hive: Hive,
    key_path: &str,
    value_name: &str,
    written: &Written,
    current: Option<&RawValue>,
) -> bool {
    same_setting(hive, key_path, value_name, written_value(written), current)
}

/// The value Cairn wrote, `None` for a delete.
fn written_value(written: &Written) -> Option<&RawValue> {
    match written {
        Written::Value { value } => Some(value),
        Written::Deleted => None,
    }
}

/// Short name of a registry target in one-line texts: its value name, or "(Default)".
pub(crate) fn registry_label(value_name: &str) -> String {
    if value_name.is_empty() {
        "(Default)".to_string()
    } else {
        value_name.to_string()
    }
}

/// Short name of a service in one-line texts: "service SysMain".
pub(crate) fn service_label(name: &str) -> String {
    format!("service {name}")
}

/// Short name of a scheduled task in one-line texts: "task Proxy" for `\…\Autochk\Proxy`.
pub(crate) fn task_label(path: &str) -> String {
    let name = path
        .rsplit('\\')
        .find(|part| !part.is_empty())
        .unwrap_or(path);
    format!("task {name}")
}

/// Short name of a DNS record's target in one-line texts: "DNS servers of Wi-Fi (IPv4)".
pub(crate) fn dns_label(rec: &DnsRecord) -> String {
    format!(
        "DNS servers of {} ({})",
        rec.adapter_name,
        rec.family.label()
    )
}

/// "enabled" or "disabled", as scheduled tasks are reported.
pub(crate) fn enabled_text(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

/// A start type as reports show it, with " (delayed)" for a delayed automatic start.
pub(crate) fn start_text(start: StartType, delayed: bool) -> String {
    if start == StartType::Automatic && delayed {
        format!("{} (delayed)", start.label())
    } else {
        start.label().to_string()
    }
}

/// Start type of service `name` and whether an automatic start is delayed; `None` when the
/// service is not installed. Read-only.
pub(crate) fn read_start(name: &str) -> Result<Option<(StartType, bool)>> {
    let scm = Scm::connect()?;
    let Some(service) = scm.open(name, READ_ACCESS)? else {
        return Ok(None);
    };
    let config = service.config()?;
    Ok(Some((config.start_type, config.delayed_auto_start)))
}

/// A power scheme as reports show it: the name of a scheme Windows ships or of Cairn's own
/// scheme, else `name` (the scheme's own name, when it was read), else "power plan {guid}".
/// The power module's placeholder for a name it could not read counts as no name, so a
/// recorded scheme and the active one are worded alike.
pub(crate) fn scheme_text(guid: &str, name: Option<&str>) -> String {
    let known = [
        (power::BALANCED_GUID, "Balanced"),
        (power::HIGH_PERFORMANCE_GUID, "High performance"),
        (POWER_SAVER_GUID, "Power saver"),
        (power::ULTIMATE_PERFORMANCE_GUID, "Ultimate Performance"),
        (power::APP_SCHEME_GUID, power::APP_SCHEME_NAME),
    ];
    let guid = guid.trim().trim_start_matches('{').trim_end_matches('}');
    match known.iter().find(|(id, _)| id.eq_ignore_ascii_case(guid)) {
        Some((_, label)) => (*label).to_string(),
        None => match name
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != power::UNNAMED_SCHEME)
        {
            Some(name) => name.to_string(),
            None => format!("power plan {guid}"),
        },
    }
}

/// `template` with every `{name}` that `args` names replaced by its value, in one pass:
/// substituted text is never scanned again, so values may contain braces.
pub(crate) fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let named = after.find('}').and_then(|end| {
            args.iter()
                .find(|(name, _)| *name == &after[..end])
                .map(|(_, value)| (end, *value))
        });
        match named {
            Some((end, value)) => {
                out.push_str(value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::registry::delete_sandbox_tree;

    const USER_APPROVED_RUN: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    const TASK_KEY: &str = concat!(
        r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion",
        r"\AppModel\SystemAppData\Contoso.App_0000000000000\StartTask"
    );

    #[test]
    fn mismatch_json_without_original_deserializes() {
        let json = r#"{"target": "HKLM\\SOFTWARE\\Policies\\Example\\Value", "label": "Value",
                       "expected": "0", "found": "1"}"#;
        let mismatch: Mismatch = serde_json::from_str(json).unwrap();
        assert_eq!(mismatch.label, "Value");
        assert_eq!(mismatch.original, None);
        assert!(!mismatch.back_to_original);
        let round: Mismatch =
            serde_json::from_str(&serde_json::to_string(&mismatch).unwrap()).unwrap();
        assert_eq!(round, mismatch);
    }

    #[test]
    fn read_back_helpers_keep_their_signatures() {
        let _: fn(Hive, &str, &str) -> bool = unverified;
        let _: fn(Hive, &str, &str, Option<&RegValue>) -> String = value_text;
        let _: fn(Hive, &str, &str) -> Result<Option<RawValue>> = read_raw;
        assert_eq!(SETTLE, Duration::from_millis(500));
    }

    #[test]
    fn value_texts() {
        let text = |key_path: &str, name: &str, value: Option<RegValue>| {
            value_text(Hive::CurrentUser, key_path, name, value.as_ref())
        };
        let sandbox = r"Software\PCOptimizer\SelfTest\Example";
        assert_eq!(
            text(sandbox, "Value", Some(RegValue::Dword(1))),
            "0x00000001 (1)"
        );
        assert_eq!(
            text(sandbox, "Value", Some(RegValue::Sz("High".into()))),
            "\"High\""
        );
        assert_eq!(text(sandbox, "Value", None), "not set");

        let on = RegValue::Binary(vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let off = RegValue::Binary(vec![0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(text(USER_APPROVED_RUN, "Contoso", Some(on)), "on");
        assert_eq!(text(USER_APPROVED_RUN, "Contoso", Some(off)), "off");
        assert_eq!(text(USER_APPROVED_RUN, "Contoso", None), "not set");
        assert_eq!(text(TASK_KEY, "State", Some(RegValue::Dword(2))), "on");
        assert_eq!(text(TASK_KEY, "State", Some(RegValue::Dword(1))), "off");

        let raw = RegValue::Dword(7).to_raw();
        assert_eq!(
            raw_text(Hive::LocalMachine, sandbox, "Value", Some(&raw)),
            "0x00000007 (7)"
        );
        assert_eq!(
            written_text(Hive::LocalMachine, sandbox, "Value", &Written::Deleted),
            "not set"
        );
        assert_eq!(
            written_text(
                Hive::LocalMachine,
                sandbox,
                "Value",
                &Written::Value { value: raw }
            ),
            "0x00000007 (7)"
        );
    }

    #[test]
    fn unverified_targets_are_the_pause_values_and_last_set_time() {
        let ux = r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings";
        let pause = crate::updates::wu::pause_targets();
        let names: Vec<&str> = pause.iter().map(|t| t.value_name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PauseFeatureUpdatesStartTime",
                "PauseQualityUpdatesStartTime",
                "PauseUpdatesStartTime",
                "PauseFeatureUpdatesEndTime",
                "PauseQualityUpdatesEndTime",
                "PauseUpdatesExpiryTime",
            ]
        );
        for target in &pause {
            assert_eq!(target.hive, Hive::LocalMachine);
            assert!(target.key_path.eq_ignore_ascii_case(ux), "{target:?}");
            assert!(unverified(
                Hive::LocalMachine,
                &target.key_path.to_ascii_lowercase(),
                &target.value_name.to_ascii_uppercase()
            ));
            assert!(
                !unverified(Hive::CurrentUser, &target.key_path, &target.value_name),
                "another hive"
            );
        }
        assert!(!unverified(Hive::LocalMachine, ux, "ActiveHoursStart"));

        let webcam = format!(r"{CONSENT_STORE}\webcam\Contoso.Meet_0000000000000");
        assert!(unverified(Hive::CurrentUser, &webcam, "LastSetTime"));
        assert!(unverified(
            Hive::LocalMachine,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\location",
            "lastsettime"
        ));
        assert!(!unverified(Hive::CurrentUser, &webcam, "Value"));
        assert!(!unverified(
            Hive::CurrentUser,
            r"Software\PCOptimizer\SelfTest\ConsentStore\webcam",
            "LastSetTime"
        ));
    }

    #[test]
    fn startup_switches_compare_by_state_and_other_values_by_type_and_data() {
        let approved = |data: &[u8]| Some(RegValue::Binary(data.to_vec()).to_raw());
        let off_then = approved(&[0x03, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
        let off_now = approved(&[0x03, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9]);
        let off_odd = approved(&[0x07, 0, 0, 0]);
        let on = approved(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let same = |a: &Option<RawValue>, b: &Option<RawValue>| {
            same_setting(
                Hive::CurrentUser,
                USER_APPROVED_RUN,
                "Contoso",
                a.as_ref(),
                b.as_ref(),
            )
        };
        assert!(
            same(&off_then, &off_now),
            "a new time stamp is not a change"
        );
        assert!(same(&off_then, &off_odd));
        assert!(!same(&off_then, &on));
        assert!(same(&on, &None), "a missing StartupApproved value is on");
        assert!(!same(&off_then, &None));

        let sandbox = r"Software\PCOptimizer\SelfTest\Example";
        let dword = Some(RegValue::Dword(1).to_raw());
        let text = Some(RegValue::Sz("1".into()).to_raw());
        let other = |a: &Option<RawValue>, b: &Option<RawValue>| {
            same_setting(Hive::CurrentUser, sandbox, "Value", a.as_ref(), b.as_ref())
        };
        assert!(other(&dword, &dword));
        assert!(!other(&dword, &text), "the type counts");
        assert!(!other(&dword, &None));
        assert!(other(&None, &None));

        let written = Written::Value {
            value: off_then.clone().unwrap(),
        };
        assert!(holds_written(
            Hive::CurrentUser,
            USER_APPROVED_RUN,
            "Contoso",
            &written,
            off_now.as_ref()
        ));
        assert!(!holds_written(
            Hive::CurrentUser,
            USER_APPROVED_RUN,
            "Contoso",
            &written,
            None
        ));
        assert!(holds_written(
            Hive::CurrentUser,
            sandbox,
            "Value",
            &Written::Deleted,
            None
        ));
        assert!(!holds_written(
            Hive::CurrentUser,
            sandbox,
            "Value",
            &Written::Deleted,
            dword.as_ref()
        ));
    }

    #[test]
    fn labels_and_texts_name_targets_briefly() {
        assert_eq!(registry_label("AllowTelemetry"), "AllowTelemetry");
        assert_eq!(registry_label(""), "(Default)");
        assert_eq!(service_label("SysMain"), "service SysMain");
        assert_eq!(
            task_label(r"\Microsoft\Windows\Autochk\Proxy"),
            "task Proxy"
        );
        assert_eq!(task_label("Proxy"), "task Proxy");
        let dns = DnsRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            interface_guid: "{00000000-0000-0000-0000-00000000c0de}".to_string(),
            family: crate::network::IpFamily::Ipv6,
            adapter_name: "Wi-Fi".to_string(),
            previous_servers: String::new(),
            target_servers: "::1".to_string(),
            active: true,
            reverted_at: None,
        };
        assert_eq!(dns_label(&dns), "DNS servers of Wi-Fi (IPv6)");
        assert_eq!(enabled_text(true), "enabled");
        assert_eq!(enabled_text(false), "disabled");
        assert_eq!(
            start_text(StartType::Automatic, true),
            "automatic (delayed)"
        );
        assert_eq!(start_text(StartType::Automatic, false), "automatic");
        assert_eq!(start_text(StartType::Disabled, true), "disabled");
        assert_eq!(
            scheme_text(power::BALANCED_GUID, Some("Ausbalanciert")),
            "Balanced"
        );
        assert_eq!(
            scheme_text(
                &format!("{{{}}}", power::APP_SCHEME_GUID.to_uppercase()),
                None
            ),
            power::APP_SCHEME_NAME
        );
        assert_eq!(
            scheme_text("00000000-0000-0000-0000-00000000c0de", Some("Quiet")),
            "Quiet"
        );
        assert_eq!(
            scheme_text("00000000-0000-0000-0000-00000000c0de", None),
            "power plan 00000000-0000-0000-0000-00000000c0de"
        );
        assert_eq!(
            scheme_text(
                "00000000-0000-0000-0000-00000000c0de",
                Some(power::UNNAMED_SCHEME)
            ),
            "power plan 00000000-0000-0000-0000-00000000c0de"
        );
    }

    #[test]
    fn fill_replaces_each_named_field_once() {
        assert_eq!(
            fill(
                "{label} is {found}, not {expected}.",
                &[
                    ("label", "Value"),
                    ("found", "{expected}"),
                    ("expected", "0")
                ]
            ),
            "Value is {expected}, not 0."
        );
        assert_eq!(
            fill("{unknown} and {", &[("label", "x")]),
            "{unknown} and {"
        );
        assert_eq!(fill("no fields", &[]), "no fields");
    }

    #[test]
    fn read_raw_reads_a_sandbox_value_and_absence() {
        let key = r"Software\PCOptimizer\SelfTest\SafetyVerifyRead";
        delete_sandbox_tree(key).unwrap();
        assert_eq!(read_raw(Hive::CurrentUser, key, "Probe").unwrap(), None);
        let (handle, _) = Key::create(Hive::CurrentUser, key).unwrap();
        handle.set("Probe", &RegValue::Dword(3)).unwrap();
        drop(handle);
        let read = read_raw(Hive::CurrentUser, key, "Probe");
        delete_sandbox_tree(key).unwrap();
        assert_eq!(read.unwrap(), Some(RegValue::Dword(3).to_raw()));
        assert_eq!(read_raw(Hive::CurrentUser, key, "Probe").unwrap(), None);
    }

    #[test]
    fn service_start_is_read_only_and_none_for_a_missing_service() {
        let (start, _) = read_start("Dnscache")
            .unwrap()
            .expect("Dnscache exists on Windows");
        assert_ne!(start, StartType::Boot);
        assert_eq!(read_start("PCOptimizerNoSuchService").unwrap(), None);
    }
}
