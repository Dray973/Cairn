//! Scheduled maintenance through the public API. Read-only: journals live in temporary
//! folders, Task Scheduler is only read, and nothing is registered, run or deleted (the one
//! test that deletes through Task Scheduler is ignored and runs only by exact name).

use std::sync::Arc;

use optimizer_core::maintenance::{
    self, RunOrigin, RunRequest, RunState, ScheduleConfig, ScheduleDay, ScheduleResult,
    ScheduleTime,
};
use optimizer_core::safety::state_log::{Journal, NewMaintenanceRun, NewTaskDefinitionRecord};
use optimizer_core::safety::{rollback_filtered, RollbackFilter};
use optimizer_core::win::mutex::{self, MutexPresence};

fn temp_journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    (dir, journal)
}

#[test]
fn a_dry_run_opens_no_session_and_creates_no_task() {
    let (_dir, journal) = temp_journal();
    let config = ScheduleConfig {
        day: ScheduleDay::Tuesday,
        time: ScheduleTime {
            hour: 3,
            minute: 30,
        },
        targets: vec!["windows_temp".into(), "user_temp".into()],
        system_file_check: true,
        component_store_check: false,
    };
    let result = maintenance::set_schedule(Arc::clone(&journal), &config, true).unwrap();
    let ScheduleResult::Plan(plan) = result else {
        panic!("a dry run returns a plan");
    };
    assert_eq!(plan.config.targets, ["user_temp", "windows_temp"]);
    assert!(plan.next_run.ends_with("T03:30:00"), "{}", plan.next_run);
    assert!(plan
        .arguments
        .contains(&format!("--journal \"{}\"", journal.path().display())));
    assert!(journal.sessions().unwrap().is_empty());
    assert!(journal.ops(10).unwrap().is_empty());
    assert!(journal.active_task_definitions().unwrap().is_empty());
    let json = serde_json::to_value(ScheduleResult::Plan(plan)).unwrap();
    assert!(json.get("blocked_reason").is_some());
}

#[test]
fn a_schedule_with_the_recycle_bin_is_refused() {
    let (_dir, journal) = temp_journal();
    let config = ScheduleConfig {
        targets: vec!["recycle_bin".into()],
        ..ScheduleConfig::default_config()
    };
    let err = maintenance::set_schedule(Arc::clone(&journal), &config, true).unwrap_err();
    assert!(err.to_string().contains("Recycle Bin"), "{err}");
    assert!(journal.sessions().unwrap().is_empty());
}

#[test]
fn status_reads_without_error() {
    let (_dir, journal) = temp_journal();
    let status = maintenance::status_in(&journal).unwrap();
    assert!(!status.recorded);
    assert!(status.runs.is_empty());
    assert!(!status.running);
    assert_eq!(status.targets.len(), 11);
    assert_eq!(status.defaults, ScheduleConfig::default_config());
    assert!(status
        .task_path
        .unwrap()
        .starts_with(r"\Cairn\Maintenance-S-1-5-"));
    assert!(journal.sessions().unwrap().is_empty());
    let json = serde_json::to_value(maintenance::status_in(&journal).unwrap()).unwrap();
    assert_eq!(json["defaults"]["day"], "sunday");
}

#[test]
fn a_run_plan_lists_its_steps() {
    let plan = maintenance::plan_run(&RunRequest {
        targets: vec!["user_temp".into()],
        system_file_check: false,
        component_store_check: true,
        origin: RunOrigin::Cli,
    })
    .unwrap();
    let steps: Vec<&str> = plan.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(steps, ["cleanup", "component_store"]);
}

#[test]
fn runs_and_acknowledgements_read_the_journal() {
    let (_dir, journal) = temp_journal();
    assert!(maintenance::runs(&journal, 10).unwrap().is_empty());
    assert!(!maintenance::acknowledge(&journal, 1).unwrap());
    assert!(maintenance::open_log(&journal, 1).is_err());

    // A run row left `running` while no administrator holds the run lock ended without
    // finishing: marking it as seen closes it as interrupted. The lock is only read here.
    let run_id = journal
        .insert_maintenance_run(&NewMaintenanceRun {
            origin: "task".into(),
            state: "running".into(),
            request_json: "{}".into(),
        })
        .unwrap();
    assert_eq!(run_id, 1);
    if mutex::presence(maintenance::RUN_MUTEX) == MutexPresence::Admin {
        // A real run holds the lock right now, so the row counts as in progress.
        assert!(!maintenance::acknowledge(&journal, run_id).unwrap());
        return;
    }
    assert!(maintenance::runs(&journal, 10).unwrap()[0].stale);
    assert!(maintenance::acknowledge(&journal, run_id).unwrap());
    let run = maintenance::runs(&journal, 10).unwrap().remove(0);
    assert_eq!(run.state, RunState::Interrupted);
    assert!(run.acknowledged && !run.stale && run.ended_at.is_some());
    let rows = journal.ops(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].op.as_str(), rows[0].outcome.as_str()),
        ("maintenance", "interrupted")
    );
    assert!(!maintenance::acknowledge(&journal, run_id).unwrap());
}

#[test]
fn no_run_is_in_progress_for_a_missing_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    assert!(!maintenance::run_in_progress(&path).unwrap());
    assert!(!path.exists());
}

/// Rolls back a record of a task that does not exist through the live Task Scheduler: the
/// task is reported not found and the record closed. Needs an elevated process; run by exact
/// name only.
#[test]
#[ignore = "reaches the live Task Scheduler; run elevated by exact name"]
fn sandbox_task_definition_rollback_reports_not_found() {
    let (_dir, journal) = temp_journal();
    let path = r"\PCOptimizerSelfTest\NoSuchMaintenance";
    let session = journal.begin_session("sandbox", "test").unwrap();
    assert!(journal
        .record_task_definition(
            session,
            &NewTaskDefinitionRecord {
                path: path.to_string(),
                purpose: "maintenance".to_string(),
                folder_created: false,
            },
        )
        .unwrap());
    journal.end_session(session).unwrap();
    let filter = RollbackFilter {
        task_definitions: vec![path.to_string()],
        ..RollbackFilter::default()
    };
    let report = rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.task_definitions_deleted, 1);
    assert!(journal.active_task_definitions().unwrap().is_empty());
    let row = journal
        .ops(10)
        .unwrap()
        .into_iter()
        .find(|r| r.op == "rollback_task_definition")
        .unwrap();
    assert_eq!(row.outcome, "not_found");
    assert_eq!(row.detail.as_deref(), Some("folder kept"));
}
