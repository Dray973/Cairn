//! Scheduled task actions: the enabled flag of Windows tasks, changed through Task
//! Scheduler after the task's current flag is journaled.
//!
//! The logic runs against a `TaskStore`, which is Task Scheduler itself in production and
//! a fake in tests. One `TaskConnection` is shared by every task action of a scan or an
//! apply, so Task Scheduler is connected to at most once per operation. Tasks under
//! `\Microsoft\Windows\` are machine-wide, so no per-user check applies.

use std::cell::OnceCell;

use tracing::{info, warn};

use super::catalog::ScheduledTaskAction;
use super::{ActionState, ActionStatus};
use crate::safety::state_log::{
    scheduled_task_target, JournalTable, NewScheduledTaskRecord, ScheduledTaskRecord,
};
use crate::safety::{MutationOutcome, Safety};
use crate::win::task_scheduler::{TaskRunState, TaskScheduler};
use crate::{Error, Result};

pub(crate) const OP_SET: &str = "set_scheduled_task";
pub(crate) const OP_ROLLBACK: &str = "rollback_scheduled_task";

/// The state of one registered task that the actions compare with their target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskInfo {
    pub enabled: bool,
    pub running: bool,
}

/// Registered tasks addressed by full path.
pub(crate) trait TaskStore {
    /// The task's state, or `None` when it does not exist.
    fn query(&self, path: &str) -> Result<Option<TaskInfo>>;
    /// Sets the task's enabled flag. Fails when the task does not exist.
    fn set_enabled(&self, path: &str, enabled: bool) -> Result<()>;
}

impl<T: TaskStore + ?Sized> TaskStore for &T {
    fn query(&self, path: &str) -> Result<Option<TaskInfo>> {
        (**self).query(path)
    }

    fn set_enabled(&self, path: &str, enabled: bool) -> Result<()> {
        (**self).set_enabled(path, enabled)
    }
}

impl TaskStore for TaskScheduler {
    fn query(&self, path: &str) -> Result<Option<TaskInfo>> {
        let Some(task) = self.task(path)? else {
            return Ok(None);
        };
        Ok(Some(TaskInfo {
            enabled: task.enabled()?,
            running: task.state()? == TaskRunState::Running,
        }))
    }

    fn set_enabled(&self, path: &str, enabled: bool) -> Result<()> {
        match self.task(path)? {
            Some(task) => task.set_enabled(enabled),
            None => Err(Error::Other(format!(
                "scheduled task {path} does not exist"
            ))),
        }
    }
}

/// Connects to Task Scheduler at most once, on first use; single-threaded.
#[derive(Debug, Default)]
pub(crate) struct TaskConnection {
    inner: OnceCell<std::result::Result<TaskScheduler, String>>,
}

impl TaskConnection {
    pub fn new() -> Self {
        Self::default()
    }

    /// A connection whose connect already failed with `message`.
    #[cfg(test)]
    pub(crate) fn failed(message: &str) -> Self {
        let conn = Self::default();
        let _ = conn.inner.set(Err(message.to_string()));
        conn
    }

    /// The connection, opened on the first call. A failed connect is kept, so every later
    /// call fails with the same message without trying again.
    pub fn store(&self) -> Result<&TaskScheduler> {
        let connection = self.inner.get_or_init(|| {
            TaskScheduler::connect().map_err(|e| format!("cannot connect to Task Scheduler: {e}"))
        });
        match connection {
            Ok(scheduler) => Ok(scheduler),
            Err(message) => Err(Error::Other(message.clone())),
        }
    }
}

/// Outcome of writing a recorded enabled flag back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRestore {
    /// The recorded flag was written.
    Restored,
    /// The task already had the recorded flag, so nothing was written.
    AlreadyInState,
    /// The task no longer exists.
    Missing,
}

/// `scheduled task <path>`, the target used in reports, logs and the journal.
pub fn describe(action: &ScheduledTaskAction) -> String {
    scheduled_task_target(action.path)
}

fn enabled_text(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

/// Reads the task and compares its enabled flag with the target. Read-only.
pub fn status(action: &ScheduledTaskAction) -> Result<ActionStatus> {
    status_in(&TaskConnection::new(), action)
}

/// Journals the task's enabled flag, then sets the target flag. A missing task is skipped
/// and a task already at the target is left alone.
pub fn apply(safety: &Safety, action: &ScheduledTaskAction) -> Result<MutationOutcome> {
    safety.ensure_elevated()?;
    apply_in(safety, &TaskConnection::new(), action)
}

/// [`status`] through a shared connection.
pub(crate) fn status_in(
    conn: &TaskConnection,
    action: &ScheduledTaskAction,
) -> Result<ActionStatus> {
    status_with(conn.store()?, action)
}

/// [`apply`] through a shared connection. The elevation check runs before connecting.
pub(crate) fn apply_in(
    safety: &Safety,
    conn: &TaskConnection,
    action: &ScheduledTaskAction,
) -> Result<MutationOutcome> {
    safety.ensure_elevated()?;
    apply_with(safety, conn.store()?, action)
}

/// Applied when the task's enabled flag equals the target; unavailable when the task does
/// not exist on this PC.
pub(crate) fn status_with(
    store: &dyn TaskStore,
    action: &ScheduledTaskAction,
) -> Result<ActionStatus> {
    let target = describe(action);
    let Some(info) = store.query(action.path)? else {
        return Ok(ActionStatus::new(
            ActionState::Unavailable,
            format!("{target} is not on this PC"),
        ));
    };
    let state = if info.enabled == action.enabled {
        ActionState::Applied
    } else {
        ActionState::NotApplied
    };
    Ok(ActionStatus::new(
        state,
        format!(
            "{target}: {}{}; target {}",
            enabled_text(info.enabled),
            if info.running { ", running" } else { "" },
            enabled_text(action.enabled)
        ),
    ))
}

/// Sets the task's enabled flag to the target. A missing task is skipped and a task already
/// at the target is left alone; neither is journaled. Otherwise the current flag is
/// journaled before the write (an older active baseline is kept). When the write fails,
/// the record this call inserted is withdrawn again, so the journal never claims a change
/// that did not happen. Disabling a task does not stop a run in progress.
pub(crate) fn apply_with(
    safety: &Safety,
    store: &dyn TaskStore,
    action: &ScheduledTaskAction,
) -> Result<MutationOutcome> {
    safety.ensure_elevated()?;
    let target = describe(action);
    let Some(info) = store.query(action.path)? else {
        let reason = format!("{target} is not on this PC");
        safety.log_op(OP_SET, &target, "skipped", Some(&reason))?;
        return Ok(MutationOutcome::Skipped(reason));
    };
    if info.enabled == action.enabled {
        safety.log_op(OP_SET, &target, "already_in_desired_state", None)?;
        return Ok(MutationOutcome::AlreadyInDesiredState);
    }

    let captured = safety.record_scheduled_task(&NewScheduledTaskRecord {
        path: action.path.to_string(),
        was_enabled: info.enabled,
    })?;
    if let Err(e) = store.set_enabled(action.path, action.enabled) {
        if captured {
            withdraw(safety, action.path);
        }
        safety.log_op(OP_SET, &target, "failed", Some(&e.to_string()))?;
        return Err(e);
    }
    safety.log_op(
        OP_SET,
        &target,
        "applied",
        Some(enabled_text(action.enabled)),
    )?;
    info!(
        task = action.path,
        enabled = action.enabled,
        "scheduled task changed"
    );
    Ok(MutationOutcome::Applied)
}

/// Marks this session's active record of `path` as reverted. Best effort: a journal error
/// is only logged.
fn withdraw(safety: &Safety, path: &str) {
    let active = match safety.journal().active_scheduled_tasks() {
        Ok(active) => active,
        Err(e) => {
            warn!(task = path, error = %e, "cannot read the scheduled task journal to withdraw a record");
            return;
        }
    };
    let own = active
        .iter()
        .find(|r| r.session_id == safety.session_id() && r.path.eq_ignore_ascii_case(path));
    if let Some(rec) = own {
        if let Err(e) = safety
            .journal()
            .mark_reverted(JournalTable::ScheduledTask, rec.id)
        {
            warn!(task = path, error = %e, "cannot withdraw the scheduled task journal record");
        }
    }
}

/// Writes the recorded enabled flag back, unless the task already has it or is gone.
pub(crate) fn restore_with(
    store: &dyn TaskStore,
    rec: &ScheduledTaskRecord,
) -> Result<TaskRestore> {
    match store.query(&rec.path)? {
        None => Ok(TaskRestore::Missing),
        Some(info) if info.enabled == rec.was_enabled => Ok(TaskRestore::AlreadyInState),
        Some(_) => {
            store.set_enabled(&rec.path, rec.was_enabled)?;
            info!(task = %rec.path, enabled = rec.was_enabled, "scheduled task restored");
            Ok(TaskRestore::Restored)
        }
    }
}

/// An in-memory [`TaskStore`] for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use super::{TaskInfo, TaskStore};
    use crate::{Error, Result};

    /// Called with the path and flag at the start of every `set_enabled`.
    pub(crate) type WriteHook = Box<dyn Fn(&str, bool)>;

    /// Tasks keyed by lowercase path.
    #[derive(Default)]
    pub(crate) struct FakeTasks {
        pub tasks: RefCell<BTreeMap<String, TaskInfo>>,
        /// Calls of `set_enabled`, including failed ones.
        pub writes: Cell<usize>,
        /// Every `set_enabled` fails with this message.
        pub fail_writes: Option<String>,
        pub before_write: Option<WriteHook>,
    }

    impl FakeTasks {
        /// One task per `(path, enabled, running)`.
        pub(crate) fn with(tasks: &[(&str, bool, bool)]) -> FakeTasks {
            let fake = FakeTasks::default();
            for &(path, enabled, running) in tasks {
                fake.tasks
                    .borrow_mut()
                    .insert(path.to_ascii_lowercase(), TaskInfo { enabled, running });
            }
            fake
        }

        pub(crate) fn get(&self, path: &str) -> Option<TaskInfo> {
            self.tasks.borrow().get(&path.to_ascii_lowercase()).copied()
        }

        /// Changes a task's flag without counting a write, as Windows or the user would.
        pub(crate) fn set(&self, path: &str, enabled: bool) {
            if let Some(info) = self.tasks.borrow_mut().get_mut(&path.to_ascii_lowercase()) {
                info.enabled = enabled;
            }
        }
    }

    impl TaskStore for FakeTasks {
        fn query(&self, path: &str) -> Result<Option<TaskInfo>> {
            Ok(self.get(path))
        }

        fn set_enabled(&self, path: &str, enabled: bool) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            if let Some(hook) = &self.before_write {
                hook(path, enabled);
            }
            if let Some(message) = &self.fail_writes {
                return Err(Error::Other(message.clone()));
            }
            match self.tasks.borrow_mut().get_mut(&path.to_ascii_lowercase()) {
                Some(info) => {
                    info.enabled = enabled;
                    Ok(())
                }
                None => Err(Error::Other(format!(
                    "scheduled task {path} does not exist"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::fake::FakeTasks;
    use super::*;
    use crate::debloat::catalog::{self, Action};
    use crate::debloat::engine::catalog_view;
    use crate::safety::state_log::Journal;
    use crate::safety::test_safety;

    const TASK: &str = r"\PCOptimizerSelfTest\Unit";
    const OTHER: &str = r"\PCOptimizerSelfTest\Other";

    static DISABLE: ScheduledTaskAction = ScheduledTaskAction {
        path: TASK,
        enabled: false,
    };

    fn journal(dir: &tempfile::TempDir) -> Arc<Journal> {
        Arc::new(Journal::open(dir.path().join("journal.db")).unwrap())
    }

    fn session(journal: &Arc<Journal>) -> Safety {
        test_safety(journal.clone(), "scheduled task unit", false)
    }

    fn disable(path: &'static str) -> ScheduledTaskAction {
        ScheduledTaskAction {
            path,
            enabled: false,
        }
    }

    fn record(path: &str, was_enabled: bool) -> ScheduledTaskRecord {
        ScheduledTaskRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            path: path.to_string(),
            was_enabled,
            active: true,
            reverted_at: None,
        }
    }

    /// `(op, target, outcome, detail)` of every audit row, oldest first.
    fn ops(journal: &Journal) -> Vec<(String, String, String, Option<String>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .map(|o| (o.op, o.target, o.outcome, o.detail))
            .collect();
        rows.reverse();
        rows
    }

    fn op(
        target_path: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> (String, String, String, Option<String>) {
        (
            OP_SET.to_string(),
            scheduled_task_target(target_path),
            outcome.to_string(),
            detail.map(str::to_string),
        )
    }

    #[test]
    fn status_reports_enabled_disabled_running_and_missing() {
        let tasks = FakeTasks::with(&[
            (r"\T\Enabled", true, false),
            (r"\T\Running", true, true),
            (r"\T\Disabled", false, false),
        ]);
        let check = |action: ScheduledTaskAction, state: ActionState, detail: &str| {
            let status = status_with(&tasks, &action).unwrap();
            assert_eq!(status.state, state, "{}", action.path);
            assert_eq!(status.detail, detail);
        };
        check(
            disable(r"\T\Enabled"),
            ActionState::NotApplied,
            r"scheduled task \T\Enabled: enabled; target disabled",
        );
        check(
            disable(r"\T\Running"),
            ActionState::NotApplied,
            r"scheduled task \T\Running: enabled, running; target disabled",
        );
        check(
            disable(r"\T\Disabled"),
            ActionState::Applied,
            r"scheduled task \T\Disabled: disabled; target disabled",
        );
        check(
            disable(r"\t\enabled"),
            ActionState::NotApplied,
            r"scheduled task \t\enabled: enabled; target disabled",
        );
        check(
            ScheduledTaskAction {
                path: r"\T\Enabled",
                enabled: true,
            },
            ActionState::Applied,
            r"scheduled task \T\Enabled: enabled; target enabled",
        );
        check(
            disable(r"\T\Missing"),
            ActionState::Unavailable,
            r"scheduled task \T\Missing is not on this PC",
        );
        assert_eq!(tasks.writes.get(), 0, "status never writes");
    }

    #[test]
    fn apply_records_the_baseline_before_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let safety = session(&journal);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = FakeTasks::with(&[(TASK, true, true)]);
        let (log, hook_journal) = (seen.clone(), journal.clone());
        tasks.before_write = Some(Box::new(move |path, enabled| {
            let active: Vec<(String, bool)> = hook_journal
                .active_scheduled_tasks()
                .unwrap()
                .into_iter()
                .map(|r| (r.path, r.was_enabled))
                .collect();
            log.borrow_mut().push((path.to_string(), enabled, active));
        }));

        let outcome = apply_with(&safety, &tasks, &DISABLE).unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
        assert_eq!(
            *seen.borrow(),
            vec![(TASK.to_string(), false, vec![(TASK.to_string(), true)])],
            "the write saw the baseline already journaled"
        );
        assert_eq!(tasks.get(TASK).map(|t| t.enabled), Some(false));
        let active = journal.active_scheduled_tasks().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, safety.session_id());
        assert_eq!(ops(&journal), vec![op(TASK, "applied", Some("disabled"))]);
    }

    #[test]
    fn already_disabled_task_is_not_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let safety = session(&journal);
        let tasks = FakeTasks::with(&[(TASK, false, false)]);

        let outcome = apply_with(&safety, &tasks, &DISABLE).unwrap();
        assert_eq!(outcome, MutationOutcome::AlreadyInDesiredState);
        assert_eq!(tasks.writes.get(), 0);
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 0);
        assert_eq!(
            ops(&journal),
            vec![op(TASK, "already_in_desired_state", None)]
        );
    }

    #[test]
    fn missing_task_is_skipped_and_not_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let safety = session(&journal);
        let tasks = FakeTasks::default();

        let reason = format!(r"scheduled task {TASK} is not on this PC");
        let outcome = apply_with(&safety, &tasks, &DISABLE).unwrap();
        assert_eq!(outcome, MutationOutcome::Skipped(reason.clone()));
        assert_eq!(tasks.writes.get(), 0);
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 0);
        assert_eq!(ops(&journal), vec![op(TASK, "skipped", Some(&reason))]);
    }

    #[test]
    fn failed_write_withdraws_only_its_own_record() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        // An older session disabled OTHER; Windows has turned it back on since.
        let older = test_safety(journal.clone(), "older", false);
        assert!(older
            .record_scheduled_task(&NewScheduledTaskRecord {
                path: OTHER.to_string(),
                was_enabled: true,
            })
            .unwrap());

        let safety = session(&journal);
        let mut tasks = FakeTasks::with(&[(TASK, true, false), (OTHER, true, false)]);
        tasks.fail_writes = Some("Access is denied.".to_string());
        for action in [disable(TASK), disable(OTHER)] {
            let err = apply_with(&safety, &tasks, &action).unwrap_err();
            assert_eq!(err.to_string(), "Access is denied.");
        }
        assert_eq!(tasks.writes.get(), 2);

        let active = journal.active_scheduled_tasks().unwrap();
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].path, OTHER);
        assert_eq!(active[0].session_id, older.session_id());
        let all = journal.all_scheduled_tasks().unwrap();
        let withdrawn = all.iter().find(|r| r.path == TASK).unwrap();
        assert!(!withdrawn.active && withdrawn.reverted_at.is_some());
        assert_eq!(
            ops(&journal),
            vec![
                op(TASK, "failed", Some("Access is denied.")),
                op(OTHER, "failed", Some("Access is denied.")),
            ]
        );
    }

    #[test]
    fn reapplying_after_windows_re_enabled_keeps_the_first_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let tasks = FakeTasks::with(&[(TASK, true, false)]);
        let first_session = session(&journal);
        assert_eq!(
            apply_with(&first_session, &tasks, &DISABLE).unwrap(),
            MutationOutcome::Applied
        );
        let first = journal.active_scheduled_tasks().unwrap();

        tasks.set(TASK, true);
        let second_session = session(&journal);
        let status = status_with(&tasks, &DISABLE).unwrap();
        assert_eq!(status.state, ActionState::NotApplied);
        assert_eq!(
            apply_with(&second_session, &tasks, &DISABLE).unwrap(),
            MutationOutcome::Applied
        );

        let active = journal.active_scheduled_tasks().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, first[0].id);
        assert_eq!(active[0].session_id, first_session.session_id());
        assert!(active[0].was_enabled);
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 1);
        assert_eq!(tasks.writes.get(), 2);
        assert_eq!(tasks.get(TASK).map(|t| t.enabled), Some(false));
    }

    #[test]
    fn restore_with_writes_skips_or_reports_missing() {
        let tasks = FakeTasks::with(&[(TASK, false, false), (OTHER, true, false)]);
        assert_eq!(
            restore_with(&tasks, &record(TASK, true)).unwrap(),
            TaskRestore::Restored
        );
        assert_eq!(tasks.get(TASK).map(|t| t.enabled), Some(true));
        assert_eq!(tasks.writes.get(), 1);

        assert_eq!(
            restore_with(&tasks, &record(&OTHER.to_uppercase(), true)).unwrap(),
            TaskRestore::AlreadyInState
        );
        assert_eq!(
            tasks.writes.get(),
            1,
            "no write when the flag is already right"
        );

        assert_eq!(
            restore_with(&tasks, &record(r"\PCOptimizerSelfTest\Gone", false)).unwrap(),
            TaskRestore::Missing
        );
        assert_eq!(tasks.writes.get(), 1);

        let mut failing = FakeTasks::with(&[(TASK, false, false)]);
        failing.fail_writes = Some("Access is denied.".to_string());
        let err = restore_with(&failing, &record(TASK, true)).unwrap_err();
        assert_eq!(err.to_string(), "Access is denied.");
    }

    #[test]
    fn catalog_targets_match_journal_targets() {
        let view = catalog_view();
        let mut count = 0;
        for t in catalog::TWEAKS {
            let entry = view.iter().find(|e| e.id == t.id).unwrap();
            for (index, a) in t.actions.iter().enumerate() {
                let Action::ScheduledTask(s) = a else {
                    continue;
                };
                let target = describe(s);
                assert_eq!(target, format!("scheduled task {}", s.path));
                assert_eq!(target, record(s.path, true).target());
                assert_eq!(entry.targets[index], target, "{}", t.id);
                count += 1;
            }
        }
        assert_eq!(count, 12);
    }

    #[test]
    fn refused_elevation_writes_and_journals_nothing() {
        if crate::is_elevated() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let safety = test_safety(journal.clone(), "unelevated", true);
        let tasks = FakeTasks::with(&[(TASK, true, false)]);

        let err = apply_with(&safety, &tasks, &DISABLE).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        let conn = TaskConnection::new();
        let err = apply_in(&safety, &conn, &DISABLE).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert!(conn.inner.get().is_none(), "refused before connecting");
        let err = apply(&safety, &DISABLE).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");

        assert_eq!(tasks.writes.get(), 0);
        assert_eq!(tasks.get(TASK).map(|t| t.enabled), Some(true));
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 0);
        assert!(journal.ops(10).unwrap().is_empty());
    }

    #[test]
    fn one_connection_serves_every_status_read() {
        // Read-only: only looks tasks up.
        let conn = TaskConnection::new();
        let first: *const TaskScheduler = conn.store().unwrap();
        let second: *const TaskScheduler = conn.store().unwrap();
        assert_eq!(first, second);
        let missing = status_in(&conn, &DISABLE).unwrap();
        assert_eq!(missing.state, ActionState::Unavailable);
        assert_eq!(
            missing.detail,
            format!(r"scheduled task {TASK} is not on this PC")
        );
        assert_eq!(status(&DISABLE).unwrap().detail, missing.detail);
    }

    /// Registers a probe task under `\PCOptimizerSelfTest\`, disables it through the
    /// engine, restores it through the journal and deletes it again. Run elevated, by exact
    /// name, with OPTIMIZER_FORBID_RESTORE_POINT=1.
    #[test]
    #[ignore = "requires elevation; registers and deletes a sandbox scheduled task"]
    fn sandbox_task_round_trip() {
        use crate::safety::rollback::{rollback_filtered, RollbackFilter};
        use crate::safety::{RestorePointPolicy, SafetyOptions};
        use crate::win::task_scheduler::probe::{folder_exists, ProbeTask, FOLDER};

        assert!(crate::is_elevated(), "run this test elevated");
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let probe = ProbeTask::register().expect("register the probe task");
        let path: &'static str = Box::leak(probe.path().to_string().into_boxed_str());
        let action = disable(path);
        {
            let safety = Safety::begin(
                journal.clone(),
                SafetyOptions {
                    label: "scheduled task sandbox".into(),
                    restore_point: RestorePointPolicy::Skip,
                    require_elevation: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(safety.restore_point().is_none());
            assert_eq!(status(&action).unwrap().state, ActionState::NotApplied);
            assert_eq!(apply(&safety, &action).unwrap(), MutationOutcome::Applied);
            assert_eq!(
                apply(&safety, &action).unwrap(),
                MutationOutcome::AlreadyInDesiredState
            );
            assert_eq!(status(&action).unwrap().state, ActionState::Applied);
        }

        let records = journal.active_scheduled_tasks().unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].path, path);
        assert!(records[0].was_enabled);

        let filter = RollbackFilter {
            scheduled_tasks: vec![path.to_uppercase()],
            ..Default::default()
        };
        let report = rollback_filtered(&journal, &filter, false).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.scheduled_tasks_restored, 1);
        assert_eq!(
            report.actions,
            vec![format!("enable scheduled task {path}")]
        );
        assert_eq!(status(&action).unwrap().state, ActionState::NotApplied);
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());

        drop(probe);
        let scheduler = TaskScheduler::connect().unwrap();
        assert!(
            scheduler.task(path).unwrap().is_none(),
            "probe task left behind"
        );
        assert!(
            !folder_exists(&scheduler, &format!(r"\{FOLDER}")).unwrap(),
            "probe folder left behind"
        );
    }
}
