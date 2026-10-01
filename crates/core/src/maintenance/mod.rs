//! Scheduled maintenance: a weekly Task Scheduler task that cleans the chosen locations and
//! runs read-only system checks.
//!
//! - **Journaled:** only the task definition. Its record ("no task at this path", and whether
//!   the `\Cairn` folder was created) is written before Task Scheduler is touched, so turning
//!   maintenance off, History's undo and Revert All all delete the task the same way.
//! - **Logged only:** the runs. Each run writes an `ops_log` "started" row and a run row
//!   (`maintenance_runs`) before anything is deleted, then a final row, the report and a
//!   transcript under `<data folder>\maintenance`. Runs never repair anything: the only steps
//!   are allow-listed cleanup targets, `sfc /verifyonly` and DISM CheckHealth.
//! - **Who and where:** the task `\Cairn\Maintenance-<SID>` runs `cairn-maintenance.exe` as
//!   the signed-in account with its highest rights, only while idle and on AC power, and never
//!   wakes the PC. It can be turned on only when the program, the DLLs it loads and every
//!   folder above it are administrator-only, only by an elevated process of the signed-in
//!   account itself, and the task is always registered with an administrator-only security
//!   descriptor that is read back.
//! - **Run lock:** `Global\Cairn.Maintenance`, created administrator-owned. A run is in
//!   progress only while an administrator holds it and a run row is `running`; a mutex of
//!   that name owned by anyone else is reported, never trusted.

pub mod config;
pub mod monitor;
mod program;
pub mod report;
pub mod run;
pub mod schedule;
mod sfc;
pub mod task;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

pub use config::{
    schedulable_targets, SchedulableTarget, ScheduleConfig, ScheduleDay, ScheduleTime,
};
pub use monitor::{monitor, RunMonitor, RunObservation};
pub use report::{
    fmt_size, CheckStep, CleanupStep, CleanupTargetResult, MaintenanceReport, MaintenanceRun,
    RunProgress,
};
pub use run::{PlannedStep, RunOrigin, RunPlan, RunRequest, RunState, StepOutcome};
pub use schedule::{
    AccountStatus, MaintenanceStatus, ProgramStatus, RemoveResult, RunNowResult, ScheduleOutcome,
    SchedulePlan, ScheduleReport, ScheduleResult, TaskStatus,
};
pub use task::task_result_text;

pub use crate::safety::state_log::task_definition_target;

use crate::profiles::step::{MaintenanceChoice, SettingStep, StepResult};
use crate::safety::rollback::{FolderRemoval, RollbackFilter, TaskDefinitionRemover};
use crate::safety::state_log::{self, Journal, JournalTable};
use crate::safety::Safety;
use crate::win::mutex::{self, MutexPresence};
use crate::{Error, Result};
use run::LiveMaintenance;
use schedule::{Host, StoreRef};
use task::{interpret, Expected, LiveDefinitions, OwnRemover, TaskDefinitionStore};

/// Task Scheduler folder of the maintenance task (`\Cairn`).
pub const TASK_FOLDER: &str = "Cairn";
/// Name of the maintenance task in [`TASK_FOLDER`], followed by the account's SID.
pub const TASK_NAME_PREFIX: &str = "Maintenance-";
/// Lock held by a running maintenance run. Always created with
/// [`crate::win::mutex::ADMIN_LOCK_SDDL`]; a mutex of this name owned by anyone but
/// Administrators or SYSTEM is not a run.
pub const RUN_MUTEX: &str = r"Global\Cairn.Maintenance";

/// Every step finished without a problem.
pub const EXIT_COMPLETED: i32 = 0;
/// A check found something that needs attention.
pub const EXIT_ATTENTION: i32 = 10;
/// A step failed.
pub const EXIT_FAILED: i32 = 11;
/// The run stopped early (battery power or its time budget).
pub const EXIT_STOPPED: i32 = 12;
/// Nothing ran (another run held the lock, or the PC was on battery power).
pub const EXIT_SKIPPED: i32 = 13;
/// The task's arguments or the environment were refused; nothing was written.
pub const EXIT_REFUSED: i32 = 14;

/// Purpose of the task definition record of the maintenance task.
pub const PURPOSE: &str = "maintenance";
/// `ops_log` op of a maintenance run.
pub const OP_RUN: &str = "maintenance";
/// `ops_log` op of starting the task on demand.
pub const OP_RUN_NOW: &str = "maintenance_run_now";
/// `ops_log` op of registering the task.
pub(crate) const OP_SET_TASK: &str = "set_task_definition";
/// `ops_log` op of deleting a task Cairn has no record of, and of the uninstaller's removal.
pub(crate) const OP_DELETE_TASK: &str = "delete_task_definition";
/// Time budget of one run, under the task's four-hour limit.
pub const RUN_LIMIT: std::time::Duration = std::time::Duration::from_secs(3 * 3600 + 1800);
/// Run rows kept in the journal.
pub const KEEP_RUN_ROWS: usize = 100;

/// A task with administrator rights must not run a program others can change.
pub const PROGRAM_UNSAFE: &str = "Scheduled maintenance needs Cairn installed where only \
    administrators can change its files, such as Program Files. {path} can be changed by other \
    accounts or programs ({why}), so a task with administrator rights must not run it.";
/// The maintenance program is not in Cairn's folder.
pub const PROGRAM_MISSING: &str =
    "{file} is missing from Cairn's folder ({folder}); reinstall Cairn.";
/// The elevated process's account is not the signed-in one.
pub const NOT_ADMIN_ACCOUNT: &str = "Scheduled maintenance runs as your account with \
    administrator rights, so your account must be an administrator.";
/// This window runs as another account than the signed-in user.
pub const OTHER_ACCOUNT: &str = "This window runs as a different account than the signed-in \
    user, so a schedule would clean that account's files instead of yours; nothing was changed. \
    Start Cairn as your own account; it must be an administrator.";
/// This process runs as a service account.
pub const SERVICE_ACCOUNT: &str = "Cairn is running under a service account. Start it as your \
    own account to set up scheduled maintenance.";
/// The account's SID is not one a maintenance task path may carry (`S-1-5-…`), so Cairn could
/// never remove the task again; Microsoft Entra ID accounts (`S-1-12-1-…`) are such accounts.
pub const UNSUPPORTED_ACCOUNT: &str = "Scheduled maintenance can't be set up for this kind of \
    account, such as a Microsoft Entra ID (work or school) account; nothing was changed.";
/// The `\Cairn` folder lets others add or change tasks.
pub const FOLDER_UNSAFE: &str = "The Task Scheduler folder \\Cairn lets other accounts add or \
    change tasks, so Cairn doesn't put an administrator task there. Delete that folder in Task \
    Scheduler and try again.";
/// A task exists at the maintenance path without a journal record.
pub const UNRECORDED_TEXT: &str = "A maintenance task for your account exists in Task Scheduler, \
    but Cairn's journal has no record of creating it (the journal may have been reset). Cairn \
    doesn't change a task it can't undo.";
/// Refusal to change a task Cairn has no record of.
pub const FOREIGN_TASK: &str = UNRECORDED_TEXT;
/// The registered task's permissions were not administrator-only.
pub const TASK_UNSAFE: &str = "Windows gave the new task permissions that would let other \
    programs change it, so Cairn removed it again; nothing was changed.";
/// The task is disabled in Task Scheduler.
pub const TASK_DISABLED: &str =
    "The task is disabled in Task Scheduler. Save the schedule again to turn it back on.";
/// A run is in progress.
pub const ALREADY_RUNNING: &str = "Maintenance is already running.";
/// Task Scheduler cannot be reached; `{error}` is the reason.
pub const SCHEDULER_UNAVAILABLE: &str = "Task Scheduler isn't available: {error}";
/// `RegisterTask` failed with access denied.
pub const REGISTER_DENIED: &str = "Windows refused to create the task (access denied). A Task \
    Scheduler policy on this PC may prevent it.";
/// Detail of a run skipped because another program holds the run lock.
pub const FOREIGN_LOCK_TEXT: &str =
    "Another program holds Cairn's maintenance lock, so this run was skipped.";
/// Warning while another program holds the run lock.
pub const FOREIGN_LOCK_WARNING: &str = "Another program holds Cairn's maintenance lock; \
    scheduled runs are skipped until it is released.";
/// Run now needs the schedule to be on.
pub const TURN_ON_FIRST_TEXT: &str =
    "Turn on scheduled maintenance to run it now. For a one-time cleanup, use Cleanup.";
/// Maintenance runs only on AC power.
pub const ON_BATTERY_TEXT: &str =
    "The PC is running on battery power. Maintenance runs only when it's plugged in.";
/// The task was changed outside Cairn, so it is not started on demand.
pub const CHANGED_OUTSIDE_TEXT: &str = "The task was changed outside Cairn. Save the schedule \
    again before running it.";

/// `\Cairn\Maintenance-<SID>`: the maintenance task of the account with this SID.
pub fn task_path(sid: &str) -> String {
    format!(r"\{TASK_FOLDER}\{TASK_NAME_PREFIX}{sid}")
}

// ───────────────────────────── Live store ─────────────────────────────

fn connect() -> std::result::Result<LiveDefinitions, String> {
    LiveDefinitions::connect().map_err(|e| e.to_string())
}

fn store_ref(store: &std::result::Result<LiveDefinitions, String>) -> StoreRef<'_> {
    match store {
        Ok(store) => Ok(store as &dyn TaskDefinitionStore),
        Err(e) => Err(e.clone()),
    }
}

/// A Task Scheduler connection for the calls that connect only after their refusals.
fn connect_store() -> Result<Box<dyn TaskDefinitionStore>> {
    Ok(Box::new(LiveDefinitions::connect()?))
}

// ───────────────────────────── The app's calls ─────────────────────────────

/// Everything the Maintenance section shows, for the default journal. Read-only; Task
/// Scheduler errors become warnings.
pub fn status() -> Result<MaintenanceStatus> {
    status_in(&Journal::open_default()?)
}

/// [`status`] for `journal`.
pub fn status_in(journal: &Journal) -> Result<MaintenanceStatus> {
    let host = Host::live(journal.path());
    let store = connect();
    schedule::status_with(
        &host,
        journal,
        store_ref(&store),
        mutex::presence(RUN_MUTEX),
    )
}

/// Turns maintenance on or saves `config`. With `dry_run`, only the plan: no session and
/// nothing written. Otherwise the refusals come first, then one session "maintenance
/// schedule" without a restore point records the task before Task Scheduler registers it.
pub fn set_schedule(
    journal: Arc<Journal>,
    config: &ScheduleConfig,
    dry_run: bool,
) -> Result<ScheduleResult> {
    let config = config.clone().validated()?;
    let host = Host::live(journal.path());
    if dry_run {
        let store = connect();
        return Ok(ScheduleResult::Plan(schedule::plan_with(
            &host,
            &journal,
            store_ref(&store),
            &config,
        )?));
    }
    let report = schedule::set_schedule_with(journal, &host, connect_store, &config)?;
    Ok(ScheduleResult::Report(report))
}

/// Deletes the account's maintenance task when the journal has no record of it. Irreversible
/// and logged ("started" before the delete); with `dry_run` nothing is removed. Without
/// `dry_run`, an unelevated process is refused before Task Scheduler is contacted.
pub fn remove_unrecorded(journal: &Journal, dry_run: bool) -> Result<RemoveResult> {
    let host = Host::live(journal.path());
    schedule::remove_unrecorded_with(journal, &host, connect_store, dry_run)
}

/// Starts the scheduled task now; the run happens in its own process. The elevation and
/// account refusals come before Task Scheduler is contacted.
pub fn run_now(journal: &Journal) -> Result<RunNowResult> {
    let host = Host::live(journal.path());
    schedule::run_now_with(journal, &host, connect_store, mutex::presence(RUN_MUTEX))
}

/// Whether a run is in progress for the journal at `journal_path`: an administrator holds the
/// run lock and the journal has a running row. A missing journal is never created.
pub fn run_in_progress(journal_path: &std::path::Path) -> Result<bool> {
    let presence = mutex::presence(RUN_MUTEX);
    if presence != MutexPresence::Admin || !journal_path.is_file() {
        return Ok(false);
    }
    schedule::run_in_progress(&Journal::open(journal_path)?, presence)
}

/// Runs maintenance now in this process (`optctl maintenance run`). The data folder is the
/// journal's folder, which must pass the data-folder check.
pub fn run(journal: Arc<Journal>, request: &RunRequest) -> Result<MaintenanceReport> {
    run_live(journal, request, None)
}

/// [`run`], handing every line of the run's transcript to `echo` as it is written, so a
/// console can follow the run.
pub fn run_echoing(
    journal: Arc<Journal>,
    request: &RunRequest,
    echo: &dyn Fn(&str),
) -> Result<MaintenanceReport> {
    run_live(journal, request, Some(echo))
}

fn run_live(
    journal: Arc<Journal>,
    request: &RunRequest,
    echo: Option<&dyn Fn(&str)>,
) -> Result<MaintenanceReport> {
    let data_dir = program::data_dir_of(journal.path())?;
    program::check_data_dir(journal.path())?;
    let sys = LiveMaintenance::new(&data_dir);
    run::run_echoing_with(&sys, journal, &data_dir, request, echo)
}

/// What a run of `request` would do now. Read-only.
pub fn plan_run(request: &RunRequest) -> Result<RunPlan> {
    run::plan_with(
        request,
        crate::is_elevated(),
        crate::win::power::power_source().on_battery == Some(true),
    )
}

/// Marks a run that is over as seen. False when it was seen before, is in progress or does
/// not exist.
///
/// When the row of run `run_id` is still `running` while no administrator holds the run lock,
/// that run ended without finishing (the PC shut down, the task was stopped). Only that row is
/// closed as interrupted first, with the audit row the next run would write for it, so the run
/// can be marked instead of being announced again at every start. Every other row is left
/// alone: the lock is read before the row is changed, and a run that takes it in between
/// inserts a newer row.
pub fn acknowledge(journal: &Journal, run_id: i64) -> Result<bool> {
    acknowledge_with(journal, run_id, mutex::presence(RUN_MUTEX))
}

/// [`acknowledge`] with `presence` as the holder of the run lock.
fn acknowledge_with(journal: &Journal, run_id: i64, presence: MutexPresence) -> Result<bool> {
    if presence != MutexPresence::Admin && journal.interrupt_maintenance_run(run_id)? {
        journal.log_op(
            None,
            OP_RUN,
            &run::run_target(run_id),
            "interrupted",
            Some(run::INTERRUPTED_DETAIL),
        )?;
    }
    journal.acknowledge_maintenance_run(run_id)
}

/// Opens a run's transcript in Notepad; only a transcript in this journal's maintenance
/// folder.
pub fn open_log(journal: &Journal, run_id: i64) -> Result<()> {
    let row = journal
        .maintenance_runs(KEEP_RUN_ROWS * 2)?
        .into_iter()
        .find(|r| r.id == run_id)
        .ok_or_else(|| Error::Other(format!("there is no maintenance run {run_id}")))?;
    let log = row
        .log_path
        .ok_or_else(|| Error::Other(format!("maintenance run {run_id} has no log")))?;
    let path = program::transcript_to_open(journal.path(), &log)?;
    let mut command = crate::win::process::system_command("notepad.exe")?;
    command.arg(&path);
    command.spawn()?;
    Ok(())
}

/// The newest `limit` runs, newest first.
pub fn runs(journal: &Journal, limit: usize) -> Result<Vec<MaintenanceRun>> {
    let presence = mutex::presence(RUN_MUTEX);
    Ok(journal
        .maintenance_runs(limit)?
        .iter()
        .map(|row| MaintenanceRun::from_row(row, presence))
        .collect())
}

// ───────────────────────────── cairn-maintenance.exe ─────────────────────────────

/// Names that start with `OPTIMIZER_` (ASCII case ignored, as Windows compares them).
fn optimizer_variables(names: impl Iterator<Item = OsString>) -> Vec<OsString> {
    names
        .filter(|name| {
            name.to_str()
                .and_then(|n| n.get(..10))
                .is_some_and(|head| head.eq_ignore_ascii_case("OPTIMIZER_"))
        })
        .collect()
}

/// The journal, its data folder and the run the task's arguments name, once the arguments
/// have the exact grammar and the data folder passes its check; nothing is opened or written.
fn scheduled_request(args: Vec<OsString>) -> Option<(PathBuf, PathBuf, RunRequest)> {
    let tokens = args
        .into_iter()
        .map(|a| a.into_string().ok())
        .collect::<Option<Vec<String>>>()?;
    let (journal, selection) = config::parse_task_tokens(&tokens)?;
    let data_dir = program::data_dir_of(&journal).ok()?;
    program::check_data_dir(&journal).ok()?;
    Some((journal, data_dir, selection.request(RunOrigin::Task)))
}

/// Entry point of cairn-maintenance.exe: runs the maintenance the task's arguments describe
/// and returns the process exit code.
///
/// Every `OPTIMIZER_*` variable is removed first (the process starts single-threaded), so
/// nothing of the account's environment redirects the run; the data folder is only the
/// folder of the `--journal` path. Arguments outside the exact grammar, a data folder that
/// fails its check and a journal that cannot be opened end with [`EXIT_REFUSED`] and nothing
/// written. The run itself ends with its state's exit code.
pub fn run_scheduled<I: IntoIterator<Item = OsString>>(args: I) -> i32 {
    for name in optimizer_variables(std::env::vars_os().map(|(name, _)| name)) {
        std::env::remove_var(name);
    }
    let Some((journal_path, data_dir, request)) = scheduled_request(args.into_iter().collect())
    else {
        return EXIT_REFUSED;
    };
    let journal = match Journal::open(&journal_path) {
        Ok(journal) => Arc::new(journal),
        Err(_) => return EXIT_REFUSED,
    };
    program::enter_background_mode();
    let sys = LiveMaintenance::new(&data_dir);
    match run::run_with(&sys, journal, &data_dir, &request) {
        Ok(report) => report.state.exit_code(),
        Err(_) => EXIT_REFUSED,
    }
}

// ───────────────────────────── optctl and the uninstaller ─────────────────────────────

/// One line for `optctl doctor`: `on (every Sunday at 12:00)`, `off`, `not recorded` or
/// `not available: <reason>`. Read-only; a missing journal is not created.
pub fn doctor_line() -> String {
    let journal_path = state_log::default_path();
    let journal = if journal_path.is_file() {
        match Journal::open(&journal_path) {
            Ok(journal) => Some(journal),
            Err(e) => return format!("not available: {e}"),
        }
    } else {
        None
    };
    let sid = match crate::win::session::current_user_sid() {
        Ok(sid) => sid,
        Err(e) => return format!("not available: {e}"),
    };
    let store = connect();
    doctor_line_with(&sid, journal.as_ref(), store_ref(&store), &journal_path)
}

fn doctor_line_with(
    sid: &str,
    journal: Option<&Journal>,
    store: StoreRef<'_>,
    journal_path: &std::path::Path,
) -> String {
    let path = task_path(sid);
    let store = match store {
        Ok(store) => store,
        Err(e) => {
            return format!(
                "not available: {}",
                SCHEDULER_UNAVAILABLE.replace("{error}", &e)
            )
        }
    };
    let def = match store.read(&path) {
        Ok(Some(def)) => def,
        Ok(None) => return "off".to_string(),
        Err(e) => return format!("not available: {e}"),
    };
    let recorded = journal
        .and_then(|j| j.active_task_definitions().ok())
        .is_some_and(|active| active.iter().any(|r| r.path.eq_ignore_ascii_case(&path)));
    if !recorded {
        return "not recorded".to_string();
    }
    let expected = Expected {
        program: program::runner_program().unwrap_or_default(),
        journal: journal_path.to_path_buf(),
        user_sid: sid.to_string(),
    };
    let when = match interpret(&def, &expected, None).config {
        Some(c) => format!("every {} at {}", c.day.label(), c.time),
        None => "changed outside Cairn".to_string(),
    };
    if def.enabled {
        format!("on ({when})")
    } else {
        format!("on ({when}; disabled in Task Scheduler)")
    }
}

/// Removes every maintenance task in `\Cairn`, for the uninstaller. Never fails; what was
/// done (or went wrong) is in the text.
///
/// When the running account's journal already exists it is opened and each deletion gets a
/// `delete_task_definition` "started" row before the delete and a final row after it, and an
/// active record of that task is marked reverted; the journal is never created.
pub fn remove_all_for_uninstall() -> Result<String> {
    let journal_path = state_log::default_path();
    let (journal, note) = if journal_path.is_file() {
        match Journal::open(&journal_path) {
            Ok(journal) => (Some(journal), None),
            Err(e) => (
                None,
                Some(format!(
                    "the journal could not be opened, so no rows were written ({e})"
                )),
            ),
        }
    } else {
        (None, None)
    };
    let store = connect();
    Ok(remove_all_with(store_ref(&store), journal.as_ref(), note))
}

fn remove_all_with(store: StoreRef<'_>, journal: Option<&Journal>, note: Option<String>) -> String {
    let mut lines: Vec<String> = note.into_iter().collect();
    let store = match store {
        Ok(store) => store,
        Err(e) => {
            lines.push(SCHEDULER_UNAVAILABLE.replace("{error}", &e));
            return lines.join("; ");
        }
    };
    let folder = format!(r"\{TASK_FOLDER}");
    let names = match store.task_names(&folder) {
        Ok(names) => names,
        Err(e) => {
            lines.push(format!("cannot list the tasks in {folder}: {e}"));
            return lines.join("; ");
        }
    };
    let remover = OwnRemover::new(store);
    let paths: Vec<String> = names
        .iter()
        .map(|name| format!(r"{folder}\{name}"))
        .filter(|path| task::is_own_task_path(path))
        .collect();
    if paths.is_empty() {
        lines.push("no maintenance tasks".to_string());
    }
    for path in paths {
        let target = task_definition_target(&path);
        if let Some(journal) = journal {
            if let Err(e) = journal.log_op(None, OP_DELETE_TASK, &target, "started", None) {
                lines.push(format!(
                    "{path}: the journal row could not be written ({e})"
                ));
            }
        }
        let (outcome, detail) = match remover.delete(&path) {
            Ok(true) => ("deleted", None),
            Ok(false) => ("not_found", None),
            Err(e) => ("failed", Some(e.to_string())),
        };
        if let Some(journal) = journal {
            let _ = journal.log_op(None, OP_DELETE_TASK, &target, outcome, detail.as_deref());
            if outcome != "failed" {
                for rec in journal
                    .active_task_definitions()
                    .unwrap_or_default()
                    .iter()
                    .filter(|r| r.path.eq_ignore_ascii_case(&path))
                {
                    let _ = journal.mark_reverted(JournalTable::TaskDefinition, rec.id);
                }
            }
        }
        lines.push(match detail {
            Some(detail) => format!("{path}: {outcome} ({detail})"),
            None => format!("{path}: {outcome}"),
        });
    }
    match remover.delete_folder_if_empty(&folder) {
        Ok(FolderRemoval::Removed) => lines.push(format!("{folder} removed")),
        Ok(FolderRemoval::NotEmpty) => lines.push(format!("{folder} kept: not empty")),
        Ok(FolderRemoval::Missing) => {}
        Err(e) => lines.push(format!("{folder} kept: {e}")),
    }
    lines.join("; ")
}

// ───────────────────────────── Profiles ─────────────────────────────

/// The recorded, enabled schedule as a profile section, or None when there is none.
pub fn profile_current(journal: &Journal) -> Result<Option<MaintenanceChoice>> {
    let host = Host::live(journal.path());
    let store = connect();
    schedule::profile_current_with(&host, journal, store_ref(&store))
}

/// What applying `want` would change, as profile steps. Reads only.
pub fn profile_plan(journal: &Journal, want: &MaintenanceChoice) -> Result<Vec<SettingStep>> {
    let host = Host::live(journal.path());
    let store = connect();
    schedule::profile_plan_with(&host, journal, store_ref(&store), want)
}

/// Applies `want` under the caller's open session and returns the step results and the
/// rollback filter that undoes them (only a schedule this call created).
pub fn profile_apply_in(
    safety: &Safety,
    want: &MaintenanceChoice,
) -> Result<(Vec<StepResult>, RollbackFilter)> {
    let host = Host::live(safety.journal().path());
    let store = connect();
    schedule::profile_apply_with(safety, &host, store_ref(&store), want)
}

#[cfg(test)]
mod tests;
