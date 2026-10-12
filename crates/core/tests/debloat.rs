//! Integration tests for the debloat engine. Everything here is read-only for the
//! machine except values under HKCU\Software\PCOptimizer\SelfTest and, in the ignored
//! elevated test, a throwaway service that the test creates and deletes. Scheduled tasks
//! are only read.

use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;

use optimizer_core::debloat::catalog::{
    self, Category, RegData, RegistryAction, ScheduledTaskAction, ServiceAction, BLOAT_PACKAGES,
    TWEAKS,
};
use optimizer_core::debloat::engine::{catalog_view, ItemKind, ItemState, REINSTALLED_NOTE};
use optimizer_core::debloat::requirements::Unavailable;
use optimizer_core::debloat::{
    registry, scheduled_tasks, services, ActionState, ApplyOptions, DriftKind, Engine, ItemOutcome,
    Mismatch, RestartNeed,
};
use optimizer_core::safety::rollback::{rollback_filtered, RollbackFilter};
use optimizer_core::safety::state_log::{
    Journal, NewAppxRecord, NewRegistryRecord, NewScheduledTaskRecord,
};
use optimizer_core::safety::{MutationOutcome, RestorePointPolicy, Safety, SafetyOptions};
use optimizer_core::win::registry::{delete_key_if_empty, exists, read_value, Hive, Key, RegValue};
use optimizer_core::win::scm::{Scm, StartType, READ_ACCESS};
use optimizer_core::{is_elevated, Error};

const SANDBOX: &str = r"Software\PCOptimizer\SelfTest\Debloat";

fn temp_journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let journal = Journal::open(dir.path().join("journal.db")).expect("open journal");
    (dir, Arc::new(journal))
}

fn sandbox_session(journal: &Arc<Journal>) -> Safety {
    Safety::begin(
        journal.clone(),
        SafetyOptions {
            label: "debloat-test".into(),
            restore_point: RestorePointPolicy::Skip,
            require_elevation: false,
            ..Default::default()
        },
    )
    .expect("begin session")
}

/// Logs the write of `key_path\value_name` under `session`, as the safety layer does after it
/// records the baseline and writes the value; a record without it stands for a write that
/// failed and changed nothing.
fn log_write(journal: &Journal, session: i64, hive: Hive, key_path: &str, value_name: &str) {
    let target = format!(r"{}\{key_path}\{value_name}", hive.short());
    journal
        .log_op(
            Some(session),
            "set_registry_value",
            &target,
            "applied",
            None,
        )
        .unwrap();
}

fn clean_sandbox(names: &[&str]) {
    if let Ok(Some(key)) = Key::open(Hive::CurrentUser, SANDBOX, true) {
        for n in names {
            let _ = key.delete_value(n);
        }
    }
    let _ = delete_key_if_empty(Hive::CurrentUser, SANDBOX);
}

/// The note a scan shows for a tweak whose recorded targets no longer hold, in the words the
/// scan uses.
fn drift_note(drift: &[Mismatch]) -> Option<String> {
    let labels = |drift: &[Mismatch]| match drift {
        [first, second] => format!("{} and {}", first.label, second.label),
        [first, second, rest @ ..] => {
            format!("{}, {} and {} more", first.label, second.label, rest.len())
        }
        _ => unreachable!("several mismatches"),
    };
    match drift {
        [] => None,
        [one] if one.back_to_original => Some(format!(
            "Changed back since Cairn applied it: {} is {} again, its value before Cairn's \
             change. Apply sets it again.",
            one.label, one.found
        )),
        [one] => Some(format!(
            "Changed since Cairn applied it: {} is {}, not {}. Apply sets it again.",
            one.label, one.found, one.expected
        )),
        several => Some(format!(
            "Changed since Cairn applied it: {} settings no longer hold ({}). Apply sets them \
             again.",
            several.len(),
            labels(several)
        )),
    }
}

#[test]
fn catalog_view_lists_every_entry_once() {
    let view = catalog_view();
    assert_eq!(view.len(), TWEAKS.len() + BLOAT_PACKAGES.len());
    let ids: HashSet<&str> = view.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids.len(), view.len(), "duplicate ids in catalog view");
    for e in &view {
        assert!(!e.targets.is_empty(), "{} lists no targets", e.id);
        match e.kind {
            ItemKind::Tweak => assert_ne!(e.category, Category::Bloatware),
            ItemKind::Appx => assert_eq!(e.category, Category::Bloatware),
        }
    }
    serde_json::to_string(&view).expect("catalog view serialises");
}

#[test]
fn registry_action_round_trips_through_the_journal() {
    static PROBE: RegistryAction = RegistryAction {
        hive: Hive::CurrentUser,
        path: SANDBOX,
        name: "Probe",
        data: RegData::Dword(1),
    };
    static TEXT: RegistryAction = RegistryAction {
        hive: Hive::CurrentUser,
        path: SANDBOX,
        name: "Text",
        data: RegData::Sz("High"),
    };
    clean_sandbox(&["Probe", "Text"]);
    assert!(!exists(Hive::CurrentUser, SANDBOX).unwrap());

    let (_dir, journal) = temp_journal();
    assert_eq!(
        registry::status(&PROBE).unwrap().state,
        ActionState::NotApplied
    );
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            registry::apply(&safety, &PROBE).unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(
            registry::apply(&safety, &TEXT).unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(
            registry::apply(&safety, &PROBE).unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );
    }
    assert_eq!(
        registry::status(&PROBE).unwrap().state,
        ActionState::Applied
    );
    assert_eq!(registry::status(&TEXT).unwrap().state, ActionState::Applied);

    // Reverting one target leaves the other applied.
    let only_probe = RollbackFilter {
        registry: vec![registry::target(&PROBE)],
        ..Default::default()
    };
    let report = rollback_filtered(&journal, &only_probe, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 1);
    assert_eq!(
        registry::status(&PROBE).unwrap().state,
        ActionState::NotApplied
    );
    assert_eq!(registry::status(&TEXT).unwrap().state, ActionState::Applied);

    let rest = RollbackFilter {
        registry: vec![registry::target(&TEXT)],
        ..Default::default()
    };
    let report = rollback_filtered(&journal, &rest, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert!(
        !exists(Hive::CurrentUser, SANDBOX).unwrap(),
        "created key should be removed"
    );
    assert_eq!(journal.summary().unwrap().registry_active, 0);
}

#[test]
fn scan_is_read_only_and_covers_every_tweak() {
    let (_dir, journal) = temp_journal();
    let engine = Engine::new(journal.clone());
    let report = engine.scan().expect("scan");

    for t in TWEAKS {
        let item = report
            .item(t.id)
            .unwrap_or_else(|| panic!("{} missing from scan", t.id));
        assert_eq!(item.actions.len(), t.actions.len(), "{} action count", t.id);
        assert_eq!(item.kind, ItemKind::Tweak);
        assert_eq!(item.category, t.category);
        assert_eq!(item.topic, Some(t.topic), "{}", t.id);
        assert_eq!(item.undo_limit.as_deref(), t.undo_limit, "{}", t.id);
        assert!(item.mode_undo || !t.default_on, "{} mode undo", t.id);
        // An empty journal: nothing recorded, so nothing is held, drifted or former.
        assert!(item.actions.iter().all(|a| a.held.is_none()), "{}", t.id);
        assert!(item.drift.is_empty() && item.former.is_empty(), "{}", t.id);
        if t.default_on {
            let power = t
                .actions
                .iter()
                .any(|a| matches!(a, catalog::Action::Power(_)));
            assert_eq!(
                item.recommended,
                !(power && report.has_battery),
                "{} recommended",
                t.id
            );
        } else {
            assert!(!item.recommended, "{} is recommended but not default", t.id);
        }
    }
    for item in report.items.iter().filter(|i| i.kind == ItemKind::Appx) {
        let name = catalog::appx_name_from_item_id(&item.id).expect("appx id");
        assert!(
            catalog::bloat_entry_for(name).is_some(),
            "{} is not in the bloat catalog",
            name
        );
        assert!(
            !catalog::is_protected_package(name),
            "{} is protected",
            name
        );
        assert_eq!(
            item.state,
            ItemState::NotApplied,
            "installed package reported as removed"
        );
        assert!(
            !item.revertible,
            "{} revertible with an empty journal",
            item.id
        );
        assert_eq!(item.note, None);
    }
    for item in &report.items {
        assert!(
            !item.revertible,
            "{} revertible with an empty journal",
            item.id
        );
    }
    assert_eq!(report.categories.len(), Category::ALL.len());
    assert_eq!(report.elevated, is_elevated());
    assert_eq!(
        journal.summary().unwrap().sessions,
        0,
        "scan opened a journal session"
    );
    serde_json::to_string(&report).expect("scan report serialises");
    println!(
        "scan: {} items in {} ms, warnings: {:?}",
        report.items.len(),
        report.duration_ms,
        report.warnings
    );
}

#[test]
fn requirement_items_are_unavailable_with_a_note_or_read() {
    let (_dir, journal) = temp_journal();
    let report = Engine::new(journal.clone()).scan().expect("scan");
    let mut checked = 0;
    for t in TWEAKS {
        let Some(req) = t.requires else {
            continue;
        };
        checked += 1;
        let missing = req.missing_text();
        let item = report.item(t.id).expect("scanned");
        if item.unavailable == Some(Unavailable::NotOnThisPc) {
            assert_eq!(item.state, ItemState::Unavailable, "{}", t.id);
            // The note is the requirement's own text, or the more specific reason its probe
            // gives (the Office probe tells a PC without Office from a consumer edition).
            let note = item.note.clone().unwrap_or_default();
            assert!(
                note == missing
                    || (req == catalog::Requirement::OfficeBusiness && !note.is_empty()),
                "{}: {note:?}",
                t.id
            );
            assert!(
                item.actions
                    .iter()
                    .all(|a| a.state == ActionState::Unavailable && a.detail.ends_with(&note)),
                "{}: {:?}",
                t.id,
                item.actions
            );
            println!("{}: not on this PC ({note})", t.id);
        } else {
            let read_failed = report
                .warnings
                .iter()
                .any(|w| w.starts_with(&format!("{}:", t.id)));
            // Retired or outside its Windows range: unavailable for that reason instead.
            let other_reason = item
                .unavailable
                .is_some_and(|kind| kind != Unavailable::NotOnThisPc);
            assert!(
                item.state != ItemState::Unavailable || read_failed || other_reason,
                "{} is unavailable without its reason or a warning",
                t.id
            );
            assert!(
                item.actions.iter().all(|a| !a.detail.ends_with(missing)),
                "{}",
                t.id
            );
            println!("{}: {:?}, note {:?}", t.id, item.state, item.note);
        }
    }
    let with_requirement = TWEAKS.iter().filter(|t| t.requires.is_some()).count();
    assert!(with_requirement > 0);
    assert_eq!(checked, with_requirement);
    assert_eq!(journal.summary().unwrap().sessions, 0);
}

#[test]
fn dry_run_changes_nothing_and_opens_no_session() {
    let (_dir, journal) = temp_journal();
    let engine = Engine::new(journal.clone());
    let ids: Vec<String> = TWEAKS.iter().map(|t| t.id.to_string()).collect();
    let opts = ApplyOptions {
        restore_point: RestorePointPolicy::Skip,
        dry_run: true,
    };
    let report = engine.apply(&ids, &opts).expect("dry run");
    assert!(report.dry_run);
    assert!(report.session_id.is_none());
    assert_eq!(report.results.len(), ids.len());
    for r in &report.results {
        assert!(
            matches!(
                r.outcome,
                ItemOutcome::Planned | ItemOutcome::AlreadyApplied | ItemOutcome::Skipped
            ),
            "{} -> {:?}",
            r.id,
            r.outcome
        );
    }
    for c in Category::ALL {
        engine.apply_category(c, &opts).expect("category dry run");
        let revert = engine.revert_category(c, true).expect("revert dry run");
        assert!(revert.dry_run);
        assert_eq!(revert.restart, RestartNeed::None, "nothing journaled");
    }
    assert_eq!(journal.summary().unwrap().sessions, 0);
}

#[test]
fn scheduled_task_status_is_read_only() {
    static MISSING: ScheduledTaskAction = ScheduledTaskAction {
        path: r"\PCOptimizerSelfTest\NoSuchTask",
        enabled: false,
    };
    let status = scheduled_tasks::status(&MISSING).expect("read a missing task");
    assert_eq!(status.state, ActionState::Unavailable);
    assert!(
        status.detail.ends_with("is not on this PC"),
        "{}",
        status.detail
    );
    assert_eq!(
        scheduled_tasks::describe(&MISSING),
        r"scheduled task \PCOptimizerSelfTest\NoSuchTask"
    );

    let mut seen = 0;
    for t in TWEAKS {
        for a in t.actions {
            let catalog::Action::ScheduledTask(task) = a else {
                continue;
            };
            let status = scheduled_tasks::status(task)
                .unwrap_or_else(|e| panic!("{}: cannot read {}: {e}", t.id, task.path));
            let prefix = scheduled_tasks::describe(task);
            assert!(status.detail.starts_with(&prefix), "{}", status.detail);
            println!("{:?}  {}", status.state, status.detail);
            seen += 1;
        }
    }
    assert_eq!(seen, 12);
}

#[test]
fn unknown_ids_fail_without_touching_anything() {
    let (_dir, journal) = temp_journal();
    let engine = Engine::new(journal.clone());
    let ids = vec![
        "privacy.no_such_tweak".to_string(),
        "appx.Microsoft.WindowsStore".to_string(),
    ];
    let report = engine
        .apply(&ids, &ApplyOptions::default())
        .expect("apply unknown ids");
    assert_eq!(report.results.len(), 2);
    assert!(report
        .results
        .iter()
        .all(|r| r.outcome == ItemOutcome::Failed));
    assert!(report.session_id.is_none());

    let revert = engine.revert(&ids, false).expect("revert unknown ids");
    assert_eq!(revert.total_reverted(), 0);
    assert_eq!(journal.summary().unwrap().sessions, 0);
}

#[test]
fn apply_requires_elevation() {
    if is_elevated() {
        return;
    }
    let (_dir, journal) = temp_journal();
    let engine = Engine::new(journal.clone());
    // A tweak offered on every PC: retired and unavailable tweaks are skipped before the
    // elevation check, so the apply would succeed with nothing to do.
    let id = TWEAKS
        .iter()
        .find(|t| t.retired.is_none() && t.requires.is_none() && t.windows == catalog::ANY_WINDOWS)
        .expect("a tweak offered on every PC")
        .id;
    let err = engine
        .apply(&[id.to_string()], &ApplyOptions::default())
        .expect_err("apply without elevation");
    assert!(matches!(err, Error::NotElevated), "{err}");
    assert_eq!(journal.summary().unwrap().sessions, 0);
}

/// Creates a throwaway auto-start service, applies a ServiceAction to it through the
/// safety layer, reverts it, and deletes the service. Run from an elevated shell:
/// `cargo test -p optimizer_core --test debloat -- --ignored`.
#[test]
#[ignore = "requires elevation; creates and deletes a throwaway service"]
fn service_action_round_trip_on_throwaway_service() {
    const NAME: &str = "PCOptimizerSelfTest";
    static ACTION: ServiceAction = ServiceAction {
        name: NAME,
        start: StartType::Manual,
        stop: true,
    };
    assert!(is_elevated(), "run this test elevated");

    let sc = |args: &[&str]| Command::new("sc.exe").args(args).output().expect("sc.exe");
    let _ = sc(&["delete", NAME]);
    let created = sc(&[
        "create",
        NAME,
        "binPath=",
        r"C:\Windows\System32\cmd.exe /c exit 0",
        "start=",
        "auto",
    ]);
    assert!(
        created.status.success(),
        "sc create: {}",
        String::from_utf8_lossy(&created.stdout)
    );

    let result = std::panic::catch_unwind(|| {
        let (_dir, journal) = temp_journal();
        assert_eq!(
            services::status(&ACTION).unwrap().state,
            ActionState::NotApplied
        );
        {
            let safety = sandbox_session(&journal);
            let outcome = services::apply(&safety, &ACTION).unwrap();
            assert_eq!(
                outcome.outcome,
                MutationOutcome::Applied,
                "{:?}",
                outcome.notes
            );
        }
        assert_eq!(
            services::status(&ACTION).unwrap().state,
            ActionState::Applied
        );

        let filter = RollbackFilter {
            services: vec![NAME.to_string()],
            ..Default::default()
        };
        let report = rollback_filtered(&journal, &filter, false).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.services_restored, 1);

        let scm = Scm::connect().unwrap();
        let svc = scm
            .open(NAME, READ_ACCESS)
            .unwrap()
            .expect("service exists");
        assert_eq!(svc.config().unwrap().start_type, StartType::Automatic);
    });
    let _ = sc(&["delete", NAME]);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Separate from [`SANDBOX`], which other tests expect to disappear after their rollback.
const FLAGS_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DebloatFlags";

fn clean_flags_sandbox() {
    if let Ok(Some(key)) = Key::open(Hive::CurrentUser, FLAGS_SANDBOX, true) {
        let _ = key.delete_value("Flags");
    }
    let _ = delete_key_if_empty(Hive::CurrentUser, FLAGS_SANDBOX);
}

#[test]
fn flags_action_clears_one_bit_and_rolls_back_exactly() {
    static FLAGS: RegistryAction = RegistryAction {
        hive: Hive::CurrentUser,
        path: FLAGS_SANDBOX,
        name: "Flags",
        data: RegData::FlagsSzClear {
            clear: 0x4,
            default: 510,
        },
    };
    let read = || read_value(Hive::CurrentUser, FLAGS_SANDBOX, "Flags").unwrap();
    let set = |text: &str| {
        let (key, _) = Key::create(Hive::CurrentUser, FLAGS_SANDBOX).unwrap();
        key.set("Flags", &RegValue::Sz(text.to_string())).unwrap();
    };

    // Feature on (0x1) with the shortcut on (0x4): only the shortcut bit goes.
    clean_flags_sandbox();
    set("511");
    let (_dir, journal) = temp_journal();
    assert_eq!(
        registry::status(&FLAGS).unwrap().state,
        ActionState::NotApplied
    );
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            registry::apply(&safety, &FLAGS).unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(read(), Some(RegValue::Sz("507".into())));
        assert_eq!(
            registry::apply(&safety, &FLAGS).unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );
    }
    assert_eq!(
        registry::status(&FLAGS).unwrap().state,
        ActionState::Applied
    );
    let filter = RollbackFilter {
        registry: vec![registry::target(&FLAGS)],
        ..Default::default()
    };
    let report = rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_restored, 1);
    assert_eq!(
        read(),
        Some(RegValue::Sz("511".into())),
        "original string restored"
    );

    // Shortcut already off: applied, and applying writes nothing new.
    set("498");
    assert_eq!(
        registry::status(&FLAGS).unwrap().state,
        ActionState::Applied
    );
    let (_dir, journal) = temp_journal();
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            registry::apply(&safety, &FLAGS).unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );
    }
    assert_eq!(read(), Some(RegValue::Sz("498".into())));

    // Not a number: skipped without journaling or writing.
    set("not a number");
    let (_dir, journal) = temp_journal();
    {
        let safety = sandbox_session(&journal);
        assert!(matches!(
            registry::apply(&safety, &FLAGS).unwrap(),
            MutationOutcome::Skipped(_)
        ));
    }
    assert_eq!(journal.summary().unwrap().registry_active, 0);
    assert_eq!(read(), Some(RegValue::Sz("not a number".into())));

    // Absent: Windows' default without the shortcut bit; rollback deletes it again.
    clean_flags_sandbox();
    let (_dir, journal) = temp_journal();
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            registry::apply(&safety, &FLAGS).unwrap(),
            MutationOutcome::Applied
        );
    }
    assert_eq!(read(), Some(RegValue::Sz("506".into())));
    let report = rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(read(), None);
    assert!(!exists(Hive::CurrentUser, FLAGS_SANDBOX).unwrap());
    clean_flags_sandbox();
}

#[test]
fn scan_marks_journaled_items_revertible() {
    let (_dir, journal) = temp_journal();
    let session = journal.begin_session("scan-selftest", "test").unwrap();
    // A record for one catalog target. Scanning only reads, so nothing is changed.
    let t = catalog::tweak("interface.file_extensions").expect("tweak");
    let catalog::Action::Registry(action) = t.actions[0] else {
        panic!("file_extensions writes a registry value")
    };
    assert!(journal
        .record_registry(
            session,
            &NewRegistryRecord {
                hive: action.hive,
                key_path: action.path.to_ascii_uppercase(),
                value_name: action.name.to_string(),
                key_existed: true,
                value_existed: true,
                original: Some(RegValue::Dword(1).to_raw()),
                created_root: None,
            },
        )
        .unwrap());
    log_write(&journal, session, action.hive, action.path, action.name);
    // A removed Copilot app: shown as removed, or as installed again with a note.
    let family = "Microsoft.Copilot_8wekyb3d8bbwe";
    assert!(journal
        .record_appx(
            session,
            &NewAppxRecord {
                package_full_name: "Microsoft.Copilot_1.0.0.0_neutral__8wekyb3d8bbwe".into(),
                package_family: family.into(),
                install_location: String::new(),
                all_users: false,
            },
        )
        .unwrap());
    journal.end_session(session).unwrap();
    let sessions = journal.summary().unwrap().sessions;

    let report = Engine::new(journal.clone()).scan().expect("scan");
    for item in report.items.iter().filter(|i| i.kind == ItemKind::Tweak) {
        assert_eq!(
            item.revertible,
            item.id == "interface.file_extensions",
            "{} revertible",
            item.id
        );
        // Tweaks not offered here carry the reason, and tweaks with a requirement may carry its
        // note (GPU scheduling state); the recorded tweak has the drift note when its value
        // no longer holds.
        let requires = catalog::tweak(&item.id).expect("catalog tweak").requires;
        if requires.is_none() && item.unavailable.is_none() {
            assert_eq!(item.note, drift_note(&item.drift), "{}", item.id);
        }
    }
    // The record says the value was 1 before Cairn set it: the drift says so whatever this PC
    // holds now.
    let extensions = report.item("interface.file_extensions").expect("scanned");
    let held = extensions.actions[0]
        .held
        .expect("the recorded target was read");
    assert_eq!(extensions.drift.is_empty(), held);
    for m in &extensions.drift {
        assert_eq!(m.label, action.name);
        assert_eq!(m.expected, "0x00000000 (0)");
        assert_eq!(m.original.as_deref(), Some("0x00000001 (1)"));
        assert_eq!(m.back_to_original, m.found == "0x00000001 (1)");
    }
    let copilot = report.item("appx.Microsoft.Copilot").expect("Copilot item");
    assert!(copilot.revertible);
    match copilot.state {
        ItemState::Applied => assert_eq!(copilot.note, None),
        ItemState::NotApplied => assert_eq!(copilot.note.as_deref(), Some(REINSTALLED_NOTE)),
        other => panic!("unexpected Copilot state {other:?}"),
    }
    let json = serde_json::to_value(copilot).unwrap();
    assert!(json["revertible"].as_bool().unwrap());
    assert!(json.get("note").is_some(), "note is always present: {json}");
    assert_eq!(
        journal.summary().unwrap().sessions,
        sessions,
        "scan is read-only"
    );
}

#[test]
fn scan_notes_recorded_tasks_that_are_enabled_again() {
    let (_dir, journal) = temp_journal();
    let session = journal.begin_session("task-note-selftest", "test").unwrap();
    // A record for every catalog task, spelled in another letter case than the catalog.
    // Scanning only reads, so nothing is changed.
    for t in TWEAKS {
        for a in t.actions {
            if let catalog::Action::ScheduledTask(task) = a {
                let rec = NewScheduledTaskRecord {
                    path: task.path.to_ascii_uppercase(),
                    was_enabled: true,
                };
                assert!(journal.record_scheduled_task(session, &rec).unwrap());
            }
        }
    }
    journal.end_session(session).unwrap();
    let sessions = journal.summary().unwrap().sessions;

    let report = Engine::new(journal.clone()).scan().expect("scan");
    let is_task = |a: &catalog::Action| matches!(a, catalog::Action::ScheduledTask(_));
    let mut seen = 0;
    for t in TWEAKS {
        if !t.actions.iter().any(is_task) {
            continue;
        }
        let item = report.item(t.id).expect("task tweak");
        assert!(item.revertible, "{}", t.id);
        seen += 1;
        if item.unavailable == Some(Unavailable::Retired) {
            assert!(
                item.drift.is_empty(),
                "{}: a retired row lists no drift",
                t.id
            );
            continue;
        }
        // The drift follows the scanned states, so the test holds whichever tasks are enabled
        // on this PC: every recorded task enabled again is listed, back at its recorded flag.
        let enabled_again: Vec<&str> = t
            .actions
            .iter()
            .zip(&item.actions)
            .filter_map(|(a, status)| match a {
                catalog::Action::ScheduledTask(task) if status.state == ActionState::NotApplied => {
                    Some(task.path)
                }
                _ => None,
            })
            .collect();
        let drifted: Vec<&str> = item.drift.iter().map(|m| m.target.as_str()).collect();
        let targets: Vec<String> = enabled_again
            .iter()
            .map(|path| format!("scheduled task {path}"))
            .collect();
        assert_eq!(drifted, targets, "{}", t.id);
        for (m, path) in item.drift.iter().zip(&enabled_again) {
            let name = path.rsplit('\\').next().expect("a task name");
            assert_eq!(m.label, format!("task {name}"));
            assert_eq!(
                (m.expected.as_str(), m.found.as_str()),
                ("disabled", "enabled")
            );
            assert_eq!(m.original.as_deref(), Some("enabled"));
            assert!(m.back_to_original, "{}: {m:?}", t.id);
        }
        assert_eq!(item.note, drift_note(&item.drift), "{}", t.id);
        // Every recorded task that could be read has `held` from that same read; a task that
        // no longer exists is not checked and so holds.
        for status in &item.actions {
            match status.held {
                Some(held) => assert_eq!(held, status.state != ActionState::NotApplied, "{}", t.id),
                None => assert_eq!(status.state, ActionState::Unavailable, "{}", t.id),
            }
        }
        println!("{}: {:?}", t.id, item.note);
    }
    assert_eq!(seen, 4);
    for item in report.items.iter().filter(|i| i.kind == ItemKind::Tweak) {
        let tweak = catalog::tweak(&item.id).expect("catalog tweak");
        // Tweaks not offered here, or with a requirement, may carry their note instead.
        if !tweak.actions.iter().any(is_task)
            && tweak.requires.is_none()
            && item.unavailable.is_none()
        {
            assert_eq!(item.note, None, "{} has no scheduled tasks", item.id);
        }
    }
    assert_eq!(
        journal.summary().unwrap().sessions,
        sessions,
        "scan is read-only"
    );
}

#[test]
fn real_scan_without_records_has_no_held_flags() {
    let (_dir, journal) = temp_journal();
    let engine = Engine::new(journal.clone());
    let report = engine.scan().expect("scan");
    for item in &report.items {
        assert!(item.actions.iter().all(|a| a.held.is_none()), "{}", item.id);
        assert!(item.drift.is_empty(), "{}", item.id);
        if let Some(note) = &item.note {
            assert!(!note.starts_with("Changed"), "{}: {note}", item.id);
        }
    }

    // Nothing recorded: nothing to read, and nothing that no longer holds.
    let drift = engine.drift().expect("drift");
    assert!(drift.entries.is_empty() && drift.warnings.is_empty());
    assert_eq!((drift.checked, drift.unchecked), (0, 0));
    let ids: Vec<String> = TWEAKS.iter().map(|t| t.id.to_string()).collect();
    let verify = engine.verify(&ids).expect("verify");
    assert!(verify.entries.is_empty());
    assert_eq!(verify.checked, 0);
    assert_eq!(journal.summary().unwrap().sessions, 0);
}

#[test]
fn real_drift_check_reads_a_recorded_catalog_target() {
    let (_dir, journal) = temp_journal();
    // A record for one catalog target, written into the temporary journal only: the checks
    // below read the PC and change nothing on it.
    let t = catalog::tweak("interface.file_extensions").expect("tweak");
    let catalog::Action::Registry(action) = t.actions[0] else {
        panic!("file_extensions writes a registry value")
    };
    let session = journal.begin_session("drift-selftest", "test").unwrap();
    assert!(journal
        .record_registry(
            session,
            &NewRegistryRecord {
                hive: action.hive,
                key_path: action.path.to_string(),
                value_name: action.name.to_string(),
                key_existed: true,
                value_existed: true,
                original: Some(RegValue::Dword(1).to_raw()),
                created_root: None,
            },
        )
        .unwrap());
    log_write(&journal, session, action.hive, action.path, action.name);
    journal.end_session(session).unwrap();
    let before = read_value(action.hive, action.path, action.name).unwrap();
    let holds = before == Some(RegValue::Dword(0));
    let engine = Engine::new(journal.clone());

    let report = engine.drift().expect("drift");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!((report.checked, report.unchecked), (1, 0));
    assert_eq!(report.entries.is_empty(), holds, "{:?}", report.entries);
    for entry in &report.entries {
        assert_eq!(entry.item.as_deref(), Some("interface.file_extensions"));
        assert_eq!(entry.kind, DriftKind::Registry);
        assert_eq!(entry.mismatch.label, "HideFileExt");
        assert_eq!(entry.mismatch.expected, "0x00000000 (0)");
        assert_eq!(entry.mismatch.original.as_deref(), Some("0x00000001 (1)"));
        assert_eq!(entry.filter.registry, vec![registry::target(&action)]);
    }
    let only = engine
        .verify(&["interface.file_extensions".to_string()])
        .expect("verify");
    assert_eq!(only.entries.len(), report.entries.len());
    let other = engine
        .verify(&["privacy.error_reporting".to_string()])
        .expect("verify");
    assert!(other.entries.is_empty());
    assert_eq!(other.checked, 0);

    // Keep as is changes only the journal: a target that still holds is refused, and one that
    // changed is no longer tracked; the PC keeps its value either way.
    let filter = RollbackFilter {
        registry: vec![registry::target(&action)],
        ..Default::default()
    };
    let kept = engine.forget(&filter).expect("forget");
    assert_eq!(kept.refused.len(), usize::from(holds), "{:?}", kept.refused);
    assert_eq!(kept.forgotten.len(), usize::from(!holds));
    assert_eq!(
        read_value(action.hive, action.path, action.name).unwrap(),
        before
    );
    assert_eq!(journal.summary().unwrap().registry_active, i64::from(holds));
    assert_eq!(journal.summary().unwrap().sessions, 1);
}
