//! Turning scheduled maintenance on or saving it, reading its state, Run now, and removing a
//! task Cairn has no record of.
//!
//! The task definition is the only journaled change: its record ("no task at this path") is
//! written before Task Scheduler is touched, so turning maintenance off is an undo of that
//! record. Every refusal (elevation, a service account, an account whose task could not be
//! removed again, another account, an unsafe program location) happens before any session,
//! record or Task Scheduler write; turning on, Run now and removing a task connect to Task
//! Scheduler only after their elevation and account refusals.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Local, NaiveDateTime};
use serde::Serialize;

use super::config::{
    next_start, schedulable_targets, task_arguments, SchedulableTarget, ScheduleConfig,
    ScheduleDay, ScheduleTime,
};
use super::program::{program_problem, runner_program};
use super::report::MaintenanceRun;
use super::task::{
    interpret, is_own_task_path, task_folder_problem, task_object_problem, task_result_text,
    task_xml, Expected, OwnRemover, RegisteredDefinition, TaskDefinitionStore, TaskSpec,
    FOLDER_SDDL,
};
use super::{
    task_path, ALREADY_RUNNING, CHANGED_OUTSIDE_TEXT, FOLDER_UNSAFE, FOREIGN_LOCK_WARNING,
    FOREIGN_TASK, ON_BATTERY_TEXT, OP_DELETE_TASK, OP_RUN_NOW, OP_SET_TASK, OTHER_ACCOUNT, PURPOSE,
    REGISTER_DENIED, SCHEDULER_UNAVAILABLE, SERVICE_ACCOUNT, TASK_DISABLED, TASK_FOLDER,
    TASK_UNSAFE, TURN_ON_FIRST_TEXT, UNSUPPORTED_ACCOUNT,
};
use crate::profiles::step::{
    MaintenanceChoice, SettingStep, StepOutcome as ProfileOutcome, StepReason, StepResult,
    StepStatus,
};
use crate::safety::rollback::{RollbackFilter, TaskDefinitionRemover};
use crate::safety::state_log::{
    task_definition_target, Journal, JournalTable, NewTaskDefinitionRecord,
};
use crate::safety::{RestorePointPolicy, Safety, SafetyOptions};
use crate::win::mutex::MutexPresence;
use crate::win::session;
use crate::win::task_scheduler::TaskRunState;
use crate::{Error, Result};

/// Session label of turning maintenance on or saving it.
pub(crate) const SESSION_LABEL: &str = "maintenance schedule";
/// Note shown for PCs with a battery.
pub const BATTERY_NOTE: &str = "This PC has a battery: maintenance waits until it's plugged in.";

// ───────────────────────────── Results ─────────────────────────────

/// What turning maintenance on or saving it would do. Nothing is written to build it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedulePlan {
    pub config: ScheduleConfig,
    pub task_path: String,
    /// The program the task starts; empty when it is missing.
    pub program: String,
    pub arguments: String,
    /// `DOMAIN\user` the task runs as.
    pub account: String,
    /// Next run, local time (`2026-10-04T12:00:00`).
    pub next_run: String,
    /// No task exists yet.
    pub creates: bool,
    /// The task already runs exactly this schedule.
    pub unchanged: bool,
    /// Why it cannot be turned on (every refusal but elevation); `None` when it can.
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleOutcome {
    Created,
    Updated,
    Unchanged,
}

/// What turning maintenance on or saving it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduleReport {
    pub session_id: Option<i64>,
    pub task_path: String,
    pub outcome: ScheduleOutcome,
    /// Next run, local time.
    pub next_run: Option<String>,
}

/// A plan (dry run) or a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ScheduleResult {
    Plan(SchedulePlan),
    Report(ScheduleReport),
}

/// What removing a task Cairn has no record of did, or would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveResult {
    pub task_path: String,
    /// `None` in a dry run; false when the task was already gone.
    pub removed: Option<bool>,
    /// A dry run: nothing was removed.
    pub planned: bool,
}

/// Run now was requested from Task Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunNowResult {
    pub requested: bool,
    pub task_path: String,
}

/// Everything the Maintenance section shows. Read-only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaintenanceStatus {
    pub elevated: bool,
    pub account: AccountStatus,
    pub program: ProgramStatus,
    /// `\Cairn\Maintenance-<SID>` of this account; `None` when the SID is unknown.
    pub task_path: Option<String>,
    /// The registered task; `None` when there is none (or Task Scheduler is unavailable).
    pub task: Option<TaskStatus>,
    /// The journal has an active record of the task.
    pub recorded: bool,
    pub defaults: ScheduleConfig,
    pub targets: Vec<SchedulableTarget>,
    /// Newest first, at most 10.
    pub runs: Vec<MaintenanceRun>,
    /// A run is in progress: an administrator holds the run lock and a run row is running.
    pub running: bool,
    pub on_battery: bool,
    pub has_battery: bool,
    /// Why maintenance cannot be turned on or saved (every refusal but elevation).
    pub blocked_reason: Option<String>,
    /// Sentences worth showing.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountStatus {
    pub sid: Option<String>,
    /// `DOMAIN\user`.
    pub name: Option<String>,
    /// This process runs as another account than the signed-in user; `None` when unknown.
    pub other_user: Option<bool>,
    pub service_account: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgramStatus {
    pub path: Option<String>,
    /// Only administrators can change the program, its DLLs and its folders.
    pub safe: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskStatus {
    pub enabled: bool,
    pub state: TaskRunState,
    /// The schedule the task carries; `None` when it is not one Cairn writes.
    pub config: Option<ScheduleConfig>,
    /// How the task differs from what Cairn registers.
    pub drift: Vec<String>,
    pub program: Option<String>,
    /// Local time.
    pub last_run_time: Option<String>,
    pub last_result: Option<i32>,
    pub last_result_hex: Option<String>,
    pub last_result_text: Option<String>,
    /// Local time.
    pub next_run_time: Option<String>,
    pub missed_runs: i32,
}

// ───────────────────────────── Host ─────────────────────────────

/// What the schedule depends on besides Task Scheduler and the journal: the process's
/// account and rights, the program, the power source and the clock.
#[derive(Debug, Clone)]
pub(crate) struct Host {
    pub(crate) elevated: bool,
    pub(crate) sid: std::result::Result<String, String>,
    pub(crate) account: String,
    pub(crate) other_user: std::result::Result<bool, String>,
    pub(crate) program: std::result::Result<PathBuf, String>,
    /// [`program_problem`] of the program.
    pub(crate) program_problem: Option<String>,
    pub(crate) journal_path: PathBuf,
    pub(crate) system_dir: std::result::Result<PathBuf, String>,
    pub(crate) now: NaiveDateTime,
    pub(crate) has_battery: bool,
    pub(crate) on_battery: bool,
}

impl Host {
    /// This process and PC, for the journal at `journal_path`.
    pub(crate) fn live(journal_path: &Path) -> Host {
        let program = runner_program().map_err(|e| e.to_string());
        let program_problem = program.as_ref().ok().and_then(|p| program_problem(p));
        let power = crate::win::power::power_source();
        Host {
            elevated: crate::is_elevated(),
            sid: session::current_user_sid().map_err(|e| e.to_string()),
            account: session::process_account_name().unwrap_or_else(|_| "your account".into()),
            other_user: session::elevated_as_other_user().map_err(|e| e.to_string()),
            program,
            program_problem,
            journal_path: journal_path.to_path_buf(),
            system_dir: crate::win::paths::system_dir().map_err(|e| e.to_string()),
            now: Local::now().naive_local(),
            has_battery: power.has_battery,
            on_battery: power.on_battery == Some(true),
        }
    }

    /// Refusals tied to the account: an unknown SID, a service account, an account whose
    /// task path the remover would refuse (so the task could never be removed again), another
    /// account than the signed-in user (or one that could not be compared).
    fn account_refusal(&self) -> Option<String> {
        match &self.sid {
            Err(e) => {
                return Some(format!(
                    "Cairn couldn't read your account ({e}), so scheduled maintenance can't be \
                     set up."
                ))
            }
            Ok(sid) if session::is_service_account_sid(sid) => {
                return Some(SERVICE_ACCOUNT.to_string())
            }
            Ok(sid) if !is_own_task_path(&task_path(sid)) => {
                return Some(UNSUPPORTED_ACCOUNT.to_string())
            }
            Ok(_) => {}
        }
        match self.other_user {
            Ok(false) => None,
            _ => Some(OTHER_ACCOUNT.to_string()),
        }
    }

    /// Refusals tied to the program: missing, or where others can change it.
    fn program_refusal(&self) -> Option<String> {
        match &self.program {
            Err(e) => Some(e.clone()),
            Ok(_) => self.program_problem.clone(),
        }
    }

    /// Every refusal but elevation, in order: account, then program.
    pub(crate) fn refusal(&self) -> Option<String> {
        self.account_refusal().or_else(|| self.program_refusal())
    }

    fn task_path(&self) -> Result<String> {
        self.sid
            .as_ref()
            .map(|sid| task_path(sid))
            .map_err(|e| Error::Other(format!("cannot read this account's SID: {e}")))
    }

    fn expected(&self) -> Expected {
        Expected {
            program: self.program.clone().unwrap_or_default(),
            journal: self.journal_path.clone(),
            user_sid: self.sid.clone().unwrap_or_default(),
        }
    }
}

/// The store, or why Task Scheduler cannot be reached.
pub(crate) type StoreRef<'a> = std::result::Result<&'a dyn TaskDefinitionStore, String>;

fn unavailable(error: &str) -> String {
    SCHEDULER_UNAVAILABLE.replace("{error}", error)
}

fn local_text(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Whether the journal has an active record of the task at `path`.
fn recorded(journal: &Journal, path: &str) -> Result<bool> {
    Ok(journal
        .active_task_definitions()?
        .iter()
        .any(|r| r.path.eq_ignore_ascii_case(path)))
}

/// A run is in progress: an administrator holds the run lock and a run row is running.
pub(crate) fn run_in_progress(journal: &Journal, presence: MutexPresence) -> Result<bool> {
    if presence != MutexPresence::Admin {
        return Ok(false);
    }
    Ok(journal
        .maintenance_runs(10)?
        .iter()
        .any(|r| r.state == super::run::RunState::Running.as_str()))
}

fn folder_path() -> String {
    format!(r"\{TASK_FOLDER}")
}

// ───────────────────────────── Plan and set ─────────────────────────────

/// What turning maintenance on with `config` would do. Reads only.
pub(crate) fn plan_with(
    host: &Host,
    journal: &Journal,
    store: StoreRef<'_>,
    config: &ScheduleConfig,
) -> Result<SchedulePlan> {
    let config = config.clone().validated()?;
    let mut blocked = host.refusal();
    let path = host.task_path().unwrap_or_default();
    let mut creates = true;
    let mut unchanged = false;
    match store {
        Err(e) => {
            blocked.get_or_insert_with(|| unavailable(&e));
        }
        Ok(store) if !path.is_empty() => {
            if let Some(sddl) = store.folder_sddl(&folder_path())? {
                if task_folder_problem(&sddl)?.is_some() {
                    blocked.get_or_insert_with(|| FOLDER_UNSAFE.to_string());
                }
            }
            if let Some(def) = store.read(&path)? {
                creates = false;
                if !recorded(journal, &path)? {
                    blocked.get_or_insert_with(|| FOREIGN_TASK.to_string());
                }
                unchanged = interpret(&def, &host.expected(), Some(&config)).matches_spec;
            }
        }
        Ok(_) => {}
    }
    let arguments = match task_arguments(&host.journal_path, &config) {
        Ok(arguments) => arguments,
        Err(e) => {
            blocked.get_or_insert_with(|| e.to_string());
            String::new()
        }
    };
    let mut notes = Vec::new();
    if host.has_battery {
        notes.push(BATTERY_NOTE.to_string());
    }
    Ok(SchedulePlan {
        next_run: local_text(next_start(host.now, config.day, config.time)),
        config,
        task_path: path,
        program: host
            .program
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        arguments,
        account: host.account.clone(),
        creates,
        unchanged,
        blocked_reason: blocked,
        notes,
    })
}

/// Turns maintenance on or saves it: refusals first, then a session "maintenance schedule"
/// without a restore point, then [`set_with`].
pub(crate) fn set_schedule_with(
    journal: Arc<Journal>,
    host: &Host,
    connect: impl FnOnce() -> Result<Box<dyn TaskDefinitionStore>>,
    config: &ScheduleConfig,
) -> Result<ScheduleReport> {
    let config = config.clone().validated()?;
    if !host.elevated {
        return Err(Error::NotElevated);
    }
    if let Some(reason) = host.refusal() {
        return Err(Error::Other(reason));
    }
    let store = connect().map_err(|e| Error::Other(unavailable(&e.to_string())))?;
    let safety = Safety::begin(
        journal,
        SafetyOptions {
            label: SESSION_LABEL.to_string(),
            restore_point: RestorePointPolicy::Skip,
            restore_description: String::new(),
            require_elevation: true,
        },
    )?;
    set_with(&safety, host, &*store, &config)
}

/// Marks this session's active record of `path` as reverted. Best effort: a journal error is
/// only logged.
fn withdraw(safety: &Safety, path: &str) {
    let own = match safety.journal().active_task_definitions() {
        Ok(active) => active
            .into_iter()
            .find(|r| r.session_id == safety.session_id() && r.path.eq_ignore_ascii_case(path)),
        Err(e) => {
            tracing::warn!(task = path, error = %e, "cannot read the task definition journal to withdraw a record");
            return;
        }
    };
    if let Some(rec) = own {
        if let Err(e) = safety
            .journal()
            .mark_reverted(JournalTable::TaskDefinition, rec.id)
        {
            tracing::warn!(task = path, error = %e, "cannot withdraw the task definition record");
        }
    }
}

/// Registers the task for `config` under `safety`'s session: refusals, the folder check, a
/// task without a record is refused, an identical task is left alone; otherwise the record is
/// written before any Task Scheduler write, the folder is created when missing, the task is
/// registered and its security read back. On any failure after the record, what this call
/// created is removed again and its record withdrawn.
pub(crate) fn set_with(
    safety: &Safety,
    host: &Host,
    store: &dyn TaskDefinitionStore,
    config: &ScheduleConfig,
) -> Result<ScheduleReport> {
    let config = config.clone().validated()?;
    safety.ensure_elevated()?;
    if let Some(reason) = host.refusal() {
        return Err(Error::Other(reason));
    }
    let path = host.task_path()?;
    let target = task_definition_target(&path);
    let folder = folder_path();
    let folder_sddl = store.folder_sddl(&folder)?;
    if let Some(sddl) = &folder_sddl {
        if let Some(why) = task_folder_problem(sddl)? {
            safety.log_op(
                OP_SET_TASK,
                &target,
                "skipped",
                Some(&format!("{folder}: {why}")),
            )?;
            return Err(Error::Other(FOLDER_UNSAFE.to_string()));
        }
    }
    let existing = store.read(&path)?;
    if existing.is_some() && !recorded(safety.journal(), &path)? {
        safety.log_op(
            OP_SET_TASK,
            &target,
            "skipped",
            Some("a task exists without a journal record"),
        )?;
        return Err(Error::Other(FOREIGN_TASK.to_string()));
    }
    if let Some(def) = &existing {
        if interpret(def, &host.expected(), Some(&config)).matches_spec {
            safety.log_op(OP_SET_TASK, &target, "already_in_desired_state", None)?;
            return Ok(ScheduleReport {
                session_id: Some(safety.session_id()),
                task_path: path,
                outcome: ScheduleOutcome::Unchanged,
                next_run: def
                    .next_run_time
                    .or_else(|| Some(next_start(host.now, config.day, config.time)))
                    .map(local_text),
            });
        }
    }
    let arguments = task_arguments(&host.journal_path, &config)?;
    let program = host.program.clone().map_err(Error::Other)?;
    let working_dir = host.system_dir.clone().map_err(Error::Other)?;
    let spec = TaskSpec {
        path: path.clone(),
        user_sid: host.sid.clone().map_err(Error::Other)?,
        program,
        arguments,
        working_dir,
        start: next_start(host.now, config.day, config.time),
        day: config.day,
        enabled: true,
    };

    let captured = safety.record_task_definition(&NewTaskDefinitionRecord {
        path: path.clone(),
        purpose: PURPOSE.to_string(),
        folder_created: folder_sddl.is_none(),
    })?;
    let mut created_folder = false;
    let mut registered = false;
    let written = (|| -> Result<()> {
        if folder_sddl.is_none() {
            created_folder = store.create_folder(&folder, FOLDER_SDDL)?;
            if !created_folder {
                let sddl = store.folder_sddl(&folder)?.unwrap_or_default();
                if task_folder_problem(&sddl)?.is_some() {
                    return Err(Error::Other(FOLDER_UNSAFE.to_string()));
                }
            }
        }
        store
            .register(&spec.path, &task_xml(&spec))
            .map_err(|e| match e.win32_code() {
                Some(5) => Error::Other(REGISTER_DENIED.to_string()),
                _ => e,
            })?;
        registered = true;
        let sddl = store.task_sddl(&spec.path)?.ok_or_else(|| {
            Error::Other("the task disappeared right after it was registered".into())
        })?;
        if let Some(why) = task_object_problem(&sddl)? {
            tracing::warn!(task = %spec.path, reason = %why, "the registered task is not administrator-only");
            return Err(Error::Other(TASK_UNSAFE.to_string()));
        }
        Ok(())
    })();
    if let Err(e) = written {
        if registered {
            if let Err(err) = store.delete(&spec.path) {
                tracing::warn!(task = %spec.path, error = %err, "cannot remove the task again");
            }
        }
        if created_folder {
            if let Err(err) = store.delete_folder_if_empty(&folder) {
                tracing::warn!(folder = %folder, error = %err, "cannot remove the task folder again");
            }
        }
        if captured {
            withdraw(safety, &spec.path);
        }
        safety.log_op(OP_SET_TASK, &target, "failed", Some(&e.to_string()))?;
        return Err(e);
    }
    let outcome = if existing.is_some() {
        ScheduleOutcome::Updated
    } else {
        ScheduleOutcome::Created
    };
    let verb = if outcome == ScheduleOutcome::Created {
        "created"
    } else {
        "updated"
    };
    safety.log_op(
        OP_SET_TASK,
        &target,
        "applied",
        Some(&format!("{verb}: {}", config.describe())),
    )?;
    tracing::info!(task = %spec.path, outcome = verb, "scheduled maintenance task registered");
    Ok(ScheduleReport {
        session_id: Some(safety.session_id()),
        task_path: spec.path,
        outcome,
        next_run: Some(local_text(spec.start)),
    })
}

// ───────────────────────────── Remove unrecorded, Run now ─────────────────────────────

/// Deletes the account's maintenance task when the journal has no record of it (the journal
/// was reset): irreversible and only logged, "started" before the delete. Refused before
/// `connect` is called: not elevated (unless `dry_run`), and a task path the remover would
/// refuse.
pub(crate) fn remove_unrecorded_with<'s>(
    journal: &Journal,
    host: &Host,
    connect: impl FnOnce() -> Result<Box<dyn TaskDefinitionStore + 's>>,
    dry_run: bool,
) -> Result<RemoveResult> {
    if !dry_run && !host.elevated {
        return Err(Error::NotElevated);
    }
    let path = host.task_path()?;
    if !is_own_task_path(&path) {
        return Err(Error::Other(UNSUPPORTED_ACCOUNT.to_string()));
    }
    let store = connect().map_err(|e| Error::Other(unavailable(&e.to_string())))?;
    if store.read(&path)?.is_none() {
        return Err(Error::Other(
            "There is no maintenance task for your account in Task Scheduler.".into(),
        ));
    }
    if recorded(journal, &path)? {
        return Err(Error::Other(
            "Cairn has a record of creating this task; turn maintenance off instead.".into(),
        ));
    }
    if dry_run {
        return Ok(RemoveResult {
            task_path: path,
            removed: None,
            planned: true,
        });
    }
    let target = task_definition_target(&path);
    journal.log_op(None, OP_DELETE_TASK, &target, "started", None)?;
    let remover = OwnRemover::new(&*store);
    match remover.delete(&path) {
        Ok(deleted) => {
            journal.log_op(
                None,
                OP_DELETE_TASK,
                &target,
                if deleted { "deleted" } else { "not_found" },
                None,
            )?;
            if let Err(e) = remover.delete_folder_if_empty(&folder_path()) {
                tracing::warn!(error = %e, "cannot remove the empty maintenance task folder");
            }
            Ok(RemoveResult {
                task_path: path,
                removed: Some(deleted),
                planned: false,
            })
        }
        Err(e) => {
            journal.log_op(
                None,
                OP_DELETE_TASK,
                &target,
                "failed",
                Some(&e.to_string()),
            )?;
            Err(e)
        }
    }
}

/// Starts the task now in Task Scheduler (it runs as scheduled, outside the app). Refused, in
/// order: not elevated, the account (both before `connect` is called), Task Scheduler
/// unavailable, no task, no record, disabled, a run in progress, another program holding the
/// lock, battery power, the program, a task changed outside Cairn. The "requested" row is
/// written before Task Scheduler is asked.
pub(crate) fn run_now_with<'s>(
    journal: &Journal,
    host: &Host,
    connect: impl FnOnce() -> Result<Box<dyn TaskDefinitionStore + 's>>,
    presence: MutexPresence,
) -> Result<RunNowResult> {
    if !host.elevated {
        return Err(Error::NotElevated);
    }
    if let Some(reason) = host.account_refusal() {
        return Err(Error::Other(reason));
    }
    let path = host.task_path()?;
    let store = connect().map_err(|e| Error::Other(unavailable(&e.to_string())))?;
    let refuse = |text: &str| Err(Error::Other(text.to_string()));
    let Some(def) = store.read(&path)? else {
        return refuse(TURN_ON_FIRST_TEXT);
    };
    if !recorded(journal, &path)? {
        return refuse(FOREIGN_TASK);
    }
    if !def.enabled {
        return refuse(TASK_DISABLED);
    }
    if run_in_progress(journal, presence)? {
        return refuse(ALREADY_RUNNING);
    }
    if presence == MutexPresence::Other {
        return refuse(FOREIGN_LOCK_WARNING);
    }
    if host.on_battery {
        return refuse(ON_BATTERY_TEXT);
    }
    if let Some(reason) = host.program_refusal() {
        return Err(Error::Other(reason));
    }
    if !interpret(&def, &host.expected(), None).drift.is_empty() {
        return refuse(CHANGED_OUTSIDE_TEXT);
    }
    let target = task_definition_target(&path);
    journal.log_op(None, OP_RUN_NOW, &target, "requested", None)?;
    if let Err(e) = store.run_now(&path) {
        journal.log_op(None, OP_RUN_NOW, &target, "failed", Some(&e.to_string()))?;
        return Err(e);
    }
    Ok(RunNowResult {
        requested: true,
        task_path: path,
    })
}

// ───────────────────────────── Status ─────────────────────────────

fn task_status(def: &RegisteredDefinition, host: &Host) -> TaskStatus {
    let seen = interpret(def, &host.expected(), None);
    TaskStatus {
        enabled: def.enabled,
        state: def.state,
        config: seen.config,
        drift: seen.drift,
        program: seen.program,
        last_run_time: def.last_run_time.map(local_text),
        last_result: Some(def.last_result),
        last_result_hex: Some(format!("0x{:08X}", def.last_result as u32)),
        last_result_text: task_result_text(def.last_result),
        next_run_time: def.next_run_time.map(local_text),
        missed_runs: def.missed_runs,
    }
}

/// Everything the Maintenance section shows. Task Scheduler errors become warnings.
pub(crate) fn status_with(
    host: &Host,
    journal: &Journal,
    store: StoreRef<'_>,
    presence: MutexPresence,
) -> Result<MaintenanceStatus> {
    let mut warnings = Vec::new();
    let path = host.task_path().ok();
    let is_recorded = match &path {
        Some(path) => recorded(journal, path)?,
        None => false,
    };
    let mut blocked = host.refusal();
    let mut task = None;
    match (&store, &path) {
        (Err(e), _) => warnings.push(unavailable(e)),
        (Ok(store), Some(path)) => {
            match store.folder_sddl(&folder_path()) {
                Ok(Some(sddl)) if !matches!(task_folder_problem(&sddl), Ok(None)) => {
                    blocked.get_or_insert_with(|| FOLDER_UNSAFE.to_string());
                }
                Ok(_) => {}
                Err(e) => warnings.push(unavailable(&e.to_string())),
            }
            match store.read(path) {
                Ok(Some(def)) => task = Some(task_status(&def, host)),
                Ok(None) => {}
                Err(e) => warnings.push(unavailable(&e.to_string())),
            }
        }
        (Ok(_), None) => {}
    }
    if let Some(t) = &task {
        if !is_recorded {
            blocked.get_or_insert_with(|| FOREIGN_TASK.to_string());
        } else {
            if !t.enabled {
                warnings.push(TASK_DISABLED.to_string());
            }
            if !t.drift.is_empty() {
                warnings.push(format!(
                    "The task was changed outside Cairn: {}.",
                    t.drift.join("; ")
                ));
            }
        }
    }
    if presence == MutexPresence::Other {
        warnings.push(FOREIGN_LOCK_WARNING.to_string());
    }
    let runs: Vec<MaintenanceRun> = journal
        .maintenance_runs(10)?
        .iter()
        .map(|row| MaintenanceRun::from_row(row, presence))
        .collect();
    let running = run_in_progress(journal, presence)?;
    let sid = host.sid.as_ref().ok().cloned();
    Ok(MaintenanceStatus {
        elevated: host.elevated,
        account: AccountStatus {
            service_account: sid.as_deref().is_some_and(session::is_service_account_sid),
            sid,
            name: Some(host.account.clone()),
            other_user: host.other_user.as_ref().ok().copied(),
        },
        program: ProgramStatus {
            path: host.program.as_ref().ok().map(|p| p.display().to_string()),
            safe: host.program_refusal().is_none(),
            reason: host.program_refusal(),
        },
        task_path: path,
        task,
        recorded: is_recorded,
        defaults: ScheduleConfig::default_config(),
        targets: schedulable_targets(),
        runs,
        running,
        on_battery: host.on_battery,
        has_battery: host.has_battery,
        blocked_reason: blocked,
        warnings,
    })
}

// ───────────────────────────── Profiles ─────────────────────────────

const PROFILE_KEY: &str = "maintenance";
const PROFILE_TITLE: &str = "Scheduled maintenance";
const TURN_OFF_ELSEWHERE: &str = "Turn scheduled maintenance off in Maintenance or History.";
const REMOVE_IT: &str = "Turn it off in Maintenance or History to remove it.";

/// "12:00 PM".
fn time_label(time: ScheduleTime) -> String {
    let hour = match time.hour % 12 {
        0 => 12,
        h => h,
    };
    let half = if time.hour < 12 { "AM" } else { "PM" };
    format!("{hour}:{:02} {half}", time.minute)
}

fn titles(targets: &[String]) -> Vec<String> {
    let all = schedulable_targets();
    targets
        .iter()
        .map(|id| {
            all.iter()
                .find(|t| &t.id == id)
                .map_or_else(|| id.clone(), |t| t.title.clone())
        })
        .collect()
}

/// "every Sunday at 12:00 PM; cleans Temporary files, Error reports; checks system files and
/// the component store".
fn summary(config: &ScheduleConfig) -> String {
    let cleans = if config.targets.is_empty() {
        "cleans nothing".to_string()
    } else {
        format!("cleans {}", titles(&config.targets).join(", "))
    };
    let checks = match (config.system_file_check, config.component_store_check) {
        (true, true) => "checks system files and the component store",
        (true, false) => "checks system files",
        (false, true) => "checks the component store",
        (false, false) => "runs no checks",
    };
    format!(
        "every {} at {}; {cleans}; {checks}",
        config.day.label(),
        time_label(config.time)
    )
}

/// The caution of a row that turns maintenance on (`creates`) or changes the schedule that is
/// on: it runs unattended with administrator rights and deletes files for good. Undoing the
/// profile removes only a schedule the profile created, so a changed schedule keeps running.
pub(crate) fn caution(config: &ScheduleConfig, creates: bool) -> String {
    let when = format!(
        "Runs every {} at {} with administrator rights",
        config.day.label(),
        time_label(config.time)
    );
    let undo = if creates {
        "Undo removes the task"
    } else {
        "Undoing the profile keeps this schedule"
    };
    if config.targets.is_empty() {
        format!("{when} and only runs read-only checks. {undo}.")
    } else {
        let deleted = if creates {
            "; files already deleted stay deleted"
        } else {
            ", and files already deleted stay deleted"
        };
        format!(
            "{when} and permanently deletes files in: {}. {undo}{deleted}.",
            titles(&config.targets).join(", ")
        )
    }
}

/// The schedule a profile section asks for: Sunday at 12:00 unless it names a day and time.
fn choice_config(want: &MaintenanceChoice) -> Result<ScheduleConfig> {
    let time = match &want.time {
        Some(text) => ScheduleTime::parse(text)?,
        None => ScheduleTime::NOON,
    };
    ScheduleConfig {
        day: want.day.unwrap_or(ScheduleDay::Sunday),
        time,
        targets: want.clean.clone(),
        system_file_check: want.sfc_verify,
        component_store_check: want.dism_check,
    }
    .validated()
}

fn step(
    status: StepStatus,
    detail: String,
    reason: Option<StepReason>,
    caution: Option<String>,
) -> SettingStep {
    SettingStep {
        key: PROFILE_KEY.to_string(),
        title: PROFILE_TITLE.to_string(),
        status,
        detail,
        reason,
        caution,
    }
}

fn result(outcome: ProfileOutcome, details: Vec<String>) -> StepResult {
    StepResult {
        key: PROFILE_KEY.to_string(),
        outcome,
        details,
    }
}

/// The recorded, enabled schedule as a profile section; `None` without one.
pub(crate) fn profile_current_with(
    host: &Host,
    journal: &Journal,
    store: StoreRef<'_>,
) -> Result<Option<MaintenanceChoice>> {
    let Ok(path) = host.task_path() else {
        return Ok(None);
    };
    if !recorded(journal, &path)? {
        return Ok(None);
    }
    let store = match store {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the maintenance task for a profile");
            return Ok(None);
        }
    };
    let def = match store.read(&path) {
        Ok(Some(def)) if def.enabled => def,
        Ok(_) => return Ok(None),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the maintenance task for a profile");
            return Ok(None);
        }
    };
    Ok(interpret(&def, &host.expected(), None)
        .config
        .map(|c| MaintenanceChoice {
            enabled: true,
            day: Some(c.day),
            time: Some(c.time.text()),
            clean: c.targets,
            sfc_verify: c.system_file_check,
            dism_check: c.component_store_check,
        }))
}

/// What applying the profile section `want` would change, as one step. Reads only.
pub(crate) fn profile_plan_with(
    host: &Host,
    journal: &Journal,
    store: StoreRef<'_>,
    want: &MaintenanceChoice,
) -> Result<Vec<SettingStep>> {
    if !want.enabled {
        return Ok(vec![step(
            StepStatus::Skipped,
            TURN_OFF_ELSEWHERE.to_string(),
            Some(StepReason::Unsupported),
            None,
        )]);
    }
    let config = match choice_config(want) {
        Ok(config) => config,
        Err(e) => {
            return Ok(vec![step(
                StepStatus::Skipped,
                e.to_string(),
                Some(StepReason::Unsupported),
                None,
            )])
        }
    };
    if let Err(e) = &store {
        return Ok(vec![step(
            StepStatus::Skipped,
            unavailable(e),
            Some(StepReason::Unreadable),
            None,
        )]);
    }
    let plan = plan_with(host, journal, store, &config)?;
    if let Some(reason) = plan.blocked_reason {
        let why = if reason == OTHER_ACCOUNT {
            StepReason::OtherAccount
        } else {
            StepReason::CannotChange
        };
        return Ok(vec![step(StepStatus::Skipped, reason, Some(why), None)]);
    }
    if plan.unchanged {
        return Ok(vec![step(
            StepStatus::Already,
            format!("Already runs {}.", summary(&config)),
            None,
            None,
        )]);
    }
    let detail = if plan.creates {
        format!("Turns on scheduled maintenance: {}.", summary(&config))
    } else {
        format!(
            "Changes scheduled maintenance to {}. {REMOVE_IT}",
            summary(&config)
        )
    };
    Ok(vec![step(
        StepStatus::Change,
        detail,
        None,
        Some(caution(&config, plan.creates)),
    )])
}

/// Applies the profile section `want` under `safety`'s session. The filter undoes only a
/// schedule this call created; a schedule that existed before stays when the profile is
/// undone.
pub(crate) fn profile_apply_with(
    safety: &Safety,
    host: &Host,
    store: StoreRef<'_>,
    want: &MaintenanceChoice,
) -> Result<(Vec<StepResult>, RollbackFilter)> {
    let none = RollbackFilter::default();
    if !want.enabled {
        return Ok((
            vec![result(
                ProfileOutcome::Skipped,
                vec![TURN_OFF_ELSEWHERE.to_string()],
            )],
            none,
        ));
    }
    let config = match choice_config(want) {
        Ok(config) => config,
        Err(e) => {
            return Ok((
                vec![result(ProfileOutcome::Skipped, vec![e.to_string()])],
                none,
            ))
        }
    };
    let store = match store {
        Ok(store) => store,
        Err(e) => {
            return Ok((
                vec![result(ProfileOutcome::Skipped, vec![unavailable(&e)])],
                none,
            ))
        }
    };
    if let Some(reason) = host.refusal() {
        return Ok((vec![result(ProfileOutcome::Skipped, vec![reason])], none));
    }
    match set_with(safety, host, store, &config) {
        Ok(report) => match report.outcome {
            ScheduleOutcome::Created => Ok((
                vec![result(
                    ProfileOutcome::Applied,
                    vec![format!(
                        "Turned on scheduled maintenance: {}.",
                        summary(&config)
                    )],
                )],
                RollbackFilter {
                    task_definitions: vec![report.task_path],
                    ..RollbackFilter::default()
                },
            )),
            ScheduleOutcome::Updated => Ok((
                vec![result(
                    ProfileOutcome::Applied,
                    vec![
                        format!("Changed scheduled maintenance to {}.", summary(&config)),
                        REMOVE_IT.to_string(),
                    ],
                )],
                none,
            )),
            ScheduleOutcome::Unchanged => Ok((
                vec![result(
                    ProfileOutcome::AlreadySet,
                    vec![format!("Already runs {}.", summary(&config))],
                )],
                none,
            )),
        },
        Err(e) => Ok((
            vec![result(ProfileOutcome::Failed, vec![e.to_string()])],
            none,
        )),
    }
}

#[cfg(test)]
mod tests;
