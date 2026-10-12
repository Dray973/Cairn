//! Reading a catalog target back and comparing it with what its action sets: once after an
//! apply has settled, and in every scan for targets with an active journal record.
//!
//! A target holds what its action sets when a registry value has the action's type and data
//! (for a flags action, only the bits it clears count), a service has the action's start type
//! (whether it runs is not compared: a stop Windows refused is in the apply's details, and
//! Windows starts manual services on demand), a scheduled task has the action's enabled flag,
//! and the active power scheme satisfies the plan. A service or scheduled task that no longer
//! exists is not checked.

use tracing::warn;

use super::catalog::{Action, RegData, RegistryAction, ScheduledTaskAction, ServiceAction};
use super::engine::{action_label, registry_key, ActiveRecords, EngineSystem, ItemResult};
use super::power::{self, PowerScheme};
use super::scheduled_tasks::{self, TaskConnection, TaskInfo, TaskStore};
use super::{registry, services, ActionState, ActionStatus};
use crate::safety::rollback::RegistryTarget;
use crate::safety::verify::{
    enabled_text, fill, raw_text, read_raw, read_start, registry_label, same_setting, scheme_text,
    service_label, task_label, unverified, value_text, Mismatch, NOT_ON_THIS_PC, NOT_SET,
    POWER_PLAN_LABEL,
};
use crate::safety::{registry_target, Safety};
use crate::win::registry::{read_value, RawValue, RegValue};
use crate::win::scm::StartType;
use crate::{Error, Result};

/// Audit log operation of a target that did not hold when it was read back after an apply.
pub(crate) const OP_VERIFY_AFTER_APPLY: &str = "verify_after_apply";

/// Detail of an applied item whose target could not be read back: `{label}` (the target's
/// short name), `{error}`.
pub(crate) const READ_BACK_DETAIL: &str = "{label}: could not be read back ({error})";

/// Apply warning of a target that could not be read back: `{item}`, `{target}`, `{error}`.
pub(crate) const READ_BACK_WARNING: &str = "{item}: {target} could not be read back ({error}).";

/// Apply warning of a read-back whose audit row could not be written: `{target}`, `{error}`.
pub(crate) const AUDIT_FAILED: &str =
    "The read-back of {target} could not be written to the audit log ({error}).";

/// What a catalog action's target holds now, compared with what the action sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadBack {
    /// The target holds what the action sets.
    pub held: bool,
    /// What the action sets, as reports show it.
    pub expected: String,
    /// What the target holds now, in the same form.
    pub found: String,
}

/// Reads one action's target, for both its scan status and its read-back; scheduled tasks go
/// through `tasks`, which connects to Task Scheduler on first use. A scheduled task is queried
/// once for both. A registry value, a service and the power scheme are read for the status by
/// their own module and once more here, in the same call.
pub(crate) fn action_read(
    action: &Action,
    tasks: &TaskConnection,
) -> Result<(ActionStatus, ReadBack)> {
    match action {
        Action::Registry(r) => {
            let status = registry::status(r)?;
            let current = read_value(r.hive, r.path, r.name)?;
            Ok((status, registry_read_back(r, current.as_ref())))
        }
        Action::Service(s) => {
            let status = services::status(s)?;
            let start = read_start(s.name)?.map(|(start, _)| start);
            Ok((status, service_read_back(s, start)))
        }
        Action::ScheduledTask(s) => task_read(tasks.store()?, s),
        Action::Power(plan) => {
            let status = power::status(*plan)?;
            let active = power::active_scheme()?;
            let held = status.state == ActionState::Applied;
            Ok((status, power_read_back(held, &active)))
        }
    }
}

/// [`action_read`] of a scheduled task through `store`, which is queried once.
pub(crate) fn task_read(
    store: &dyn TaskStore,
    action: &ScheduledTaskAction,
) -> Result<(ActionStatus, ReadBack)> {
    let info = store.query(action.path)?;
    let status = scheduled_tasks::status_with(&Known(info), action)?;
    Ok((status, task_read_back(action, info)))
}

/// A task store that answers with the state of a task already read.
struct Known(Option<TaskInfo>);

impl TaskStore for Known {
    fn query(&self, _path: &str) -> Result<Option<TaskInfo>> {
        Ok(self.0)
    }

    fn set_enabled(&self, path: &str, _enabled: bool) -> Result<()> {
        Err(Error::Other(format!(
            "scheduled task {path} is only read here"
        )))
    }
}

/// What a registry action's target holds (`current`, `None` when absent) compared with what
/// the action sets, by the rule of the scan's status: a fixed value must have the action's
/// type and data; a flags value must have none of the bits the action clears, and an absent
/// one stands for the action's default.
pub(crate) fn registry_read_back(action: &RegistryAction, current: Option<&RegValue>) -> ReadBack {
    let held = match action.data {
        RegData::FlagsSzClear { clear, default } => match current {
            None => default & clear == 0,
            Some(value) => flags_number(value).is_some_and(|n| n & clear == 0),
        },
        RegData::Dword(_) | RegData::Sz(_) => current == fixed_value(action.data).as_ref(),
    };
    ReadBack {
        held,
        expected: expected_text(action),
        found: registry_value_text(action, current),
    }
}

/// What a registry action sets, as reports show it: the value, or for a flags action
/// "bit 0x4 clear".
pub(crate) fn expected_text(action: &RegistryAction) -> String {
    match (action.data, fixed_value(action.data)) {
        (RegData::FlagsSzClear { clear, .. }, _) => format!("{} clear", bits_text(clear)),
        (_, value) => value_text(action.hive, action.path, action.name, value.as_ref()),
    }
}

/// What a registry action's target holds (`None`: absent), as reports show it: a flags value
/// as its number and whether the bits the action clears are set, any other value as
/// `safety::verify::value_text` words it.
pub(crate) fn registry_value_text(action: &RegistryAction, value: Option<&RegValue>) -> String {
    let RegData::FlagsSzClear { clear, default } = action.data else {
        return value_text(action.hive, action.path, action.name, value);
    };
    match value {
        None => format!("{NOT_SET} (Windows uses {default})"),
        Some(value) => match flags_number(value) {
            Some(n) => {
                let state = if n & clear == 0 { "clear" } else { "set" };
                format!("{n} ({} {state})", bits_text(clear))
            }
            None => value.display(),
        },
    }
}

/// The value a fixed registry action writes; `None` for a flags action.
fn fixed_value(data: RegData) -> Option<RegValue> {
    match data {
        RegData::Dword(v) => Some(RegValue::Dword(v)),
        RegData::Sz(s) => Some(RegValue::Sz(s.to_string())),
        RegData::FlagsSzClear { .. } => None,
    }
}

/// Number held by a flags value: a decimal REG_SZ (surrounding blanks ignored) or a DWORD.
fn flags_number(value: &RegValue) -> Option<u32> {
    match value {
        RegValue::Sz(s) => s.trim().parse().ok(),
        RegValue::Dword(v) => Some(*v),
        _ => None,
    }
}

/// "bit 0x4", or "bits 0x6" for several.
fn bits_text(bits: u32) -> String {
    if bits.count_ones() == 1 {
        format!("bit 0x{bits:x}")
    } else {
        format!("bits 0x{bits:x}")
    }
}

/// What a service action's target holds (`start`: its start type, `None` when the service
/// is not installed). Only the start type counts.
pub(crate) fn service_read_back(action: &ServiceAction, start: Option<StartType>) -> ReadBack {
    let expected = action.start.label().to_string();
    match start {
        Some(start) => ReadBack {
            held: start == action.start,
            expected,
            found: start.label().to_string(),
        },
        None => ReadBack {
            held: true,
            expected,
            found: NOT_ON_THIS_PC.to_string(),
        },
    }
}

/// What a scheduled task action's target holds (`info`: the task's state, `None` when it
/// does not exist). Only the enabled flag counts.
pub(crate) fn task_read_back(action: &ScheduledTaskAction, info: Option<TaskInfo>) -> ReadBack {
    let expected = enabled_text(action.enabled).to_string();
    match info {
        Some(info) => ReadBack {
            held: info.enabled == action.enabled,
            expected,
            found: enabled_text(info.enabled).to_string(),
        },
        None => ReadBack {
            held: true,
            expected,
            found: NOT_ON_THIS_PC.to_string(),
        },
    }
}

/// What the power plan action's target holds: the active scheme, which satisfies the plan
/// when `held` (the power module decides which schemes do).
pub(crate) fn power_read_back(held: bool, active: &PowerScheme) -> ReadBack {
    ReadBack {
        held,
        expected: power::APP_SCHEME_NAME.to_string(),
        found: scheme_text(&active.guid, Some(&active.name)),
    }
}

/// Short name of an action's target in one-line texts: the value name, "service SysMain",
/// "task Proxy" or "the power plan".
pub(crate) fn target_label(action: &Action) -> String {
    match action {
        Action::Registry(r) => registry_label(r.name),
        Action::Service(s) => service_label(s.name),
        Action::ScheduledTask(s) => task_label(s.path),
        Action::Power(_) => POWER_PLAN_LABEL.to_string(),
    }
}

/// The value an action's target had before Cairn's first change, from its active record, in
/// the form of the action's read-back; `None` when the target has no active record.
pub(crate) fn recorded_original(action: &Action, records: &ActiveRecords) -> Option<String> {
    match action {
        Action::Registry(r) => records
            .registry
            .get(&registry_key(r.hive, r.path, r.name))
            .map(|rec| registry_value_text(r, rec.original_decoded().as_ref())),
        Action::Service(s) => records
            .services
            .get(&s.name.to_ascii_lowercase())
            .map(|rec| rec.start_type.label().to_string()),
        Action::ScheduledTask(s) => records
            .scheduled_tasks
            .get(&s.path.to_ascii_lowercase())
            .map(|rec| enabled_text(rec.was_enabled).to_string()),
        Action::Power(_) => records.power.first().map(|rec| {
            let name = power::scheme_name(&rec.previous_scheme);
            scheme_text(&rec.previous_scheme, name.as_deref())
        }),
    }
}

/// The mismatch of an action's target that did not hold when it was read (`read`), with the
/// target's value from before Cairn's first change (`original`), when it is known.
pub(crate) fn action_mismatch(
    action: &Action,
    read: &ReadBack,
    original: Option<String>,
) -> Mismatch {
    Mismatch {
        target: action_label(action),
        label: target_label(action),
        expected: read.expected.clone(),
        found: read.found.clone(),
        back_to_original: original.as_deref() == Some(read.found.as_str()),
        original,
    }
}

/// A target an apply wrote, to be read back once the apply has settled.
#[derive(Debug, Clone)]
pub(crate) enum WrittenTarget {
    /// A catalog action whose outcome was applied.
    Action(&'static Action),
    /// A value an earlier version of Cairn set for the tweak, restored in this apply to its
    /// recorded original (`None`: the value did not exist).
    Former {
        target: RegistryTarget,
        original: Option<RawValue>,
    },
}

/// Short name of a written target in one-line texts, as [`target_label`] names it.
fn written_label(target: &WrittenTarget) -> String {
    match target {
        WrittenTarget::Action(action) => target_label(action),
        WrittenTarget::Former { target, .. } => registry_label(&target.value_name),
    }
}

/// Reads every written target once (the caller already waited `settle`), fills `not_kept`
/// of the item at each target's result index and logs `verify_after_apply` rows. A read
/// error adds a detail to the item and a warning. Targets Windows changes on its own
/// (`safety::verify::unverified`) are not read. The checks read the PC directly, so `_sys`,
/// the engine's other seams, is not needed.
pub(crate) fn after_apply(
    _sys: &EngineSystem,
    safety: &Safety,
    written: &[(usize, WrittenTarget)],
    results: &mut [ItemResult],
    warnings: &mut Vec<String>,
) -> Result<()> {
    after_apply_with(&TaskConnection::new(), safety, written, results, warnings)
}

/// [`after_apply`] with scheduled tasks read through `tasks`.
fn after_apply_with(
    tasks: &TaskConnection,
    safety: &Safety,
    written: &[(usize, WrittenTarget)],
    results: &mut [ItemResult],
    warnings: &mut Vec<String>,
) -> Result<()> {
    if written.is_empty() {
        return Ok(());
    }
    let records = ActiveRecords::load(safety.journal())?;
    for (index, target) in written {
        let Some(result) = results.get_mut(*index) else {
            continue;
        };
        let (text, read) = match target {
            WrittenTarget::Action(action) => (
                action_label(action),
                action_not_kept(action, tasks, &records),
            ),
            WrittenTarget::Former { target, original } => (
                registry_target(target.hive, &target.key_path, &target.value_name),
                former_not_kept(target, original.as_ref()),
            ),
        };
        match read {
            Ok(None) => {}
            Ok(Some(mismatch)) => {
                let detail = format!("found {}; Cairn set {}", mismatch.found, mismatch.expected);
                if let Err(e) =
                    safety.log_op(OP_VERIFY_AFTER_APPLY, &text, "not_kept", Some(&detail))
                {
                    warn!(target = %text, error = %e, "cannot log a read-back");
                    let error = e.to_string();
                    warnings.push(fill(AUDIT_FAILED, &[("target", &text), ("error", &error)]));
                }
                result.not_kept.push(mismatch);
            }
            Err(e) => {
                let (label, error) = (written_label(target), e.to_string());
                result.details.push(fill(
                    READ_BACK_DETAIL,
                    &[("label", &label), ("error", &error)],
                ));
                warnings.push(fill(
                    READ_BACK_WARNING,
                    &[("item", &result.id), ("target", &text), ("error", &error)],
                ));
            }
        }
    }
    Ok(())
}

/// The mismatch of a catalog action's target that did not hold what the apply wrote; `None`
/// when it held or is not read.
fn action_not_kept(
    action: &Action,
    tasks: &TaskConnection,
    records: &ActiveRecords,
) -> Result<Option<Mismatch>> {
    if let Action::Registry(r) = action {
        if unverified(r.hive, r.path, r.name) {
            return Ok(None);
        }
    }
    let (_, read) = action_read(action, tasks)?;
    Ok((!read.held).then(|| action_mismatch(action, &read, recorded_original(action, records))))
}

/// The mismatch of a value an earlier version set that this apply restored to `original`
/// (`None`: absent) and that does not hold it; `None` when it held or is not read.
fn former_not_kept(
    target: &RegistryTarget,
    original: Option<&RawValue>,
) -> Result<Option<Mismatch>> {
    let (hive, key_path, value_name) = (target.hive, &target.key_path, &target.value_name);
    if unverified(hive, key_path, value_name) {
        return Ok(None);
    }
    let current = read_raw(hive, key_path, value_name)?;
    if same_setting(hive, key_path, value_name, original, current.as_ref()) {
        return Ok(None);
    }
    let expected = raw_text(hive, key_path, value_name, original);
    Ok(Some(Mismatch {
        target: registry_target(hive, key_path, value_name),
        label: registry_label(value_name),
        found: raw_text(hive, key_path, value_name, current.as_ref()),
        original: Some(expected.clone()),
        expected,
        back_to_original: false,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::debloat::catalog::TWEAKS;
    use crate::debloat::scheduled_tasks::fake::FakeTasks;
    use crate::debloat::ItemOutcome;
    use crate::safety::state_log::{
        Journal, NewPowerRecord, NewRegistryRecord, NewScheduledTaskRecord, NewServiceRecord,
    };
    use crate::safety::{test_safety, MutationOutcome};
    use crate::win::registry::{delete_sandbox_tree, Hive, Key};

    type ActionRead = fn(&Action, &TaskConnection) -> Result<(ActionStatus, ReadBack)>;
    type AfterApply = fn(
        &EngineSystem,
        &Safety,
        &[(usize, WrittenTarget)],
        &mut [ItemResult],
        &mut Vec<String>,
    ) -> Result<()>;

    #[test]
    fn read_back_functions_keep_their_signatures() {
        let _: ActionRead = action_read;
        let _: AfterApply = after_apply;
        let read = ReadBack {
            held: false,
            expected: "0".into(),
            found: "1".into(),
        };
        assert!(!read.held);
        assert_ne!(read.expected, read.found);
    }

    #[test]
    fn written_targets_name_an_action_or_a_former_value() {
        let action: &'static Action = &TWEAKS[0].actions[0];
        let former = RegistryTarget {
            hive: Hive::CurrentUser,
            key_path: r"Software\PCOptimizer\SelfTest\Verify".into(),
            value_name: "Former".into(),
        };
        let written = [
            WrittenTarget::Action(action),
            WrittenTarget::Former {
                target: former.clone(),
                original: None,
            },
        ];
        for target in &written {
            match target {
                WrittenTarget::Action(a) => assert_eq!(*a, action),
                WrittenTarget::Former { target, original } => {
                    assert_eq!(*target, former);
                    assert!(original.is_none());
                }
            }
        }
    }

    const EXAMPLE: &str = r"Software\PCOptimizer\SelfTest\Example";

    fn read(held: bool, expected: &str, found: &str) -> ReadBack {
        ReadBack {
            held,
            expected: expected.to_string(),
            found: found.to_string(),
        }
    }

    #[test]
    fn registry_read_back_compares_type_and_data() {
        let action = RegistryAction {
            hive: Hive::CurrentUser,
            path: EXAMPLE,
            name: "Value",
            data: RegData::Dword(1),
        };
        let back = |value: Option<RegValue>| registry_read_back(&action, value.as_ref());
        assert_eq!(
            back(Some(RegValue::Dword(1))),
            read(true, "0x00000001 (1)", "0x00000001 (1)")
        );
        assert_eq!(
            back(Some(RegValue::Dword(0))),
            read(false, "0x00000001 (1)", "0x00000000 (0)")
        );
        assert_eq!(
            back(Some(RegValue::Sz("1".into()))),
            read(false, "0x00000001 (1)", "\"1\""),
            "the same data as text is another type"
        );
        assert!(!back(Some(RegValue::Qword(1))).held);
        assert_eq!(back(None), read(false, "0x00000001 (1)", "not set"));

        let text = RegistryAction {
            data: RegData::Sz("High"),
            ..action
        };
        let back = |value: RegValue| registry_read_back(&text, Some(&value));
        assert_eq!(
            back(RegValue::Sz("High".into())),
            read(true, "\"High\"", "\"High\"")
        );
        assert!(!back(RegValue::ExpandSz("High".into())).held);
        assert!(!back(RegValue::Sz("high".into())).held);
    }

    #[test]
    fn flags_read_back_looks_at_the_cleared_bits_only() {
        let action = RegistryAction {
            hive: Hive::CurrentUser,
            path: EXAMPLE,
            name: "Flags",
            data: RegData::FlagsSzClear {
                clear: 0x4,
                default: 510,
            },
        };
        let back = |value: Option<RegValue>| registry_read_back(&action, value.as_ref());
        let sz = |s: &str| Some(RegValue::Sz(s.to_string()));
        assert_eq!(
            back(sz("507")),
            read(true, "bit 0x4 clear", "507 (bit 0x4 clear)")
        );
        assert!(back(sz("2")).held, "the other bits are the user's");
        assert!(back(sz(" 506 ")).held);
        assert_eq!(
            back(sz("511")),
            read(false, "bit 0x4 clear", "511 (bit 0x4 set)")
        );
        assert_eq!(
            back(Some(RegValue::Dword(2))),
            read(true, "bit 0x4 clear", "2 (bit 0x4 clear)")
        );
        assert_eq!(
            back(None),
            read(false, "bit 0x4 clear", "not set (Windows uses 510)"),
            "Windows' default has the bit set"
        );
        assert_eq!(
            back(sz("garbage")),
            read(false, "bit 0x4 clear", "\"garbage\"")
        );

        let quiet_default = RegistryAction {
            data: RegData::FlagsSzClear {
                clear: 0x6,
                default: 1,
            },
            ..action
        };
        assert_eq!(
            registry_read_back(&quiet_default, None),
            read(true, "bits 0x6 clear", "not set (Windows uses 1)")
        );
    }

    #[test]
    fn service_read_back_ignores_the_running_state() {
        let action = ServiceAction {
            name: "PCOptimizerSelfTest",
            start: StartType::Disabled,
            stop: true,
        };
        // Whether the service still runs (a stop Windows refused) is not read at all: a
        // disabled service holds.
        assert_eq!(
            service_read_back(&action, Some(StartType::Disabled)),
            read(true, "disabled", "disabled")
        );
        assert_eq!(
            service_read_back(&action, Some(StartType::Manual)),
            read(false, "disabled", "manual")
        );
        assert_eq!(
            service_read_back(&action, None),
            read(true, "disabled", "not on this PC"),
            "a service that is gone is not checked"
        );
    }

    const DISABLED_TASK: &str = r"\PCOptimizerSelfTest\VerifyDisabled";
    const ENABLED_TASK: &str = r"\PCOptimizerSelfTest\VerifyEnabled";
    const MISSING_TASK: &str = r"\PCOptimizerSelfTest\VerifyMissing";

    #[test]
    fn task_read_back_uses_the_store() {
        let tasks = FakeTasks::with(&[(DISABLED_TASK, false, false), (ENABLED_TASK, true, true)]);
        let cases = [
            (
                DISABLED_TASK,
                ActionState::Applied,
                read(true, "disabled", "disabled"),
            ),
            (
                ENABLED_TASK,
                ActionState::NotApplied,
                read(false, "disabled", "enabled"),
            ),
            (
                MISSING_TASK,
                ActionState::Unavailable,
                read(true, "disabled", "not on this PC"),
            ),
        ];
        for (path, state, expected) in cases {
            let action = ScheduledTaskAction {
                path,
                enabled: false,
            };
            let (status, back) = task_read(&tasks, &action).unwrap();
            let direct = scheduled_tasks::status_with(&tasks, &action).unwrap();
            assert_eq!(status.state, state, "{path}");
            assert_eq!(status.detail, direct.detail, "the status of one query");
            assert_eq!(back, expected, "{path}");
        }
        assert_eq!(tasks.writes.get(), 0, "read-only");
        assert!(Known(None).set_enabled(DISABLED_TASK, true).is_err());
    }

    #[test]
    fn power_read_back_names_the_active_plan() {
        let balanced = PowerScheme {
            guid: power::BALANCED_GUID.to_string(),
            name: "Ausbalanciert".to_string(),
        };
        assert_eq!(
            power_read_back(false, &balanced),
            read(false, "Ultimate Performance (Cairn)", "Balanced")
        );
        let custom = PowerScheme {
            guid: "00000000-0000-0000-0000-00000000c0de".to_string(),
            name: "Quiet".to_string(),
        };
        assert_eq!(
            power_read_back(true, &custom),
            read(true, "Ultimate Performance (Cairn)", "Quiet")
        );
    }

    fn temp_journal(dir: &tempfile::TempDir) -> Arc<Journal> {
        Arc::new(Journal::open(dir.path().join("journal.db")).unwrap())
    }

    static SERVICE: Action = Action::Service(ServiceAction {
        name: "PCOptimizerSelfTest",
        start: StartType::Disabled,
        stop: true,
    });
    static TASK: Action = Action::ScheduledTask(ScheduledTaskAction {
        path: DISABLED_TASK,
        enabled: false,
    });
    static POWER: Action = Action::Power(crate::debloat::catalog::PowerPlan::UltimatePerformance);
    static FLAGS: Action = Action::Registry(RegistryAction {
        hive: Hive::CurrentUser,
        path: EXAMPLE,
        name: "Flags",
        data: RegData::FlagsSzClear {
            clear: 0x4,
            default: 510,
        },
    });

    #[test]
    fn mismatches_name_the_target_and_its_recorded_original() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let session = journal.begin_session("verify unit", "test").unwrap();
        journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: Hive::CurrentUser,
                    key_path: EXAMPLE.to_ascii_uppercase(),
                    value_name: "flags".to_string(),
                    key_existed: true,
                    value_existed: true,
                    original: Some(RegValue::Sz("511".into()).to_raw()),
                    created_root: None,
                },
            )
            .unwrap();
        journal
            .record_service(
                session,
                &NewServiceRecord {
                    name: "pcoptimizerselftest".to_string(),
                    display_name: "Cairn self-test".to_string(),
                    start_type: StartType::Manual,
                    delayed_auto_start: false,
                    was_running: false,
                },
            )
            .unwrap();
        journal
            .record_scheduled_task(
                session,
                &NewScheduledTaskRecord {
                    path: DISABLED_TASK.to_ascii_lowercase(),
                    was_enabled: true,
                },
            )
            .unwrap();
        journal
            .record_power(
                session,
                &NewPowerRecord {
                    previous_scheme: power::BALANCED_GUID.to_string(),
                    target_scheme: power::APP_SCHEME_GUID.to_string(),
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
        let records = ActiveRecords::load(&journal).unwrap();

        let originals: Vec<Option<String>> = [&FLAGS, &SERVICE, &TASK, &POWER]
            .map(|action| recorded_original(action, &records))
            .into();
        assert_eq!(
            originals,
            [
                Some("511 (bit 0x4 set)".to_string()),
                Some("manual".to_string()),
                Some("enabled".to_string()),
                Some("Balanced".to_string()),
            ]
        );
        let labels = [&FLAGS, &SERVICE, &TASK, &POWER].map(target_label);
        assert_eq!(
            labels,
            [
                "Flags",
                "service PCOptimizerSelfTest",
                "task VerifyDisabled",
                "the power plan"
            ]
        );
        let empty = ActiveRecords::load(&temp_journal(&tempfile::tempdir().unwrap())).unwrap();
        assert_eq!(recorded_original(&FLAGS, &empty), None);

        let back = read(false, "bit 0x4 clear", "511 (bit 0x4 set)");
        let mismatch = action_mismatch(&FLAGS, &back, recorded_original(&FLAGS, &records));
        assert_eq!(
            mismatch,
            Mismatch {
                target: format!(r"HKCU\{EXAMPLE}\Flags"),
                label: "Flags".to_string(),
                expected: "bit 0x4 clear".to_string(),
                found: "511 (bit 0x4 set)".to_string(),
                original: Some("511 (bit 0x4 set)".to_string()),
                back_to_original: true,
            }
        );
        let elsewhere = action_mismatch(&TASK, &read(false, "disabled", "enabled"), None);
        assert_eq!(elsewhere.target, format!("scheduled task {DISABLED_TASK}"));
        assert!(!elsewhere.back_to_original, "no original to compare with");
    }

    #[test]
    fn a_recorded_power_plan_is_worded_like_the_active_one() {
        // The active scheme is only read; the record goes to a temporary journal.
        let active = power::active_scheme().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let session = journal.begin_session("verify unit", "test").unwrap();
        journal
            .record_power(
                session,
                &NewPowerRecord {
                    previous_scheme: active.guid.clone(),
                    target_scheme: power::APP_SCHEME_GUID.to_string(),
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
        let records = ActiveRecords::load(&journal).unwrap();

        let read = power_read_back(false, &active);
        let original = recorded_original(&POWER, &records);
        assert_eq!(original.as_deref(), Some(read.found.as_str()));
        assert!(action_mismatch(&POWER, &read, original).back_to_original);
    }

    /// Removes a sandbox key when dropped, so a failing test leaves nothing behind.
    struct SandboxKey(&'static str);

    impl Drop for SandboxKey {
        fn drop(&mut self) {
            let _ = delete_sandbox_tree(self.0);
        }
    }

    fn set_sandbox(path: &str, name: &str, value: u32) {
        let (key, _) = Key::create(Hive::CurrentUser, path).unwrap();
        key.set(name, &RegValue::Dword(value)).unwrap();
    }

    fn applied(id: &str) -> ItemResult {
        ItemResult {
            id: id.to_string(),
            outcome: ItemOutcome::Applied,
            details: Vec::new(),
            not_kept: Vec::new(),
        }
    }

    /// `(op, target, outcome, detail)` of every read-back row, oldest first.
    fn read_back_rows(journal: &Journal) -> Vec<(String, String, Option<String>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|op| op.op == OP_VERIFY_AFTER_APPLY)
            .map(|op| (op.target, op.outcome, op.detail))
            .collect();
        rows.reverse();
        rows
    }

    const RESET_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\VerifyUnitReset";
    static RESET: Action = Action::Registry(RegistryAction {
        hive: Hive::CurrentUser,
        path: RESET_SANDBOX,
        name: "Value",
        data: RegData::Dword(1),
    });

    #[test]
    fn a_value_windows_resets_is_not_kept_and_stays_recorded() {
        let _cleanup = SandboxKey(RESET_SANDBOX);
        set_sandbox(RESET_SANDBOX, "Value", 0);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "verify unit", false);
        let Action::Registry(value) = &RESET else {
            unreachable!("a registry action")
        };
        assert_eq!(
            registry::apply(&safety, value).unwrap(),
            MutationOutcome::Applied
        );
        // Another program puts the old value back while the apply waits.
        set_sandbox(RESET_SANDBOX, "Value", 0);

        let mut results = vec![applied("selftest.reset")];
        let mut warnings = Vec::new();
        after_apply(
            &EngineSystem::LIVE,
            &safety,
            &[(0, WrittenTarget::Action(&RESET))],
            &mut results,
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let target = format!(r"HKCU\{RESET_SANDBOX}\Value");
        assert_eq!(
            results[0].not_kept,
            vec![Mismatch {
                target: target.clone(),
                label: "Value".to_string(),
                expected: "0x00000001 (1)".to_string(),
                found: "0x00000000 (0)".to_string(),
                original: Some("0x00000000 (0)".to_string()),
                back_to_original: true,
            }]
        );
        assert_eq!(
            results[0].outcome,
            ItemOutcome::Applied,
            "the write happened"
        );
        assert!(results[0].details.is_empty());
        assert_eq!(
            read_back_rows(&journal),
            vec![(
                target,
                "not_kept".to_string(),
                Some("found 0x00000000 (0); Cairn set 0x00000001 (1)".to_string())
            )]
        );
        let logged = journal.ops(100).unwrap();
        assert!(logged
            .iter()
            .filter(|op| op.op == OP_VERIFY_AFTER_APPLY)
            .all(|op| op.session_id == Some(safety.session_id())));
        let active = journal.active_registry().unwrap();
        assert_eq!(
            active.len(),
            1,
            "the record stays, so Undo restores the original"
        );
    }

    const KEPT_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\VerifyUnitKept";
    static KEPT_FIRST: Action = Action::Registry(RegistryAction {
        hive: Hive::CurrentUser,
        path: KEPT_SANDBOX,
        name: "First",
        data: RegData::Dword(1),
    });
    static KEPT_SECOND: Action = Action::Registry(RegistryAction {
        hive: Hive::CurrentUser,
        path: KEPT_SANDBOX,
        name: "Second",
        data: RegData::Dword(1),
    });

    #[test]
    fn only_the_written_targets_are_read_back() {
        let _cleanup = SandboxKey(KEPT_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "verify unit", false);
        for action in [&KEPT_FIRST, &KEPT_SECOND] {
            let Action::Registry(value) = action else {
                unreachable!("a registry action")
            };
            registry::apply(&safety, value).unwrap();
        }
        // Both values change, but this apply wrote only the first; the second was already
        // set, so it is not read back.
        set_sandbox(KEPT_SANDBOX, "First", 4);
        set_sandbox(KEPT_SANDBOX, "Second", 4);
        let mut results = vec![applied("selftest.kept")];
        let mut warnings = Vec::new();
        after_apply(
            &EngineSystem::LIVE,
            &safety,
            &[(0, WrittenTarget::Action(&KEPT_FIRST))],
            &mut results,
            &mut warnings,
        )
        .unwrap();
        let labels: Vec<&str> = results[0]
            .not_kept
            .iter()
            .map(|m| m.label.as_str())
            .collect();
        assert_eq!(labels, ["First"]);

        // A value that holds adds nothing, and an index past the results is ignored.
        set_sandbox(KEPT_SANDBOX, "First", 1);
        let mut results = vec![applied("selftest.kept")];
        after_apply(
            &EngineSystem::LIVE,
            &safety,
            &[
                (0, WrittenTarget::Action(&KEPT_FIRST)),
                (3, WrittenTarget::Action(&KEPT_SECOND)),
            ],
            &mut results,
            &mut warnings,
        )
        .unwrap();
        assert!(results[0].not_kept.is_empty());
        assert!(results[0].details.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            read_back_rows(&journal).len(),
            1,
            "only the first check logged"
        );

        // Nothing written: nothing is read, not even the journal.
        let mut results = vec![applied("selftest.kept")];
        after_apply(
            &EngineSystem::LIVE,
            &safety,
            &[],
            &mut results,
            &mut warnings,
        )
        .unwrap();
        assert!(results[0].not_kept.is_empty());
    }

    const FORMER_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\VerifyUnitFormer";

    #[test]
    fn former_restore_is_read_back() {
        let _cleanup = SandboxKey(FORMER_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "verify unit", false);
        let former = |name: &str| RegistryTarget {
            hive: Hive::CurrentUser,
            key_path: FORMER_SANDBOX.to_string(),
            value_name: name.to_string(),
        };
        // `Removed` was restored to not set, but holds a value again; `Restored` holds its
        // restored original.
        set_sandbox(FORMER_SANDBOX, "Removed", 0);
        set_sandbox(FORMER_SANDBOX, "Restored", 1);
        let written = [
            (
                0,
                WrittenTarget::Former {
                    target: former("Removed"),
                    original: None,
                },
            ),
            (
                0,
                WrittenTarget::Former {
                    target: former("Restored"),
                    original: Some(RegValue::Dword(1).to_raw()),
                },
            ),
        ];
        let mut results = vec![ItemResult {
            outcome: ItemOutcome::AlreadyApplied,
            ..applied("selftest.former")
        }];
        let mut warnings = Vec::new();
        after_apply(
            &EngineSystem::LIVE,
            &safety,
            &written,
            &mut results,
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let target = format!(r"HKCU\{FORMER_SANDBOX}\Removed");
        assert_eq!(
            results[0].not_kept,
            vec![Mismatch {
                target: target.clone(),
                label: "Removed".to_string(),
                expected: "not set".to_string(),
                found: "0x00000000 (0)".to_string(),
                original: Some("not set".to_string()),
                back_to_original: false,
            }]
        );
        assert_eq!(
            read_back_rows(&journal),
            vec![(
                target,
                "not_kept".to_string(),
                Some("found 0x00000000 (0); Cairn set not set".to_string())
            )]
        );
    }

    #[test]
    fn read_errors_add_a_detail_and_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "verify unit", false);
        let error = "cannot connect to Task Scheduler: unavailable";
        let mut results = vec![applied("selftest.task")];
        let mut warnings = Vec::new();
        after_apply_with(
            &TaskConnection::failed(error),
            &safety,
            &[(0, WrittenTarget::Action(&TASK))],
            &mut results,
            &mut warnings,
        )
        .unwrap();
        let target = format!("scheduled task {DISABLED_TASK}");
        assert_eq!(
            results[0].details,
            vec![format!(
                "task VerifyDisabled: could not be read back ({error})"
            )],
            "the detail names the target by its short label"
        );
        assert_eq!(
            warnings,
            vec![format!(
                "selftest.task: {target} could not be read back ({error})."
            )]
        );
        assert!(results[0].not_kept.is_empty(), "no mismatch without a read");
        assert!(read_back_rows(&journal).is_empty());
    }

    #[test]
    fn written_targets_are_named_by_their_short_label() {
        let former = |value_name: &str| WrittenTarget::Former {
            target: RegistryTarget {
                hive: Hive::LocalMachine,
                key_path: r"SOFTWARE\Policies\Example".to_string(),
                value_name: value_name.to_string(),
            },
            original: None,
        };
        assert_eq!(written_label(&former("AllowTelemetry")), "AllowTelemetry");
        assert_eq!(written_label(&former("")), "(Default)");
        assert_eq!(
            written_label(&WrittenTarget::Action(&TASK)),
            "task VerifyDisabled"
        );
    }

    static PAUSE: Action = Action::Registry(RegistryAction {
        hive: Hive::LocalMachine,
        path: r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings",
        name: "PauseUpdatesExpiryTime",
        data: RegData::Sz("2000-01-01T00:00:00Z"),
    });

    #[test]
    fn values_windows_changes_on_its_own_are_not_read_back_after_an_apply() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "verify unit", false);
        let Action::Registry(pause) = &PAUSE else {
            unreachable!("a registry action")
        };
        let written = [
            (0, WrittenTarget::Action(&PAUSE)),
            (
                0,
                WrittenTarget::Former {
                    target: registry::target(pause),
                    original: None,
                },
            ),
        ];
        let mut results = vec![applied("selftest.pause")];
        let mut warnings = Vec::new();
        after_apply_with(
            &TaskConnection::failed("Task Scheduler must not be reached"),
            &safety,
            &written,
            &mut results,
            &mut warnings,
        )
        .unwrap();
        assert!(results[0].not_kept.is_empty());
        assert!(results[0].details.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(read_back_rows(&journal).is_empty());
    }
}
