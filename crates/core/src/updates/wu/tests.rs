use std::sync::{Arc, Mutex, MutexGuard};

use super::*;
use crate::debloat::catalog::{Action, TWEAKS};
use crate::safety::{rollback_filtered, test_safety};
use crate::win::registry::{delete_key_if_empty, delete_sandbox_tree, exists, read_value};

/// Parent of the per-test sandbox keys; each test uses its own subkey.
const SANDBOX: &str = r"Software\PCOptimizer\SelfTest\Updates";

static LOCK: Mutex<()> = Mutex::new(());

/// A sandbox layout under HKCU and a temp journal. The keys are removed on drop, even when
/// the test panics.
struct Sandbox {
    root: String,
    layout: WuLayout,
    journal: Arc<Journal>,
    _dir: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = format!(r"{SANDBOX}\{name}");
        delete_sandbox_tree(&root).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        Sandbox {
            layout: WuLayout {
                hive: Hive::CurrentUser,
                ux: format!(r"{root}\UX"),
                policy: format!(r"{root}\Policy"),
                policy_au: format!(r"{root}\PolicyAU"),
                reboot_required: format!(r"{root}\Reboot"),
            },
            root,
            journal,
            _dir: dir,
            _lock: lock,
        }
    }

    fn safety(&self) -> Safety {
        test_safety(Arc::clone(&self.journal), "windows update test", false)
    }

    fn seed(&self, key_path: &str, name: &str, value: RegValue) {
        let (key, _) = Key::create(Hive::CurrentUser, key_path).unwrap();
        key.set(name, &value).unwrap();
    }

    fn value(&self, key_path: &str, name: &str) -> Option<RegValue> {
        read_value(Hive::CurrentUser, key_path, name).unwrap()
    }

    fn state(&self, env: WuEnv) -> WuState {
        wu_state_with(&self.layout, env, Some(&self.journal)).unwrap()
    }

    fn apply(&self, env: WuEnv, change: &WuChange) -> Result<WuReport> {
        plan_or_apply_wu_with(&self.layout, env, change, false, || Ok(self.safety()))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = delete_sandbox_tree(&self.root);
        let _ = delete_key_if_empty(Hive::CurrentUser, SANDBOX);
    }
}

fn pro() -> Edition {
    Edition::from_parts(
        Some("Professional"),
        Some("Windows 10 Pro"),
        Some("25H2"),
        Some("26200"),
        Some(9457),
    )
}

fn home() -> Edition {
    Edition::from_parts(
        Some("Core"),
        Some("Windows 10 Home"),
        Some("25H2"),
        Some("26200"),
        Some(9457),
    )
}

fn unknown_edition() -> Edition {
    Edition::from_parts(None, None, None, None, None)
}

fn manual() -> Result<Option<StartType>> {
    Ok(Some(StartType::Manual))
}

fn disabled() -> Result<Option<StartType>> {
    Ok(Some(StartType::Disabled))
}

fn now() -> DateTime<Utc> {
    parse_time("2026-09-25T10:00:00Z").unwrap()
}

const PRO: WuEnv = WuEnv {
    edition: pro,
    service_start: manual,
    now,
};

const HOME: WuEnv = WuEnv {
    edition: home,
    service_start: manual,
    now,
};

/// Three days and two hours after [`now`].
fn later() -> DateTime<Utc> {
    parse_time("2026-09-28T12:00:00Z").unwrap()
}

const PRO_LATER: WuEnv = WuEnv { now: later, ..PRO };

fn setting(state: &WuState, id: WuSettingId) -> &WuSetting {
    state.settings.iter().find(|s| s.id == id).unwrap()
}

fn never() -> Result<Safety> {
    panic!("a session was opened")
}

fn sz(text: &str) -> RegValue {
    RegValue::Sz(text.to_string())
}

#[test]
fn empty_keys_read_as_windows_defaults() {
    let sb = Sandbox::new("Empty");
    let state = sb.state(PRO);
    assert_eq!(state.edition.name, "Windows 11 Pro");
    assert!(!state.edition.home);
    assert_eq!(state.edition.build, "26200.9457");
    assert_eq!(state.service, ServiceState::Manual);
    assert!(!state.restart_pending);
    assert!(state.managed.is_empty());
    assert!(state.warnings.is_empty());
    let ids: Vec<WuSettingId> = state.settings.iter().map(|s| s.id).collect();
    assert_eq!(ids, WuSettingId::ALL);
    assert_eq!(
        setting(&state, WuSettingId::Pause).value,
        WuValue::Pause {
            paused: false,
            until: None,
            started: None,
            expired: false
        }
    );
    assert_eq!(
        setting(&state, WuSettingId::ActiveHours).value,
        WuValue::ActiveHours {
            automatic: true,
            start: None,
            end: None,
            policy: false
        }
    );
    assert_eq!(
        setting(&state, WuSettingId::ExcludeDrivers).value,
        WuValue::Switch { on: false }
    );
    assert_eq!(
        setting(&state, WuSettingId::DeferFeature).value,
        WuValue::Defer { days: None }
    );
    assert_eq!(
        setting(&state, WuSettingId::RestartNotify).value,
        WuValue::Switch { on: false }
    );
    for s in &state.settings {
        assert!(s.available, "{:?}", s.id);
        assert!(!s.by_cairn && !s.differs, "{:?}", s.id);
        assert_eq!(s.targets, sb.layout.targets(s.id));
    }
    assert_eq!(
        setting(&state, WuSettingId::ExcludeDrivers)
            .caveat
            .as_deref(),
        Some(POLICY_CAVEAT)
    );
    assert_eq!(
        setting(&state, WuSettingId::DeferFeature).caveat.as_deref(),
        Some(POLICY_CAVEAT)
    );

    let without_journal = wu_state_with(&sb.layout, PRO, None).unwrap();
    assert_eq!(
        without_journal.warnings,
        vec![JOURNAL_UNREADABLE.to_string()]
    );
}

#[test]
fn pause_writes_six_values_after_recording_their_baselines() {
    let sb = Sandbox::new("Pause");
    let report = sb.apply(PRO, &WuChange::Pause { days: 14 }).unwrap();
    assert!(!report.dry_run);
    assert!(report.session_id.is_some());
    assert_eq!(report.setting, WuSettingId::Pause);
    let targets: Vec<String> = sb
        .layout
        .targets(WuSettingId::Pause)
        .iter()
        .map(target_text)
        .collect();
    let written: Vec<String> = report.writes.iter().map(|w| w.target.clone()).collect();
    assert_eq!(written, targets);
    assert!(report
        .writes
        .iter()
        .all(|w| w.outcome == Some(MutationOutcome::Applied) && w.before.is_none()));

    let start = sz("2026-09-25T10:00:00Z");
    let end = sz("2026-10-09T10:00:00Z");
    for name in [PAUSE_FEATURE_START, PAUSE_QUALITY_START, PAUSE_START] {
        assert_eq!(sb.value(&sb.layout.ux, name), Some(start.clone()), "{name}");
    }
    for name in [PAUSE_FEATURE_END, PAUSE_QUALITY_END, PAUSE_EXPIRY] {
        assert_eq!(sb.value(&sb.layout.ux, name), Some(end.clone()), "{name}");
    }

    // Each baseline says the value was absent, though it exists now: it was recorded first.
    let records = sb.journal.active_registry().unwrap();
    assert_eq!(records.len(), 6);
    assert!(records
        .iter()
        .all(|r| r.original.is_none() && !r.value_existed));
    let mut recorded: Vec<String> = records.iter().map(RegistryRecord::target).collect();
    recorded.sort();
    let mut expected = targets.clone();
    expected.sort();
    assert_eq!(recorded, expected);

    let state = sb.state(PRO);
    let pause = setting(&state, WuSettingId::Pause);
    assert_eq!(
        pause.value,
        WuValue::Pause {
            paused: true,
            until: Some("2026-10-09T10:00:00Z".into()),
            started: Some("2026-09-25T10:00:00Z".into()),
            expired: false
        }
    );
    assert!(pause.by_cairn && pause.differs);
}

const PAUSE_ENDS: [&str; 3] = [PAUSE_FEATURE_END, PAUSE_QUALITY_END, PAUSE_EXPIRY];
const PAUSE_STARTS: [&str; 3] = [PAUSE_FEATURE_START, PAUSE_QUALITY_START, PAUSE_START];

/// The six pause values, in target order.
fn pause_values(sb: &Sandbox) -> Vec<Option<RegValue>> {
    WuSettingId::Pause
        .values()
        .iter()
        .map(|(_, name)| sb.value(&sb.layout.ux, name))
        .collect()
}

#[test]
fn extending_a_pause_keeps_the_first_baseline() {
    let sb = Sandbox::new("Extend");
    let outside = sz("2026-01-01T00:00:00Z");
    sb.seed(&sb.layout.ux, PAUSE_EXPIRY, outside.clone());
    sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    // Three days later, 14 more days count from the end of the running pause, not from now,
    // and only the end values change.
    let report = sb.apply(PRO_LATER, &WuChange::Pause { days: 14 }).unwrap();
    let written: Vec<String> = report.writes.iter().map(|w| w.target.clone()).collect();
    let ends: Vec<String> = PAUSE_ENDS
        .iter()
        .map(|name| target_text(&sb.layout.target(Place::Ux, name)))
        .collect();
    assert_eq!(written, ends);
    for name in PAUSE_STARTS {
        assert_eq!(
            sb.value(&sb.layout.ux, name),
            Some(sz("2026-09-25T10:00:00Z")),
            "{name}"
        );
    }
    for name in PAUSE_ENDS {
        assert_eq!(
            sb.value(&sb.layout.ux, name),
            Some(sz("2026-10-16T10:00:00Z")),
            "{name}"
        );
    }
    let records = sb.journal.active_registry().unwrap();
    assert_eq!(records.len(), 6);
    let expiry = records
        .iter()
        .find(|r| r.value_name == PAUSE_EXPIRY)
        .unwrap();
    assert_eq!(expiry.original, Some(outside.to_raw()));
}

#[test]
fn a_pause_is_extended_to_at_most_five_weeks_from_its_start() {
    let sb = Sandbox::new("ExtendCap");
    sb.apply(PRO, &WuChange::Pause { days: 28 }).unwrap();
    sb.apply(PRO_LATER, &WuChange::Pause { days: 14 }).unwrap();
    for name in PAUSE_ENDS {
        assert_eq!(
            sb.value(&sb.layout.ux, name),
            Some(sz("2026-10-30T10:00:00Z")),
            "{name}: 35 days after the start, not 42"
        );
    }
    assert_eq!(
        sb.value(&sb.layout.ux, PAUSE_START),
        Some(sz("2026-09-25T10:00:00Z"))
    );
}

#[test]
fn a_pause_at_its_limit_is_not_extended_or_shortened() {
    let sb = Sandbox::new("ExtendLimit");
    sb.apply(PRO, &WuChange::Pause { days: 35 }).unwrap();
    let values = pause_values(&sb);
    let sessions = sb.journal.sessions().unwrap().len();
    for env in [PRO, PRO_LATER] {
        for dry_run in [true, false] {
            let err = plan_or_apply_wu_with(
                &sb.layout,
                env,
                &WuChange::Pause { days: 7 },
                dry_run,
                never,
            )
            .unwrap_err();
            assert_eq!(err.to_string(), PAUSE_LIMIT_TEXT);
        }
    }
    assert_eq!(pause_values(&sb), values, "nothing was written");
    assert_eq!(sb.journal.sessions().unwrap().len(), sessions);
    let state = sb.state(PRO_LATER);
    assert!(matches!(
        &setting(&state, WuSettingId::Pause).value,
        WuValue::Pause { paused: true, until: Some(until), .. } if until == "2026-10-30T10:00:00Z"
    ));
}

#[test]
fn extending_a_pause_set_outside_cairn_writes_only_its_end() {
    let sb = Sandbox::new("ExtendOutside");
    let ux = sb.layout.ux.clone();
    sb.seed(&ux, PAUSE_START, sz("2026-09-20T08:00:00Z"));
    sb.seed(&ux, PAUSE_QUALITY_END, sz("2026-09-27T08:00:00Z"));
    sb.seed(&ux, PAUSE_EXPIRY, sz("2026-09-27T08:00:00Z"));
    let report = sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    assert_eq!(report.writes.len(), 3);
    for name in PAUSE_ENDS {
        assert_eq!(
            sb.value(&ux, name),
            Some(sz("2026-10-04T08:00:00Z")),
            "{name}"
        );
    }
    assert_eq!(sb.value(&ux, PAUSE_START), Some(sz("2026-09-20T08:00:00Z")));
    assert_eq!(sb.value(&ux, PAUSE_FEATURE_START), None);
    assert_eq!(sb.value(&ux, PAUSE_QUALITY_START), None);
    let mut recorded: Vec<String> = sb
        .journal
        .active_registry()
        .unwrap()
        .iter()
        .map(|r| r.value_name.clone())
        .collect();
    recorded.sort();
    let mut ends: Vec<String> = PAUSE_ENDS.iter().map(|n| n.to_string()).collect();
    ends.sort();
    assert_eq!(recorded, ends, "only the values written are journaled");
}

#[test]
fn a_pause_without_a_start_is_extended_to_five_weeks_from_now() {
    let sb = Sandbox::new("ExtendNoStart");
    sb.seed(&sb.layout.ux, PAUSE_EXPIRY, sz("2026-10-20T00:00:00Z"));
    sb.apply(PRO, &WuChange::Pause { days: 35 }).unwrap();
    for name in PAUSE_ENDS {
        assert_eq!(
            sb.value(&sb.layout.ux, name),
            Some(sz("2026-10-30T10:00:00Z")),
            "{name}"
        );
    }
    assert_eq!(sb.value(&sb.layout.ux, PAUSE_START), None);
}

#[test]
fn resume_deletes_every_pause_value_expiry_first() {
    let sb = Sandbox::new("Resume");
    sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    let report = sb.apply(PRO, &WuChange::Resume).unwrap();
    assert_eq!(report.writes.len(), 6);
    assert!(report.writes[0].target.ends_with(PAUSE_EXPIRY));
    assert!(report
        .writes
        .iter()
        .all(|w| w.after.is_none() && w.before.is_some()));
    for (_, name) in WuSettingId::Pause.values() {
        assert_eq!(sb.value(&sb.layout.ux, name), None, "{name}");
    }
    let state = sb.state(PRO);
    let pause = setting(&state, WuSettingId::Pause);
    assert!(matches!(pause.value, WuValue::Pause { paused: false, .. }));
    assert!(pause.by_cairn && !pause.differs);
}

#[test]
fn undo_restores_absence_and_removes_the_keys_cairn_created() {
    let sb = Sandbox::new("Undo");
    sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    assert!(exists(Hive::CurrentUser, &sb.layout.ux).unwrap());
    let filter = RollbackFilter {
        registry: sb.layout.targets(WuSettingId::Pause),
        ..Default::default()
    };
    let report = rollback_filtered(&sb.journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 6);
    assert!(!exists(Hive::CurrentUser, &sb.layout.ux).unwrap());
    assert!(sb.journal.active_registry().unwrap().is_empty());
    let state = sb.state(PRO);
    assert!(!setting(&state, WuSettingId::Pause).by_cairn);
}

#[test]
fn invalid_values_fail_before_a_session() {
    let sb = Sandbox::new("Invalid");
    for change in [
        WuChange::ActiveHours { start: 8, end: 3 },
        WuChange::ActiveHours { start: 5, end: 5 },
        WuChange::ActiveHours { start: 24, end: 3 },
        WuChange::ActiveHours { start: 3, end: 24 },
        WuChange::Pause { days: 0 },
        WuChange::Pause { days: 36 },
        WuChange::DeferFeature(Some(0)),
        WuChange::DeferFeature(Some(366)),
    ] {
        let err = plan_or_apply_wu_with(&sb.layout, PRO, &change, false, never).unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{change:?}: {err}");
    }
    assert_eq!(
        WuChange::ActiveHours { start: 8, end: 3 }
            .validate()
            .unwrap_err()
            .to_string(),
        "Active hours can span at most 18 hours."
    );
    assert_eq!(
        WuChange::ActiveHours { start: 5, end: 5 }
            .validate()
            .unwrap_err()
            .to_string(),
        "Start and end must differ."
    );
    WuChange::ActiveHours { start: 8, end: 2 }
        .validate()
        .unwrap();
    WuChange::ActiveHours { start: 22, end: 6 }
        .validate()
        .unwrap();
    assert!(!exists(Hive::CurrentUser, &sb.layout.ux).unwrap());
    assert!(sb.journal.sessions().unwrap().is_empty());
}

#[test]
fn home_refuses_a_feature_update_delay_but_may_clear_one() {
    let sb = Sandbox::new("Home");
    let err = plan_or_apply_wu_with(
        &sb.layout,
        HOME,
        &WuChange::DeferFeature(Some(30)),
        false,
        never,
    )
    .unwrap_err();
    assert_eq!(err.to_string(), HOME_DEFER_TEXT);
    let plan = plan_or_apply_wu_with(&sb.layout, HOME, &WuChange::DeferFeature(None), true, never)
        .unwrap();
    assert_eq!(plan.writes.len(), 2);

    let state = sb.state(HOME);
    assert!(state.edition.home);
    let defer = setting(&state, WuSettingId::DeferFeature);
    assert!(!defer.available);
    assert_eq!(defer.unavailable_reason.as_deref(), Some(HOME_DEFER_TEXT));
    assert_eq!(
        setting(&state, WuSettingId::ExcludeDrivers)
            .caveat
            .as_deref(),
        Some(HOME_DRIVERS_CAVEAT)
    );
    let drivers = plan_or_apply_wu_with(
        &sb.layout,
        HOME,
        &WuChange::ExcludeDrivers(true),
        true,
        never,
    )
    .unwrap();
    assert_eq!(drivers.warnings, vec![HOME_DRIVERS_CAVEAT.to_string()]);
}

#[test]
fn organization_policies_refuse_pause_and_active_hours() {
    let sb = Sandbox::new("Policy");
    sb.seed(&sb.layout.policy, POLICY_NO_PAUSE, RegValue::Dword(1));
    sb.seed(&sb.layout.policy, POLICY_SET_ACTIVE, RegValue::Dword(1));
    sb.seed(&sb.layout.policy, ACTIVE_START, RegValue::Dword(9));
    sb.seed(&sb.layout.policy, ACTIVE_END, RegValue::Dword(17));

    let err = plan_or_apply_wu_with(&sb.layout, PRO, &WuChange::Pause { days: 7 }, false, never)
        .unwrap_err();
    assert_eq!(err.to_string(), PAUSE_POLICY_TEXT);
    plan_or_apply_wu_with(&sb.layout, PRO, &WuChange::Resume, true, never).unwrap();
    for change in [
        WuChange::ActiveHours { start: 8, end: 17 },
        WuChange::AutomaticActiveHours,
    ] {
        let err = plan_or_apply_wu_with(&sb.layout, PRO, &change, false, never).unwrap_err();
        assert_eq!(err.to_string(), ACTIVE_HOURS_POLICY_TEXT);
    }

    let state = sb.state(PRO);
    let pause = setting(&state, WuSettingId::Pause);
    assert!(!pause.available);
    assert_eq!(pause.unavailable_reason.as_deref(), Some(PAUSE_POLICY_TEXT));
    let hours = setting(&state, WuSettingId::ActiveHours);
    assert!(!hours.available);
    assert_eq!(
        hours.unavailable_reason.as_deref(),
        Some("Active hours are set by a policy on this PC (09:00–17:00).")
    );
    assert_eq!(
        hours.value,
        WuValue::ActiveHours {
            automatic: false,
            start: Some(9),
            end: Some(17),
            policy: true
        }
    );
}

#[test]
fn a_dry_run_opens_no_session_and_writes_nothing() {
    let sb = Sandbox::new("DryRun");
    sb.seed(&sb.layout.ux, ACTIVE_START, RegValue::Dword(9));
    let sessions = sb.journal.sessions().unwrap().len();
    let report = plan_or_apply_wu_with(
        &sb.layout,
        PRO,
        &WuChange::ActiveHours { start: 8, end: 17 },
        true,
        never,
    )
    .unwrap();
    assert!(report.dry_run);
    assert_eq!(report.session_id, None);
    assert_eq!(report.writes.len(), 3);
    assert_eq!(report.writes[0].before.as_deref(), Some("0x00000009 (9)"));
    assert_eq!(report.writes[0].after.as_deref(), Some("0x00000008 (8)"));
    assert_eq!(report.writes[1].before, None);
    assert!(report.writes.iter().all(|w| w.outcome.is_none()));
    assert_eq!(sb.journal.sessions().unwrap().len(), sessions);
    assert_eq!(
        sb.value(&sb.layout.ux, ACTIVE_START),
        Some(RegValue::Dword(9))
    );
    assert_eq!(sb.value(&sb.layout.ux, ACTIVE_END), None);
    assert!(sb.journal.active_registry().unwrap().is_empty());
}

#[test]
fn a_refused_session_writes_nothing() {
    let sb = Sandbox::new("NotElevated");
    let err = plan_or_apply_wu_with(
        &sb.layout,
        PRO,
        &WuChange::RestartNotify(true),
        false,
        || Err(Error::NotElevated),
    )
    .unwrap_err();
    assert!(matches!(err, Error::NotElevated));
    assert!(!exists(Hive::CurrentUser, &sb.layout.ux).unwrap());
    assert!(sb.journal.active_registry().unwrap().is_empty());
}

#[test]
fn state_reads_every_value_and_policy_note() {
    let sb = Sandbox::new("Values");
    let ux = sb.layout.ux.clone();
    let policy = sb.layout.policy.clone();
    sb.seed(&ux, PAUSE_START, sz("2026-09-20T00:00:00Z"));
    sb.seed(&ux, PAUSE_QUALITY_END, sz("2026-09-30T00:00:00Z"));
    sb.seed(&ux, PAUSE_EXPIRY, sz("2026-10-01T00:00:00Z"));
    sb.seed(&ux, ACTIVE_START, RegValue::Dword(8));
    sb.seed(&ux, ACTIVE_END, RegValue::Dword(17));
    sb.seed(&ux, SMART_ACTIVE, RegValue::Dword(0));
    sb.seed(&ux, RESTART_NOTIFY, RegValue::Dword(1));
    sb.seed(&policy, EXCLUDE_DRIVERS, RegValue::Dword(1));
    sb.seed(&policy, DEFER_ON, RegValue::Dword(1));
    sb.seed(&policy, DEFER_DAYS, RegValue::Dword(90));
    sb.seed(&policy, POLICY_WU_SERVER, sz("http://wsus.contoso.test"));
    sb.seed(&policy, POLICY_NO_ACCESS, RegValue::Dword(1));
    sb.seed(&sb.layout.policy_au, AU_NO_AUTO_UPDATE, RegValue::Dword(1));
    Key::create(Hive::CurrentUser, &sb.layout.reboot_required).unwrap();

    let state = sb.state(PRO);
    assert!(state.restart_pending);
    assert_eq!(
        state.managed,
        vec![
            WSUS_NOTE.to_string(),
            NO_AUTO_UPDATE_NOTE.to_string(),
            NO_ACCESS_NOTE.to_string()
        ]
    );
    assert_eq!(
        setting(&state, WuSettingId::Pause).value,
        WuValue::Pause {
            paused: true,
            until: Some("2026-10-01T00:00:00Z".into()),
            started: Some("2026-09-20T00:00:00Z".into()),
            expired: false
        }
    );
    assert_eq!(
        setting(&state, WuSettingId::ActiveHours).value,
        WuValue::ActiveHours {
            automatic: false,
            start: Some(8),
            end: Some(17),
            policy: false
        }
    );
    assert_eq!(
        setting(&state, WuSettingId::ExcludeDrivers).value,
        WuValue::Switch { on: true }
    );
    assert_eq!(
        setting(&state, WuSettingId::DeferFeature).value,
        WuValue::Defer { days: Some(90) }
    );
    assert_eq!(
        setting(&state, WuSettingId::RestartNotify).value,
        WuValue::Switch { on: true }
    );
    assert!(state.settings.iter().all(|s| !s.by_cairn));

    // A period without DeferFeatureUpdates = 1 is no delay; smart hours are automatic.
    sb.seed(&policy, DEFER_ON, RegValue::Dword(0));
    sb.seed(&ux, SMART_ACTIVE, RegValue::Dword(1));
    let state = sb.state(PRO);
    assert_eq!(
        setting(&state, WuSettingId::DeferFeature).value,
        WuValue::Defer { days: None }
    );
    assert!(matches!(
        setting(&state, WuSettingId::ActiveHours).value,
        WuValue::ActiveHours {
            automatic: true,
            ..
        }
    ));
}

#[test]
fn an_ended_pause_reads_as_expired() {
    let sb = Sandbox::new("Expired");
    sb.seed(&sb.layout.ux, PAUSE_EXPIRY, sz("2026-09-01T00:00:00Z"));
    let state = sb.state(PRO);
    assert_eq!(
        setting(&state, WuSettingId::Pause).value,
        WuValue::Pause {
            paused: false,
            until: Some("2026-09-01T00:00:00Z".into()),
            started: None,
            expired: true
        }
    );
}

#[test]
fn by_cairn_and_differs_follow_the_journal() {
    let sb = Sandbox::new("Marks");
    sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    let state = sb.state(PRO);
    let pause = setting(&state, WuSettingId::Pause);
    assert!(pause.by_cairn && pause.differs);
    let notify = setting(&state, WuSettingId::RestartNotify);
    assert!(!notify.by_cairn && !notify.differs);

    // Settings' Resume deletes the values; the records stay, and Undo would change nothing.
    let key = Key::open(Hive::CurrentUser, &sb.layout.ux, true)
        .unwrap()
        .unwrap();
    for (_, name) in WuSettingId::Pause.values() {
        key.delete_value(name).unwrap();
    }
    drop(key);
    let state = sb.state(PRO);
    let pause = setting(&state, WuSettingId::Pause);
    assert!(pause.by_cairn && !pause.differs);
}

#[test]
fn a_disabled_service_is_reported_with_each_change() {
    let sb = Sandbox::new("Service");
    let env = WuEnv {
        service_start: disabled,
        ..PRO
    };
    assert_eq!(sb.state(env).service, ServiceState::Disabled);
    let report =
        plan_or_apply_wu_with(&sb.layout, env, &WuChange::RestartNotify(true), true, never)
            .unwrap();
    assert_eq!(report.warnings, vec![SERVICE_DISABLED_WARNING.to_string()]);

    let err: Result<Option<StartType>> = Err(Error::Other("no".into()));
    assert_eq!(ServiceState::from_start(&err), ServiceState::Unknown);
    assert_eq!(ServiceState::from_start(&Ok(None)), ServiceState::Missing);
    assert_eq!(
        ServiceState::from_start(&Ok(Some(StartType::Automatic))),
        ServiceState::Automatic
    );
}

#[test]
fn an_unreadable_edition_is_treated_as_pro_with_a_warning() {
    let sb = Sandbox::new("Edition");
    let env = WuEnv {
        edition: unknown_edition,
        ..PRO
    };
    let state = sb.state(env);
    assert!(!state.edition.home);
    assert_eq!(state.warnings, vec![EDITION_UNKNOWN_WARNING.to_string()]);
    assert!(setting(&state, WuSettingId::DeferFeature).available);
}

#[test]
fn editions_are_read_from_their_parts() {
    let e = Edition::from_parts(
        Some("CoreSingleLanguage"),
        Some("Windows 10 Home Single Language"),
        Some(" 25H2 "),
        Some("26200"),
        Some(9457),
    );
    assert!(e.home);
    assert_eq!(e.name, "Windows 11 Home Single Language");
    assert_eq!(e.version.as_deref(), Some("25H2"));
    assert_eq!(e.build, "26200.9457");
    let e = Edition::from_parts(Some("Professional"), None, Some(""), Some("26100"), None);
    assert!(!e.home);
    assert_eq!(e.name, "Windows 11 Pro");
    assert_eq!(e.version, None);
    assert_eq!(e.build, "26100");
    let e = unknown_edition();
    assert!(!e.home && e.id.is_empty() && e.build.is_empty());
}

#[test]
fn the_catalog_names_every_setting_with_journal_target_strings() {
    let catalog = wu_catalog();
    let ids: Vec<&str> = catalog.iter().map(|e| e.id).collect();
    assert_eq!(
        ids,
        [
            "wu.pause",
            "wu.active_hours",
            "wu.exclude_drivers",
            "wu.defer_feature",
            "wu.restart_notify"
        ]
    );
    assert_eq!(catalog[0].title, "Windows Update: pause");
    assert_eq!(catalog[0].targets.len(), 6);
    let layout = WuLayout::system();
    for (entry, id) in catalog.iter().zip(WuSettingId::ALL) {
        let recorded: Vec<String> = layout
            .targets(id)
            .into_iter()
            .map(|t| {
                RegistryRecord {
                    id: 1,
                    session_id: 1,
                    recorded_at: String::new(),
                    hive: t.hive,
                    key_path: t.key_path,
                    value_name: t.value_name,
                    key_existed: true,
                    value_existed: false,
                    original: None,
                    active: true,
                    reverted_at: None,
                    created_root: None,
                }
                .target()
            })
            .collect();
        assert_eq!(entry.targets, recorded, "{}", entry.id);
        assert_eq!(entry.title, id.history_title());
    }
    assert_eq!(
        catalog[4].targets,
        [format!(r"HKLM\{UX_SETTINGS}\{RESTART_NOTIFY}")]
    );
}

#[test]
fn no_catalog_tweak_writes_a_windows_update_value() {
    let layout = WuLayout::system();
    let targets: Vec<RegistryTarget> = WuSettingId::ALL
        .into_iter()
        .flat_map(|id| layout.targets(id))
        .collect();
    for tweak in TWEAKS {
        for action in tweak.actions {
            let Action::Registry(a) = action else {
                continue;
            };
            for t in &targets {
                assert!(
                    !(a.hive == t.hive
                        && a.path.eq_ignore_ascii_case(&t.key_path)
                        && a.name.eq_ignore_ascii_case(&t.value_name)),
                    "{} writes {}",
                    tweak.id,
                    target_text(t)
                );
            }
        }
    }
}

#[test]
fn times_show_in_local_time() {
    let utc = parse_time("2026-10-09T10:00:00Z").unwrap();
    let expected = utc
        .with_timezone(&Local)
        .format("%a %d %b %Y, %H:%M")
        .to_string();
    assert_eq!(local_time_text("2026-10-09T10:00:00Z"), expected);
    assert_eq!(local_time_text("soon"), "soon");
    assert_eq!(format_time(utc), "2026-10-09T10:00:00Z");
}

#[test]
fn values_read_as_optctl_prints_them() {
    let pause = |paused: bool, until: Option<&str>, expired: bool| WuValue::Pause {
        paused,
        until: until.map(str::to_string),
        started: None,
        expired,
    };
    // `optctl updates wu` prints the value after the title "Pause updates": updates that are
    // not paused must not read "on".
    let empty = Sandbox::new("Text");
    let state = empty.state(PRO);
    let pause_row = setting(&state, WuSettingId::Pause);
    let row = format!("{:<32}{}", pause_row.title, value_text(&pause_row.value));
    assert_eq!(row, "Pause updates                   updates are on");
    assert_eq!(value_text(&pause(false, None, false)), "updates are on");
    assert_eq!(
        value_text(&pause(false, Some("2026-09-01T00:00:00Z"), true)),
        format!(
            "updates are on (the last pause ended on {})",
            local_time_text("2026-09-01T00:00:00Z")
        )
    );
    assert_eq!(
        value_text(&pause(true, Some("2026-10-09T10:00:00Z"), false)),
        format!("paused until {}", local_time_text("2026-10-09T10:00:00Z"))
    );
    let hours = |automatic: bool, start: Option<u32>, end: Option<u32>, policy: bool| {
        WuValue::ActiveHours {
            automatic,
            start,
            end,
            policy,
        }
    };
    assert_eq!(
        value_text(&hours(false, Some(8), Some(17), false)),
        "08:00–17:00"
    );
    assert_eq!(
        value_text(&hours(false, Some(8), Some(17), true)),
        "08:00–17:00 (set by a policy)"
    );
    assert_eq!(
        value_text(&hours(true, Some(8), Some(17), false)),
        "adjusted automatically"
    );
    assert_eq!(value_text(&hours(false, None, None, true)), "unknown");
    assert_eq!(value_text(&WuValue::Switch { on: true }), "on");
    assert_eq!(value_text(&WuValue::Switch { on: false }), "off");
    assert_eq!(value_text(&WuValue::Defer { days: Some(90) }), "90 days");
    assert_eq!(value_text(&WuValue::Defer { days: None }), "no delay");
}

#[test]
fn optctl_wu_set_prints_a_dry_run_only_when_nothing_is_changed() {
    let sb = Sandbox::new("Cli");
    let on = WuChange::RestartNotify(true);
    let target = format!(r"HKCU\{}\{RESTART_NOTIFY}", sb.layout.ux);
    let planned = format!("  {target}: (absent) → 0x00000001 (1)");
    // With --dry-run, without --yes, or with both: the dry run, and no session.
    for (dry_run, yes) in [(true, false), (false, false), (true, true)] {
        let lines = cli_set_lines_with(&sb.layout, PRO, &on, dry_run, yes, never).unwrap();
        assert_eq!(
            lines,
            ["dry run: nothing was changed".to_string(), planned.clone()],
            "--dry-run {dry_run}, --yes {yes}"
        );
    }
    assert!(sb.journal.sessions().unwrap().is_empty());
    assert_eq!(sb.value(&sb.layout.ux, RESTART_NOTIFY), None);

    // --yes alone: the change is made and listed without a dry run before it.
    let lines = cli_set_lines_with(&sb.layout, PRO, &on, false, true, || Ok(sb.safety())).unwrap();
    let session = sb.journal.sessions().unwrap()[0].id;
    assert_eq!(
        lines,
        [
            format!("{planned}  [applied]"),
            format!("journal session     {session}")
        ]
    );
    assert_eq!(
        sb.value(&sb.layout.ux, RESTART_NOTIFY),
        Some(RegValue::Dword(1))
    );
    let off = WuChange::RestartNotify(false);
    let lines = cli_set_lines_with(&sb.layout, PRO, &off, false, true, || Ok(sb.safety())).unwrap();
    let session = sb.journal.sessions().unwrap()[0].id;
    assert_eq!(
        lines,
        [
            format!("  {target}: 0x00000001 (1) → (deleted)  [applied]"),
            format!("journal session     {session}")
        ]
    );

    // Warnings follow the writes.
    let drivers = WuChange::ExcludeDrivers(true);
    let lines = cli_set_lines_with(&sb.layout, HOME, &drivers, false, false, never).unwrap();
    assert_eq!(
        lines,
        [
            "dry run: nothing was changed".to_string(),
            format!(
                r"  HKCU\{}\{EXCLUDE_DRIVERS}: (absent) → 0x00000001 (1)",
                sb.layout.policy
            ),
            format!("warning: {HOME_DRIVERS_CAVEAT}")
        ]
    );
}

#[test]
fn setting_ids_parse_and_serialize() {
    for id in WuSettingId::ALL {
        assert_eq!(WuSettingId::parse(id.as_str()), Some(id));
        assert_eq!(serde_json::to_value(id).unwrap(), id.as_str());
    }
    assert_eq!(
        WuSettingId::parse(" Active-Hours "),
        Some(WuSettingId::ActiveHours)
    );
    assert_eq!(
        WuSettingId::parse("RESTART_NOTIFY"),
        Some(WuSettingId::RestartNotify)
    );
    assert_eq!(WuSettingId::parse("pauses"), None);
    assert_eq!(
        WuSettingId::ExcludeDrivers.title(),
        "Skip drivers in Windows Update"
    );
}

#[test]
fn command_line_forms_parse() {
    let parse = |s: &str, v: &str| WuChange::parse_cli(s, v);
    assert_eq!(parse("pause", "14").unwrap(), WuChange::Pause { days: 14 });
    assert_eq!(parse("pause", "OFF").unwrap(), WuChange::Resume);
    assert_eq!(
        parse("active-hours", "8-17").unwrap(),
        WuChange::ActiveHours { start: 8, end: 17 }
    );
    assert_eq!(
        parse("active_hours", "auto").unwrap(),
        WuChange::AutomaticActiveHours
    );
    assert_eq!(
        parse("exclude-drivers", "on").unwrap(),
        WuChange::ExcludeDrivers(true)
    );
    assert_eq!(
        parse("defer-feature", "90").unwrap(),
        WuChange::DeferFeature(Some(90))
    );
    assert_eq!(
        parse("defer-feature", "off").unwrap(),
        WuChange::DeferFeature(None)
    );
    assert_eq!(
        parse("restart-notify", "off").unwrap(),
        WuChange::RestartNotify(false)
    );
    for (s, v) in [
        ("pause", "99"),
        ("pause", "soon"),
        ("active-hours", "8"),
        ("active-hours", "8-3"),
        ("restart-notify", "maybe"),
        ("bogus", "1"),
    ] {
        assert!(parse(s, v).is_err(), "{s} {v}");
    }
}

#[test]
fn changes_name_their_setting_and_session() {
    assert_eq!(
        WuChange::Pause { days: 14 }.label(),
        "windows update: pause 14 days"
    );
    assert_eq!(
        WuChange::ActiveHours { start: 8, end: 17 }.label(),
        "windows update: active hours 08:00-17:00"
    );
    assert_eq!(
        WuChange::DeferFeature(None).label(),
        "windows update: delay feature updates off"
    );
    assert_eq!(WuChange::Resume.setting(), WuSettingId::Pause);
    assert_eq!(
        WuChange::AutomaticActiveHours.setting(),
        WuSettingId::ActiveHours
    );
    assert_eq!(active_span(22, 6), 8);
    assert_eq!(active_span(8, 2), 18);
}

// ───────────────────────────── Profiles ─────────────────────────────

fn want_all() -> WindowsUpdateChoice {
    WindowsUpdateChoice {
        active_hours: Some(ActiveHoursChoice {
            automatic: false,
            start: Some(8),
            end: Some(20),
        }),
        restart_notify: Some(true),
        exclude_drivers: true,
        defer_feature_days: Some(30),
    }
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

#[test]
fn profile_plan_has_one_row_per_field() {
    let sb = Sandbox::new("ProfilePlan");
    sb.seed(&sb.layout.ux, RESTART_NOTIFY, RegValue::Dword(1));
    let steps = profile_plan_with(&sb.layout, PRO, &sb.journal, &want_all()).unwrap();
    let rows: Vec<(&str, StepStatus)> = steps.iter().map(|s| (s.key.as_str(), s.status)).collect();
    assert_eq!(
        rows,
        [
            (KEY_ACTIVE_HOURS, StepStatus::Change),
            (KEY_RESTART_NOTIFY, StepStatus::Already),
            (KEY_EXCLUDE_DRIVERS, StepStatus::Change),
            (KEY_DEFER_FEATURE, StepStatus::Change),
        ]
    );
    assert_eq!(steps[0].detail, "08:00 to 20:00");
    assert_eq!(steps[0].title, "Active hours");
    assert_eq!(steps[1].detail, "On");
    assert_eq!(steps[2].detail, DRIVERS_OFF_TEXT);
    assert_eq!(steps[3].detail, "30 days");
    assert!(steps[..3].iter().all(|s| s.caution.is_none()));
    // Planning changes nothing.
    assert!(sb.journal.sessions().unwrap().is_empty());
    assert_eq!(sb.value(&sb.layout.ux, ACTIVE_START), None);
}

#[test]
fn profile_defer_feature_row_is_opt_in() {
    let sb = Sandbox::new("ProfileDefer");
    let want = WindowsUpdateChoice {
        defer_feature_days: Some(30),
        ..Default::default()
    };
    let steps = profile_plan_with(&sb.layout, PRO, &sb.journal, &want).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, StepStatus::Change);
    assert_eq!(
        steps[0].caution.as_deref(),
        Some(
            "Windows Update waits 30 days after each new Windows version is released before \
             offering it. This is a policy, so Windows Settings will say some settings are \
             managed by your organization."
        )
    );

    let steps = profile_plan_with(&sb.layout, HOME, &sb.journal, &want).unwrap();
    assert_eq!(steps[0].status, StepStatus::Skipped);
    assert_eq!(steps[0].reason, Some(StepReason::Edition));
    assert_eq!(steps[0].detail, HOME_DEFER_TEXT);
    assert_eq!(steps[0].caution, None);
}

#[test]
fn profile_active_hours_set_by_a_policy_are_skipped() {
    let sb = Sandbox::new("ProfilePolicy");
    sb.seed(&sb.layout.policy, POLICY_SET_ACTIVE, RegValue::Dword(1));
    let want = WindowsUpdateChoice {
        active_hours: Some(ActiveHoursChoice {
            automatic: true,
            start: None,
            end: None,
        }),
        ..Default::default()
    };
    let steps = profile_plan_with(&sb.layout, PRO, &sb.journal, &want).unwrap();
    assert_eq!(steps[0].status, StepStatus::Skipped);
    assert_eq!(steps[0].reason, Some(StepReason::CannotChange));
    assert_eq!(steps[0].detail, ACTIVE_HOURS_POLICY_TEXT);
}

#[test]
fn profile_apply_writes_the_chosen_keys_and_returns_their_targets() {
    let sb = Sandbox::new("ProfileApply");
    let safety = sb.safety();
    let chosen = keys(&[
        KEY_ACTIVE_HOURS,
        KEY_EXCLUDE_DRIVERS,
        "windows_update:bogus",
    ]);
    let want = WindowsUpdateChoice {
        defer_feature_days: None,
        ..want_all()
    };
    let (results, filter) =
        profile_apply_in_with(&sb.layout, PRO, &safety, &want, &chosen).unwrap();
    let outcomes: Vec<(&str, StepOutcome)> = results
        .iter()
        .map(|r| (r.key.as_str(), r.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            (KEY_ACTIVE_HOURS, StepOutcome::Applied),
            (KEY_EXCLUDE_DRIVERS, StepOutcome::Applied),
            ("windows_update:bogus", StepOutcome::Skipped),
        ]
    );
    assert_eq!(results[2].details, vec![UNKNOWN_KEY.to_string()]);
    let mut expected = sb.layout.targets(WuSettingId::ActiveHours);
    expected.extend(sb.layout.targets(WuSettingId::ExcludeDrivers));
    assert_eq!(filter.registry, expected);
    assert!(filter.task_definitions.is_empty());
    assert_eq!(
        sb.value(&sb.layout.ux, ACTIVE_START),
        Some(RegValue::Dword(8))
    );
    assert_eq!(
        sb.value(&sb.layout.ux, SMART_ACTIVE),
        Some(RegValue::Dword(0))
    );
    assert_eq!(
        sb.value(&sb.layout.policy, EXCLUDE_DRIVERS),
        Some(RegValue::Dword(1))
    );
    // Not chosen: untouched.
    assert_eq!(sb.value(&sb.layout.ux, RESTART_NOTIFY), None);

    // A key the profile does not set is skipped; applying again changes nothing.
    let (results, filter) = profile_apply_in_with(
        &sb.layout,
        PRO,
        &safety,
        &want,
        &keys(&[KEY_ACTIVE_HOURS, KEY_DEFER_FEATURE]),
    )
    .unwrap();
    assert_eq!(results[0].outcome, StepOutcome::AlreadySet);
    assert_eq!(results[1].outcome, StepOutcome::Skipped);
    assert_eq!(results[1].details, vec![NOT_IN_PROFILE.to_string()]);
    assert!(filter.registry.is_empty());
    drop(safety);

    let undo = RollbackFilter {
        registry: expected,
        ..Default::default()
    };
    rollback_filtered(&sb.journal, &undo, false).unwrap();
    assert!(!exists(Hive::CurrentUser, &sb.layout.ux).unwrap());
    assert!(!exists(Hive::CurrentUser, &sb.layout.policy).unwrap());
}

#[test]
fn profile_apply_skips_what_the_edition_refuses() {
    let sb = Sandbox::new("ProfileHome");
    let safety = sb.safety();
    let (results, filter) = profile_apply_in_with(
        &sb.layout,
        HOME,
        &safety,
        &want_all(),
        &keys(&[KEY_DEFER_FEATURE]),
    )
    .unwrap();
    assert_eq!(results[0].outcome, StepOutcome::Skipped);
    assert_eq!(results[0].details, vec![HOME_DEFER_TEXT.to_string()]);
    assert!(filter.registry.is_empty());
    assert!(!exists(Hive::CurrentUser, &sb.layout.policy).unwrap());
    assert!(sb.journal.active_registry().unwrap().is_empty());
}

#[test]
fn profile_current_lists_what_cairn_changed_that_still_differs() {
    let sb = Sandbox::new("ProfileCurrent");
    sb.apply(PRO, &WuChange::ActiveHours { start: 8, end: 20 })
        .unwrap();
    sb.apply(PRO, &WuChange::RestartNotify(true)).unwrap();
    sb.apply(PRO, &WuChange::Pause { days: 7 }).unwrap();
    sb.seed(&sb.layout.policy, EXCLUDE_DRIVERS, RegValue::Dword(1));
    let choice = choice_from_state(&sb.state(PRO));
    assert_eq!(
        choice,
        WindowsUpdateChoice {
            active_hours: Some(ActiveHoursChoice {
                automatic: false,
                start: Some(8),
                end: Some(20),
            }),
            restart_notify: Some(true),
            exclude_drivers: false,
            defer_feature_days: None,
        }
    );

    sb.apply(PRO, &WuChange::DeferFeature(Some(60))).unwrap();
    sb.apply(PRO, &WuChange::AutomaticActiveHours).unwrap();
    let choice = choice_from_state(&sb.state(PRO));
    assert_eq!(choice.defer_feature_days, Some(60));
    assert_eq!(
        choice.active_hours,
        Some(ActiveHoursChoice {
            automatic: true,
            start: None,
            end: None
        })
    );
}
