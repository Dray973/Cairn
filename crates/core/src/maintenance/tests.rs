use std::path::Path;

use super::*;
use crate::maintenance::task::fake::FakeDefinitions;
use crate::maintenance::task::{task_xml, TaskSpec, FOLDER_SDDL};
use crate::safety::state_log::{MaintenanceRunRow, NewMaintenanceRun, NewTaskDefinitionRecord};

const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
const OTHER_SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";

fn xml(path: &str, sid: &str, journal: &Path, enabled: bool) -> String {
    let config = ScheduleConfig::default_config();
    task_xml(&TaskSpec {
        path: path.to_string(),
        user_sid: sid.to_string(),
        program: PathBuf::from(r"C:\Program Files\Cairn\cairn-maintenance.exe"),
        arguments: config::task_arguments(journal, &config).unwrap(),
        working_dir: PathBuf::from(r"C:\Windows\System32"),
        start: chrono::NaiveDate::from_ymd_opt(2026, 10, 4)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        day: config.day,
        enabled,
    })
}

fn record(journal: &Journal, path: &str) {
    let session = journal
        .begin_session("maintenance schedule", "test")
        .unwrap();
    journal
        .record_task_definition(
            session,
            &NewTaskDefinitionRecord {
                path: path.to_string(),
                purpose: PURPOSE.to_string(),
                folder_created: true,
            },
        )
        .unwrap();
}

#[test]
fn task_paths_carry_the_sid() {
    assert_eq!(
        task_path(SID),
        r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001"
    );
    assert!(task::is_own_task_path(&task_path(SID)));
}

#[test]
fn only_optimizer_variables_are_dropped() {
    let names = [
        "OPTIMIZER_DATA_DIR",
        "optimizer_journal",
        "Optimizer_Log",
        "OPTIMIZER",
        "PATH",
        "MY_OPTIMIZER_X",
        "TEMP",
    ]
    .map(OsString::from);
    assert_eq!(
        optimizer_variables(names.into_iter()),
        ["OPTIMIZER_DATA_DIR", "optimizer_journal", "Optimizer_Log"].map(OsString::from)
    );
}

#[test]
fn scheduled_arguments_are_refused_unless_exact() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("data").join("journal.db");
    let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
    let journal_text = journal.display().to_string();
    let (path, data_dir, request) = scheduled_request(args(&[
        "--journal",
        &journal_text,
        "--targets",
        "user_temp",
        "--sfc",
    ]))
    .unwrap();
    assert_eq!(path, journal);
    assert_eq!(data_dir, dir.path().join("data"));
    assert_eq!(request.targets, ["user_temp"]);
    assert!(request.system_file_check && !request.component_store_check);
    assert_eq!(request.origin, RunOrigin::Task);
    for bad in [
        vec![],
        args(&["--journal", &journal_text]),
        args(&["maintenance", "run", "--scheduled"]),
        args(&["--journal", &journal_text, "--sfc", "--sfc"]),
        args(&["--journal", "journal.db", "--sfc"]),
        args(&["--journal", &journal_text, "--targets", "recycle_bin"]),
    ] {
        assert!(scheduled_request(bad.clone()).is_none(), "{bad:?}");
    }
    // A hard-linked journal fails the data-folder check.
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, b"x").unwrap();
    std::fs::hard_link(&journal, dir.path().join("copy.db")).unwrap();
    assert!(scheduled_request(args(&["--journal", &journal_text, "--dism"])).is_none());
    assert!(
        !dir.path().join("data").join("maintenance").exists(),
        "nothing was written"
    );
}

#[test]
fn run_in_progress_never_creates_a_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.db");
    assert!(!run_in_progress(&journal).unwrap());
    assert!(!journal.exists());
}

fn running_run() -> NewMaintenanceRun {
    NewMaintenanceRun {
        origin: "task".into(),
        state: "running".into(),
        request_json: "{}".into(),
    }
}

fn run_row(journal: &Journal, id: i64) -> MaintenanceRunRow {
    journal
        .maintenance_runs(10)
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap()
}

#[test]
fn acknowledging_a_run_that_ended_without_finishing_closes_it_first() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().join("journal.db")).unwrap();
    let run_id = journal.insert_maintenance_run(&running_run()).unwrap();
    // The row of a run that took the run lock after the lock was read.
    let newer = journal.insert_maintenance_run(&running_run()).unwrap();

    // An administrator holds the run lock: the run is in progress and stays as it is.
    assert!(!acknowledge_with(&journal, run_id, MutexPresence::Admin).unwrap());
    let row = run_row(&journal, run_id);
    assert_eq!(row.state, "running");
    assert_eq!(row.acknowledged_at, None);
    assert!(journal.ops(10).unwrap().is_empty());

    // Nobody holds it: the run ended without finishing, so its row is closed and marked.
    // Only that run's row: the newer run keeps running.
    assert!(acknowledge_with(&journal, run_id, MutexPresence::Absent).unwrap());
    let row = run_row(&journal, run_id);
    assert_eq!(row.state, "interrupted");
    assert!(row.ended_at.is_some() && row.acknowledged_at.is_some());
    let seen = MaintenanceRun::from_row(&row, MutexPresence::Absent);
    assert_eq!(seen.state, RunState::Interrupted);
    assert!(seen.acknowledged && !seen.stale);
    let other = run_row(&journal, newer);
    assert_eq!(other.state, "running");
    assert_eq!(other.ended_at, None);
    assert_eq!(other.acknowledged_at, None);
    let rows = journal.ops(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, None);
    assert_eq!(rows[0].op, OP_RUN);
    assert_eq!(rows[0].target, format!("maintenance run {run_id}"));
    assert_eq!(rows[0].outcome, "interrupted");
    assert_eq!(rows[0].detail.as_deref(), Some(run::INTERRUPTED_DETAIL));

    // Marking it again changes nothing.
    assert!(!acknowledge_with(&journal, run_id, MutexPresence::Absent).unwrap());
    assert_eq!(journal.ops(10).unwrap().len(), 1);
    assert_eq!(run_row(&journal, newer).state, "running");

    // A lock another program holds is not a run either.
    assert!(acknowledge_with(&journal, newer, MutexPresence::Other).unwrap());
    assert_eq!(run_row(&journal, newer).state, "interrupted");
    let rows = journal.ops(10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].target, format!("maintenance run {newer}"));

    // A finished run is marked as before, without a row; an unknown run is not.
    let done = journal.insert_maintenance_run(&running_run()).unwrap();
    journal
        .finish_maintenance_run(done, "completed", "{}", None)
        .unwrap();
    assert!(acknowledge_with(&journal, done, MutexPresence::Absent).unwrap());
    assert_eq!(run_row(&journal, done).state, "completed");
    assert!(!acknowledge_with(&journal, done + 1, MutexPresence::Absent).unwrap());
    assert_eq!(journal.ops(10).unwrap().len(), 2);
}

#[test]
fn acknowledging_never_closes_a_run_that_started_after_the_lock_was_read() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().join("journal.db")).unwrap();
    let stale = journal.insert_maintenance_run(&running_run()).unwrap();
    // The lock was read as free. Then a run took it and, as every run does, closed the
    // stale row before inserting its own.
    run::close_interrupted_runs(&journal).unwrap();
    let started = journal.insert_maintenance_run(&running_run()).unwrap();

    assert!(acknowledge_with(&journal, stale, MutexPresence::Absent).unwrap());
    let row = run_row(&journal, stale);
    assert_eq!(row.state, "interrupted");
    assert!(row.acknowledged_at.is_some());
    let row = run_row(&journal, started);
    assert_eq!(row.state, "running", "the run in progress is left alone");
    assert_eq!(row.ended_at, None);
    let rows = journal.ops(10).unwrap();
    assert_eq!(rows.len(), 1, "the stale run is closed once");
    assert_eq!(rows[0].target, format!("maintenance run {stale}"));
    assert_eq!(rows[0].outcome, "interrupted");
}

#[test]
fn the_uninstaller_logs_started_before_each_delete_and_marks_records() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    let own = task_path(SID);
    let other = task_path(OTHER_SID);
    record(&journal, &own);
    let watched = Arc::clone(&journal);
    let fake = FakeDefinitions {
        before_write: Some(Box::new(move |op: &str| {
            if let Some(path) = op.strip_prefix("delete ") {
                let target = task_definition_target(path);
                let last = watched
                    .ops(1)
                    .unwrap()
                    .into_iter()
                    .next()
                    .expect("a row before the delete");
                assert_eq!(last.op, OP_DELETE_TASK);
                assert_eq!(last.target, target);
                assert_eq!(last.outcome, "started");
            }
        })),
        ..FakeDefinitions::new()
    }
    .with_folder(r"\Cairn", FOLDER_SDDL)
    .with_task(&own, &xml(&own, SID, journal.path(), true))
    .with_task(&other, &xml(&other, OTHER_SID, journal.path(), true))
    .with_task(
        r"\Cairn\Unrelated",
        &xml(r"\Cairn\Unrelated", SID, journal.path(), true),
    );
    let text = remove_all_with(Ok(&fake), Some(&journal), None);
    assert!(fake.task(&own).is_none() && fake.task(&other).is_none());
    assert!(
        fake.task(r"\Cairn\Unrelated").is_some(),
        "not a maintenance task"
    );
    assert!(text.contains(&format!("{own}: deleted")), "{text}");
    assert!(text.contains(r"\Cairn kept: not empty"), "{text}");
    assert!(journal.active_task_definitions().unwrap().is_empty());
    let rows: Vec<(String, String)> = journal
        .ops(10)
        .unwrap()
        .into_iter()
        .rev()
        .map(|r| (r.target, r.outcome))
        .collect();
    assert_eq!(
        rows,
        [
            (task_definition_target(&own), "started".to_string()),
            (task_definition_target(&own), "deleted".to_string()),
            (task_definition_target(&other), "started".to_string()),
            (task_definition_target(&other), "deleted".to_string()),
        ]
    );
}

#[test]
fn the_uninstaller_works_without_a_journal() {
    let own = task_path(SID);
    let journal_path = Path::new(r"C:\Users\Test\AppData\Local\PCOptimizer\journal.db");
    let fake = FakeDefinitions::new()
        .with_folder(r"\Cairn", FOLDER_SDDL)
        .with_task(&own, &xml(&own, SID, journal_path, true));
    let text = remove_all_with(Ok(&fake), None, Some("no rows".into()));
    assert!(fake.task(&own).is_none());
    assert!(!fake.has_folder(r"\Cairn"));
    assert_eq!(text, format!(r"no rows; {own}: deleted; \Cairn removed"));
    assert_eq!(
        remove_all_with(Ok(&FakeDefinitions::new()), None, None),
        "no maintenance tasks"
    );
    assert_eq!(
        remove_all_with(Err("RPC down".into()), None, None),
        "Task Scheduler isn't available: RPC down"
    );
    let failing = FakeDefinitions {
        fail_delete: Some("access denied".into()),
        ..FakeDefinitions::new()
    }
    .with_folder(r"\Cairn", FOLDER_SDDL)
    .with_task(&own, &xml(&own, SID, journal_path, true));
    let text = remove_all_with(Ok(&failing), None, None);
    assert!(
        text.contains(&format!("{own}: failed (access denied)")),
        "{text}"
    );
}

#[test]
fn the_doctor_line_names_the_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().join("journal.db")).unwrap();
    let own = task_path(SID);
    let empty = FakeDefinitions::new();
    assert_eq!(
        doctor_line_with(SID, Some(&journal), Ok(&empty), journal.path()),
        "off"
    );
    let on = FakeDefinitions::new().with_task(&own, &xml(&own, SID, journal.path(), true));
    assert_eq!(
        doctor_line_with(SID, Some(&journal), Ok(&on), journal.path()),
        "not recorded"
    );
    assert_eq!(
        doctor_line_with(SID, None, Ok(&on), journal.path()),
        "not recorded"
    );
    record(&journal, &own);
    assert_eq!(
        doctor_line_with(SID, Some(&journal), Ok(&on), journal.path()),
        "on (every Sunday at 12:00)"
    );
    let off = FakeDefinitions::new().with_task(&own, &xml(&own, SID, journal.path(), false));
    assert_eq!(
        doctor_line_with(SID, Some(&journal), Ok(&off), journal.path()),
        "on (every Sunday at 12:00; disabled in Task Scheduler)"
    );
    assert_eq!(
        doctor_line_with(SID, Some(&journal), Err("RPC down".into()), journal.path()),
        "not available: Task Scheduler isn't available: RPC down"
    );
}

#[test]
fn the_doctor_line_reads_this_pc() {
    // Read-only: Task Scheduler lookups; cargo points the default journal at test data.
    let line = doctor_line();
    assert!(
        line == "off"
            || line == "not recorded"
            || line.starts_with("on (")
            || line.starts_with("not available: "),
        "{line}"
    );
}

#[test]
fn a_dry_run_opens_no_session_and_registers_nothing() {
    // Read-only: a plan reads Task Scheduler and the journal in a temporary folder.
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    let result = set_schedule(
        Arc::clone(&journal),
        &ScheduleConfig::default_config(),
        true,
    )
    .unwrap();
    let ScheduleResult::Plan(plan) = result else {
        panic!("a dry run returns a plan");
    };
    assert!(plan.task_path.starts_with(r"\Cairn\Maintenance-S-1-5-"));
    assert!(
        plan.blocked_reason.is_some(),
        "a dev build is never a safe program location"
    );
    assert!(journal.sessions().unwrap().is_empty());
    assert!(journal.ops(10).unwrap().is_empty());
    let status = status_in(&journal).unwrap();
    assert_eq!(status.task_path.as_deref(), Some(plan.task_path.as_str()));
    assert!(!status.program.safe);
    assert!(journal.sessions().unwrap().is_empty());
}
