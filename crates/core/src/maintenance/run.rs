//! One maintenance run: the cleanup and the read-only checks the schedule names.
//!
//! A run is irreversible and only logged: the "started" audit row and the run row are written
//! before anything is deleted, then each step runs, and the run row, a final audit row and a
//! transcript record the result. Runs never repair: the only steps are the cleanup of
//! allow-listed targets, `sfc /verifyonly` and DISM CheckHealth. Every system access goes
//! through [`MaintenanceSystem`], so the whole run is testable without the live system.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::config::{allowed, schedulable_targets};
use super::program::{MAINTENANCE_DIR, TOOLS_DIR};
use super::report::{
    fmt_size, CheckStep, CleanupStep, CleanupTargetResult, MaintenanceReport, RunProgress,
};
use super::sfc::{self, SfcMessages};
use super::{
    EXIT_ATTENTION, EXIT_COMPLETED, EXIT_FAILED, EXIT_SKIPPED, EXIT_STOPPED, FOREIGN_LOCK_TEXT,
    KEEP_RUN_ROWS, OP_RUN, RUN_LIMIT, RUN_MUTEX,
};
use crate::cleanup::{self, CleanupReport};
use crate::safety::state_log::{Journal, NewMaintenanceRun};
use crate::tools::logs;
use crate::tools::runner::fmt_duration;
use crate::tools::{
    Environment, JobSnapshot, JobState, SystemLauncher, ToolId, ToolRequest, ToolRunner,
};
use crate::win::mutex::{Acquire, NamedMutex, ADMIN_LOCK_SDDL};
use crate::win::registry::{self, Hive};
use crate::{Error, Result, VERSION};

/// Who started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    /// The scheduled task (on its schedule, or started with Run now).
    Task,
    /// `optctl maintenance run`.
    Cli,
}

impl RunOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOrigin::Task => "task",
            RunOrigin::Cli => "cli",
        }
    }

    pub fn parse(s: &str) -> Option<RunOrigin> {
        [RunOrigin::Task, RunOrigin::Cli]
            .into_iter()
            .find(|o| o.as_str() == s)
    }
}

/// What one run does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    /// Schedulable cleanup target ids.
    pub targets: Vec<String>,
    /// `sfc /verifyonly`.
    pub system_file_check: bool,
    /// `DISM /Online /Cleanup-Image /CheckHealth`.
    pub component_store_check: bool,
    pub origin: RunOrigin,
}

impl RunRequest {
    /// Refuses unknown or unschedulable targets and a run with nothing to do.
    pub fn check(&self) -> Result<()> {
        for id in &self.targets {
            if allowed(id).is_none() {
                return Err(Error::Other(format!(
                    "{id:?} is not a cleanup location scheduled maintenance can clean"
                )));
            }
        }
        if self.targets.is_empty() && !self.system_file_check && !self.component_store_check {
            return Err(Error::Other(
                "a maintenance run needs at least one cleanup location or one check".into(),
            ));
        }
        Ok(())
    }

    /// For the audit log: "task; cleanup: user_temp, windows_temp; checks: sfc.exe
    /// /verifyonly, dism.exe /Online /Cleanup-Image /CheckHealth".
    fn describe(&self) -> String {
        let cleanup = if self.targets.is_empty() {
            "none".to_string()
        } else {
            self.targets.join(", ")
        };
        let mut checks = Vec::new();
        if self.system_file_check {
            checks.push(command_line(ToolId::SfcVerify));
        }
        if self.component_store_check {
            checks.push(command_line(ToolId::DismCheck));
        }
        let checks = if checks.is_empty() {
            "none".to_string()
        } else {
            checks.join(", ")
        };
        format!(
            "{}; cleanup: {cleanup}; checks: {checks}",
            self.origin.as_str()
        )
    }
}

/// Where a run stands, or how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    /// Every step finished without a problem.
    Completed,
    /// A check found something worth looking at.
    Attention,
    /// A step failed.
    Failed,
    /// It stopped early: battery power or its time limit.
    Stopped,
    /// Nothing ran.
    Skipped,
    /// It ended without finishing (shutdown, sign-out, the task was stopped).
    Interrupted,
}

impl RunState {
    pub const ALL: [RunState; 7] = [
        RunState::Running,
        RunState::Completed,
        RunState::Attention,
        RunState::Failed,
        RunState::Stopped,
        RunState::Skipped,
        RunState::Interrupted,
    ];

    /// The state as the run row stores it.
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Completed => "completed",
            RunState::Attention => "attention",
            RunState::Failed => "failed",
            RunState::Stopped => "stopped",
            RunState::Skipped => "skipped",
            RunState::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<RunState> {
        RunState::ALL.into_iter().find(|state| state.as_str() == s)
    }

    /// Process exit code of a run that ended in this state.
    pub fn exit_code(self) -> i32 {
        match self {
            RunState::Completed => EXIT_COMPLETED,
            RunState::Attention => EXIT_ATTENTION,
            RunState::Stopped => EXIT_STOPPED,
            RunState::Skipped => EXIT_SKIPPED,
            RunState::Failed | RunState::Running | RunState::Interrupted => EXIT_FAILED,
        }
    }

    /// Outcome of the run's final audit row.
    pub fn audit_outcome(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Completed => "completed",
            RunState::Attention => "problems_found",
            RunState::Failed => "failed",
            RunState::Stopped => "stopped",
            RunState::Skipped => "skipped",
            RunState::Interrupted => "interrupted",
        }
    }

    pub fn is_finished(self) -> bool {
        self != RunState::Running
    }
}

/// How one step of a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Ok,
    Attention,
    Failed,
    Skipped,
    /// The run stopped before this step.
    NotRun,
    /// The run's time limit ended while the check still ran; it finishes on its own.
    LeftRunning,
    /// Finished, but its result is only in the log.
    Unknown,
}

// ───────────────────────────── Texts ─────────────────────────────

const CLEANUP_TITLE: &str = "Clean up";
const SFC_TITLE: &str = "Check system files";
const DISM_TITLE: &str = "Check the component store";
const NEEDS_ADMIN_DETAIL: &str = "needs administrator rights";
const BUSY_DETAIL: &str = "another maintenance run is in progress";
pub(crate) const INTERRUPTED_DETAIL: &str = "the run ended without finishing: the PC shut \
                                             down, you signed out, or the task was stopped";
const BATTERY_AT_START: &str = "the PC runs on battery power";
const BATTERY_LATER: &str = "the PC switched to battery power; the rest runs next time";
const OUT_OF_TIME: &str = "the run reached its time limit; the rest runs next time";
const LEFT_RUNNING: &str = "a check was still running when the run reached its time limit; it \
                            finishes on its own";
const SERVICING_TEXT: &str =
    "Windows Update is working or waiting for a restart; this is cleaned next time";
const UNSCHEDULABLE: &str = "not a location scheduled maintenance can clean";
/// Programs that mean Windows Update is installing or about to.
const SERVICING_PROCESSES: [&str; 3] = ["tiworker.exe", "mousocoreworker.exe", "wuauclt.exe"];
const REBOOT_PENDING_KEYS: [&str; 2] = [
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
];
/// Output lines of a check kept for its verdict.
const KEEP_CHECK_LINES: usize = 200;

fn command_line(tool: ToolId) -> String {
    ToolRequest { tool, volume: None }.command_line()
}

fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Target of a run's audit rows.
pub(crate) fn run_target(id: i64) -> String {
    format!("maintenance run {id}")
}

// ───────────────────────────── Seam ─────────────────────────────

/// The run lock while a run holds it; released on drop.
pub(crate) trait RunLock: fmt::Debug {}

impl RunLock for NamedMutex {}

/// Result of asking for the run lock.
#[derive(Debug)]
pub(crate) enum LockState {
    Held(Box<dyn RunLock>),
    /// Another run holds it.
    Busy,
    /// A program other than an administrator's holds an object of that name.
    Foreign,
}

/// What polling a running check found.
#[derive(Debug, Clone, Default)]
pub(crate) struct CheckPoll {
    pub(crate) lines: Vec<String>,
    pub(crate) percent: Option<f64>,
    /// The final snapshot once the check ended; every line was delivered before it.
    pub(crate) finished: Option<JobSnapshot>,
}

/// A check started by [`MaintenanceSystem::start_check`].
pub(crate) trait CheckJob {
    fn poll(&mut self) -> CheckPoll;
    /// Stops following the check (it runs to completion on its own).
    fn abandon(&mut self);
    /// The check's readable transcript.
    fn log_path(&self) -> Option<String>;
}

pub(crate) enum CheckStart<'a> {
    Started(Box<dyn CheckJob + 'a>),
    /// The check cannot start now; the reason.
    Blocked(String),
}

/// Everything a run reads from or does to the system.
pub(crate) trait MaintenanceSystem {
    fn elevated(&self) -> bool;
    fn acquire_lock(&self) -> Result<LockState>;
    /// `Some(true)` from the power status counts as battery; unknown counts as AC.
    fn on_battery(&self) -> bool;
    /// Why per-user targets must be skipped (a service account, another account than the
    /// signed-in user, or the accounts could not be compared); `None` when they may run.
    fn per_user_refusal(&self) -> Option<String>;
    /// Lowercase names of the running programs; `None` when unreadable.
    fn processes(&self) -> Option<HashSet<String>>;
    /// Windows servicing or Windows Update waits for a restart; unreadable counts as true.
    fn servicing_reboot_pending(&self) -> bool;
    fn clean(&self, journal: &Journal, ids: &[String]) -> Result<CleanupReport>;
    fn start_check<'a>(&'a self, journal: Arc<Journal>, tool: ToolId) -> Result<CheckStart<'a>>;
    fn sfc_messages(&self) -> SfcMessages;
    fn now(&self) -> DateTime<Local>;
    /// Time budget of the run.
    fn run_limit(&self) -> Duration {
        RUN_LIMIT
    }
    /// How often a running check is polled.
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(250)
    }
    /// Least time between two progress writes of one step.
    fn progress_interval(&self) -> Duration {
        Duration::from_secs(2)
    }
}

// ───────────────────────────── Live system ─────────────────────────────

/// This PC. Its sfc and DISM runs use their own tool runner, with transcripts in
/// `<data folder>\maintenance\tools`; the data folder is always the folder of the run's
/// journal.
#[derive(Debug)]
pub(crate) struct LiveMaintenance {
    runner: ToolRunner,
}

impl LiveMaintenance {
    /// For the journal whose folder is `data_dir`.
    pub(crate) fn new(data_dir: &Path) -> LiveMaintenance {
        LiveMaintenance {
            runner: ToolRunner::new(
                Arc::new(SystemLauncher::new()),
                Environment::SYSTEM,
                data_dir.join(MAINTENANCE_DIR).join(TOOLS_DIR),
            ),
        }
    }

    /// Where the transcripts of its checks go.
    #[cfg(test)]
    pub(crate) fn tools_dir(&self) -> &Path {
        self.runner.log_dir()
    }
}

impl Drop for LiveMaintenance {
    fn drop(&mut self) {
        self.runner.shutdown();
    }
}

/// A check run by the live tool runner.
struct LiveCheck<'a> {
    runner: &'a ToolRunner,
    id: crate::tools::JobId,
    after: u64,
    log_path: String,
}

impl CheckJob for LiveCheck<'_> {
    fn poll(&mut self) -> CheckPoll {
        let mut poll = CheckPoll::default();
        loop {
            let Some(view) =
                self.runner
                    .view(self.id, self.after, crate::tools::MAX_LINES_PER_VIEW)
            else {
                return poll;
            };
            self.after = view.next;
            poll.lines.extend(view.lines);
            poll.percent = view.job.progress;
            if !view.more {
                if view.job.state.is_finished() {
                    poll.finished = Some(view.job);
                }
                return poll;
            }
        }
    }

    fn abandon(&mut self) {
        self.runner.shutdown();
    }

    fn log_path(&self) -> Option<String> {
        Some(self.log_path.clone())
    }
}

impl MaintenanceSystem for LiveMaintenance {
    fn elevated(&self) -> bool {
        crate::is_elevated()
    }

    fn acquire_lock(&self) -> Result<LockState> {
        Ok(
            match NamedMutex::try_acquire(RUN_MUTEX, Some(ADMIN_LOCK_SDDL))? {
                Acquire::Acquired(lock) => LockState::Held(Box::new(lock)),
                Acquire::Busy => LockState::Busy,
                Acquire::Foreign => LockState::Foreign,
            },
        )
    }

    fn on_battery(&self) -> bool {
        crate::win::power::power_source().on_battery == Some(true)
    }

    fn per_user_refusal(&self) -> Option<String> {
        per_user_refusal_for(
            crate::win::session::current_user_sid(),
            crate::win::session::elevated_as_other_user(),
        )
    }

    fn processes(&self) -> Option<HashSet<String>> {
        crate::win::process::running_process_names()
    }

    fn servicing_reboot_pending(&self) -> bool {
        REBOOT_PENDING_KEYS
            .iter()
            .any(|key| !matches!(registry::exists(Hive::LocalMachine, key), Ok(false)))
    }

    fn clean(&self, journal: &Journal, ids: &[String]) -> Result<CleanupReport> {
        cleanup::clean(journal, ids)
    }

    fn start_check<'a>(&'a self, journal: Arc<Journal>, tool: ToolId) -> Result<CheckStart<'a>> {
        let request = ToolRequest::new(tool, None)?;
        let plan = self.runner.plan(&request)?;
        if let Some(reason) = plan.blocked_reason {
            return Ok(CheckStart::Blocked(reason));
        }
        let job = self.runner.start(journal, &request)?;
        Ok(CheckStart::Started(Box::new(LiveCheck {
            runner: &self.runner,
            id: job.id,
            after: 0,
            log_path: job.log_path,
        })))
    }

    fn sfc_messages(&self) -> SfcMessages {
        match crate::win::paths::system_dir() {
            Ok(dir) => SfcMessages::load(&dir),
            Err(_) => SfcMessages::default(),
        }
    }

    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// Why per-user targets are skipped for this process's account: a service account, another
/// account than the signed-in user, or accounts that could not be compared.
pub(crate) fn per_user_refusal_for(
    sid: Result<String>,
    other_user: Result<bool>,
) -> Option<String> {
    match sid {
        Ok(sid) if crate::win::session::is_service_account_sid(&sid) => {
            return Some(
                "the run uses a service account, so no account's own files were cleaned".into(),
            )
        }
        Ok(_) => {}
        Err(e) => {
            return Some(format!(
                "Cairn couldn't read the run's account ({e}), so no account's own files were \
                 cleaned"
            ))
        }
    }
    match other_user {
        Ok(false) => None,
        Ok(true) => Some(
            "the run uses another account than the signed-in user, so that user's files were \
             left alone"
                .into(),
        ),
        Err(e) => Some(format!(
            "Cairn couldn't confirm the signed-in account ({e}), so your account's files were \
             left alone"
        )),
    }
}

// ───────────────────────────── Plan ─────────────────────────────

/// One step a run would take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStep {
    /// `cleanup`, `system_files` or `component_store`.
    pub step: String,
    pub title: String,
    pub detail: String,
}

/// What a run would do now; nothing is started or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunPlan {
    pub request: RunRequest,
    pub steps: Vec<PlannedStep>,
    /// Why the run would not start; `None` when it would.
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

/// The steps of `request` and what would keep it from running on this PC now. Read-only.
pub(crate) fn plan_with(request: &RunRequest, elevated: bool, on_battery: bool) -> Result<RunPlan> {
    request.check()?;
    let targets = schedulable_targets();
    let mut steps = Vec::new();
    if !request.targets.is_empty() {
        let titles: Vec<String> = request
            .targets
            .iter()
            .map(|id| {
                targets
                    .iter()
                    .find(|t| &t.id == id)
                    .map_or_else(|| id.clone(), |t| t.title.clone())
            })
            .collect();
        steps.push(PlannedStep {
            step: "cleanup".into(),
            title: CLEANUP_TITLE.into(),
            detail: format!("deletes files permanently in: {}", titles.join(", ")),
        });
    }
    if request.system_file_check {
        steps.push(PlannedStep {
            step: "system_files".into(),
            title: SFC_TITLE.into(),
            detail: command_line(ToolId::SfcVerify),
        });
    }
    if request.component_store_check {
        steps.push(PlannedStep {
            step: "component_store".into(),
            title: DISM_TITLE.into(),
            detail: command_line(ToolId::DismCheck),
        });
    }
    let mut notes = Vec::new();
    if on_battery {
        notes.push("The PC runs on battery power, so the run would be skipped.".into());
    }
    Ok(RunPlan {
        request: request.clone(),
        steps,
        blocked_reason: (!elevated).then(|| "Maintenance needs administrator rights.".into()),
        notes,
    })
}

// ───────────────────────────── Run ─────────────────────────────

/// The run's transcript file.
struct Transcript {
    stem: String,
    path: PathBuf,
    file: File,
}

/// Where a run's transcript lines go: the transcript file, when it could be created, and the
/// caller's echo (the console of a manual run), line by line as they are written. Write
/// failures are logged and otherwise ignored.
struct Log<'a> {
    transcript: Option<Transcript>,
    echo: Option<&'a dyn Fn(&str)>,
}

impl Log<'_> {
    fn lines(&mut self, lines: &[String]) {
        if let Some(echo) = self.echo {
            for line in lines {
                echo(line);
            }
        }
        if let Some(t) = self.transcript.as_mut() {
            if let Err(e) = logs::append_lines(&mut t.file, lines) {
                warn!(path = %t.path.display(), error = %e, "cannot write the maintenance transcript");
            }
        }
    }

    fn line(&mut self, line: String) {
        self.lines(&[line]);
    }

    /// The closing block: an empty line, then the lines of `text`.
    fn footer(&mut self, text: &str) {
        if let Some(echo) = self.echo {
            echo("");
            for line in text.lines() {
                echo(line);
            }
        }
        if let Some(t) = self.transcript.as_mut() {
            if let Err(e) = logs::append_footer(&mut t.file, text) {
                warn!(path = %t.path.display(), error = %e, "cannot finish the maintenance transcript");
            }
        }
    }
}

/// Writes the run row's progress, at most once per `interval` unless forced.
struct Progress<'a> {
    journal: &'a Journal,
    id: i64,
    count: u32,
    interval: Duration,
    last: Option<Instant>,
}

impl Progress<'_> {
    fn set(&mut self, step: &str, title: &str, index: u32, percent: Option<f64>, force: bool) {
        if !force && self.last.is_some_and(|last| last.elapsed() < self.interval) {
            return;
        }
        let progress = RunProgress {
            step: step.to_string(),
            title: title.to_string(),
            index,
            count: self.count,
            percent,
            updated_at: utc_now(),
        };
        match serde_json::to_string(&progress) {
            Ok(json) => {
                if let Err(e) = self.journal.update_maintenance_progress(self.id, &json) {
                    warn!(error = %e, "cannot store the maintenance run's progress");
                }
            }
            Err(e) => warn!(error = %e, "cannot serialize the maintenance run's progress"),
        }
        self.last = Some(Instant::now());
    }
}

/// A step of a run, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Cleanup,
    SystemFiles,
    ComponentStore,
}

impl Step {
    fn key(self) -> &'static str {
        match self {
            Step::Cleanup => "cleanup",
            Step::SystemFiles => "system_files",
            Step::ComponentStore => "component_store",
        }
    }

    fn doing(self) -> &'static str {
        match self {
            Step::Cleanup => "Cleaning up…",
            Step::SystemFiles => "Checking system files…",
            Step::ComponentStore => "Checking the component store…",
        }
    }
}

/// A report of a run that stopped before its run row: `run_id` 0, state skipped.
fn skipped_report(request: &RunRequest, reason: &str, started_at: String) -> MaintenanceReport {
    MaintenanceReport {
        run_id: 0,
        origin: request.origin,
        state: RunState::Skipped,
        ended_at: Some(started_at.clone()),
        started_at,
        duration_ms: 0,
        request: request.clone(),
        cleanup: None,
        checks: Vec::new(),
        stopped_reason: Some(reason.to_string()),
        attention: Vec::new(),
        headline: format!("Skipped: {reason}"),
        log_path: None,
    }
}

/// Marks every run row still `running` as interrupted and writes its audit row. Only for the
/// holder of the run lock: a caller that merely saw the lock free can meet the row of a run
/// that took the lock since.
pub(crate) fn close_interrupted_runs(journal: &Journal) -> Result<()> {
    for stale in journal.interrupt_running_maintenance_runs()? {
        journal.log_op(
            None,
            OP_RUN,
            &run_target(stale),
            "interrupted",
            Some(INTERRUPTED_DETAIL),
        )?;
    }
    Ok(())
}

/// Runs the maintenance `request` describes, writing rows into `journal` and the transcript
/// into `<data_dir>\maintenance`.
///
/// Refused runs (not elevated, the lock could not be read) write one `skipped` audit row and
/// return the error; a run another run or another program blocks writes one `skipped` row and
/// returns a skipped report without a run row. Otherwise the run row and the `started` row are
/// written before any step, and every later problem ends up in the report.
pub(crate) fn run_with(
    sys: &dyn MaintenanceSystem,
    journal: Arc<Journal>,
    data_dir: &Path,
    request: &RunRequest,
) -> Result<MaintenanceReport> {
    run_echoing_with(sys, journal, data_dir, request, None)
}

/// [`run_with`]; `echo` gets every transcript line as it is written, also when the transcript
/// file could not be created.
pub(crate) fn run_echoing_with(
    sys: &dyn MaintenanceSystem,
    journal: Arc<Journal>,
    data_dir: &Path,
    request: &RunRequest,
    echo: Option<&dyn Fn(&str)>,
) -> Result<MaintenanceReport> {
    request.check()?;
    let clock = Instant::now();
    let started_at = utc_now();
    if !sys.elevated() {
        journal.log_op(None, OP_RUN, OP_RUN, "skipped", Some(NEEDS_ADMIN_DETAIL))?;
        return Err(Error::NotElevated);
    }
    let _lock = match sys.acquire_lock() {
        Ok(LockState::Held(lock)) => lock,
        Ok(LockState::Busy) => {
            journal.log_op(None, OP_RUN, OP_RUN, "skipped", Some(BUSY_DETAIL))?;
            return Ok(skipped_report(request, BUSY_DETAIL, started_at));
        }
        Ok(LockState::Foreign) => {
            journal.log_op(None, OP_RUN, OP_RUN, "skipped", Some(FOREIGN_LOCK_TEXT))?;
            return Ok(skipped_report(request, FOREIGN_LOCK_TEXT, started_at));
        }
        Err(e) => {
            let detail = format!("the run lock could not be taken: {e}");
            journal.log_op(None, OP_RUN, OP_RUN, "skipped", Some(&detail))?;
            return Err(e);
        }
    };

    close_interrupted_runs(&journal)?;
    let id = journal.insert_maintenance_run(&NewMaintenanceRun {
        origin: request.origin.as_str().to_string(),
        state: RunState::Running.as_str().to_string(),
        request_json: serde_json::to_string(request)?,
    })?;
    let target = run_target(id);
    if let Err(e) = journal.log_op(None, OP_RUN, &target, "started", Some(&request.describe())) {
        let mut report = skipped_report(request, "the run could not be recorded", started_at);
        report.run_id = id;
        report.state = RunState::Failed;
        if let Ok(json) = serde_json::to_string(&report) {
            let _ = journal.finish_maintenance_run(id, RunState::Failed.as_str(), &json, None);
        }
        return Err(e);
    }

    let dir = data_dir.join(MAINTENANCE_DIR);
    let header = format!(
        "Cairn {VERSION}  ·  maintenance run {id} ({})",
        request.origin.as_str()
    );
    let transcript = match logs::create_transcript(&dir, "maintenance", &header, sys.now()) {
        Ok((stem, path, file)) => Some(Transcript { stem, path, file }),
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "cannot create the maintenance transcript");
            None
        }
    };
    let log_path = transcript.as_ref().map(|t| t.path.display().to_string());
    if let Some(echo) = echo {
        echo(&header);
    }
    let mut log = Log { transcript, echo };
    log.line(format!("Started {}", sys.now().format("%Y-%m-%d %H:%M:%S")));
    log.line(format!("Plan: {}", request.describe()));
    log.line(String::new());

    let mut steps = Vec::new();
    if !request.targets.is_empty() {
        steps.push(Step::Cleanup);
    }
    if request.system_file_check {
        steps.push(Step::SystemFiles);
    }
    if request.component_store_check {
        steps.push(Step::ComponentStore);
    }
    let mut progress = Progress {
        journal: &journal,
        id,
        count: steps.len() as u32,
        interval: sys.progress_interval(),
        last: None,
    };

    let mut cleanup_step: Option<CleanupStep> = None;
    let mut checks: Vec<CheckStep> = Vec::new();
    let mut stopped_reason: Option<String> = None;
    let mut skipped_all = false;
    let mut outcomes: Vec<(Step, StepOutcome)> = Vec::new();
    for (index, step) in steps.iter().copied().enumerate() {
        if stopped_reason.is_none() {
            if sys.on_battery() {
                if index == 0 {
                    skipped_all = true;
                    stopped_reason = Some(BATTERY_AT_START.to_string());
                } else {
                    stopped_reason = Some(BATTERY_LATER.to_string());
                }
            } else if clock.elapsed() > sys.run_limit() {
                stopped_reason = Some(OUT_OF_TIME.to_string());
            }
        }
        if stopped_reason.is_some() {
            match step {
                Step::Cleanup => cleanup_step = Some(not_run_cleanup(request)),
                Step::SystemFiles => checks.push(not_run_check(ToolId::SfcVerify)),
                Step::ComponentStore => checks.push(not_run_check(ToolId::DismCheck)),
            }
            outcomes.push((step, StepOutcome::NotRun));
            continue;
        }
        progress.set(step.key(), step.doing(), index as u32 + 1, None, true);
        log.line(format!("{}:", step_title(step)));
        let outcome = match step {
            Step::Cleanup => {
                let result = run_cleanup(sys, &journal, request, &mut log);
                let outcome = result.outcome;
                cleanup_step = Some(result);
                outcome
            }
            Step::SystemFiles | Step::ComponentStore => {
                let tool = if step == Step::SystemFiles {
                    ToolId::SfcVerify
                } else {
                    ToolId::DismCheck
                };
                let result = run_check(
                    sys,
                    &journal,
                    tool,
                    CheckContext {
                        clock,
                        index: index as u32 + 1,
                        step,
                    },
                    &mut progress,
                    &mut log,
                );
                if result.outcome == StepOutcome::LeftRunning {
                    stopped_reason = Some(LEFT_RUNNING.to_string());
                }
                let outcome = result.outcome;
                checks.push(result);
                outcome
            }
        };
        outcomes.push((step, outcome));
        log.line(String::new());
    }
    progress.set("finishing", "Finishing…", progress.count, None, true);

    let attention = attention_list(cleanup_step.as_ref(), &checks);
    let any = |wanted: StepOutcome| outcomes.iter().any(|(_, o)| *o == wanted);
    let state = if any(StepOutcome::Failed) {
        RunState::Failed
    } else if any(StepOutcome::Attention) {
        RunState::Attention
    } else if skipped_all {
        RunState::Skipped
    } else if stopped_reason.is_some() {
        RunState::Stopped
    } else {
        RunState::Completed
    };
    let headline = headline(
        cleanup_step.as_ref(),
        &checks,
        stopped_reason.as_deref(),
        state,
    );
    let elapsed = clock.elapsed();
    let report = MaintenanceReport {
        run_id: id,
        origin: request.origin,
        state,
        started_at,
        ended_at: Some(utc_now()),
        duration_ms: elapsed.as_millis() as u64,
        request: request.clone(),
        cleanup: cleanup_step,
        checks,
        stopped_reason,
        attention,
        headline,
        log_path: log_path.clone(),
    };

    match serde_json::to_string(&report) {
        Ok(json) => {
            if let Err(e) =
                journal.finish_maintenance_run(id, state.as_str(), &json, log_path.as_deref())
            {
                warn!(run = id, error = %e, "cannot store the maintenance run's result");
            }
        }
        Err(e) => warn!(run = id, error = %e, "cannot serialize the maintenance report"),
    }
    let log_text = log_path
        .as_deref()
        .map_or_else(|| "no log".to_string(), |p| format!("log {p}"));
    let detail = format!(
        "{} · {} · {log_text}",
        report.headline,
        fmt_duration(elapsed)
    );
    if let Err(e) = journal.log_op(None, OP_RUN, &target, state.audit_outcome(), Some(&detail)) {
        warn!(run = id, error = %e, "cannot write the maintenance run's final audit row");
    }
    log.footer(&format!(
        "{}: {}\r\nFinished {}  ·  {}",
        state_label(state),
        report.headline,
        sys.now().format("%Y-%m-%d %H:%M:%S"),
        fmt_duration(elapsed)
    ));
    if let Some(t) = &log.transcript {
        logs::prune(&dir, logs::KEEP_RUNS, std::slice::from_ref(&t.stem));
    }
    if let Err(e) = journal.prune_maintenance_runs(KEEP_RUN_ROWS) {
        warn!(error = %e, "cannot prune old maintenance runs");
    }
    Ok(report)
}

fn state_label(state: RunState) -> &'static str {
    match state {
        RunState::Running => "Running",
        RunState::Completed => "Finished",
        RunState::Attention => "Finished with something to look at",
        RunState::Failed => "Failed",
        RunState::Stopped => "Stopped early",
        RunState::Skipped => "Skipped",
        RunState::Interrupted => "Interrupted",
    }
}

fn step_title(step: Step) -> &'static str {
    match step {
        Step::Cleanup => CLEANUP_TITLE,
        Step::SystemFiles => SFC_TITLE,
        Step::ComponentStore => DISM_TITLE,
    }
}

fn target_title(id: &str) -> String {
    schedulable_targets()
        .into_iter()
        .find(|t| t.id == id)
        .map_or_else(|| id.to_string(), |t| t.title)
}

fn not_run_cleanup(request: &RunRequest) -> CleanupStep {
    CleanupStep {
        outcome: StepOutcome::NotRun,
        freed_bytes: 0,
        deleted_files: 0,
        skipped_files: 0,
        targets: request
            .targets
            .iter()
            .map(|id| CleanupTargetResult {
                id: id.clone(),
                title: target_title(id),
                outcome: "skipped".into(),
                freed_bytes: 0,
                deleted_files: 0,
                reason: Some("the run stopped before the cleanup".into()),
            })
            .collect(),
        error: None,
    }
}

fn check_title(tool: ToolId) -> &'static str {
    if tool == ToolId::SfcVerify {
        SFC_TITLE
    } else {
        DISM_TITLE
    }
}

fn not_run_check(tool: ToolId) -> CheckStep {
    CheckStep {
        tool,
        title: check_title(tool).to_string(),
        command_line: command_line(tool),
        outcome: StepOutcome::NotRun,
        text: "Not run: the run stopped before this check.".into(),
        windows_message: None,
        hint: None,
        exit_code_hex: None,
        restart_required: false,
        log_path: None,
        elapsed_ms: 0,
    }
}

/// The cleanup step: per-user targets are skipped when the account check refuses them, and
/// Windows Update targets while Windows Update works or waits for a restart; the rest go to
/// the unchanged cleanup.
fn run_cleanup(
    sys: &dyn MaintenanceSystem,
    journal: &Journal,
    request: &RunRequest,
    log: &mut Log<'_>,
) -> CleanupStep {
    let per_user = sys.per_user_refusal();
    let guarded = request
        .targets
        .iter()
        .any(|id| allowed(id).is_some_and(|a| a.servicing_guard));
    let servicing = guarded && {
        let busy = match sys.processes() {
            Some(names) => SERVICING_PROCESSES.iter().any(|p| names.contains(*p)),
            None => true,
        };
        busy || sys.servicing_reboot_pending()
    };
    let mut results: Vec<CleanupTargetResult> = Vec::new();
    let mut to_clean: Vec<String> = Vec::new();
    for id in &request.targets {
        let reason = match allowed(id) {
            None => Some(UNSCHEDULABLE.to_string()),
            Some(a) if a.per_user && per_user.is_some() => per_user.clone(),
            Some(a) if a.servicing_guard && servicing => Some(SERVICING_TEXT.to_string()),
            Some(_) => None,
        };
        match reason {
            Some(reason) => results.push(CleanupTargetResult {
                id: id.clone(),
                title: target_title(id),
                outcome: "skipped".into(),
                freed_bytes: 0,
                deleted_files: 0,
                reason: Some(reason),
            }),
            None => to_clean.push(id.clone()),
        }
    }
    let mut error = None;
    let mut skipped_files = 0;
    if !to_clean.is_empty() {
        match sys.clean(journal, &to_clean) {
            Ok(report) => {
                for r in report.results {
                    skipped_files += r.skipped_files;
                    let outcome = if r.skipped_reason.is_some() {
                        "skipped"
                    } else if !r.errors.is_empty() && r.deleted_files == 0 && r.freed_bytes == 0 {
                        "failed"
                    } else {
                        "cleaned"
                    };
                    results.push(CleanupTargetResult {
                        title: target_title(&r.id),
                        outcome: outcome.into(),
                        freed_bytes: r.freed_bytes,
                        deleted_files: r.deleted_files,
                        reason: r.skipped_reason.or_else(|| r.errors.first().cloned()),
                        id: r.id,
                    });
                }
            }
            Err(e) => {
                error = Some(e.to_string());
                for id in &to_clean {
                    results.push(CleanupTargetResult {
                        id: id.clone(),
                        title: target_title(id),
                        outcome: "failed".into(),
                        freed_bytes: 0,
                        deleted_files: 0,
                        reason: Some(e.to_string()),
                    });
                }
            }
        }
    }
    results.sort_by_key(|r| {
        request
            .targets
            .iter()
            .position(|t| t == &r.id)
            .unwrap_or(usize::MAX)
    });
    for r in &results {
        let line = match (r.outcome.as_str(), &r.reason) {
            ("cleaned", _) => format!(
                "  {}: freed {} ({} {})",
                r.title,
                fmt_size(r.freed_bytes),
                r.deleted_files,
                if r.deleted_files == 1 {
                    "file"
                } else {
                    "files"
                }
            ),
            (outcome, Some(reason)) => format!("  {}: {outcome}: {reason}", r.title),
            (outcome, None) => format!("  {}: {outcome}", r.title),
        };
        log.line(line);
    }
    if let Some(e) = &error {
        log.line(format!("  The cleanup failed: {e}"));
    }
    let outcome = if error.is_some() {
        StepOutcome::Failed
    } else if results.iter().all(|r| r.outcome == "skipped") {
        StepOutcome::Skipped
    } else if results.iter().any(|r| r.outcome == "failed") {
        StepOutcome::Attention
    } else {
        StepOutcome::Ok
    };
    CleanupStep {
        outcome,
        freed_bytes: results.iter().map(|r| r.freed_bytes).sum(),
        deleted_files: results.iter().map(|r| r.deleted_files).sum(),
        skipped_files,
        targets: results,
        error,
    }
}

/// Where a check runs inside its run.
#[derive(Debug, Clone, Copy)]
struct CheckContext {
    clock: Instant,
    index: u32,
    step: Step,
}

/// A read-only check: started through the tool runner, followed until it ends or the run's
/// budget does, and judged (sfc by its message table, DISM by the runner's verdict).
fn run_check(
    sys: &dyn MaintenanceSystem,
    journal: &Arc<Journal>,
    tool: ToolId,
    ctx: CheckContext,
    progress: &mut Progress<'_>,
    log: &mut Log<'_>,
) -> CheckStep {
    let started = Instant::now();
    let mut step = CheckStep {
        tool,
        title: check_title(tool).to_string(),
        command_line: command_line(tool),
        outcome: StepOutcome::Failed,
        text: String::new(),
        windows_message: None,
        hint: None,
        exit_code_hex: None,
        restart_required: false,
        log_path: None,
        elapsed_ms: 0,
    };
    let mut job = match sys.start_check(Arc::clone(journal), tool) {
        Ok(CheckStart::Started(job)) => job,
        Ok(CheckStart::Blocked(reason)) => {
            step.outcome = StepOutcome::Skipped;
            step.text = format!("Not checked: {reason}");
            log.line(format!("  {}", step.text));
            return step;
        }
        Err(e) => {
            step.text = format!("{} couldn't start: {e}", step.title);
            log.line(format!("  {}", step.text));
            return step;
        }
    };
    step.log_path = job.log_path();
    let mut kept: Vec<String> = Vec::new();
    let mut last_percent = None;
    let finished = loop {
        let poll = job.poll();
        if !poll.lines.is_empty() {
            let prefixed: Vec<String> = poll.lines.iter().map(|l| format!("  | {l}")).collect();
            log.lines(&prefixed);
            kept.extend(poll.lines);
            if kept.len() > KEEP_CHECK_LINES {
                kept.drain(..kept.len() - KEEP_CHECK_LINES);
            }
        }
        if let Some(snapshot) = poll.finished {
            break Some(snapshot);
        }
        if poll.percent.is_some() && poll.percent != last_percent {
            last_percent = poll.percent;
            let title = format!(
                "{} {:.0}%",
                ctx.step.doing(),
                poll.percent.unwrap_or_default()
            );
            progress.set(ctx.step.key(), &title, ctx.index, poll.percent, false);
        }
        if ctx.clock.elapsed() > sys.run_limit() {
            job.abandon();
            break None;
        }
        std::thread::sleep(sys.poll_interval());
    };
    step.elapsed_ms = started.elapsed().as_millis() as u64;
    let Some(snapshot) = finished else {
        step.outcome = StepOutcome::LeftRunning;
        step.text =
            "Still running when the run reached its time limit; it finishes on its own.".into();
        log.line(format!("  {}", step.text));
        return step;
    };
    step.exit_code_hex = snapshot.exit_code_hex.clone();
    step.restart_required = snapshot.restart_required;
    if tool == ToolId::SfcVerify {
        let judged = sfc::judge(&kept, &sys.sfc_messages(), snapshot.exit_code.unwrap_or(-1));
        step.outcome = judged.outcome;
        step.text = judged.text;
        step.hint = judged.hint;
        step.windows_message = judged.windows_message;
    } else {
        let (outcome, text, hint) = match snapshot.state {
            JobState::Succeeded => (
                StepOutcome::Ok,
                snapshot
                    .summary
                    .clone()
                    .unwrap_or_else(|| "No component store damage was found.".into()),
                None,
            ),
            JobState::Attention => (
                StepOutcome::Attention,
                "The component store needs repair.".to_string(),
                snapshot.hint.clone(),
            ),
            JobState::Completed => (
                StepOutcome::Unknown,
                "Finished; the result is in the log.".to_string(),
                None,
            ),
            JobState::Failed | JobState::Cancelled | JobState::Running => (
                StepOutcome::Failed,
                "DISM couldn't check the component store.".to_string(),
                snapshot.hint.clone().or_else(|| snapshot.summary.clone()),
            ),
        };
        step.outcome = outcome;
        step.text = text;
        step.hint = hint;
    }
    let mut line = format!("  {}", step.text);
    if let Some(hint) = &step.hint {
        line.push(' ');
        line.push_str(hint);
    }
    log.line(line);
    step
}

/// The things worth looking at, most important first: failed steps, then checks that need
/// attention, then cleanup locations that could not be cleaned.
fn attention_list(cleanup: Option<&CleanupStep>, checks: &[CheckStep]) -> Vec<String> {
    let sentence = |text: &str, hint: Option<&String>| match hint {
        Some(hint) => format!("{text} {hint}"),
        None => text.to_string(),
    };
    let mut list = Vec::new();
    if let Some(error) = cleanup.and_then(|c| c.error.as_ref()) {
        list.push(format!("The cleanup failed: {error}"));
    }
    for check in checks.iter().filter(|c| c.outcome == StepOutcome::Failed) {
        list.push(sentence(&check.text, check.hint.as_ref()));
    }
    for check in checks
        .iter()
        .filter(|c| c.outcome == StepOutcome::Attention)
    {
        list.push(sentence(&check.text, check.hint.as_ref()));
    }
    if let Some(cleanup) = cleanup.filter(|c| c.error.is_none()) {
        for target in cleanup.targets.iter().filter(|t| t.outcome == "failed") {
            list.push(match &target.reason {
                Some(reason) => format!("{} couldn't be cleaned: {reason}", target.title),
                None => format!("{} couldn't be cleaned.", target.title),
            });
        }
    }
    list
}

/// The result in one line, parts joined with "  ·  ".
fn headline(
    cleanup: Option<&CleanupStep>,
    checks: &[CheckStep],
    stopped: Option<&str>,
    state: RunState,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(cleanup) = cleanup {
        match cleanup.outcome {
            StepOutcome::Ok | StepOutcome::Attention => {
                parts.push(format!("Freed {}", fmt_size(cleanup.freed_bytes)))
            }
            StepOutcome::Skipped => parts.push("cleanup skipped".into()),
            StepOutcome::Failed => parts.push("cleanup failed".into()),
            _ => {}
        }
    }
    for check in checks {
        let sfc = check.tool == ToolId::SfcVerify;
        let part = match check.outcome {
            StepOutcome::Ok if sfc => "no system file problems",
            StepOutcome::Ok => "component store OK",
            StepOutcome::Attention if sfc && check.text.starts_with("Windows found damaged") => {
                "damaged system files found"
            }
            StepOutcome::Attention if sfc => "system files need a look",
            StepOutcome::Attention => "component store needs repair",
            StepOutcome::Failed if sfc => "system file check failed",
            StepOutcome::Failed => "component store check failed",
            StepOutcome::Skipped if sfc => "system files not checked",
            StepOutcome::Skipped => "component store not checked",
            StepOutcome::LeftRunning if sfc => "system file check still running",
            StepOutcome::LeftRunning => "component store check still running",
            StepOutcome::Unknown if sfc => "system file check finished",
            StepOutcome::Unknown => "component store check finished",
            StepOutcome::NotRun => continue,
        };
        parts.push(part.to_string());
    }
    if parts.is_empty() {
        let reason = stopped.unwrap_or("nothing ran");
        return match state {
            RunState::Skipped => format!("Skipped: {reason}"),
            _ => format!("Stopped: {reason}"),
        };
    }
    let mut text = parts.join("  ·  ");
    if let Some(first) = text.get(..1) {
        let upper = first.to_uppercase();
        text.replace_range(..1, &upper);
    }
    text
}

#[cfg(test)]
mod tests;
