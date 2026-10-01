//! End-to-end checks of the safety layer against HKCU (no elevation required) and a
//! read-only look at the Service Control Manager.
//!
//! Service, power and Appx records are inserted into the journal directly and only ever
//! rolled back in dry runs, or in paths that return before touching the system. Scheduled
//! task and DNS records are rolled back only in dry runs, or (scheduled tasks only) for a
//! task path that does not exist. Task definition records are rolled back only in dry runs:
//! a real rollback would delete through Task Scheduler.

use std::path::Path;
use std::sync::Arc;

use optimizer_core::debloat::appx;
use optimizer_core::debloat::RestartNeed;
use optimizer_core::network::IpFamily;
use optimizer_core::safety::rollback::{RegistryTarget, RollbackFilter};
use optimizer_core::safety::state_log::{
    Journal, NewAppxRecord, NewDnsRecord, NewPowerRecord, NewRegistryRecord,
    NewScheduledTaskRecord, NewServiceRecord, NewTaskDefinitionRecord,
};
use optimizer_core::safety::{
    rollback, MutationOutcome, RestorePointPolicy, Safety, SafetyOptions,
};
use optimizer_core::win::registry::{delete_key_if_empty, exists, read_value, Hive, Key, RegValue};
use optimizer_core::win::scm::{Scm, StartType, READ_ACCESS};
use optimizer_core::{is_elevated, Error};

const ROOT: &str = r"Software\PCOptimizer\SelfTest";

/// Family of a package that is never installed; its install location never exists.
const FAKE_FAMILY: &str = "PCOptimizer.SelfTest_0000000000000";
const FAKE_FULL_NAME: &str = "PCOptimizer.SelfTest_1.0.0.0_x64__0000000000000";
const FAKE_SERVICE: &str = "PCOptimizerNoSuchService";
const PREVIOUS_SCHEME: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
const TARGET_SCHEME: &str = "00000000-0000-0000-0000-00000000c0de";
/// A scheduled task that never exists.
const FAKE_TASK: &str = r"\PCOptimizerSelfTest\NoSuchTask";
/// An interface GUID no adapter has.
const FAKE_INTERFACE: &str = "{00000000-0000-0000-0000-00000000c0de}";
const FAKE_ADAPTER: &str = "Cairn self-test";
/// A task definition that never exists.
const FAKE_DEFINITION: &str = r"\PCOptimizerSelfTest\NoSuchMaintenance";

fn journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let journal = Journal::open(dir.path().join("journal.db")).expect("open journal");
    (dir, Arc::new(journal))
}

fn options(label: &str) -> SafetyOptions {
    SafetyOptions {
        label: label.to_string(),
        restore_point: RestorePointPolicy::Skip,
        require_elevation: false,
        ..Default::default()
    }
}

fn clean(path: &str) {
    if let Ok(Some(key)) = Key::open(Hive::CurrentUser, path, true) {
        let _ = key.delete_value("Probe");
        let _ = key.delete_value("Marker");
    }
    let _ = delete_key_if_empty(Hive::CurrentUser, path);
}

fn hkcu_target(key_path: &str, value_name: &str) -> RegistryTarget {
    RegistryTarget {
        hive: Hive::CurrentUser,
        key_path: key_path.to_string(),
        value_name: value_name.to_string(),
    }
}

/// Sets `Probe` = 1 and `Marker` = 2 under `path` in one journaled session.
fn set_probe_and_marker(journal: &Arc<Journal>, path: &str, label: &str) {
    let safety = Safety::begin(journal.clone(), options(label)).unwrap();
    for (name, value) in [("Probe", 1), ("Marker", 2)] {
        let outcome = safety
            .set_registry_value(Hive::CurrentUser, path, name, &RegValue::Dword(value))
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
    }
}

fn assert_probe_and_marker(path: &str, probe: Option<u32>, marker: Option<u32>) {
    assert_eq!(
        read_value(Hive::CurrentUser, path, "Probe").unwrap(),
        probe.map(RegValue::Dword)
    );
    assert_eq!(
        read_value(Hive::CurrentUser, path, "Marker").unwrap(),
        marker.map(RegValue::Dword)
    );
}

#[test]
fn created_key_is_removed_when_its_values_are_reverted_one_at_a_time() {
    let path = format!(r"{ROOT}\CreatedKeyPartial");
    clean(&path);
    assert!(!exists(Hive::CurrentUser, &path).unwrap());

    let (_dir, journal) = journal();
    // Probe is recorded with key_existed = false; Marker, written after the key exists,
    // with key_existed = true.
    set_probe_and_marker(&journal, &path, "created-key-partial");

    let probe_only = RollbackFilter {
        registry: vec![hkcu_target(&path, "Probe")],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &probe_only, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_probe_and_marker(&path, None, Some(2));
    assert!(
        exists(Hive::CurrentUser, &path).unwrap(),
        "key still holds Marker"
    );

    let marker_only = RollbackFilter {
        registry: vec![hkcu_target(&path, "Marker")],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &marker_only, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert!(
        !exists(Hive::CurrentUser, &path).unwrap(),
        "created key left behind empty"
    );
    assert_eq!(journal.summary().unwrap().registry_active, 0);
}

/// Reverts every remaining HKCU record of a test journal and removes the test key.
fn rollback_and_clean(journal: &Journal, path: &str) {
    let report = rollback::rollback_journal(journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(journal.summary().unwrap().registry_active, 0);
    clean(path);
}

/// Journals a package whose install location does not exist, without touching the system.
fn record_fake_appx(journal: &Journal, dir: &Path) {
    let install_location = dir.join("missing-package");
    assert!(!install_location.exists());
    let session = journal.begin_session("appx-selftest", "test").unwrap();
    let captured = journal
        .record_appx(
            session,
            &NewAppxRecord {
                package_full_name: FAKE_FULL_NAME.to_string(),
                package_family: FAKE_FAMILY.to_string(),
                install_location: install_location.display().to_string(),
                all_users: false,
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

/// Journals a service that does not exist, without touching the Service Control Manager.
fn record_fake_service(journal: &Journal) {
    let session = journal.begin_session("service-selftest", "test").unwrap();
    let captured = journal
        .record_service(
            session,
            &NewServiceRecord {
                name: FAKE_SERVICE.to_string(),
                display_name: "Cairn self-test".to_string(),
                start_type: StartType::Manual,
                delayed_auto_start: false,
                was_running: false,
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

/// Journals a change of a scheduled task that does not exist, without touching Task
/// Scheduler. Such a record is only rolled back in a dry run, or for real on a journal that
/// holds nothing else, where the missing task is marked reverted without a write.
fn record_fake_scheduled_task(journal: &Journal) {
    let session = journal.begin_session("task-selftest", "test").unwrap();
    let captured = journal
        .record_scheduled_task(
            session,
            &NewScheduledTaskRecord {
                path: FAKE_TASK.to_string(),
                was_enabled: true,
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

/// Journals automatic IPv4 DNS servers for an interface no adapter has, without touching
/// the network configuration. Such a record must only ever be rolled back in a dry run.
fn record_fake_dns(journal: &Journal) {
    let session = journal.begin_session("dns-selftest", "test").unwrap();
    let captured = journal
        .record_dns(
            session,
            &NewDnsRecord {
                interface_guid: FAKE_INTERFACE.to_string(),
                family: IpFamily::Ipv4,
                adapter_name: FAKE_ADAPTER.to_string(),
                previous_servers: String::new(),
                target_servers: "1.1.1.1,1.0.0.1".to_string(),
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

/// Journals a scheduled task Cairn registered, without touching Task Scheduler. Such a record
/// must only ever be rolled back in a dry run.
fn record_fake_task_definition(journal: &Journal) {
    let session = journal
        .begin_session("definition-selftest", "test")
        .unwrap();
    let captured = journal
        .record_task_definition(
            session,
            &NewTaskDefinitionRecord {
                path: FAKE_DEFINITION.to_string(),
                purpose: "maintenance".to_string(),
                folder_created: false,
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

/// Journals a power scheme change without changing the active scheme. Such a record must
/// only ever be rolled back in a dry run.
fn record_fake_power(journal: &Journal) {
    let session = journal.begin_session("power-selftest", "test").unwrap();
    let captured = journal
        .record_power(
            session,
            &NewPowerRecord {
                previous_scheme: PREVIOUS_SCHEME.to_string(),
                target_scheme: TARGET_SCHEME.to_string(),
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();
}

#[test]
fn created_value_is_removed_on_rollback() {
    let path = format!(r"{ROOT}\Created");
    clean(&path);
    assert!(!exists(Hive::CurrentUser, &path).unwrap());

    let (_dir, journal) = journal();
    {
        let safety = Safety::begin(journal.clone(), options("created")).unwrap();
        let outcome = safety
            .set_registry_value(Hive::CurrentUser, &path, "Probe", &RegValue::Dword(1))
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
        assert_eq!(
            read_value(Hive::CurrentUser, &path, "Probe").unwrap(),
            Some(RegValue::Dword(1))
        );

        // A second write in the same session must not disturb the "absent" baseline.
        safety
            .set_registry_value(Hive::CurrentUser, &path, "Probe", &RegValue::Dword(2))
            .unwrap();
        let again = safety
            .set_registry_value(Hive::CurrentUser, &path, "Probe", &RegValue::Dword(2))
            .unwrap();
        assert_eq!(again, MutationOutcome::AlreadyInDesiredState);
    }

    let summary = journal.summary().unwrap();
    assert_eq!(summary.registry_active, 1);
    assert_eq!(summary.sessions, 1);

    let plan = rollback::rollback_journal(&journal, true).unwrap();
    assert!(plan.dry_run);
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(
        read_value(Hive::CurrentUser, &path, "Probe").unwrap(),
        Some(RegValue::Dword(2))
    );

    let report = rollback::rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 1);
    assert!(
        !exists(Hive::CurrentUser, &path).unwrap(),
        "created key should be removed"
    );
    assert_eq!(journal.summary().unwrap().registry_active, 0);
}

#[test]
fn preexisting_value_is_restored_byte_for_byte() {
    let path = format!(r"{ROOT}\Existing");
    clean(&path);
    let (key, _) = Key::create(Hive::CurrentUser, &path).unwrap();
    key.set(
        "Probe",
        &RegValue::ExpandSz("%SystemRoot%\\original".into()),
    )
    .unwrap();
    key.set("Marker", &RegValue::Sz("untouched".into()))
        .unwrap();
    drop(key);

    let (_dir, journal) = journal();
    {
        let safety = Safety::begin(journal.clone(), options("existing")).unwrap();
        safety
            .set_registry_value(Hive::CurrentUser, &path, "Probe", &RegValue::Dword(7))
            .unwrap();
        assert_eq!(
            read_value(Hive::CurrentUser, &path, "Probe").unwrap(),
            Some(RegValue::Dword(7))
        );
    }

    let report = rollback::rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_restored, 1);
    assert_eq!(
        read_value(Hive::CurrentUser, &path, "Probe").unwrap(),
        Some(RegValue::ExpandSz("%SystemRoot%\\original".into()))
    );
    assert_eq!(
        read_value(Hive::CurrentUser, &path, "Marker").unwrap(),
        Some(RegValue::Sz("untouched".into()))
    );
    assert!(
        exists(Hive::CurrentUser, &path).unwrap(),
        "pre-existing key must survive"
    );
    clean(&path);
}

#[test]
fn deleted_value_comes_back() {
    let path = format!(r"{ROOT}\Deleted");
    clean(&path);
    let (key, _) = Key::create(Hive::CurrentUser, &path).unwrap();
    key.set("Probe", &RegValue::MultiSz(vec!["a".into(), "b".into()]))
        .unwrap();
    drop(key);

    let (_dir, journal) = journal();
    {
        let safety = Safety::begin(journal.clone(), options("deleted")).unwrap();
        let outcome = safety
            .delete_registry_value(Hive::CurrentUser, &path, "Probe")
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
        assert_eq!(read_value(Hive::CurrentUser, &path, "Probe").unwrap(), None);
    }

    let report = rollback::rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(
        read_value(Hive::CurrentUser, &path, "Probe").unwrap(),
        Some(RegValue::MultiSz(vec!["a".into(), "b".into()]))
    );
    clean(&path);
}

#[test]
fn journal_export_is_valid_json() {
    let (_dir, journal) = journal();
    let json = journal.export_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value["summary"]["sessions"].is_number());
}

#[test]
fn service_configuration_is_readable() {
    let scm = Scm::connect().unwrap();
    let svc = scm
        .open("Dnscache", READ_ACCESS)
        .unwrap()
        .expect("Dnscache exists on Windows");
    let cfg = svc.config().unwrap();
    assert_eq!(cfg.name, "Dnscache");
    assert!(!cfg.display_name.is_empty());
    assert!(!cfg.binary_path.is_empty());
    let status = svc.status().unwrap();
    assert!(status.pid > 0 || !status.state.is_active());
    assert!(scm
        .open("PCOptimizerNoSuchService", READ_ACCESS)
        .unwrap()
        .is_none());
}

#[test]
fn filtered_rollback_restores_only_selected_value() {
    let path = format!(r"{ROOT}\Filtered");
    clean(&path);
    let (_dir, journal) = journal();
    set_probe_and_marker(&journal, &path, "filtered");

    let filter = RollbackFilter {
        registry: vec![hkcu_target(&path, "Probe")],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert!(!report.dry_run);
    assert_eq!(report.registry_deleted, 1);
    assert_eq!(report.total_reverted(), 1);
    assert_eq!(report.actions.len(), 1, "{:?}", report.actions);
    assert_probe_and_marker(&path, None, Some(2));

    let active = journal.active_registry().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].value_name, "Marker");
    let summary = journal.summary().unwrap();
    assert_eq!(summary.sessions, 2);
    assert_eq!(summary.last_session.unwrap().label, "rollback_filtered");

    let rest = rollback::rollback_journal(&journal, false).unwrap();
    assert!(rest.is_clean(), "{:?}", rest.failures);
    assert_eq!(rest.registry_deleted, 1);
    assert_probe_and_marker(&path, None, None);
    let summary = journal.summary().unwrap();
    assert_eq!(summary.registry_active, 0);
    assert_eq!(summary.last_session.unwrap().label, "rollback_to_baseline");
    clean(&path);
}

#[test]
fn filter_matching_ignores_case_of_key_path_and_value_name() {
    let path = format!(r"{ROOT}\FilterCase");
    clean(&path);
    let (_dir, journal) = journal();
    set_probe_and_marker(&journal, &path, "filter-case");

    let other_hive = RollbackFilter {
        registry: vec![RegistryTarget {
            hive: Hive::LocalMachine,
            ..hkcu_target(&path, "Marker")
        }],
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &other_hive, true).unwrap();
    assert!(
        plan.actions.is_empty(),
        "hive must match exactly: {:?}",
        plan.actions
    );

    let filter = RollbackFilter {
        registry: vec![hkcu_target(&path.to_ascii_lowercase(), "pRoBe")],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 1);
    assert_probe_and_marker(&path, None, Some(2));
    assert_eq!(journal.summary().unwrap().registry_active, 1);

    rollback_and_clean(&journal, &path);
}

#[test]
fn empty_filter_selects_nothing_and_opens_no_session() {
    let path = format!(r"{ROOT}\FilterEmpty");
    clean(&path);
    let (_dir, journal) = journal();
    set_probe_and_marker(&journal, &path, "filter-empty");
    let sessions = journal.summary().unwrap().sessions;

    let empty = RollbackFilter::default();
    assert!(empty.is_empty());
    let unmatched = RollbackFilter {
        registry: vec![hkcu_target(&path, "Absent")],
        services: vec![FAKE_SERVICE.to_string()],
        appx_families: vec![FAKE_FAMILY.to_string()],
        power: false,
        scheduled_tasks: vec![FAKE_TASK.to_string()],
        dns: vec![FAKE_INTERFACE.to_string()],
        task_definitions: vec![FAKE_DEFINITION.to_string()],
    };
    assert!(!unmatched.is_empty());
    for filter in [&empty, &unmatched] {
        for dry_run in [false, true] {
            let report = rollback::rollback_filtered(&journal, filter, dry_run).unwrap();
            assert_eq!(report.dry_run, dry_run);
            assert!(report.actions.is_empty(), "{:?}", report.actions);
            assert_eq!(report.total_reverted(), 0);
            assert!(report.is_clean());
        }
    }

    let summary = journal.summary().unwrap();
    assert_eq!(summary.sessions, sessions);
    assert_eq!(summary.registry_active, 2);
    assert_probe_and_marker(&path, Some(1), Some(2));

    rollback_and_clean(&journal, &path);
}

#[test]
fn dry_run_filtered_rollback_lists_only_selected_actions() {
    let path = format!(r"{ROOT}\FilterDryRun");
    clean(&path);
    let (_dir, journal) = journal();
    set_probe_and_marker(&journal, &path, "filter-dry-run");
    let sessions = journal.summary().unwrap().sessions;

    let filter = RollbackFilter {
        registry: vec![hkcu_target(&path, "Marker")],
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &filter, true).unwrap();
    assert!(plan.dry_run);
    assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
    assert!(plan.actions[0].ends_with(r"\Marker"), "{:?}", plan.actions);
    assert_eq!(plan.total_reverted(), 0);
    assert!(plan.is_clean());

    let summary = journal.summary().unwrap();
    assert_eq!(summary.sessions, sessions);
    assert_eq!(summary.registry_active, 2);
    assert_probe_and_marker(&path, Some(1), Some(2));

    rollback_and_clean(&journal, &path);
}

#[test]
fn appx_without_files_requires_store_reinstall_and_stays_active() {
    let (dir, journal) = journal();
    record_fake_appx(&journal, dir.path());

    let filter = RollbackFilter {
        appx_families: vec![FAKE_FAMILY.to_string()],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.appx_restored, 0);
    assert_eq!(report.total_reverted(), 0);
    assert_eq!(report.appx_store_required.len(), 1);
    let store = &report.appx_store_required[0];
    assert!(
        store.package_family.eq_ignore_ascii_case(FAKE_FAMILY),
        "{store:?}"
    );
    assert!(store.store_link.contains(FAKE_FAMILY), "{store:?}");
    assert!(!report.is_clean());
    assert_eq!(
        report.actions,
        vec![format!("re-register Appx package {FAKE_FULL_NAME}")]
    );

    let active = journal.active_appx().unwrap();
    assert_eq!(
        active.len(),
        1,
        "a Store-only package must stay active for a later retry"
    );
    assert_eq!(active[0].package_family, FAKE_FAMILY);
    let summary = journal.summary().unwrap();
    assert_eq!(summary.last_session.unwrap().label, "rollback_filtered");
}

#[test]
fn elevation_is_required_only_by_selected_records() {
    let (dir, journal) = journal();
    record_fake_appx(&journal, dir.path());
    record_fake_service(&journal);
    record_fake_scheduled_task(&journal);
    record_fake_task_definition(&journal);

    if !is_elevated() {
        let sessions = journal.summary().unwrap().sessions;
        let services = RollbackFilter {
            services: vec![FAKE_SERVICE.to_ascii_lowercase()],
            ..Default::default()
        };
        let err = rollback::rollback_filtered(&journal, &services, false).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        let tasks = RollbackFilter {
            scheduled_tasks: vec![FAKE_TASK.to_ascii_uppercase()],
            ..Default::default()
        };
        let err = rollback::rollback_filtered(&journal, &tasks, false).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        let definitions = RollbackFilter {
            task_definitions: vec![FAKE_DEFINITION.to_string()],
            ..Default::default()
        };
        let err = rollback::rollback_filtered(&journal, &definitions, false).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        let err = rollback::rollback_journal(&journal, false).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert_eq!(
            journal.summary().unwrap().sessions,
            sessions,
            "no session before the check"
        );

        let plan = rollback::rollback_filtered(&journal, &services, true).unwrap();
        assert_eq!(
            plan.actions,
            vec![format!("set start type manual: service {FAKE_SERVICE}")]
        );
    }

    // Appx re-registration is per user, so an unselected service record does not block it.
    let appx_only = RollbackFilter {
        appx_families: vec![FAKE_FAMILY.to_string()],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &appx_only, false).unwrap();
    assert_eq!(report.appx_store_required.len(), 1);
    assert_eq!(report.services_restored, 0);
    assert_eq!(report.scheduled_tasks_restored, 0);
    let summary = journal.summary().unwrap();
    assert_eq!(summary.services_active, 1);
    assert_eq!(summary.scheduled_tasks_active, 1);
    assert_eq!(summary.task_definitions_active, 1);
}

#[test]
fn power_record_dry_run_lists_restore_and_changes_nothing() {
    let (_dir, journal) = journal();
    record_fake_power(&journal);
    let sessions = journal.summary().unwrap().sessions;

    let filter = RollbackFilter {
        power: true,
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &filter, true).unwrap();
    assert!(plan.dry_run);
    assert_eq!(
        plan.actions,
        vec![format!("restore power scheme {PREVIOUS_SCHEME}")]
    );
    assert_eq!(plan.power_restored, 0);
    assert!(plan.is_clean());

    let summary = journal.summary().unwrap();
    assert_eq!(summary.power_active, 1);
    assert_eq!(summary.sessions, sessions);
}

#[test]
fn dry_run_lists_groups_in_restore_order() {
    let (dir, journal) = journal();
    record_fake_appx(&journal, dir.path());
    record_fake_dns(&journal);
    record_fake_power(&journal);
    record_fake_task_definition(&journal);
    record_fake_scheduled_task(&journal);
    record_fake_service(&journal);
    let session = journal.begin_session("registry-selftest", "test").unwrap();
    let order_key = format!(r"{ROOT}\Order");
    for name in ["Older", "Newer"] {
        let rec = NewRegistryRecord {
            hive: Hive::CurrentUser,
            key_path: order_key.clone(),
            value_name: name.to_string(),
            key_existed: false,
            value_existed: false,
            original: None,
            created_root: None,
        };
        assert!(journal.record_registry(session, &rec).unwrap());
    }
    journal.end_session(session).unwrap();
    let sessions = journal.summary().unwrap().sessions;

    let everything = RollbackFilter {
        registry: vec![
            hkcu_target(&order_key, "Older"),
            hkcu_target(&order_key, "Newer"),
        ],
        services: vec![FAKE_SERVICE.to_string()],
        appx_families: vec![FAKE_FAMILY.to_string()],
        power: true,
        scheduled_tasks: vec![FAKE_TASK.to_string()],
        dns: vec![FAKE_INTERFACE.to_string()],
        task_definitions: vec![FAKE_DEFINITION.to_string()],
    };
    let full = rollback::rollback_journal(&journal, true).unwrap();
    let filtered = rollback::rollback_filtered(&journal, &everything, true).unwrap();
    for plan in [&full, &filtered] {
        assert_eq!(plan.actions.len(), 8, "{:?}", plan.actions);
        assert!(
            plan.actions[0].ends_with(r"\Order\Newer"),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions[1].ends_with(r"\Order\Older"),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions[2].ends_with(&format!("service {FAKE_SERVICE}")),
            "{:?}",
            plan.actions
        );
        assert_eq!(
            plan.actions[3],
            format!(r"enable scheduled task {FAKE_TASK}"),
            "{:?}",
            plan.actions
        );
        assert_eq!(
            plan.actions[4],
            format!("delete the scheduled task {FAKE_DEFINITION} that Cairn created"),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions[5].starts_with("restore power scheme "),
            "{:?}",
            plan.actions
        );
        assert_eq!(
            plan.actions[6],
            format!("restore IPv4 DNS servers of {FAKE_ADAPTER} to automatic"),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions[7].starts_with("re-register Appx package "),
            "{:?}",
            plan.actions
        );
        assert_eq!(plan.total_reverted(), 0);
    }
    assert!(!exists(Hive::CurrentUser, &order_key).unwrap());

    let summary = journal.summary().unwrap();
    assert_eq!(summary.sessions, sessions);
    assert!(summary.registry_active == 2 && summary.services_active == 1);
    assert!(summary.power_active == 1 && summary.appx_active == 1);
    assert!(summary.scheduled_tasks_active == 1 && summary.dns_active == 1);
    assert_eq!(summary.task_definitions_active, 1);
    assert_eq!(summary.pending_count(), 8);
}

#[test]
fn task_definition_record_dry_run_lists_delete_and_changes_nothing() {
    let (_dir, journal) = journal();
    record_fake_task_definition(&journal);
    let sessions = journal.summary().unwrap().sessions;

    let filter = RollbackFilter {
        task_definitions: vec![FAKE_DEFINITION.to_ascii_uppercase()],
        ..Default::default()
    };
    for plan in [
        rollback::rollback_filtered(&journal, &filter, true).unwrap(),
        rollback::rollback_journal(&journal, true).unwrap(),
    ] {
        assert!(plan.dry_run);
        assert_eq!(
            plan.actions,
            vec![format!(
                "delete the scheduled task {FAKE_DEFINITION} that Cairn created"
            )]
        );
        assert_eq!(plan.task_definitions_deleted, 0);
        assert_eq!(plan.restart, RestartNeed::None);
        assert!(plan.is_clean());
    }

    let summary = journal.summary().unwrap();
    assert_eq!(summary.task_definitions_active, 1);
    assert_eq!(summary.task_definitions_total, 1);
    assert!(summary.has_pending_changes());
    assert_eq!(summary.sessions, sessions);
    assert!(journal.ops(10).unwrap().is_empty());
    let export: serde_json::Value = serde_json::from_str(&journal.export_json().unwrap()).unwrap();
    assert_eq!(
        export["task_definitions"][0]["target"],
        format!("task {FAKE_DEFINITION}")
    );
}

#[test]
fn scheduled_task_record_dry_run_lists_restore_and_changes_nothing() {
    let (_dir, journal) = journal();
    record_fake_scheduled_task(&journal);
    let sessions = journal.summary().unwrap().sessions;

    let filter = RollbackFilter {
        scheduled_tasks: vec![FAKE_TASK.to_ascii_lowercase()],
        ..Default::default()
    };
    for plan in [
        rollback::rollback_filtered(&journal, &filter, true).unwrap(),
        rollback::rollback_journal(&journal, true).unwrap(),
    ] {
        assert!(plan.dry_run);
        assert_eq!(
            plan.actions,
            vec![format!(r"enable scheduled task {FAKE_TASK}")]
        );
        assert_eq!(plan.scheduled_tasks_restored, 0);
        assert_eq!(plan.restart, RestartNeed::None);
        assert!(plan.is_clean());
    }

    let summary = journal.summary().unwrap();
    assert_eq!(summary.scheduled_tasks_active, 1);
    assert_eq!(summary.sessions, sessions);
    assert!(journal.ops(10).unwrap().is_empty());
}

#[test]
fn dns_record_dry_run_lists_restore_and_changes_nothing() {
    let (_dir, journal) = journal();
    record_fake_dns(&journal);
    let sessions = journal.summary().unwrap().sessions;

    let filter = RollbackFilter {
        dns: vec![FAKE_INTERFACE
            .trim_matches(|c| c == '{' || c == '}')
            .to_ascii_uppercase()],
        ..Default::default()
    };
    for plan in [
        rollback::rollback_filtered(&journal, &filter, true).unwrap(),
        rollback::rollback_journal(&journal, true).unwrap(),
    ] {
        assert!(plan.dry_run);
        assert_eq!(
            plan.actions,
            vec![format!(
                "restore IPv4 DNS servers of {FAKE_ADAPTER} to automatic"
            )]
        );
        assert_eq!(plan.dns_restored, 0);
        assert!(plan.is_clean());
    }

    let summary = journal.summary().unwrap();
    assert_eq!(summary.dns_active, 1);
    assert_eq!(summary.sessions, sessions);
    assert!(journal.ops(10).unwrap().is_empty());
}

#[test]
fn missing_scheduled_task_is_marked_reverted_without_a_write() {
    if !is_elevated() {
        return;
    }
    // The journal holds only a record for a task that does not exist, so the rollback
    // only looks the task up.
    let (_dir, journal) = journal();
    record_fake_scheduled_task(&journal);
    let filter = RollbackFilter {
        scheduled_tasks: vec![FAKE_TASK.to_string()],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.scheduled_tasks_restored, 1);
    assert_eq!(report.total_reverted(), 1);
    assert_eq!(report.actions.len(), 1);
    assert!(
        report.actions[0].contains("no longer exists"),
        "{:?}",
        report.actions
    );
    assert!(journal.active_scheduled_tasks().unwrap().is_empty());
    let ops = journal.ops(10).unwrap();
    assert!(
        ops.iter().any(|o| o.op == "rollback_scheduled_task"
            && o.target == format!(r"scheduled task {FAKE_TASK}")
            && o.outcome == "not_found"),
        "{ops:?}"
    );
}

/// Removes `path` and every key below it that holds no values other than `Probe` and
/// `Marker`, deepest first.
fn clean_tree(path: &str) {
    if let Ok(Some(key)) = Key::open(Hive::CurrentUser, path, true) {
        let _ = key.delete_value("Probe");
        let _ = key.delete_value("Marker");
    }
    let mut stack = vec![path.to_string()];
    let mut all = Vec::new();
    while let Some(p) = stack.pop() {
        for child in ["A", "B", "C", "X", "Y", "Shared", "Deep"] {
            let sub = format!(r"{p}\{child}");
            if exists(Hive::CurrentUser, &sub).unwrap_or(false) {
                if let Ok(Some(key)) = Key::open(Hive::CurrentUser, &sub, true) {
                    let _ = key.delete_value("Probe");
                    let _ = key.delete_value("Marker");
                }
                stack.push(sub.clone());
                all.push(sub);
            }
        }
    }
    for p in all.iter().rev() {
        let _ = delete_key_if_empty(Hive::CurrentUser, p);
    }
    let _ = delete_key_if_empty(Hive::CurrentUser, path);
}

/// Creates `path` outside any journal, so it is a pre-existing parent for the test.
fn preexisting_parent(path: &str) {
    clean_tree(path);
    let (key, _) = Key::create(Hive::CurrentUser, path).unwrap();
    // A value keeps the parent non-empty, so only the created keys below it can go.
    key.set("Marker", &RegValue::Dword(0)).unwrap();
}

fn set_value(journal: &Arc<Journal>, path: &str, name: &str, value: u32) {
    let safety = Safety::begin(journal.clone(), options("nested")).unwrap();
    let outcome = safety
        .set_registry_value(Hive::CurrentUser, path, name, &RegValue::Dword(value))
        .unwrap();
    assert_eq!(outcome, MutationOutcome::Applied);
}

#[test]
fn rollback_removes_every_key_the_write_created() {
    let parent = format!(r"{ROOT}\NestedParent");
    preexisting_parent(&parent);
    let leaf = format!(r"{parent}\A\B\C");
    assert!(!exists(Hive::CurrentUser, &format!(r"{parent}\A")).unwrap());

    let (_dir, journal) = journal();
    set_value(&journal, &leaf, "Probe", 1);
    let rec = &journal.active_registry().unwrap()[0];
    assert!(!rec.key_existed);
    assert_eq!(
        rec.created_root.as_deref(),
        Some(format!(r"{parent}\A").as_str())
    );

    let plan = rollback::rollback_journal(&journal, true).unwrap();
    assert_eq!(
        plan.actions,
        vec![format!(
            r"delete value and created keys up to HKCU\{parent}\A: HKCU\{leaf}\Probe"
        )]
    );

    let report = rollback::rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 1);
    assert!(
        !exists(Hive::CurrentUser, &format!(r"{parent}\A")).unwrap(),
        "created ancestors left behind"
    );
    assert!(
        exists(Hive::CurrentUser, &parent).unwrap(),
        "the pre-existing parent must survive"
    );
    assert_eq!(
        read_value(Hive::CurrentUser, &parent, "Marker").unwrap(),
        Some(RegValue::Dword(0))
    );
    clean_tree(&parent);
}

#[test]
fn created_ancestors_stay_while_a_sibling_needs_them() {
    let parent = format!(r"{ROOT}\SiblingParent");
    let shared = format!(r"{parent}\Shared");
    let x = format!(r"{shared}\X");
    let y = format!(r"{shared}\Y");

    // Each order in which the two values can be reverted one at a time, then both at once.
    let orders: [&[&str]; 3] = [&["X", "Y"], &["Y", "X"], &["XY"]];
    for order in orders {
        preexisting_parent(&parent);
        let (_dir, journal) = journal();
        // X creates Shared\X and Shared; Y then only creates Shared\Y.
        set_value(&journal, &x, "Probe", 1);
        set_value(&journal, &y, "Probe", 2);
        let roots: Vec<Option<String>> = journal
            .active_registry()
            .unwrap()
            .into_iter()
            .map(|r| r.created_root)
            .collect();
        assert_eq!(roots, vec![Some(y.clone()), Some(shared.clone())]);

        for (step, which) in order.iter().enumerate() {
            let registry = match *which {
                "X" => vec![hkcu_target(&x, "Probe")],
                "Y" => vec![hkcu_target(&y, "Probe")],
                _ => vec![hkcu_target(&x, "Probe"), hkcu_target(&y, "Probe")],
            };
            let filter = RollbackFilter {
                registry,
                ..Default::default()
            };
            let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
            assert!(report.is_clean(), "{order:?}: {:?}", report.failures);
            let last = step + 1 == order.len();
            assert_eq!(
                exists(Hive::CurrentUser, &shared).unwrap(),
                !last,
                "{order:?} after {which}: Shared must stay while a sibling holds a value"
            );
        }
        assert!(!exists(Hive::CurrentUser, &x).unwrap());
        assert!(!exists(Hive::CurrentUser, &y).unwrap());
        assert!(exists(Hive::CurrentUser, &parent).unwrap(), "{order:?}");
        assert_eq!(journal.summary().unwrap().registry_active, 0);
    }
    clean_tree(&parent);
}

#[test]
fn value_written_under_a_created_key_removes_it_with_the_creating_record() {
    // Marker is recorded with key_existed = true, but Probe's older record created the
    // chain; reverting Probe first and Marker last still removes all of it.
    let parent = format!(r"{ROOT}\LaterValueParent");
    preexisting_parent(&parent);
    let leaf = format!(r"{parent}\Deep\A");
    let (_dir, journal) = journal();
    set_probe_and_marker(&journal, &leaf, "later-value");

    let probe_only = RollbackFilter {
        registry: vec![hkcu_target(&leaf, "Probe")],
        ..Default::default()
    };
    let report = rollback::rollback_filtered(&journal, &probe_only, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert!(
        exists(Hive::CurrentUser, &leaf).unwrap(),
        "key still holds Marker"
    );

    let report = rollback::rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert!(!exists(Hive::CurrentUser, &format!(r"{parent}\Deep")).unwrap());
    assert!(exists(Hive::CurrentUser, &parent).unwrap());
    clean_tree(&parent);
}

#[test]
fn created_keys_go_with_the_last_value_after_values_took_turns_holding_them() {
    // Probe's first record creates A and A\B and is reverted while Marker holds them.
    // Probe is then written again, Marker is reverted while the new Probe holds the keys,
    // and reverting that last value must still remove both created keys.
    let parent = format!(r"{ROOT}\TurnsParent");
    preexisting_parent(&parent);
    let created = format!(r"{parent}\A");
    let leaf = format!(r"{created}\B");
    let (_dir, journal) = journal();
    let revert = |name: &str| {
        let filter = RollbackFilter {
            registry: vec![hkcu_target(&leaf, name)],
            ..Default::default()
        };
        let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
        assert!(report.is_clean(), "{name}: {:?}", report.failures);
        assert_eq!(report.registry_deleted, 1, "{name}");
    };

    set_value(&journal, &leaf, "Probe", 1);
    let first = &journal.active_registry().unwrap()[0];
    assert!(!first.key_existed);
    assert_eq!(first.created_root.as_deref(), Some(created.as_str()));
    set_value(&journal, &leaf, "Marker", 2);

    revert("Probe");
    assert!(
        exists(Hive::CurrentUser, &leaf).unwrap(),
        "Marker holds the keys"
    );
    set_value(&journal, &leaf, "Probe", 3);
    revert("Marker");
    assert!(
        exists(Hive::CurrentUser, &leaf).unwrap(),
        "Probe holds the keys"
    );
    revert("Probe");

    assert!(
        !exists(Hive::CurrentUser, &created).unwrap(),
        "created keys left behind empty"
    );
    assert!(exists(Hive::CurrentUser, &parent).unwrap());
    assert_eq!(journal.summary().unwrap().registry_active, 0);
    clean_tree(&parent);
}

#[test]
fn user_policy_records_need_elevation() {
    let (_dir, journal) = journal();
    let session = journal.begin_session("policy-selftest", "test").unwrap();
    let policy_key = r"Software\Policies\PCOptimizerSelfTest";
    let captured = journal
        .record_registry(
            session,
            &NewRegistryRecord {
                hive: Hive::CurrentUser,
                key_path: policy_key.to_string(),
                value_name: "Probe".to_string(),
                key_existed: false,
                value_existed: false,
                original: None,
                created_root: Some(policy_key.to_string()),
            },
        )
        .unwrap();
    assert!(captured);
    journal.end_session(session).unwrap();

    let filter = RollbackFilter {
        registry: vec![hkcu_target(policy_key, "Probe")],
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &filter, true).unwrap();
    assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
    if !is_elevated() {
        let sessions = journal.summary().unwrap().sessions;
        let err = rollback::rollback_filtered(&journal, &filter, false).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert_eq!(journal.summary().unwrap().sessions, sessions);
        assert_eq!(journal.summary().unwrap().registry_active, 1);
    }
}

#[test]
fn report_names_the_restart_of_restored_tweaks() {
    let (_dir, journal) = journal();
    let session = journal.begin_session("restart-selftest", "test").unwrap();
    // Records for catalog targets, only ever rolled back in dry runs.
    for (hive, key_path, value_name) in [
        (
            Hive::CurrentUser,
            r"Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}\InprocServer32",
            "",
        ),
        (
            Hive::LocalMachine,
            r"SOFTWARE\Policies\Microsoft\Windows\DataCollection",
            "AllowTelemetry",
        ),
    ] {
        let rec = NewRegistryRecord {
            hive,
            key_path: key_path.to_string(),
            value_name: value_name.to_string(),
            key_existed: true,
            value_existed: false,
            original: None,
            created_root: None,
        };
        assert!(journal.record_registry(session, &rec).unwrap());
    }
    journal.end_session(session).unwrap();

    let menu = RollbackFilter {
        registry: vec![hkcu_target(
            r"Software\Classes\CLSID\{86CA1AA0-34AA-4E8B-A509-50C905BAE2A2}\InprocServer32",
            "",
        )],
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &menu, true).unwrap();
    assert_eq!(plan.restart, RestartNeed::Explorer);
    let plan = rollback::rollback_journal(&journal, true).unwrap();
    assert_eq!(plan.restart, RestartNeed::Restart, "strongest of both");
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(json["restart"], "restart");

    // Nothing restored: nothing to restart. Sandbox values belong to no tweak either.
    let empty = rollback::rollback_filtered(&journal, &RollbackFilter::default(), false).unwrap();
    assert_eq!(empty.restart, RestartNeed::None);
    let path = format!(r"{ROOT}\Restart");
    clean(&path);
    let (_dir2, sandbox) = self::journal();
    set_probe_and_marker(&sandbox, &path, "restart-sandbox");
    let report = rollback::rollback_journal(&sandbox, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.total_reverted(), 2);
    assert_eq!(report.restart, RestartNeed::None);
    clean(&path);

    // Reports written before the field existed still deserialize.
    let old: rollback::RollbackReport = serde_json::from_str(
        r#"{"dry_run":false,"registry_restored":0,"registry_deleted":0,"services_restored":0,
            "services_started":0,"appx_restored":0,"appx_store_required":[],"power_restored":0,
            "actions":[],"failures":[]}"#,
    )
    .unwrap();
    assert_eq!(old.restart, RestartNeed::None);
    assert_eq!(old.scheduled_tasks_restored, 0);
    assert_eq!(old.dns_restored, 0);
}

#[test]
fn removed_package_installed_again_is_marked_restored() {
    // Read-only: the recorded package is the Store under a version that is not
    // installed, with no files, so nothing can be registered whatever the lookup says.
    let installed = appx::inventory().expect("inventory");
    let Some(store) = installed
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("Microsoft.WindowsStore"))
    else {
        return;
    };
    let (dir, journal) = journal();
    let old_full_name = "Microsoft.WindowsStore_1.0.0.0_x64__8wekyb3d8bbwe";
    let session = journal.begin_session("appx-reinstalled", "test").unwrap();
    assert!(journal
        .record_appx(
            session,
            &NewAppxRecord {
                package_full_name: old_full_name.to_string(),
                package_family: store.family_name.clone(),
                install_location: dir.path().join("gone").display().to_string(),
                all_users: false,
            },
        )
        .unwrap());
    journal.end_session(session).unwrap();

    let filter = RollbackFilter {
        appx_families: vec![store.family_name.to_ascii_lowercase()],
        ..Default::default()
    };
    let plan = rollback::rollback_filtered(&journal, &filter, true).unwrap();
    assert_eq!(
        plan.actions,
        vec![format!("re-register Appx package {old_full_name}")]
    );

    let report = rollback::rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.appx_restored, 1);
    assert!(report.appx_store_required.is_empty());
    assert_eq!(report.actions.len(), 1);
    assert!(
        report.actions[0].starts_with("already reinstalled as ")
            && report.actions[0].ends_with(&format!("Appx package {old_full_name}")),
        "{:?}",
        report.actions
    );
    assert!(journal.active_appx().unwrap().is_empty());
    let ops = journal.ops(20).unwrap();
    assert!(ops
        .iter()
        .any(|o| o.op == "rollback_appx" && o.outcome == "already_installed"));
}

#[test]
fn cargo_env_forbids_restore_points() {
    assert_eq!(
        std::env::var("OPTIMIZER_FORBID_RESTORE_POINT").as_deref(),
        Ok("1"),
        "cargo sets OPTIMIZER_FORBID_RESTORE_POINT for every test run (.cargo/config.toml)"
    );
    assert!(optimizer_core::safety::restore_points_forbidden());
}
