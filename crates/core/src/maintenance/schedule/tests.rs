use std::cell::RefCell;
use std::rc::Rc;

use chrono::NaiveDate;

use super::*;
use crate::maintenance::task::fake::FakeDefinitions;
use crate::safety::state_log::NewMaintenanceRun;
use crate::safety::test_safety;

const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
const PATH: &str = r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001";
/// An account SID outside `S-1-5-`, as Microsoft Entra ID accounts have.
const ENTRA_SID: &str = "S-1-12-1-1111111111-2222222222-3333333333-4444444444";
const PROGRAM: &str = r"C:\Program Files\Cairn\cairn-maintenance.exe";
const ROOT_SDDL: &str =
    "O:SYD:PAI(A;CI;FA;;;BA)(A;OI;0x1f019f;;;BA)(A;CI;FA;;;SY)(A;OI;0x1f019f;;;SY)\
                         (A;CI;FW;;;AU)(A;CI;FW;;;NS)(A;CI;FW;;;LS)(A;OICIIO;FA;;;CO)";

struct Setup {
    _dir: tempfile::TempDir,
    journal: Arc<Journal>,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    Setup { _dir: dir, journal }
}

fn host(journal: &Journal) -> Host {
    Host {
        elevated: true,
        sid: Ok(SID.to_string()),
        account: r"TEST-PC\Test".to_string(),
        other_user: Ok(false),
        program: Ok(PathBuf::from(PROGRAM)),
        program_problem: None,
        journal_path: journal.path().to_path_buf(),
        system_dir: Ok(PathBuf::from(r"C:\Windows\System32")),
        now: NaiveDate::from_ymd_opt(2026, 9, 30)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap(),
        has_battery: false,
        on_battery: false,
    }
}

fn config() -> ScheduleConfig {
    ScheduleConfig {
        day: ScheduleDay::Sunday,
        time: ScheduleTime::NOON,
        targets: vec!["user_temp".into(), "windows_temp".into()],
        system_file_check: true,
        component_store_check: true,
    }
}

/// A change to the test host.
type HostChange = Box<dyn Fn(&mut Host)>;

fn session_count(journal: &Journal) -> usize {
    journal.sessions().unwrap().len()
}

fn op_rows(journal: &Journal, op: &str) -> Vec<(String, Option<String>)> {
    let mut rows: Vec<(String, Option<String>)> = journal
        .ops(100)
        .unwrap()
        .into_iter()
        .filter(|r| r.op == op)
        .map(|r| (r.outcome, r.detail))
        .collect();
    rows.reverse();
    rows
}

/// What a Task Scheduler connection that succeeds hands out: `fake`.
fn store_of(fake: &FakeDefinitions) -> Result<Box<dyn TaskDefinitionStore + '_>> {
    Ok(Box::new(fake))
}

/// A store holding the task a first `set_with` registered, and its journal record.
fn turned_on(s: &Setup) -> FakeDefinitions {
    let fake = FakeDefinitions::new();
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    set_with(&safety, &host(&s.journal), &fake, &config()).unwrap();
    fake
}

#[test]
fn the_record_is_written_before_task_scheduler_is_touched() {
    let s = setup();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let journal = Arc::clone(&s.journal);
    let log = Rc::clone(&seen);
    let fake = FakeDefinitions {
        before_write: Some(Box::new(move |op: &str| {
            let active = journal.active_task_definitions().unwrap();
            assert_eq!(active.len(), 1, "no record before {op}");
            log.borrow_mut().push(op.to_string());
        })),
        ..FakeDefinitions::new()
    };
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let report = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap();
    assert_eq!(report.outcome, ScheduleOutcome::Created);
    assert_eq!(report.task_path, PATH);
    assert_eq!(report.next_run.as_deref(), Some("2026-10-04T12:00:00"));
    assert_eq!(
        *seen.borrow(),
        [
            r"create_folder \Cairn".to_string(),
            format!("register {PATH}")
        ]
    );
    let records = s.journal.active_task_definitions().unwrap();
    assert_eq!(records[0].path, PATH);
    assert_eq!(records[0].purpose, "maintenance");
    assert!(records[0].folder_created);
    assert_eq!(
        fake.folders.borrow().get(r"\cairn").map(String::as_str),
        Some(FOLDER_SDDL)
    );
    let task = fake.task(PATH).unwrap();
    assert!(task
        .xml
        .contains("<UserId>S-1-5-21-1111111111-2222222222-3333333333-1001</UserId>"));
    assert!(task.xml.contains(&format!("<Command>{PROGRAM}</Command>")));
    assert!(task
        .xml
        .contains("--targets user_temp,windows_temp --sfc --dism"));
    assert_eq!(
        op_rows(&s.journal, OP_SET_TASK),
        [(
            "applied".to_string(),
            Some(
                "created: every Sunday at 12:00; cleans user_temp, windows_temp; checks \
                 sfc_verify, dism_check"
                    .to_string()
            )
        )]
    );
}

#[test]
fn an_existing_safe_folder_is_reused_and_not_recorded_as_created() {
    let s = setup();
    let fake = FakeDefinitions::new().with_folder(r"\Cairn", FOLDER_SDDL);
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    set_with(&safety, &host(&s.journal), &fake, &config()).unwrap();
    assert!(!s.journal.active_task_definitions().unwrap()[0].folder_created);
    assert_eq!(*fake.ops.borrow(), [format!("register {PATH}")]);
}

#[test]
fn an_unsafe_folder_is_refused_before_any_record_or_write() {
    let s = setup();
    let fake = FakeDefinitions::new().with_folder(r"\Cairn", ROOT_SDDL);
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let err = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap_err();
    assert_eq!(err.to_string(), FOLDER_UNSAFE);
    assert_eq!(fake.writes.get(), 0);
    assert!(s.journal.active_task_definitions().unwrap().is_empty());
    assert_eq!(op_rows(&s.journal, OP_SET_TASK)[0].0, "skipped");
}

#[test]
fn a_task_without_a_record_is_not_taken_over() {
    let s = setup();
    let other = FakeDefinitions::new();
    let other_setup = setup();
    let safety = test_safety(Arc::clone(&other_setup.journal), SESSION_LABEL, false);
    set_with(&safety, &host(&other_setup.journal), &other, &config()).unwrap();
    let xml = other.task(PATH).unwrap().xml;
    let fake = FakeDefinitions::new()
        .with_folder(r"\Cairn", FOLDER_SDDL)
        .with_task(PATH, &xml);
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let err = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap_err();
    assert_eq!(err.to_string(), FOREIGN_TASK);
    assert_eq!(fake.writes.get(), 0);
    assert!(s.journal.active_task_definitions().unwrap().is_empty());
}

#[test]
fn saving_again_keeps_the_first_baseline_and_same_settings_change_nothing() {
    let s = setup();
    let fake = turned_on(&s);
    let first = s.journal.active_task_definitions().unwrap();
    let writes = fake.writes.get();

    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let same = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap();
    assert_eq!(same.outcome, ScheduleOutcome::Unchanged);
    assert_eq!(fake.writes.get(), writes);
    assert_eq!(s.journal.all_task_definitions().unwrap().len(), 1);
    assert_eq!(
        op_rows(&s.journal, OP_SET_TASK).last().unwrap().0,
        "already_in_desired_state"
    );

    let mut other = config();
    other.day = ScheduleDay::Wednesday;
    other.system_file_check = false;
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let updated = set_with(&safety, &host(&s.journal), &fake, &other).unwrap();
    assert_eq!(updated.outcome, ScheduleOutcome::Updated);
    let after = s.journal.active_task_definitions().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, first[0].id, "the first baseline is kept");
    assert!(fake.task(PATH).unwrap().xml.contains("<Wednesday />"));
    assert!(op_rows(&s.journal, OP_SET_TASK)
        .last()
        .unwrap()
        .1
        .as_deref()
        .unwrap()
        .starts_with("updated: every Wednesday"));
}

#[test]
fn a_failed_registration_withdraws_the_record_and_the_folder() {
    let s = setup();
    let fake = FakeDefinitions {
        fail_register: Some("the service stopped".into()),
        ..FakeDefinitions::new()
    };
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let err = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap_err();
    assert_eq!(err.to_string(), "the service stopped");
    assert!(s.journal.active_task_definitions().unwrap().is_empty());
    assert_eq!(s.journal.all_task_definitions().unwrap().len(), 1);
    assert!(!fake.has_folder(r"\Cairn"));
    assert!(fake.task(PATH).is_none());
    assert_eq!(
        op_rows(&s.journal, OP_SET_TASK),
        [(
            "failed".to_string(),
            Some("the service stopped".to_string())
        )]
    );
}

#[test]
fn access_denied_registration_names_the_policy() {
    let s = setup();
    let fake = FakeDefinitions {
        deny_register: true,
        ..FakeDefinitions::new()
    };
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let err = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap_err();
    assert_eq!(err.to_string(), REGISTER_DENIED);
    assert!(s.journal.active_task_definitions().unwrap().is_empty());
}

#[test]
fn an_unsafe_registered_task_is_deleted_and_withdrawn() {
    let s = setup();
    let fake = FakeDefinitions {
        task_sddl_override: Some(format!("O:{SID}D:(A;;FA;;;{SID})(A;;FA;;;BA)")),
        ..FakeDefinitions::new()
    };
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    let err = set_with(&safety, &host(&s.journal), &fake, &config()).unwrap_err();
    assert_eq!(err.to_string(), TASK_UNSAFE);
    assert!(fake.task(PATH).is_none());
    assert!(!fake.has_folder(r"\Cairn"));
    assert!(s.journal.active_task_definitions().unwrap().is_empty());
    assert_eq!(
        *fake.ops.borrow(),
        [
            r"create_folder \Cairn".to_string(),
            format!("register {PATH}"),
            format!("delete {PATH}"),
            r"delete_folder \Cairn".to_string(),
        ]
    );
}

#[test]
fn refusals_come_before_any_session() {
    let s = setup();
    let before = session_count(&s.journal);
    let connects = RefCell::new(0);
    let connect = || -> Result<Box<dyn TaskDefinitionStore>> {
        *connects.borrow_mut() += 1;
        Ok(Box::new(FakeDefinitions::new()))
    };
    let mut h = host(&s.journal);
    h.elevated = false;
    assert!(matches!(
        set_schedule_with(Arc::clone(&s.journal), &h, connect, &config()),
        Err(Error::NotElevated)
    ));
    let cases: Vec<(HostChange, String)> = vec![
        (
            Box::new(|h| h.sid = Ok("S-1-5-18".into())),
            SERVICE_ACCOUNT.into(),
        ),
        (
            Box::new(|h| h.sid = Ok(ENTRA_SID.into())),
            UNSUPPORTED_ACCOUNT.into(),
        ),
        (Box::new(|h| h.other_user = Ok(true)), OTHER_ACCOUNT.into()),
        (
            Box::new(|h| h.other_user = Err("no session".into())),
            OTHER_ACCOUNT.into(),
        ),
        (
            Box::new(|h| h.program_problem = Some("the program is in a user folder".into())),
            "the program is in a user folder".into(),
        ),
        (
            Box::new(|h| h.program = Err("cairn-maintenance.exe is missing".into())),
            "cairn-maintenance.exe is missing".into(),
        ),
    ];
    for (change, expected) in cases {
        let mut h = host(&s.journal);
        change(&mut h);
        let connect = || -> Result<Box<dyn TaskDefinitionStore>> {
            *connects.borrow_mut() += 1;
            Ok(Box::new(FakeDefinitions::new()))
        };
        let err = set_schedule_with(Arc::clone(&s.journal), &h, connect, &config()).unwrap_err();
        assert_eq!(err.to_string(), expected);
    }
    let h = host(&s.journal);
    let broken =
        || -> Result<Box<dyn TaskDefinitionStore>> { Err(Error::Other("RPC down".into())) };
    let err = set_schedule_with(Arc::clone(&s.journal), &h, broken, &config()).unwrap_err();
    assert_eq!(err.to_string(), "Task Scheduler isn't available: RPC down");
    assert_eq!(*connects.borrow(), 0);
    assert_eq!(session_count(&s.journal), before);
    assert!(s.journal.ops(10).unwrap().is_empty());
    // The refusals also hold under a caller's session (profiles), before any record.
    let mut h = host(&s.journal);
    h.other_user = Ok(true);
    let fake = FakeDefinitions::new();
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    assert_eq!(
        set_with(&safety, &h, &fake, &config())
            .unwrap_err()
            .to_string(),
        OTHER_ACCOUNT
    );
    assert_eq!(fake.writes.get(), 0);
    let refused = test_safety(Arc::clone(&s.journal), SESSION_LABEL, true);
    if !crate::is_elevated() {
        assert!(matches!(
            set_with(&refused, &host(&s.journal), &fake, &config()),
            Err(Error::NotElevated)
        ));
        assert_eq!(fake.writes.get(), 0);
    }
}

#[test]
fn an_account_whose_task_could_not_be_removed_is_refused() {
    let s = setup();
    let mut h = host(&s.journal);
    h.sid = Ok(ENTRA_SID.into());
    let path = task_path(ENTRA_SID);
    assert!(!is_own_task_path(&path), "the remover refuses this path");
    let fake = FakeDefinitions::new();

    let plan = plan_with(&h, &s.journal, Ok(&fake), &config()).unwrap();
    assert_eq!(plan.blocked_reason.as_deref(), Some(UNSUPPORTED_ACCOUNT));
    let status = status_with(&h, &s.journal, Ok(&fake), MutexPresence::Absent).unwrap();
    assert_eq!(status.blocked_reason.as_deref(), Some(UNSUPPORTED_ACCOUNT));
    assert!(!status.account.service_account);

    // Under a caller's session: no record and no Task Scheduler write.
    let safety = test_safety(Arc::clone(&s.journal), SESSION_LABEL, false);
    assert_eq!(
        set_with(&safety, &h, &fake, &config())
            .unwrap_err()
            .to_string(),
        UNSUPPORTED_ACCOUNT
    );
    let steps = profile_plan_with(&h, &s.journal, Ok(&fake), &choice()).unwrap();
    assert_eq!(steps[0].status, StepStatus::Skipped);
    assert_eq!(steps[0].reason, Some(StepReason::CannotChange));
    assert_eq!(steps[0].detail, UNSUPPORTED_ACCOUNT);
    let (results, filter) = profile_apply_with(&safety, &h, Ok(&fake), &choice()).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::Skipped);
    assert_eq!(results[0].details, [UNSUPPORTED_ACCOUNT]);
    assert!(filter.is_empty());
    assert!(s.journal.all_task_definitions().unwrap().is_empty());
    assert!(op_rows(&s.journal, OP_SET_TASK).is_empty());

    // A task at that path is neither started nor removed, and nothing is logged for it.
    let other = setup();
    let xml = turned_on(&other).task(PATH).unwrap().xml;
    let registered = FakeDefinitions::new()
        .with_folder(r"\Cairn", FOLDER_SDDL)
        .with_task(&path, &xml);
    assert_eq!(
        run_now_with(
            &s.journal,
            &h,
            || store_of(&registered),
            MutexPresence::Absent
        )
        .unwrap_err()
        .to_string(),
        UNSUPPORTED_ACCOUNT
    );
    for dry_run in [true, false] {
        assert_eq!(
            remove_unrecorded_with(&s.journal, &h, || store_of(&registered), dry_run)
                .unwrap_err()
                .to_string(),
            UNSUPPORTED_ACCOUNT
        );
    }
    assert!(op_rows(&s.journal, OP_RUN_NOW).is_empty());
    assert!(op_rows(&s.journal, OP_DELETE_TASK).is_empty());
    assert_eq!(fake.writes.get() + registered.writes.get(), 0);
    assert_eq!(registered.runs.get(), 0);
    assert!(registered.task(&path).is_some());

    // Service accounts keep their own refusal.
    h.sid = Ok("S-1-5-18".into());
    assert_eq!(h.refusal().as_deref(), Some(SERVICE_ACCOUNT));
}

#[test]
fn a_plan_writes_nothing_and_says_what_blocks() {
    let s = setup();
    let fake = FakeDefinitions::new();
    let before = session_count(&s.journal);
    let plan = plan_with(&host(&s.journal), &s.journal, Ok(&fake), &config()).unwrap();
    assert!(plan.creates && !plan.unchanged);
    assert_eq!(plan.blocked_reason, None);
    assert_eq!(plan.task_path, PATH);
    assert_eq!(plan.program, PROGRAM);
    assert_eq!(plan.account, r"TEST-PC\Test");
    assert_eq!(plan.next_run, "2026-10-04T12:00:00");
    assert!(plan
        .arguments
        .ends_with("--targets user_temp,windows_temp --sfc --dism"));
    assert_eq!(fake.writes.get(), 0);
    assert_eq!(session_count(&s.journal), before);

    let mut h = host(&s.journal);
    h.elevated = false;
    h.has_battery = true;
    let plan = plan_with(&h, &s.journal, Ok(&fake), &config()).unwrap();
    assert_eq!(plan.blocked_reason, None, "elevation is not a plan refusal");
    assert_eq!(plan.notes, [BATTERY_NOTE]);
    h.program_problem = Some("unsafe".into());
    assert_eq!(
        plan_with(&h, &s.journal, Ok(&fake), &config())
            .unwrap()
            .blocked_reason
            .as_deref(),
        Some("unsafe")
    );
    let plan = plan_with(
        &host(&s.journal),
        &s.journal,
        Err("RPC down".into()),
        &config(),
    )
    .unwrap();
    assert_eq!(
        plan.blocked_reason.as_deref(),
        Some("Task Scheduler isn't available: RPC down")
    );
    let unsafe_folder = FakeDefinitions::new().with_folder(r"\Cairn", ROOT_SDDL);
    let plan = plan_with(&host(&s.journal), &s.journal, Ok(&unsafe_folder), &config()).unwrap();
    assert_eq!(plan.blocked_reason.as_deref(), Some(FOLDER_UNSAFE));

    let on = turned_on(&s);
    let plan = plan_with(&host(&s.journal), &s.journal, Ok(&on), &config()).unwrap();
    assert!(!plan.creates && plan.unchanged);
    let mut other = config();
    other.time = ScheduleTime {
        hour: 18,
        minute: 30,
    };
    let plan = plan_with(&host(&s.journal), &s.journal, Ok(&on), &other).unwrap();
    assert!(!plan.creates && !plan.unchanged);
    assert_eq!(plan.next_run, "2026-10-04T18:30:00");
    let foreign = setup();
    let plan = plan_with(
        &host(&foreign.journal),
        &foreign.journal,
        Ok(&on),
        &config(),
    )
    .unwrap();
    assert_eq!(plan.blocked_reason.as_deref(), Some(FOREIGN_TASK));
}

#[test]
fn removing_an_unrecorded_task_logs_started_before_the_delete() {
    let s = setup();
    let other = setup();
    let on = turned_on(&other);
    let xml = on.task(PATH).unwrap().xml;
    let journal = Arc::clone(&s.journal);
    let fake = FakeDefinitions {
        before_write: Some(Box::new(move |op: &str| {
            if op.starts_with("delete ") {
                let rows = op_rows(&journal, OP_DELETE_TASK);
                assert_eq!(rows, [("started".to_string(), None)]);
            }
        })),
        ..FakeDefinitions::new()
    }
    .with_folder(r"\Cairn", FOLDER_SDDL)
    .with_task(PATH, &xml);
    let dry =
        remove_unrecorded_with(&s.journal, &host(&s.journal), || store_of(&fake), true).unwrap();
    assert_eq!(dry.removed, None);
    assert!(dry.planned);
    assert_eq!(fake.writes.get(), 0);
    assert!(s.journal.ops(10).unwrap().is_empty());
    let mut h = host(&s.journal);
    h.elevated = false;
    assert!(matches!(
        remove_unrecorded_with(&s.journal, &h, || store_of(&fake), false),
        Err(Error::NotElevated)
    ));
    let done =
        remove_unrecorded_with(&s.journal, &host(&s.journal), || store_of(&fake), false).unwrap();
    assert_eq!(done.removed, Some(true));
    assert!(!done.planned);
    assert!(fake.task(PATH).is_none());
    assert!(!fake.has_folder(r"\Cairn"));
    assert_eq!(
        op_rows(&s.journal, OP_DELETE_TASK),
        [("started".to_string(), None), ("deleted".to_string(), None)]
    );
    assert!(
        remove_unrecorded_with(&s.journal, &host(&s.journal), || store_of(&fake), false).is_err()
    );
    // A recorded task is turned off instead.
    assert!(remove_unrecorded_with(
        &other.journal,
        &host(&other.journal),
        || store_of(&on),
        false
    )
    .is_err());
    assert!(on.task(PATH).is_some());
}

#[test]
fn run_now_refuses_in_order_and_logs_requested_first() {
    let s = setup();
    let empty = FakeDefinitions::new();
    let refusal = |h: &Host, store: &FakeDefinitions, presence| {
        run_now_with(&s.journal, h, || store_of(store), presence)
            .unwrap_err()
            .to_string()
    };
    let mut h = host(&s.journal);
    h.elevated = false;
    assert_eq!(
        refusal(&h, &empty, MutexPresence::Absent),
        Error::NotElevated.to_string()
    );
    let mut h = host(&s.journal);
    h.other_user = Ok(true);
    assert_eq!(refusal(&h, &empty, MutexPresence::Absent), OTHER_ACCOUNT);
    assert_eq!(
        refusal(&host(&s.journal), &empty, MutexPresence::Absent),
        TURN_ON_FIRST_TEXT
    );

    let foreign = setup();
    let on = turned_on(&foreign);
    assert_eq!(
        refusal(&host(&s.journal), &on, MutexPresence::Absent),
        FOREIGN_TASK
    );

    let fake = turned_on(&s);
    let mut disabled = fake.task(PATH).unwrap();
    disabled.definition.enabled = false;
    let disabled_store = FakeDefinitions::new().with_task(PATH, &disabled.xml);
    disabled_store
        .tasks
        .borrow_mut()
        .values_mut()
        .for_each(|t| t.definition.enabled = false);
    assert_eq!(
        refusal(&host(&s.journal), &disabled_store, MutexPresence::Absent),
        TASK_DISABLED
    );

    let running = s
        .journal
        .insert_maintenance_run(&NewMaintenanceRun {
            origin: "task".into(),
            state: "running".into(),
            request_json: "{}".into(),
        })
        .unwrap();
    assert_eq!(
        refusal(&host(&s.journal), &fake, MutexPresence::Admin),
        ALREADY_RUNNING
    );
    s.journal
        .finish_maintenance_run(running, "completed", "{}", None)
        .unwrap();
    assert_eq!(
        refusal(&host(&s.journal), &fake, MutexPresence::Other),
        FOREIGN_LOCK_WARNING
    );
    let mut h = host(&s.journal);
    h.on_battery = true;
    assert_eq!(refusal(&h, &fake, MutexPresence::Absent), ON_BATTERY_TEXT);
    let mut h = host(&s.journal);
    h.program_problem = Some("unsafe program".into());
    assert_eq!(refusal(&h, &fake, MutexPresence::Absent), "unsafe program");
    let mut h = host(&s.journal);
    h.program = Ok(PathBuf::from(r"D:\Elsewhere\cairn-maintenance.exe"));
    assert_eq!(
        refusal(&h, &fake, MutexPresence::Absent),
        CHANGED_OUTSIDE_TEXT
    );
    assert_eq!(fake.runs.get(), 0);
    assert!(op_rows(&s.journal, OP_RUN_NOW).is_empty());

    let journal = Arc::clone(&s.journal);
    let watched = FakeDefinitions {
        before_write: Some(Box::new(move |op: &str| {
            if op.starts_with("run ") {
                assert_eq!(
                    op_rows(&journal, OP_RUN_NOW),
                    [("requested".to_string(), None)]
                );
            }
        })),
        ..FakeDefinitions::new()
    }
    .with_task(PATH, &fake.task(PATH).unwrap().xml);
    let done = run_now_with(
        &s.journal,
        &host(&s.journal),
        || store_of(&watched),
        MutexPresence::Absent,
    )
    .unwrap();
    assert!(done.requested);
    assert_eq!(done.task_path, PATH);
    assert_eq!(watched.runs.get(), 1);

    let failing = FakeDefinitions {
        fail_run: Some("the service refused".into()),
        ..FakeDefinitions::new()
    }
    .with_task(PATH, &fake.task(PATH).unwrap().xml);
    assert!(run_now_with(
        &s.journal,
        &host(&s.journal),
        || store_of(&failing),
        MutexPresence::Absent
    )
    .is_err());
    assert_eq!(
        op_rows(&s.journal, OP_RUN_NOW).last().unwrap(),
        &(
            "failed".to_string(),
            Some("the service refused".to_string())
        )
    );
}

#[test]
fn run_now_and_removal_refuse_before_task_scheduler_is_contacted() {
    let s = setup();
    let on = turned_on(&s);
    let connects = std::cell::Cell::new(0);
    let counting = || {
        connects.set(connects.get() + 1);
        Ok(Box::new(&on) as Box<dyn TaskDefinitionStore + '_>)
    };
    let mut unelevated = host(&s.journal);
    unelevated.elevated = false;
    assert!(matches!(
        run_now_with(&s.journal, &unelevated, counting, MutexPresence::Absent),
        Err(Error::NotElevated)
    ));
    assert!(matches!(
        remove_unrecorded_with(&s.journal, &unelevated, counting, false),
        Err(Error::NotElevated)
    ));
    let accounts: Vec<(HostChange, &str)> = vec![
        (Box::new(|h| h.sid = Ok("S-1-5-18".into())), SERVICE_ACCOUNT),
        (
            Box::new(|h| h.sid = Ok(ENTRA_SID.into())),
            UNSUPPORTED_ACCOUNT,
        ),
        (Box::new(|h| h.other_user = Ok(true)), OTHER_ACCOUNT),
        (
            Box::new(|h| h.other_user = Err("no session".into())),
            OTHER_ACCOUNT,
        ),
    ];
    for (change, expected) in accounts {
        let mut h = host(&s.journal);
        change(&mut h);
        let err = run_now_with(&s.journal, &h, counting, MutexPresence::Absent).unwrap_err();
        assert_eq!(err.to_string(), expected);
    }
    let mut entra = host(&s.journal);
    entra.sid = Ok(ENTRA_SID.into());
    let err = remove_unrecorded_with(&s.journal, &entra, counting, false).unwrap_err();
    assert_eq!(err.to_string(), UNSUPPORTED_ACCOUNT);
    assert_eq!(connects.get(), 0, "refused before connecting");
    assert_eq!(on.runs.get(), 0);

    // Past the refusals, a connection that fails is reported as such and nothing is logged.
    let broken =
        || -> Result<Box<dyn TaskDefinitionStore>> { Err(Error::Other("RPC down".into())) };
    let h = host(&s.journal);
    for err in [
        run_now_with(&s.journal, &h, broken, MutexPresence::Absent).unwrap_err(),
        remove_unrecorded_with(&s.journal, &h, broken, false).unwrap_err(),
        remove_unrecorded_with(&s.journal, &unelevated, broken, true).unwrap_err(),
    ] {
        assert_eq!(err.to_string(), "Task Scheduler isn't available: RPC down");
    }
    assert!(op_rows(&s.journal, OP_RUN_NOW).is_empty());
    assert!(op_rows(&s.journal, OP_DELETE_TASK).is_empty());

    // A dry-run removal only reads, so it connects without elevation.
    let err = remove_unrecorded_with(&s.journal, &unelevated, counting, true).unwrap_err();
    assert!(err.to_string().contains("turn maintenance off instead"));
    assert_eq!(connects.get(), 1);
}

fn insert_running(journal: &Journal) -> i64 {
    journal
        .insert_maintenance_run(&NewMaintenanceRun {
            origin: "task".into(),
            state: "running".into(),
            request_json: r#"{"targets":[],"system_file_check":true,"component_store_check":false,"origin":"task"}"#.into(),
        })
        .unwrap()
}

#[test]
fn a_run_is_in_progress_only_with_an_administrators_lock_and_a_running_row() {
    let s = setup();
    let fake = turned_on(&s);
    let h = host(&s.journal);
    let status = |presence| status_with(&h, &s.journal, Ok(&fake), presence).unwrap();
    // Admin lock without a running row.
    let idle = status(MutexPresence::Admin);
    assert!(!idle.running);
    assert!(idle.warnings.is_empty());
    insert_running(&s.journal);
    let busy = status(MutexPresence::Admin);
    assert!(busy.running);
    assert!(!busy.runs[0].stale);
    let foreign = status(MutexPresence::Other);
    assert!(!foreign.running);
    assert_eq!(foreign.warnings, [FOREIGN_LOCK_WARNING]);
    assert!(foreign.runs[0].stale);
    let gone = status(MutexPresence::Absent);
    assert!(!gone.running);
    assert!(gone.runs[0].stale);
    assert!(run_in_progress(&s.journal, MutexPresence::Admin).unwrap());
    assert!(!run_in_progress(&s.journal, MutexPresence::Other).unwrap());
}

#[test]
fn status_reads_the_task_and_reports_problems_as_warnings() {
    let s = setup();
    let h = host(&s.journal);
    let none = status_with(
        &h,
        &s.journal,
        Ok(&FakeDefinitions::new()),
        MutexPresence::Absent,
    )
    .unwrap();
    assert_eq!(none.task, None);
    assert!(!none.recorded);
    assert_eq!(none.task_path.as_deref(), Some(PATH));
    assert_eq!(none.defaults, ScheduleConfig::default_config());
    assert_eq!(none.targets.len(), 11);
    assert_eq!(none.blocked_reason, None);
    assert_eq!(none.account.sid.as_deref(), Some(SID));
    assert!(none.program.safe);

    let fake = turned_on(&s);
    let on = status_with(&h, &s.journal, Ok(&fake), MutexPresence::Absent).unwrap();
    let task = on.task.as_ref().unwrap();
    assert!(on.recorded && task.enabled);
    assert_eq!(task.config.as_ref(), Some(&config()));
    assert!(task.drift.is_empty());
    assert_eq!(task.last_result_text.as_deref(), Some("It hasn't run yet"));
    assert_eq!(task.last_result_hex.as_deref(), Some("0x00041303"));
    assert!(on.warnings.is_empty());

    let foreign = setup();
    let seen = status_with(
        &host(&foreign.journal),
        &foreign.journal,
        Ok(&fake),
        MutexPresence::Absent,
    )
    .unwrap();
    assert!(!seen.recorded && seen.task.is_some());
    assert_eq!(seen.blocked_reason.as_deref(), Some(FOREIGN_TASK));

    let mut moved = host(&s.journal);
    moved.program = Ok(PathBuf::from(r"D:\Other\cairn-maintenance.exe"));
    let drifted = status_with(&moved, &s.journal, Ok(&fake), MutexPresence::Absent).unwrap();
    assert_eq!(
        drifted.warnings,
        [format!(
            "The task was changed outside Cairn: it runs another copy of Cairn ({PROGRAM})."
        )]
    );

    let broken = status_with(
        &h,
        &s.journal,
        Err("RPC down".into()),
        MutexPresence::Absent,
    )
    .unwrap();
    assert_eq!(broken.task, None);
    assert_eq!(
        broken.warnings,
        ["Task Scheduler isn't available: RPC down"]
    );
    let failing = FakeDefinitions {
        fail_read: Some("access denied".into()),
        ..FakeDefinitions::new()
    };
    let unreadable = status_with(&h, &s.journal, Ok(&failing), MutexPresence::Absent).unwrap();
    assert_eq!(
        unreadable.warnings,
        ["Task Scheduler isn't available: access denied"]
    );

    let mut service = host(&s.journal);
    service.sid = Ok("S-1-5-18".into());
    let refused = status_with(
        &service,
        &s.journal,
        Ok(&FakeDefinitions::new()),
        MutexPresence::Absent,
    )
    .unwrap();
    assert!(refused.account.service_account);
    assert_eq!(refused.blocked_reason.as_deref(), Some(SERVICE_ACCOUNT));
}

#[test]
fn the_status_keys_match_the_contract() {
    let s = setup();
    let fake = turned_on(&s);
    insert_running(&s.journal);
    let status = status_with(
        &host(&s.journal),
        &s.journal,
        Ok(&fake),
        MutexPresence::Absent,
    )
    .unwrap();
    let json = serde_json::to_value(&status).unwrap();
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&json),
        [
            "account",
            "blocked_reason",
            "defaults",
            "elevated",
            "has_battery",
            "on_battery",
            "program",
            "recorded",
            "running",
            "runs",
            "targets",
            "task",
            "task_path",
            "warnings"
        ]
    );
    assert_eq!(
        keys(&json["account"]),
        ["name", "other_user", "service_account", "sid"]
    );
    assert_eq!(keys(&json["program"]), ["path", "reason", "safe"]);
    assert_eq!(
        keys(&json["task"]),
        [
            "config",
            "drift",
            "enabled",
            "last_result",
            "last_result_hex",
            "last_result_text",
            "last_run_time",
            "missed_runs",
            "next_run_time",
            "program",
            "state"
        ]
    );
    assert_eq!(
        keys(&json["targets"][0]),
        [
            "default_on",
            "description",
            "id",
            "per_user",
            "requires_admin",
            "servicing_guard",
            "title"
        ]
    );
    assert_eq!(json["task"]["state"], "ready");
    assert_eq!(json["defaults"]["time"], "12:00");
    let plan = plan_with(&host(&s.journal), &s.journal, Ok(&fake), &config()).unwrap();
    assert_eq!(
        keys(&serde_json::to_value(ScheduleResult::Plan(plan)).unwrap()),
        [
            "account",
            "arguments",
            "blocked_reason",
            "config",
            "creates",
            "next_run",
            "notes",
            "program",
            "task_path",
            "unchanged"
        ]
    );
    let report = ScheduleReport {
        session_id: Some(1),
        task_path: PATH.into(),
        outcome: ScheduleOutcome::Created,
        next_run: None,
    };
    let json = serde_json::to_value(ScheduleResult::Report(report)).unwrap();
    assert_eq!(
        keys(&json),
        ["next_run", "outcome", "session_id", "task_path"]
    );
    assert_eq!(json["outcome"], "created");
}

fn choice() -> MaintenanceChoice {
    MaintenanceChoice {
        enabled: true,
        day: Some(ScheduleDay::Sunday),
        time: Some("12:00".into()),
        clean: vec!["user_temp".into(), "windows_temp".into()],
        sfc_verify: true,
        dism_check: true,
    }
}

#[test]
fn profile_maintenance_row_is_opt_in() {
    let s = setup();
    let fake = FakeDefinitions::new();
    let steps = profile_plan_with(&host(&s.journal), &s.journal, Ok(&fake), &choice()).unwrap();
    assert_eq!(steps.len(), 1);
    let row = &steps[0];
    assert_eq!(row.key, "maintenance");
    assert_eq!(row.status, StepStatus::Change);
    assert_eq!(
        row.caution.as_deref(),
        Some(
            "Runs every Sunday at 12:00 PM with administrator rights and permanently deletes \
             files in: Temporary files, Windows temp folder. Undo removes the task; files already \
             deleted stay deleted."
        )
    );
    assert!(row
        .detail
        .starts_with("Turns on scheduled maintenance: every Sunday at 12:00 PM"));
    assert_eq!(fake.writes.get(), 0);
    let checks_only = MaintenanceChoice {
        clean: vec![],
        ..choice()
    };
    let steps = profile_plan_with(&host(&s.journal), &s.journal, Ok(&fake), &checks_only).unwrap();
    assert!(steps[0]
        .caution
        .as_deref()
        .unwrap()
        .ends_with("only runs read-only checks. Undo removes the task."));

    let on = turned_on(&s);
    let steps = profile_plan_with(&host(&s.journal), &s.journal, Ok(&on), &choice()).unwrap();
    assert_eq!(steps[0].status, StepStatus::Already);
    assert_eq!(steps[0].caution, None);
    let later = MaintenanceChoice {
        time: Some("18:45".into()),
        ..choice()
    };
    let steps = profile_plan_with(&host(&s.journal), &s.journal, Ok(&on), &later).unwrap();
    assert_eq!(steps[0].status, StepStatus::Change);
    assert!(steps[0].detail.ends_with(REMOVE_IT));
    // Undoing the profile leaves a schedule that existed before it running.
    let caution = steps[0].caution.as_deref().unwrap();
    assert!(caution.contains("6:45 PM"), "{caution}");
    assert!(!caution.contains("Undo removes the task"), "{caution}");
    assert!(
        caution.ends_with(
            "Temporary files, Windows temp folder. Undoing the profile keeps this schedule, and \
             files already deleted stay deleted."
        ),
        "{caution}"
    );
    let steps = profile_plan_with(&host(&s.journal), &s.journal, Ok(&on), &checks_only).unwrap();
    assert_eq!(steps[0].status, StepStatus::Change);
    let caution = steps[0].caution.as_deref().unwrap();
    assert!(
        caution.ends_with("only runs read-only checks. Undoing the profile keeps this schedule."),
        "{caution}"
    );
}

#[test]
fn profile_rows_that_cannot_apply_say_why() {
    let s = setup();
    let fake = FakeDefinitions::new();
    let h = host(&s.journal);
    let off = MaintenanceChoice {
        enabled: false,
        day: None,
        time: None,
        clean: vec![],
        sfc_verify: false,
        dism_check: false,
    };
    let steps = profile_plan_with(&h, &s.journal, Ok(&fake), &off).unwrap();
    assert_eq!(steps[0].status, StepStatus::Skipped);
    assert_eq!(steps[0].reason, Some(StepReason::Unsupported));
    assert_eq!(steps[0].detail, TURN_OFF_ELSEWHERE);
    let bad = MaintenanceChoice {
        clean: vec!["recycle_bin".into()],
        ..choice()
    };
    assert_eq!(
        profile_plan_with(&h, &s.journal, Ok(&fake), &bad).unwrap()[0].status,
        StepStatus::Skipped
    );
    let mut other = host(&s.journal);
    other.other_user = Ok(true);
    let steps = profile_plan_with(&other, &s.journal, Ok(&fake), &choice()).unwrap();
    assert_eq!(steps[0].reason, Some(StepReason::OtherAccount));
    let mut unsafe_program = host(&s.journal);
    unsafe_program.program_problem = Some("unsafe".into());
    let steps = profile_plan_with(&unsafe_program, &s.journal, Ok(&fake), &choice()).unwrap();
    assert_eq!(steps[0].reason, Some(StepReason::CannotChange));
    let steps = profile_plan_with(&h, &s.journal, Err("RPC down".into()), &choice()).unwrap();
    assert_eq!(steps[0].reason, Some(StepReason::Unreadable));
}

#[test]
fn profile_undo_removes_only_a_schedule_it_created() {
    let s = setup();
    let fake = FakeDefinitions::new();
    let h = host(&s.journal);
    let safety = test_safety(Arc::clone(&s.journal), "profile: Test", false);
    let (results, filter) = profile_apply_with(&safety, &h, Ok(&fake), &choice()).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::Applied);
    assert_eq!(filter.task_definitions, [PATH]);
    assert!(fake.task(PATH).is_some());

    let later = MaintenanceChoice {
        day: Some(ScheduleDay::Friday),
        ..choice()
    };
    let (results, filter) = profile_apply_with(&safety, &h, Ok(&fake), &later).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::Applied);
    assert!(results[0].details.contains(&REMOVE_IT.to_string()));
    assert!(filter.is_empty());
    let (results, filter) = profile_apply_with(&safety, &h, Ok(&fake), &later).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::AlreadySet);
    assert!(filter.is_empty());

    let mut other = host(&s.journal);
    other.other_user = Ok(true);
    let (results, _) = profile_apply_with(&safety, &other, Ok(&fake), &choice()).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::Skipped);
    let failing = FakeDefinitions {
        fail_register: Some("denied".into()),
        ..FakeDefinitions::new()
    };
    let fresh = setup();
    let safety = test_safety(Arc::clone(&fresh.journal), "profile: Test", false);
    let (results, filter) =
        profile_apply_with(&safety, &host(&fresh.journal), Ok(&failing), &choice()).unwrap();
    assert_eq!(results[0].outcome, ProfileOutcome::Failed);
    assert!(filter.is_empty());
}

#[test]
fn the_current_profile_section_is_the_recorded_enabled_schedule() {
    let s = setup();
    let h = host(&s.journal);
    assert_eq!(
        profile_current_with(&h, &s.journal, Ok(&FakeDefinitions::new())).unwrap(),
        None
    );
    let fake = turned_on(&s);
    assert_eq!(
        profile_current_with(&h, &s.journal, Ok(&fake)).unwrap(),
        Some(choice())
    );
    let foreign = setup();
    assert_eq!(
        profile_current_with(&host(&foreign.journal), &foreign.journal, Ok(&fake)).unwrap(),
        None
    );
    assert_eq!(
        profile_current_with(&h, &s.journal, Err("RPC down".into())).unwrap(),
        None
    );
    fake.tasks
        .borrow_mut()
        .values_mut()
        .for_each(|t| t.definition.enabled = false);
    assert_eq!(
        profile_current_with(&h, &s.journal, Ok(&fake)).unwrap(),
        None
    );
}

#[test]
fn twelve_hour_labels() {
    for (hour, minute, label) in [
        (0, 0, "12:00 AM"),
        (9, 5, "9:05 AM"),
        (12, 30, "12:30 PM"),
        (23, 45, "11:45 PM"),
    ] {
        assert_eq!(time_label(ScheduleTime { hour, minute }), label);
    }
}
