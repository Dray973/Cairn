//! Tool jobs: one maintenance tool at a time runs as a background job that the UI polls.
//!
//! A job's process writes its output to the run's `.raw` file. A watcher thread per job
//! reads that file through a handle of its own, decodes it, appends the lines to the
//! readable `.log` transcript and keeps the newest lines in memory for polling. When the
//! process ends, the exit code is judged and one final row goes to the journal's audit log.
//! Tool runs are only audited: nothing they change is recorded for rollback.
//!
//! Locks guard in-memory state only and are held for microseconds. No lock is held across
//! file, SQLite, process or thread-spawn I/O, or across the component store probe, so
//! [`ToolRunner::view`], [`ToolRunner::jobs`] and [`ToolRunner::cancel`] never wait for any
//! of them.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::catalog::{classify, exit_code_hex, IfDisabled, Outcome, ToolId, ToolInfo, ToolRequest};
use super::launch::{CommandSpec, DetachPolicy, Launcher, RunningProcess, SystemLauncher};
use super::logs::{self, LogFiles};
use super::progress::{percent, ProgressRule, ProgressTracker};
use super::store_health::{self, StoreHealth};
use super::ToolVolume;
use crate::jobs::lines::Lines;
use crate::safety::state_log::Journal;
use crate::win::console_text::{OutputDecoder, OutputEvent};
use crate::win::registry::{self, Hive, RegValue};
use crate::win::scm::{self, Scm, StartType};
use crate::win::storage::MediaKind;
use crate::{Error, Result};

/// Most output lines one [`ToolRunner::view`] returns.
pub const MAX_LINES_PER_VIEW: usize = 500;
/// Output lines kept in memory per job; older lines are only in the `.log` file.
pub const MAX_JOB_LINES: usize = 2000;
/// Finished jobs kept for [`ToolRunner::jobs`] besides the running one.
const KEPT_FINISHED_JOBS: usize = 10;
/// How often a watcher looks at its process and output.
const DEFAULT_TICK: Duration = Duration::from_millis(100);
/// Most output read per tick; the rest waits for the next tick.
const MAX_READ_PER_TICK: usize = 1024 * 1024;
/// Output decoded per job; the rest stays in the raw file only.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const READ_BUFFER: usize = 64 * 1024;
/// How long [`ToolRunner::shutdown`] waits for a stopped job to end.
const STOP_WAIT: Duration = Duration::from_secs(3);
/// How long [`ToolRunner::shutdown`] waits for the run of a process that has ended to be
/// judged; after a DISM check that includes reading the component store state.
const FINISH_WAIT: Duration = Duration::from_secs(10);
/// How long [`ToolRunner::shutdown`] waits for a start in progress to settle.
const START_SETTLE: Duration = Duration::from_secs(3);

/// Audit log operation of tool runs.
const OP: &str = "tool";
const NEEDS_ADMIN: &str = "needs administrator rights";
pub(crate) const CLOSING: &str = "Cairn is closing; the tool was not started";
const OUTPUT_CAPPED: &str = "… output continues in the raw log file";
const READING_STORE: &str = "Reading the component store state…";
pub(crate) const STORE_NOT_READ: &str = "Cairn closed before reading the component store state";
const PROCESS_LIST_NOTE: &str = "Could not check whether another repair is running.";
const SERVICING_NOTE: &str =
    "Windows is installing updates; the tool may wait until that finishes.";
const REBOOT_PENDING_NOTE: &str = "Windows is waiting for a restart to finish installing updates; \
     SFC and DISM may refuse to run or report pending repairs until you restart.";
const HDD_NOTE: &str =
    "Defragmenting a hard disk can take a long time and slows the PC while it runs.";
pub(crate) const UNKNOWN_MEDIA_NOTE: &str = "Cairn couldn't tell whether this drive is an SSD; \
     Windows reports an error if it doesn't support retrim.";
const TRIM_OFF_NOTE: &str = "Windows TRIM is turned off (DisableDeleteNotification = 1), so \
     retrim has no effect until it is turned back on.";
const DOWNLOAD_NOTE: &str =
    "Downloads repair files from Windows Update; needs an internet connection.";
pub(crate) const RUNS_TO_COMPLETION_NOTE: &str =
    "Runs to completion; Cairn can't stop it once it starts.";

const REBOOT_PENDING_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending";
const FILE_SYSTEM_KEY: &str = r"SYSTEM\CurrentControlSet\Control\FileSystem";

// ───────────────────────────── Public types ─────────────────────────────

/// Identifies a job for the lifetime of the process; ids start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a job stands. Every state but `Running` is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    /// Finished and the result is known to be good.
    Succeeded,
    /// Finished; the tool reported its result only as text, which the output shows.
    Completed,
    /// Finished and reported problems.
    Attention,
    Failed,
    /// Stopped on request.
    Cancelled,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        self != JobState::Running
    }

    /// Outcome of the final audit row.
    pub fn audit_outcome(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Completed => "completed",
            JobState::Attention => "problems_found",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    fn label(self) -> &'static str {
        match self {
            JobState::Running => "Running",
            JobState::Succeeded => "Finished",
            JobState::Completed => "Finished; the result is in the output above",
            JobState::Attention => "Needs attention",
            JobState::Failed => "Failed",
            JobState::Cancelled => "Stopped",
        }
    }
}

/// What starting a tool would do, and why it cannot start now, if it cannot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolPlan {
    pub tool: ToolId,
    pub title: String,
    pub volume: Option<String>,
    /// Absolute path of the program.
    pub program: String,
    pub args: Vec<String>,
    pub command_line: String,
    pub requires_admin: bool,
    pub cancellable: bool,
    pub requires_detach: bool,
    pub changes_system: bool,
    pub duration_hint: String,
    /// The first reason the tool cannot start now; `None` when it can.
    pub blocked_reason: Option<String>,
    /// Things worth knowing before it starts.
    pub notes: Vec<String>,
}

/// The state of a job at one moment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub tool: ToolId,
    pub title: String,
    pub command_line: String,
    pub volume: Option<String>,
    pub state: JobState,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub finished_at: Option<String>,
    pub elapsed_ms: u64,
    /// Time since the last output (until the job finished).
    pub idle_ms: u64,
    /// Percentage read from the progress output, rounded to one decimal.
    pub progress: Option<f64>,
    /// The latest progress text the tool redrew in place.
    pub progress_line: Option<String>,
    /// Set as soon as the process has ended, while the job may still be `Running`: its
    /// output is being read to the end and its result judged.
    pub exit_code: Option<i32>,
    pub exit_code_hex: Option<String>,
    pub cancellable: bool,
    pub cancel_requested: bool,
    /// The process runs outside Cairn's job object and outlives it.
    pub detached: bool,
    pub restart_required: bool,
    pub hint: Option<String>,
    pub summary: Option<String>,
    /// The readable UTF-8 transcript.
    pub log_path: String,
    /// The tool's own output.
    pub raw_log_path: String,
    /// Output lines so far.
    pub line_count: u64,
    /// The run's audit rows were written; false when the final row could not be.
    pub logged: bool,
}

/// A job's snapshot and a page of its output lines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub job: JobSnapshot,
    /// Output lines after the requested one, oldest first.
    pub lines: Vec<String>,
    /// Number of the first line in `lines` (lines are numbered from 1); `None` when empty.
    pub first: Option<u64>,
    /// Number of the last line delivered; pass it as `after` to continue.
    pub next: u64,
    /// Lines after the requested one that are no longer kept in memory, only in the log.
    pub skipped: u64,
    /// More lines are waiting after this page.
    pub more: bool,
}

/// What closing Cairn did with a running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownAction {
    /// The job was stopped, or its process had ended and its exit code was recorded before
    /// the rest of its result was read.
    Stopped,
    /// The job was asked to stop but was still running when the wait ended.
    StopTimedOut,
    /// The job runs outside Cairn's job object and finishes on its own.
    LeftRunning,
    /// The job runs inside Cairn's job object; Windows may end it with Cairn.
    MayEndWithApp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShutdownOutcome {
    pub id: JobId,
    pub tool: ToolId,
    pub action: ShutdownAction,
}

// ───────────────────────────── Environment ─────────────────────────────

pub type ElevatedFn = fn() -> bool;
/// Lowercase executable names of the running processes; `None` when unreadable.
pub type ProcessesFn = fn() -> Option<HashSet<String>>;
pub type VolumesFn = fn() -> Result<Vec<ToolVolume>>;
/// Start type of a service; `None` when the service does not exist.
pub type ServiceStartFn = fn(&str) -> Result<Option<StartType>>;
pub type RebootPendingFn = fn() -> Result<bool>;
pub type SystemDirFn = fn() -> Result<PathBuf>;
pub type StoreHealthFn = fn() -> Result<StoreHealth>;
/// Whether Windows TRIM is turned off; `None` when unreadable.
pub type DeleteNotifyFn = fn() -> Option<bool>;
/// Title of a disk speed test running on the volume (`C:`), if any.
pub type StorageBusyFn = fn(&str) -> Option<String>;

/// Everything the runner reads from the system besides the processes it starts.
#[derive(Clone, Copy)]
pub struct Environment {
    pub elevated: ElevatedFn,
    pub processes: ProcessesFn,
    pub volumes: VolumesFn,
    pub service_start: ServiceStartFn,
    /// Windows servicing waits for a restart.
    pub servicing_reboot_pending: RebootPendingFn,
    pub system_dir: SystemDirFn,
    /// The component store state; read only after a DISM check or scan ended with 0.
    pub store_health: StoreHealthFn,
    pub delete_notify_disabled: DeleteNotifyFn,
    /// Title of a disk speed test running on the volume, if any.
    pub storage_busy_on: StorageBusyFn,
}

impl fmt::Debug for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment").finish_non_exhaustive()
    }
}

impl Environment {
    /// This PC.
    pub const SYSTEM: Environment = Environment {
        elevated: crate::is_elevated,
        processes: crate::win::process::running_process_names,
        volumes: super::volumes,
        service_start: system_service_start,
        servicing_reboot_pending: system_reboot_pending,
        system_dir: crate::win::paths::system_dir,
        store_health: store_health::check_online_image,
        delete_notify_disabled: system_delete_notify_disabled,
        storage_busy_on: crate::storage::busy_on,
    };
}

fn system_service_start(name: &str) -> Result<Option<StartType>> {
    let manager = Scm::connect()?;
    match manager.open(name, scm::READ_ACCESS)? {
        Some(service) => Ok(Some(service.config()?.start_type)),
        None => Ok(None),
    }
}

fn system_reboot_pending() -> Result<bool> {
    registry::exists(Hive::LocalMachine, REBOOT_PENDING_KEY)
}

fn system_delete_notify_disabled() -> Option<bool> {
    match registry::read_value(
        Hive::LocalMachine,
        FILE_SYSTEM_KEY,
        "DisableDeleteNotification",
    ) {
        Ok(Some(RegValue::Dword(value))) => Some(value == 1),
        Ok(None) => Some(false),
        Ok(Some(_)) | Err(_) => None,
    }
}

// ───────────────────────────── Jobs ─────────────────────────────

/// The parts of a job that change while it runs.
#[derive(Debug)]
struct Status {
    state: JobState,
    finished_at: Option<String>,
    finished: Option<Instant>,
    last_output: Instant,
    progress: Option<f32>,
    progress_line: Option<String>,
    exit_code: Option<i32>,
    restart_required: bool,
    hint: Option<String>,
    summary: Option<String>,
    logged: bool,
}

struct Job {
    id: JobId,
    request: ToolRequest,
    command_line: String,
    stem: String,
    log_path: PathBuf,
    raw_path: PathBuf,
    detached: bool,
    started: Instant,
    started_at: String,
    status: Mutex<Status>,
    /// When both are held, `status` is locked first.
    lines: Mutex<Lines>,
    /// A stop was requested.
    cancel: AtomicBool,
    /// The watcher ended the process on request, while it was still running.
    stopped: AtomicBool,
    /// Claimed by whoever writes the final audit row, so exactly one is written.
    finalized: AtomicBool,
    /// The state is final.
    done: AtomicBool,
    /// Locked only to call `try_wait` and `kill`, which do not block.
    process: Mutex<Option<Box<dyn RunningProcess>>>,
    journal: Mutex<Option<Arc<Journal>>>,
    /// Taken by the watcher when it starts.
    reader: Mutex<Option<File>>,
    /// Taken by the watcher when it starts.
    log: Mutex<Option<File>>,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("tool", &self.request.tool)
            .field("done", &self.done.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Job {
    fn info(&self) -> &'static ToolInfo {
        self.request.info()
    }

    fn is_running(&self) -> bool {
        !self.done.load(Ordering::SeqCst)
    }

    fn claim(&self) -> bool {
        self.finalized
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// The exit code once the process has ended, whether or not the watcher has seen it.
    fn exit_code(&self) -> Option<i32> {
        let seen = self.status.lock().exit_code;
        seen.or_else(
            || match self.process.lock().as_mut().map(|p| p.try_wait()) {
                Some(Ok(code)) => code,
                _ => None,
            },
        )
    }

    /// The run was stopped: the watcher ended a process that can be stopped.
    fn was_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) && self.info().cancellable
    }

    fn snapshot(&self) -> JobSnapshot {
        let status = self.status.lock();
        // Counted after the state is read: a job's last lines are pushed before its state
        // becomes final, so a finished snapshot counts every line.
        let line_count = self.lines.lock().total;
        let end = status.finished.unwrap_or_else(Instant::now);
        JobSnapshot {
            id: self.id,
            tool: self.request.tool,
            title: self.info().title.to_string(),
            command_line: self.command_line.clone(),
            volume: self.request.volume.clone(),
            state: status.state,
            started_at: self.started_at.clone(),
            finished_at: status.finished_at.clone(),
            elapsed_ms: millis(end.saturating_duration_since(self.started)),
            idle_ms: millis(end.saturating_duration_since(status.last_output)),
            // Rounded in f64, so 17.9 stays 17.9 rather than the f32 value widened.
            progress: status
                .progress
                .map(|p| (f64::from(p) * 10.0).round() / 10.0),
            progress_line: status.progress_line.clone(),
            exit_code: status.exit_code,
            exit_code_hex: status.exit_code.map(exit_code_hex),
            cancellable: self.info().cancellable,
            cancel_requested: self.cancel.load(Ordering::SeqCst),
            detached: self.detached,
            restart_required: status.restart_required,
            hint: status.hint.clone(),
            summary: status.summary.clone(),
            log_path: self.log_path.display().to_string(),
            raw_log_path: self.raw_path.display().to_string(),
            line_count,
            logged: status.logged,
        }
    }

    /// Writes an audit row for this run; false (and an error trace) when it fails.
    fn audit(&self, journal: &Journal, outcome: &str, detail: &str) -> bool {
        match journal.log_op(None, OP, &self.command_line, outcome, Some(detail)) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    job = self.id.0,
                    tool = %self.request.tool,
                    outcome,
                    error = %e,
                    "cannot write the tool run's audit row"
                );
                false
            }
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// "45 s", "12 min 5 s", "1 h 3 min".
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {} s", secs / 60, secs % 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

fn already_running(title: &str) -> String {
    format!("{title} is already running in Cairn")
}

// ───────────────────────────── Runner ─────────────────────────────

#[derive(Debug, Default)]
struct Registry {
    jobs: Vec<Arc<Job>>,
    /// A start is between its checks and registering its job.
    starting: Option<ToolId>,
}

#[derive(Debug, Default)]
struct Shared {
    closed: AtomicBool,
    registry: Mutex<Registry>,
}

impl Shared {
    fn jobs(&self) -> Vec<Arc<Job>> {
        self.registry.lock().jobs.clone()
    }

    fn find(&self, id: JobId) -> Option<Arc<Job>> {
        self.registry
            .lock()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// Title of the running job, or of the tool being started.
    fn busy_title(&self) -> Option<&'static str> {
        let registry = self.registry.lock();
        registry
            .jobs
            .iter()
            .find(|j| j.is_running())
            .map(|j| j.info().title)
            .or_else(|| registry.starting.map(|t| t.info().title))
    }

    /// Keeps the running jobs and the newest finished ones.
    fn prune(&self) {
        let mut finished: Vec<JobId> = self
            .jobs()
            .iter()
            .filter(|j| !j.is_running())
            .map(|j| j.id)
            .collect();
        finished.sort_unstable_by(|a, b| b.cmp(a));
        let dropped: HashSet<JobId> = finished.into_iter().skip(KEPT_FINISHED_JOBS).collect();
        if !dropped.is_empty() {
            self.registry
                .lock()
                .jobs
                .retain(|j| !dropped.contains(&j.id));
        }
    }
}

/// Clears the start reservation when a start ends early.
struct Reservation<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Reservation<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.registry.lock().starting = None;
        }
    }
}

/// Runs maintenance tools one at a time as background jobs.
pub struct ToolRunner {
    launcher: Arc<dyn Launcher>,
    env: Environment,
    log_dir: PathBuf,
    tick: Duration,
    keep_logs: usize,
    stop_wait: Duration,
    finish_wait: Duration,
    next_id: AtomicU64,
    shared: Arc<Shared>,
    #[cfg(test)]
    decode_hook: Option<fn(&[u8])>,
}

impl fmt::Debug for ToolRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRunner")
            .field("launcher", &self.launcher)
            .field("log_dir", &self.log_dir)
            .field("tick", &self.tick)
            .field("closed", &self.shared.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// The process-wide runner: real processes, this PC, logs next to the default journal.
pub fn runner() -> &'static ToolRunner {
    static RUNNER: OnceLock<ToolRunner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        ToolRunner::new(
            Arc::new(SystemLauncher::new()),
            Environment::SYSTEM,
            logs::default_dir(),
        )
    })
}

impl ToolRunner {
    pub fn new(launcher: Arc<dyn Launcher>, env: Environment, log_dir: PathBuf) -> ToolRunner {
        ToolRunner {
            launcher,
            env,
            log_dir,
            tick: DEFAULT_TICK,
            keep_logs: logs::KEEP_RUNS,
            stop_wait: STOP_WAIT,
            finish_wait: FINISH_WAIT,
            next_id: AtomicU64::new(1),
            shared: Arc::new(Shared::default()),
            #[cfg(test)]
            decode_hook: None,
        }
    }

    /// How often watchers look at their process and output (100 ms by default).
    pub fn with_tick(mut self, tick: Duration) -> ToolRunner {
        self.tick = tick;
        self
    }

    #[cfg(test)]
    fn with_stop_wait(mut self, wait: Duration) -> ToolRunner {
        self.stop_wait = wait;
        self
    }

    #[cfg(test)]
    fn with_finish_wait(mut self, wait: Duration) -> ToolRunner {
        self.finish_wait = wait;
        self
    }

    #[cfg(test)]
    fn with_decode_hook(mut self, hook: fn(&[u8])) -> ToolRunner {
        self.decode_hook = Some(hook);
        self
    }

    pub fn log_dir(&self) -> &std::path::Path {
        &self.log_dir
    }

    /// What starting `request` would run, and why it cannot start now. Read-only: nothing is
    /// started or written.
    pub fn plan(&self, request: &ToolRequest) -> Result<ToolPlan> {
        Ok(self.plan_checked(request)?.0)
    }

    /// The plan and whether this process is elevated.
    fn plan_checked(&self, request: &ToolRequest) -> Result<(ToolPlan, bool)> {
        let info = request.info();
        let program = (self.env.system_dir)()?.join(info.program);
        let mut blocks: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        if !program.is_file() {
            blocks.push(format!("{} is not on this PC", info.program));
        }
        let elevated = (self.env.elevated)();
        if !elevated {
            blocks.push(NEEDS_ADMIN.to_string());
        }
        if let Some(title) = self.shared.busy_title() {
            blocks.push(already_running(title));
        }
        match (self.env.processes)() {
            None => notes.push(PROCESS_LIST_NOTE.to_string()),
            Some(names) => {
                let family = &info.family;
                if let Some(conflict) = family.conflicts.iter().find(|c| names.contains(c.exe)) {
                    blocks.push(format!(
                        "{} is already running (started outside Cairn, or before it was \
                         last closed). Wait for it to finish.",
                        conflict.name
                    ));
                }
                if family.servicing_note && names.contains("tiworker.exe") {
                    notes.push(SERVICING_NOTE.to_string());
                }
            }
        }
        for need in info.services {
            match (self.env.service_start)(need.name) {
                Ok(Some(StartType::Disabled)) => match need.if_disabled {
                    IfDisabled::Block(text) => blocks.push(text.to_string()),
                    IfDisabled::Note(text) => notes.push(text.to_string()),
                },
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(service = need.name, error = %e, "cannot read a service");
                    notes.push(format!("Could not check the {} service.", need.display));
                }
            }
        }
        if info.family.servicing_note && matches!((self.env.servicing_reboot_pending)(), Ok(true)) {
            notes.push(REBOOT_PENDING_NOTE.to_string());
        }
        if let Some(letter) = &request.volume {
            self.volume_checks(request.tool, letter, &mut blocks, &mut notes);
        }
        if request.tool == ToolId::DismRestore {
            notes.push(DOWNLOAD_NOTE.to_string());
        }
        if !info.cancellable {
            notes.push(RUNS_TO_COMPLETION_NOTE.to_string());
        }

        let plan = ToolPlan {
            tool: request.tool,
            title: info.title.to_string(),
            volume: request.volume.clone(),
            program: program.display().to_string(),
            args: request.args(),
            command_line: request.command_line(),
            requires_admin: info.requires_admin,
            cancellable: info.cancellable,
            requires_detach: info.requires_detach,
            changes_system: info.changes_system,
            duration_hint: info.duration_hint.to_string(),
            blocked_reason: blocks.into_iter().next(),
            notes,
        };
        Ok((plan, elevated))
    }

    fn volume_checks(
        &self,
        tool: ToolId,
        letter: &str,
        blocks: &mut Vec<String>,
        notes: &mut Vec<String>,
    ) {
        let volumes = match (self.env.volumes)() {
            Ok(volumes) => volumes,
            Err(e) => {
                blocks.push(format!("Could not read the drives: {e}"));
                return;
            }
        };
        let Some(listed) = volumes
            .iter()
            .find(|v| v.volume.letter.eq_ignore_ascii_case(letter))
        else {
            blocks.push(format!("{letter} is not a fixed drive on this PC"));
            return;
        };
        let blocked = match tool {
            ToolId::DriveOptimize => &listed.optimize_blocked,
            ToolId::DriveRetrim => &listed.retrim_blocked,
            _ => &listed.check_blocked,
        };
        if let Some(reason) = blocked {
            blocks.push(reason.clone());
        }
        if matches!(
            tool,
            ToolId::DriveOptimize | ToolId::DriveRetrim | ToolId::DiskCheck
        ) {
            if let Some(title) = (self.env.storage_busy_on)(&listed.volume.letter) {
                blocks.push(format!(
                    "{title} is running on {letter}; wait for it to finish or stop it."
                ));
            }
        }
        let media = listed.volume.media;
        match tool {
            ToolId::DiskCheck if listed.volume.system => notes.push(format!(
                "{letter} is in use, so the check runs read-only on a live file system and can \
                 report problems that are not real."
            )),
            ToolId::DriveOptimize if media == MediaKind::Hdd => notes.push(HDD_NOTE.to_string()),
            ToolId::DriveRetrim if media == MediaKind::Unknown => {
                notes.push(UNKNOWN_MEDIA_NOTE.to_string())
            }
            _ => {}
        }
        if matches!(tool, ToolId::DriveOptimize | ToolId::DriveRetrim)
            && (self.env.delete_notify_disabled)() == Some(true)
        {
            notes.push(TRIM_OFF_NOTE.to_string());
        }
    }

    /// The plan of `request` and, unless `dry_run` is set, the job started under the journal
    /// `open_journal` returns. A dry run never calls `open_journal`: it opens no journal and
    /// starts nothing.
    pub fn plan_or_start(
        &self,
        dry_run: bool,
        open_journal: impl FnOnce() -> Result<Arc<Journal>>,
        request: &ToolRequest,
    ) -> Result<(ToolPlan, Option<JobSnapshot>)> {
        let plan = self.plan(request)?;
        if dry_run {
            return Ok((plan, None));
        }
        let job = self.start(open_journal()?, request)?;
        Ok((plan, Some(job)))
    }

    /// Starts `request` as a background job and returns its first snapshot. Refuses when this
    /// process is not elevated, when the plan is blocked, while another job runs and after
    /// [`ToolRunner::shutdown`]. The "started" audit row is written before the process
    /// starts; every later failure to start adds a "failed" row.
    pub fn start(&self, journal: Arc<Journal>, request: &ToolRequest) -> Result<JobSnapshot> {
        let (plan, elevated) = self.plan_checked(request)?;
        if !elevated {
            return Err(Error::NotElevated);
        }
        if let Some(reason) = plan.blocked_reason {
            return Err(Error::Other(reason));
        }

        let mut protect: Vec<String> = {
            let mut registry = self.shared.registry.lock();
            if self.shared.closed.load(Ordering::SeqCst) {
                return Err(Error::Other(CLOSING.to_string()));
            }
            let busy = registry
                .jobs
                .iter()
                .find(|j| j.is_running())
                .map(|j| j.info().title)
                .or_else(|| registry.starting.map(|t| t.info().title));
            if let Some(title) = busy {
                return Err(Error::Other(already_running(title)));
            }
            registry.starting = Some(request.tool);
            registry
                .jobs
                .iter()
                .filter(|j| j.is_running())
                .map(|j| j.stem.clone())
                .collect()
        };
        let mut reservation = Reservation {
            shared: &self.shared,
            armed: true,
        };

        let files = logs::create(&self.log_dir, request, Local::now())?;
        protect.push(files.stem.clone());
        logs::prune(&self.log_dir, self.keep_logs, &protect);

        let command_line = request.command_line();
        let started_detail = format!("log {}", files.log_path.display());
        if let Err(e) = journal.log_op(None, OP, &command_line, "started", Some(&started_detail)) {
            let LogFiles {
                raw_path,
                raw,
                log_path,
                log,
                ..
            } = files;
            drop((raw, log));
            let _ = fs::remove_file(&raw_path);
            let _ = fs::remove_file(&log_path);
            return Err(e);
        }
        let LogFiles {
            stem,
            raw_path,
            raw,
            log_path,
            mut log,
        } = files;
        let not_started = |log: &mut File, reason: &str| {
            if let Err(e) = journal.log_op(None, OP, &command_line, "failed", Some(reason)) {
                tracing::error!(error = %e, "cannot write the tool run's audit row");
            }
            let _ = logs::append_footer(log, &format!("Not started: {reason}"));
        };

        if self.shared.closed.load(Ordering::SeqCst) {
            not_started(&mut log, CLOSING);
            return Err(Error::Other(CLOSING.to_string()));
        }
        // The watcher reads through a handle of its own; the child's handles share the
        // write position and are never read through.
        let reader = match File::open(&raw_path) {
            Ok(reader) => reader,
            Err(e) => {
                not_started(&mut log, &format!("could not start: {e}"));
                return Err(e.into());
            }
        };
        let spec = CommandSpec {
            program: PathBuf::from(&plan.program),
            args: plan.args.clone(),
            detach: DetachPolicy::for_tool(request.info()),
        };
        let launched = match self.launcher.launch(&spec, raw) {
            Ok(launched) => launched,
            Err(e) => {
                not_started(&mut log, &format!("could not start: {e}"));
                return Err(e);
            }
        };

        let id = JobId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let now = Instant::now();
        let job = Arc::new(Job {
            id,
            request: request.clone(),
            command_line,
            stem,
            log_path,
            raw_path,
            detached: launched.detached,
            started: now,
            started_at: utc_now(),
            status: Mutex::new(Status {
                state: JobState::Running,
                finished_at: None,
                finished: None,
                last_output: now,
                progress: None,
                progress_line: None,
                exit_code: None,
                restart_required: false,
                hint: None,
                summary: None,
                logged: true,
            }),
            lines: Mutex::new(Lines::default()),
            cancel: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            finalized: AtomicBool::new(false),
            done: AtomicBool::new(false),
            process: Mutex::new(Some(launched.process)),
            journal: Mutex::new(Some(journal)),
            reader: Mutex::new(Some(reader)),
            log: Mutex::new(Some(log)),
        });
        {
            let mut registry = self.shared.registry.lock();
            registry.jobs.push(Arc::clone(&job));
            registry.starting = None;
        }
        reservation.disarm();

        let context = WatchContext {
            env: self.env,
            tick: self.tick,
            shared: Arc::clone(&self.shared),
            #[cfg(test)]
            decode_hook: self.decode_hook,
        };
        let watched = Arc::clone(&job);
        let spawned = thread::Builder::new()
            .name(format!("tool-{id}"))
            .spawn(move || run_watcher(watched, context));
        if let Err(e) = spawned {
            if let Some(process) = job.process.lock().as_mut() {
                let _ = process.kill();
            }
            let reason = format!("could not follow the tool: {e}");
            if job.claim() {
                // Bound first, so the journal lock is released before the SQLite write.
                let journal = job.journal.lock().take();
                if let Some(journal) = journal {
                    job.audit(&journal, "failed", &reason);
                }
            }
            job.done.store(true, Ordering::SeqCst);
            self.shared.registry.lock().jobs.retain(|j| j.id != job.id);
            return Err(Error::Other(reason));
        }
        Ok(job.snapshot())
    }

    /// The job's snapshot and up to `max_lines` output lines numbered after `after` (at
    /// most [`MAX_LINES_PER_VIEW`]); `None` for an unknown id.
    pub fn view(&self, id: JobId, after: u64, max_lines: usize) -> Option<JobView> {
        let job = self.shared.find(id)?;
        let max = max_lines.clamp(1, MAX_LINES_PER_VIEW);
        // A job's last lines are pushed before its state becomes final, so a page read after
        // a finished snapshot holds every line (or reports `more`).
        let snapshot = job.snapshot();
        let (lines, first, next, skipped, more) = job.lines.lock().page(after, max);
        Some(JobView {
            job: snapshot,
            lines,
            first,
            next,
            skipped,
            more,
        })
    }

    pub fn snapshot(&self, id: JobId) -> Option<JobSnapshot> {
        self.shared.find(id).map(|j| j.snapshot())
    }

    /// The running job and the newest finished ones, newest first.
    pub fn jobs(&self) -> Vec<JobSnapshot> {
        let mut jobs = self.shared.jobs();
        jobs.sort_unstable_by_key(|j| std::cmp::Reverse(j.id));
        jobs.iter().map(|j| j.snapshot()).collect()
    }

    /// The running job, if any.
    pub fn running(&self) -> Option<JobSnapshot> {
        self.shared
            .jobs()
            .iter()
            .find(|j| j.is_running())
            .map(|j| j.snapshot())
    }

    /// Asks a running job to stop; it ends within a tick. `Ok(false)` when it already
    /// finished. A process that has ended on its own by the time the job's watcher looks at
    /// it is judged as if no stop had been asked for. Fails for an unknown id and for a tool
    /// that cannot be stopped.
    pub fn cancel(&self, id: JobId) -> Result<bool> {
        let job = self
            .shared
            .find(id)
            .ok_or_else(|| Error::Other(format!("no tool job with id {id}")))?;
        let info = job.info();
        if !info.cancellable {
            return Err(Error::Other(format!(
                "{} can't be stopped; it runs to completion",
                info.title
            )));
        }
        if !job.is_running() {
            return Ok(false);
        }
        job.cancel.store(true, Ordering::SeqCst);
        Ok(true)
    }

    /// Refuses new starts; waits up to 10 s for the run of a process that has already ended
    /// to be judged, and records its exit code when that takes longer; stops the job that can
    /// be stopped (waiting up to 3 s); and records in the audit log that a job that cannot be
    /// stopped was left running. A job that finishes on its own while this waits is not
    /// listed. Waits without holding any lock.
    pub fn shutdown(&self) -> Vec<ShutdownOutcome> {
        self.shared.closed.store(true, Ordering::SeqCst);
        let settle = Instant::now() + START_SETTLE;
        while self.shared.registry.lock().starting.is_some() && Instant::now() < settle {
            thread::sleep(Duration::from_millis(10));
        }
        // Whether each process has ended is read before any stop is requested, so a process
        // stopped here is not mistaken for one that ended on its own.
        let running: Vec<(Arc<Job>, Option<i32>)> = self
            .shared
            .jobs()
            .into_iter()
            .filter(|j| j.is_running())
            .map(|j| {
                let ended = j.exit_code();
                (j, ended)
            })
            .collect();
        for (job, ended) in &running {
            if ended.is_none() && job.info().cancellable {
                job.cancel.store(true, Ordering::SeqCst);
            }
        }
        let mut outcomes = Vec::new();
        for (job, ended) in running {
            // Taken before the claim: a watcher that loses the claim drops its copy.
            let journal = job.journal.lock().clone();
            let action = if let Some(code) = ended {
                if wait_done(&job, self.finish_wait) || !job.claim() {
                    continue;
                }
                record_ended(&job, journal.as_deref(), code);
                ShutdownAction::Stopped
            } else if job.info().cancellable {
                if wait_done(&job, self.stop_wait) || !job.claim() {
                    ShutdownAction::Stopped
                } else {
                    leave_running(&job, journal.as_deref(), true);
                    ShutdownAction::StopTimedOut
                }
            } else if job.claim() {
                leave_running(&job, journal.as_deref(), job.detached);
                if job.detached {
                    ShutdownAction::LeftRunning
                } else {
                    ShutdownAction::MayEndWithApp
                }
            } else {
                continue;
            };
            outcomes.push(ShutdownOutcome {
                id: job.id,
                tool: job.request.tool,
                action,
            });
        }
        outcomes
    }

    /// Opens the job's transcript in Notepad.
    pub fn open_log(&self, id: JobId) -> Result<()> {
        let job = self
            .shared
            .find(id)
            .ok_or_else(|| Error::Other(format!("no tool job with id {id}")))?;
        let mut command = crate::win::process::system_command("notepad.exe")?;
        command.arg(&job.log_path);
        command.spawn()?;
        Ok(())
    }

    /// Waits until the job finished or `timeout` passed; the latest snapshot, `None` for an
    /// unknown id.
    pub fn wait(&self, id: JobId, timeout: Duration) -> Option<JobSnapshot> {
        let job = self.shared.find(id)?;
        wait_done(&job, timeout);
        Some(job.snapshot())
    }
}

fn wait_done(job: &Job, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !job.is_running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The final audit row of a job Cairn stops following because it closes.
fn leave_running(job: &Job, journal: Option<&Journal>, detached: bool) {
    let detail = if detached {
        format!(
            "Cairn closed while it ran; its raw output continues in {}",
            job.raw_path.display()
        )
    } else {
        "Cairn closed while it ran; Windows may end it with Cairn".to_string()
    };
    let logged = journal.is_some_and(|j| job.audit(j, "left_running", &detail));
    job.status.lock().logged = logged;
}

/// The final audit row of a job whose process ended with `code` before Cairn closed,
/// when the rest of its result was not read in time: the verdict of the exit code alone.
fn record_ended(job: &Job, journal: Option<&Journal>, code: i32) {
    let tool = job.request.tool;
    let cancelled = job.was_stopped();
    let outcome = classify(tool, code, cancelled, None);
    let mut detail = exit_detail(code, job.started.elapsed());
    if probes_store(tool, code, cancelled) {
        detail.push_str(" · ");
        detail.push_str(STORE_NOT_READ);
    }
    let logged = journal.is_some_and(|j| job.audit(j, outcome.state.audit_outcome(), &detail));
    job.status.lock().logged = logged;
}

/// Whether a run that ended with `code` is judged by the component store state as well.
fn probes_store(tool: ToolId, code: i32, cancelled: bool) -> bool {
    matches!(tool, ToolId::DismCheck | ToolId::DismScan) && code == 0 && !cancelled
}

/// The detail of a final audit row: "exit 0x00000000 (0) · 14 min 2 s".
pub(crate) fn exit_detail(code: i32, elapsed: Duration) -> String {
    format!(
        "exit {} ({code}) · {}",
        exit_code_hex(code),
        fmt_duration(elapsed)
    )
}

// ───────────────────────────── Watcher ─────────────────────────────

#[derive(Clone)]
struct WatchContext {
    env: Environment,
    tick: Duration,
    shared: Arc<Shared>,
    #[cfg(test)]
    decode_hook: Option<fn(&[u8])>,
}

impl WatchContext {
    #[cfg(test)]
    fn before_decode(&self, chunk: &[u8]) {
        if let Some(hook) = self.decode_hook {
            hook(chunk);
        }
    }

    #[cfg(not(test))]
    fn before_decode(&self, _chunk: &[u8]) {}
}

/// How a process ended, as far as Cairn knows.
enum Ended {
    Exit(i32),
    Failed(String),
}

/// The watcher's side of a job's output: the raw file read handle, the transcript, and the
/// decoding state.
struct Output {
    reader: Option<File>,
    log: Option<File>,
    decoder: OutputDecoder,
    tracker: ProgressTracker,
    buf: Vec<u8>,
    decoded: u64,
    capped: bool,
    /// The percentage and progress text the events reported last. The job shows them while
    /// no redraw with a percentage is being drawn.
    progress: Option<f32>,
    progress_line: Option<String>,
}

impl Output {
    fn take(job: &Job) -> Output {
        Output {
            reader: job.reader.lock().take(),
            log: job.log.lock().take(),
            decoder: OutputDecoder::new(job.info().encoding.text_encoding()),
            tracker: ProgressTracker::new(ProgressRule::Transient),
            buf: vec![0; READ_BUFFER],
            decoded: 0,
            capped: false,
            progress: None,
            progress_line: None,
        }
    }

    /// Reads what the process has written, up to `limit` bytes (everything when `None`),
    /// and applies the decoded events.
    fn pump(&mut self, job: &Job, context: &WatchContext, limit: Option<usize>) {
        let mut events = Vec::new();
        let limit = limit.unwrap_or(usize::MAX);
        let mut read = 0usize;
        while !self.capped && read < limit {
            let Some(reader) = self.reader.as_mut() else {
                break;
            };
            let n = match reader.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::warn!(job = job.id.0, error = %e, "cannot read the tool's output");
                    self.reader = None;
                    break;
                }
            };
            read += n;
            let room = usize::try_from(MAX_DECODED_BYTES - self.decoded).unwrap_or(usize::MAX);
            let chunk = &self.buf[..n.min(room)];
            context.before_decode(chunk);
            self.decoder.push(chunk, &mut events);
            self.decoded += chunk.len() as u64;
            if self.decoded >= MAX_DECODED_BYTES {
                self.decoder.finish(&mut events);
                events.push(OutputEvent::Line(OUTPUT_CAPPED.to_string()));
                self.capped = true;
                self.reader = None;
            }
        }
        self.apply(job, events, read > 0);
    }

    /// Ends decoding: the text after the last line end becomes a line.
    fn finish(&mut self, job: &Job) {
        let mut events = Vec::new();
        if !self.capped {
            self.decoder.finish(&mut events);
        }
        self.apply(job, events, false);
    }

    /// Records the events and publishes the job's progress: the redraw being drawn when it
    /// holds a percentage, else what the events reported last. `read` says output arrived.
    fn apply(&mut self, job: &Job, events: Vec<OutputEvent>, read: bool) {
        for event in &events {
            let read_percent = self.tracker.observe(event);
            match event {
                OutputEvent::Progress(text) => {
                    self.progress_line = Some(text.clone());
                    self.progress = read_percent.or(self.progress);
                }
                OutputEvent::Line(text) => {
                    if let Some(p) = read_percent {
                        self.progress = Some(p);
                        self.progress_line = Some(text.clone());
                    }
                }
            }
        }
        let drawing = self
            .decoder
            .pending_progress()
            .and_then(|text| percent(&text).map(|p| (p, text)));
        let output = read || !events.is_empty();
        let lines: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                OutputEvent::Line(text) => Some(text),
                OutputEvent::Progress(_) => None,
            })
            .collect();
        if !lines.is_empty() {
            if let Some(log) = self.log.as_mut() {
                if let Err(e) = logs::append_lines(log, &lines) {
                    tracing::warn!(job = job.id.0, error = %e, "cannot write the tool transcript");
                    self.log = None;
                }
            }
        }
        {
            let mut status = job.status.lock();
            if output {
                status.last_output = Instant::now();
            }
            match drawing {
                Some((p, text)) => {
                    status.progress = Some(p);
                    status.progress_line = Some(text);
                }
                None => {
                    status.progress = self.progress;
                    status.progress_line.clone_from(&self.progress_line);
                }
            }
        }
        if !lines.is_empty() {
            job.lines.lock().push_all(lines);
        }
    }
}

fn run_watcher(job: Arc<Job>, context: WatchContext) {
    let watched = panic::catch_unwind(AssertUnwindSafe(|| watch(&job, &context)));
    if let Err(payload) = watched {
        let text = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        tracing::error!(job = job.id.0, tool = %job.request.tool, "tool watcher panicked: {text}");
        if job.is_running() {
            finalize(
                &job,
                &context,
                Ended::Failed(format!("internal error: {text}")),
                None,
            );
        }
    }
}

fn watch(job: &Arc<Job>, context: &WatchContext) {
    let mut output = Output::take(job);
    loop {
        output.pump(job, context, Some(MAX_READ_PER_TICK));
        let polled = job.process.lock().as_mut().map(|p| p.try_wait());
        let ended = match polled {
            Some(Ok(None)) => {
                // Only a process still running is stopped, so a stop that arrives after it
                // ended on its own does not change how its run is judged.
                if job.cancel.load(Ordering::SeqCst)
                    && job.info().cancellable
                    && !job.stopped.load(Ordering::SeqCst)
                {
                    let killed = job.process.lock().as_mut().map(|p| p.kill());
                    match killed {
                        Some(Ok(true)) => {
                            job.stopped.store(true, Ordering::SeqCst);
                            continue;
                        }
                        Some(Ok(false)) | None => {}
                        Some(Err(e)) => {
                            tracing::warn!(job = job.id.0, error = %e, "cannot stop the tool")
                        }
                    }
                }
                thread::sleep(context.tick);
                continue;
            }
            Some(Ok(Some(code))) => {
                // Published before the output is drained and the run judged, so closing Cairn
                // meanwhile knows the process has ended.
                job.status.lock().exit_code = Some(code);
                Ended::Exit(code)
            }
            Some(Err(e)) => Ended::Failed(format!("Cairn lost track of the process: {e}")),
            None => Ended::Failed("Cairn lost track of the process".to_string()),
        };
        output.pump(job, context, None);
        output.finish(job);
        finalize(job, context, ended, Some(output));
        return;
    }
}

/// Judges the run, writes the final audit row and the transcript footer unless shutdown
/// already wrote the final row, releases the process, files and journal, and publishes the
/// final state.
fn finalize(job: &Job, context: &WatchContext, ended: Ended, output: Option<Output>) {
    let tool = job.request.tool;
    // Judged by what the watcher did rather than by the request.
    let cancelled = job.was_stopped();
    // A check that would be judged by the component store state is judged by its exit code
    // alone once Cairn is closing; the row and the transcript then say so.
    let (exit_code, mut outcome, store_skipped) = match ended {
        Ended::Exit(code) => {
            let store = probes_store(tool, code, cancelled);
            let closing = context.shared.closed.load(Ordering::SeqCst);
            let health = (store && !closing).then(|| {
                job.status.lock().progress_line = Some(READING_STORE.to_string());
                (context.env.store_health)()
            });
            if let Some(Err(e)) = &health {
                tracing::warn!(job = job.id.0, error = %e, "cannot read the component store state");
            }
            (
                Some(code),
                classify(tool, code, cancelled, health.as_ref()),
                store && closing,
            )
        }
        Ended::Failed(reason) => (
            None,
            Outcome {
                state: JobState::Failed,
                hint: Some(reason),
                summary: None,
                restart_required: false,
            },
            false,
        ),
    };
    if store_skipped {
        let note = format!("{STORE_NOT_READ}.");
        outcome.summary = Some(match outcome.summary.take() {
            Some(summary) => format!("{summary} {note}"),
            None => note,
        });
    }
    let elapsed = job.started.elapsed();

    let mut output = output;
    let mut logged = None;
    // A run whose final row was written when Cairn began closing gets neither a
    // second row nor a footer: that row says it was left running, or holds its exit code.
    if job.claim() {
        let detail = match (exit_code, &outcome.hint) {
            (Some(code), _) if store_skipped => {
                format!("{} · {STORE_NOT_READ}", exit_detail(code, elapsed))
            }
            (Some(code), _) => exit_detail(code, elapsed),
            (None, hint) => format!(
                "{} · {}",
                hint.as_deref().unwrap_or("failed"),
                fmt_duration(elapsed)
            ),
        };
        let journal = job.journal.lock().clone();
        logged =
            Some(journal.is_some_and(|j| job.audit(&j, outcome.state.audit_outcome(), &detail)));
        if let Some(log) = output.as_mut().and_then(|o| o.log.as_mut()) {
            let footer = footer(&outcome, exit_code, elapsed);
            if let Err(e) = logs::append_footer(log, &footer) {
                tracing::warn!(job = job.id.0, error = %e, "cannot finish the tool transcript");
            }
        }
    }
    drop(output);
    // Each is bound first, so its lock is released before the handle or connection closes.
    let process = job.process.lock().take();
    let reader = job.reader.lock().take();
    let log = job.log.lock().take();
    let journal = job.journal.lock().take();
    drop((process, reader, log, journal));
    {
        let mut status = job.status.lock();
        status.state = outcome.state;
        status.finished = Some(Instant::now());
        status.finished_at = Some(utc_now());
        status.exit_code = exit_code;
        status.restart_required = outcome.restart_required;
        status.hint = outcome.hint;
        status.summary = outcome.summary;
        if let Some(logged) = logged {
            status.logged = logged;
        }
        if status.progress_line.as_deref() == Some(READING_STORE) {
            status.progress_line = None;
        }
    }
    job.done.store(true, Ordering::SeqCst);
    context.shared.prune();
}

fn footer(outcome: &Outcome, exit_code: Option<i32>, elapsed: Duration) -> String {
    let mut text = format!(
        "Finished {}  ·  {}",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        outcome.state.label()
    );
    match exit_code {
        Some(code) => text.push_str(&format!(
            "\nExit code {code} ({})  ·  ran for {}",
            exit_code_hex(code),
            fmt_duration(elapsed)
        )),
        None => text.push_str(&format!("\nRan for {}", fmt_duration(elapsed))),
    }
    for line in [&outcome.hint, &outcome.summary].into_iter().flatten() {
        text.push('\n');
        text.push_str(line);
    }
    if outcome.restart_required {
        text.push_str("\nRestart Windows to finish.");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Sender};

    use super::*;
    use crate::jobs::testing::{Gate, PanicLauncher, ScriptedLauncher, Step};
    use crate::safety::state_log::OpLogEntry;
    use crate::tools::eligibility;
    use crate::tools::launch::DETACH_REFUSED;
    use crate::win::volume::FixedVolume;

    // ── environment ──

    fn yes() -> bool {
        true
    }

    fn no() -> bool {
        false
    }

    fn no_processes() -> Option<HashSet<String>> {
        Some(HashSet::new())
    }

    fn dism_running() -> Option<HashSet<String>> {
        Some(["dismhost.exe".to_string()].into())
    }

    fn installing_updates() -> Option<HashSet<String>> {
        Some(["tiworker.exe".to_string()].into())
    }

    fn unreadable_processes() -> Option<HashSet<String>> {
        None
    }

    fn fixed(letter: &str, media: MediaKind, trim: Option<bool>, system: bool) -> FixedVolume {
        FixedVolume {
            letter: letter.to_string(),
            label: String::new(),
            file_system: "NTFS".to_string(),
            size_bytes: 1 << 40,
            free_bytes: 1 << 39,
            media,
            trim,
            system,
            error: None,
        }
    }

    fn test_volumes() -> Result<Vec<ToolVolume>> {
        Ok(vec![
            eligibility(fixed("C:", MediaKind::Ssd, Some(true), true)),
            eligibility(fixed("D:", MediaKind::Hdd, None, false)),
            eligibility(fixed("F:", MediaKind::Unknown, None, false)),
        ])
    }

    fn unreadable_volumes() -> Result<Vec<ToolVolume>> {
        Err(Error::Other("drive list unavailable".to_string()))
    }

    fn manual(_: &str) -> Result<Option<StartType>> {
        Ok(Some(StartType::Manual))
    }

    fn disabled(_: &str) -> Result<Option<StartType>> {
        Ok(Some(StartType::Disabled))
    }

    fn unreadable_service(_: &str) -> Result<Option<StartType>> {
        Err(Error::Other("access denied".to_string()))
    }

    fn no_reboot() -> Result<bool> {
        Ok(false)
    }

    fn reboot_pending() -> Result<bool> {
        Ok(true)
    }

    fn real_system_dir() -> Result<PathBuf> {
        crate::win::paths::system_dir()
    }

    fn missing_system_dir() -> Result<PathBuf> {
        Ok(std::env::temp_dir().join("pcoptimizer-no-such-system-dir"))
    }

    fn never_probed() -> Result<StoreHealth> {
        panic!("the component store must not be probed here")
    }

    fn trim_on() -> Option<bool> {
        Some(false)
    }

    fn trim_off() -> Option<bool> {
        Some(true)
    }

    fn no_storage_job(_: &str) -> Option<String> {
        None
    }

    fn speed_test_on_d(letter: &str) -> Option<String> {
        letter
            .eq_ignore_ascii_case("D:")
            .then(|| "Disk speed test".to_string())
    }

    const ENV: Environment = Environment {
        elevated: yes,
        processes: no_processes,
        volumes: test_volumes,
        service_start: manual,
        servicing_reboot_pending: no_reboot,
        system_dir: real_system_dir,
        store_health: never_probed,
        delete_notify_disabled: trim_on,
        storage_busy_on: no_storage_job,
    };

    // ── scripted processes ──

    /// A launcher whose launches first check that the run's "started" row is in the journal.
    fn scripted_launcher(journal_path: PathBuf, options: &Options) -> ScriptedLauncher {
        let mut launcher = ScriptedLauncher::new();
        launcher.detached = options.detached;
        launcher.ignore_kill = options.ignore_kill;
        launcher.fail = options.fail.map(str::to_string);
        let launches = Arc::clone(&launcher.launches);
        launcher.before_launch = Some(Box::new(move || {
            let journal = Journal::open(&journal_path).unwrap();
            let last = journal.ops(1).unwrap();
            assert_eq!(
                (last[0].op.as_str(), last[0].outcome.as_str()),
                ("tool", "started"),
                "the started row comes before the launch"
            );
            let command = launches.lock().last().cloned().unwrap();
            let program = command.program.file_name().unwrap().to_string_lossy();
            assert!(last[0].target.starts_with(program.as_ref()), "{last:?}");
        }));
        launcher
    }

    struct Harness {
        dir: tempfile::TempDir,
        journal: Arc<Journal>,
        launcher: Arc<ScriptedLauncher>,
        runner: ToolRunner,
    }

    #[derive(Clone, Copy)]
    struct Options {
        env: Environment,
        detached: bool,
        ignore_kill: bool,
        fail: Option<&'static str>,
    }

    const DEFAULTS: Options = Options {
        env: ENV,
        detached: true,
        ignore_kill: false,
        fail: None,
    };

    fn harness_with(options: Options) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.db");
        let journal = Arc::new(Journal::open(&journal_path).unwrap());
        let launcher = Arc::new(scripted_launcher(journal_path, &options));
        let runner = ToolRunner::new(
            Arc::clone(&launcher) as Arc<dyn Launcher>,
            options.env,
            dir.path().join("tools"),
        )
        .with_tick(Duration::from_millis(5));
        Harness {
            dir,
            journal,
            launcher,
            runner,
        }
    }

    fn harness() -> Harness {
        harness_with(DEFAULTS)
    }

    fn request(tool: ToolId, volume: Option<&str>) -> ToolRequest {
        ToolRequest::new(tool, volume).unwrap()
    }

    impl Harness {
        fn start(&self, tool: ToolId, volume: Option<&str>) -> Result<JobSnapshot> {
            self.runner
                .start(Arc::clone(&self.journal), &request(tool, volume))
        }

        /// Starts `tool` with a fresh script.
        fn run(&self, tool: ToolId, volume: Option<&str>) -> (JobSnapshot, Sender<Step>) {
            let script = self.launcher.script();
            (self.start(tool, volume).unwrap(), script)
        }

        fn finish(&self, id: JobId) -> JobSnapshot {
            let snapshot = self.runner.wait(id, Duration::from_secs(10)).unwrap();
            assert!(snapshot.state.is_finished(), "{snapshot:?}");
            snapshot
        }

        fn tool_ops(&self) -> Vec<OpLogEntry> {
            let mut ops = self.journal.ops(10_000).unwrap();
            ops.retain(|o| o.op == "tool");
            ops.reverse();
            ops
        }

        fn log_dir(&self) -> PathBuf {
            self.dir.path().join("tools")
        }

        fn nothing_written(&self) {
            assert!(self.journal.ops(10).unwrap().is_empty());
            assert!(self.journal.sessions().unwrap().is_empty());
            assert!(!self.log_dir().exists(), "a log folder was created");
            assert!(self.runner.jobs().is_empty());
        }
    }

    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn all_lines(runner: &ToolRunner, id: JobId) -> Vec<String> {
        let mut lines = Vec::new();
        let mut after = 0;
        loop {
            let view = runner.view(id, after, MAX_LINES_PER_VIEW).unwrap();
            lines.extend(view.lines);
            after = view.next;
            if !view.more {
                return lines;
            }
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    // ── pure parts ──

    #[test]
    fn durations_read_like_the_ui() {
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45 s");
        assert_eq!(fmt_duration(Duration::from_secs(12 * 60 + 5)), "12 min 5 s");
        assert_eq!(
            fmt_duration(Duration::from_secs(3600 + 3 * 60 + 59)),
            "1 h 3 min"
        );
        assert_eq!(fmt_duration(Duration::from_millis(900)), "0 s");
    }

    #[test]
    fn snapshot_and_view_keys_match_the_contract() {
        let h = harness();
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        let json = serde_json::to_value(h.runner.view(job.id, 0, 10).unwrap()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        let mut expected = vec![
            "id",
            "tool",
            "title",
            "command_line",
            "volume",
            "state",
            "started_at",
            "finished_at",
            "elapsed_ms",
            "idle_ms",
            "progress",
            "progress_line",
            "exit_code",
            "exit_code_hex",
            "cancellable",
            "cancel_requested",
            "detached",
            "restart_required",
            "hint",
            "summary",
            "log_path",
            "raw_log_path",
            "line_count",
            "logged",
            "lines",
            "first",
            "next",
            "skipped",
            "more",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(json["id"], 1);
        assert_eq!(json["tool"], "disk_check");
        assert_eq!(json["state"], "running");
        script.send(Step::Exit(0)).unwrap();
        h.finish(job.id);
        let plan = serde_json::to_value(h.runner.plan(&request(ToolId::SfcVerify, None)).unwrap())
            .unwrap();
        assert_eq!(plan.as_object().unwrap().len(), 13);
        let outcome = ShutdownOutcome {
            id: JobId(3),
            tool: ToolId::SfcScan,
            action: ShutdownAction::MayEndWithApp,
        };
        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            serde_json::json!({"id": 3, "tool": "sfc_scan", "action": "may_end_with_app"})
        );
    }

    // ── runs ──

    #[test]
    fn start_audits_before_launch_and_streams_lines() {
        let h = harness();
        let (job, script) = h.run(ToolId::DiskCheck, Some("c:"));
        assert_eq!(job.id, JobId(1));
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.volume.as_deref(), Some("C:"));
        assert_eq!(job.command_line, "chkdsk.exe C:");
        assert!(job.cancellable && !job.detached && job.logged);
        let spec = h.launcher.launches.lock()[0].clone();
        assert!(spec.program.is_absolute(), "{spec:?}");
        assert!(spec.program.ends_with("chkdsk.exe"), "{spec:?}");
        assert_eq!(spec.args, ["C:"]);
        assert_eq!(spec.detach, DetachPolicy::None);

        // Output in the OEM code page shows from its first line end on.
        let mut first = String::from(
            "The type of the file system is NTFS.\r\nVolume label is Windows.\r\n\r\n\
             Stage 1: Examining basic file system structure ...\r\n",
        );
        for total in 1..=4 {
            first.push_str(&format!(
                "Progress: {total} of 262144 done; Stage:  {total}%; Total:  {total}%; \
                 ETA:   0:00:05  \r"
            ));
        }
        script.send(Step::Write(first.into_bytes())).unwrap();
        eventually("the first lines", || {
            h.runner.view(job.id, 0, 10).unwrap().lines.len() == 4
        });
        // The redraw on screen shows, although what ends it has not arrived.
        eventually("the newest redraw", || {
            h.runner.snapshot(job.id).unwrap().progress == Some(4.0)
        });
        let running = h.runner.view(job.id, 0, 10).unwrap();
        assert_eq!(running.lines[0], "The type of the file system is NTFS.");
        assert_eq!(running.job.state, JobState::Running);
        assert_eq!(running.job.exit_code, None);
        assert_eq!(
            running.job.progress_line.as_deref(),
            Some("Progress: 4 of 262144 done; Stage:  4%; Total:  4%; ETA:   0:00:05")
        );
        script
            .send(Step::Write(
                b"\r\nWindows has scanned the file system and found no problems.\r\n".to_vec(),
            ))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(done.exit_code_hex.as_deref(), Some("0x00000000"));
        assert_eq!(done.progress, Some(4.0));
        assert_eq!(done.line_count, 6);
        assert!(done.finished_at.is_some());
        assert_eq!(
            all_lines(&h.runner, job.id),
            [
                "The type of the file system is NTFS.",
                "Volume label is Windows.",
                "",
                "Stage 1: Examining basic file system structure ...",
                "Progress: 4 of 262144 done; Stage:  4%; Total:  4%; ETA:   0:00:05",
                "Windows has scanned the file system and found no problems."
            ]
        );

        let ops = h.tool_ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(ops[0].outcome, "started");
        assert_eq!(ops[0].target, "chkdsk.exe C:");
        assert_eq!(ops[0].session_id, None);
        let detail = ops[0].detail.as_deref().unwrap();
        assert!(
            detail.starts_with("log ") && detail.ends_with(".log"),
            "{detail}"
        );
        assert_eq!(ops[1].outcome, "succeeded");
        assert!(
            ops[1]
                .detail
                .as_deref()
                .unwrap()
                .starts_with("exit 0x00000000 (0) · "),
            "{:?}",
            ops[1].detail
        );
        assert!(
            h.journal.sessions().unwrap().is_empty(),
            "tool runs open no session"
        );
    }

    #[test]
    fn transcript_log_is_utf8_with_bom_and_decoded_lines() {
        let h = harness();
        let (job, script) = h.run(ToolId::SfcVerify, None);
        script
            .send(Step::Write(utf16(
                "\r\nBeginning system scan.  This process will take some time.\r\r\n\r\n\
                 Verification 50% complete.\rVerification 100% complete.\r\r\n\r\n\
                 Windows Resource Protection did not find any integrity violations.\r\r\n",
            )))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Completed);
        assert_eq!(done.progress, Some(100.0));

        let text = String::from_utf8(read(Path::new(&done.log_path))).unwrap();
        let body = text
            .strip_prefix('\u{feff}')
            .expect("UTF-8 byte order mark");
        let expected_start = format!(
            "Cairn {}  ·  sfc.exe /verifyonly\r\nStarted ",
            crate::VERSION
        );
        assert!(body.starts_with(&expected_start), "{body}");
        let lines = "\r\n\r\nBeginning system scan.  This process will take some time.\r\n\r\n\
                     Verification 100% complete.\r\n\r\n\
                     Windows Resource Protection did not find any integrity violations.\r\n\r\n\
                     Finished ";
        assert!(body.contains(lines), "{body}");
        assert!(
            body.contains("  ·  Finished; the result is in the output above\r\n"),
            "{body}"
        );
        assert!(
            body.contains("\r\nExit code 0 (0x00000000)  ·  ran for "),
            "{body}"
        );
        assert!(body.contains("System File Checker finished"), "{body}");
        assert!(
            !body.contains("50%"),
            "progress redraws stay out of the transcript"
        );
    }

    #[test]
    fn finish_writes_one_final_row_with_exit_code() {
        let h = harness();
        let cases = [
            (ToolId::SfcVerify, None, 0, JobState::Completed, "completed"),
            (
                ToolId::DismRestore,
                None,
                3010,
                JobState::Succeeded,
                "succeeded",
            ),
            (
                ToolId::DiskCheck,
                Some("C:"),
                3,
                JobState::Attention,
                "problems_found",
            ),
        ];
        for (tool, volume, code, state, outcome) in cases {
            let (job, script) = h.run(tool, volume);
            script.send(Step::Exit(code)).unwrap();
            let done = h.finish(job.id);
            assert_eq!(done.state, state, "{tool}");
            assert_eq!(done.exit_code, Some(code));
            assert_eq!(done.restart_required, code == 3010, "{tool}");
            let ops = h.tool_ops();
            let last = ops.last().unwrap();
            assert_eq!(last.outcome, outcome, "{tool}");
            let hex = exit_code_hex(code);
            assert!(
                last.detail
                    .as_deref()
                    .unwrap()
                    .starts_with(&format!("exit {hex} ({code}) · ")),
                "{last:?}"
            );
        }
        let outcomes: Vec<String> = h.tool_ops().into_iter().map(|o| o.outcome).collect();
        assert_eq!(
            outcomes,
            [
                "started",
                "completed",
                "started",
                "succeeded",
                "started",
                "problems_found"
            ]
        );
        let restore = h.launcher.launches.lock()[1].clone();
        assert_eq!(restore.detach, DetachPolicy::Require);
        assert_eq!(
            restore.args,
            ["/Online", "/Cleanup-Image", "/RestoreHealth", "/NoRestart"]
        );
    }

    static PROBES: AtomicUsize = AtomicUsize::new(0);

    fn repairable() -> Result<StoreHealth> {
        PROBES.fetch_add(1, Ordering::SeqCst);
        Ok(StoreHealth::Repairable)
    }

    fn probe_fails() -> Result<StoreHealth> {
        Err(Error::Other("DismOpenSession failed".to_string()))
    }

    #[test]
    fn dism_check_verdict_comes_from_the_store_health_probe() {
        let h = harness_with(Options {
            env: Environment {
                store_health: repairable,
                ..ENV
            },
            ..DEFAULTS
        });
        let (job, script) = h.run(ToolId::DismCheck, None);
        script
            .send(Step::Write(
                b"No component store corruption detected.\r\n".to_vec(),
            ))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(PROBES.load(Ordering::SeqCst), 1);
        assert_eq!(done.state, JobState::Attention);
        assert!(done.hint.unwrap().contains("Repair the component store"));
        assert_eq!(done.progress_line, None);
        assert_eq!(h.tool_ops().last().unwrap().outcome, "problems_found");

        // A failed run is judged by its exit code; the store is not probed.
        let (job, script) = h.run(ToolId::DismScan, None);
        script.send(Step::Exit(740)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.hint.as_deref(), Some("needs administrator rights"));
        assert_eq!(PROBES.load(Ordering::SeqCst), 1);

        // An unreadable store state leaves the verdict to the output.
        let h = harness_with(Options {
            env: Environment {
                store_health: probe_fails,
                ..ENV
            },
            ..DEFAULTS
        });
        let (job, script) = h.run(ToolId::DismScan, None);
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Completed);
        assert_eq!(
            done.summary.as_deref(),
            Some("DISM finished; its result is in the output above.")
        );
    }

    #[test]
    fn a_dry_run_start_opens_no_journal_and_starts_nothing() {
        let h = harness();
        let opened = std::cell::Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Ok(Arc::clone(&h.journal))
        };
        let (plan, job) = h
            .runner
            .plan_or_start(true, open, &request(ToolId::DiskCheck, Some("C:")))
            .unwrap();
        assert_eq!(plan.command_line, "chkdsk.exe C:");
        assert!(job.is_none());
        assert_eq!(opened.get(), 0, "a dry run opens no journal");
        assert!(h.runner.jobs().is_empty());
        assert!(h.tool_ops().is_empty(), "a dry run writes no audit row");

        let script = h.launcher.script();
        let open = || {
            opened.set(opened.get() + 1);
            Ok(Arc::clone(&h.journal))
        };
        let (_, job) = h
            .runner
            .plan_or_start(false, open, &request(ToolId::DiskCheck, Some("C:")))
            .unwrap();
        assert_eq!(opened.get(), 1);
        let job = job.expect("a started job");
        script.send(Step::Exit(0)).unwrap();
        assert_eq!(h.finish(job.id).state, JobState::Succeeded);

        let failing = || -> Result<Arc<Journal>> { Err(Error::Other("no journal".into())) };
        let refused =
            h.runner
                .plan_or_start(false, failing, &request(ToolId::DiskCheck, Some("C:")));
        assert!(
            refused.is_err(),
            "a journal that cannot be opened starts nothing"
        );
        assert_eq!(h.runner.jobs().len(), 1);
    }

    #[test]
    fn plan_never_launches_or_writes() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ToolRunner::new(Arc::new(PanicLauncher), ENV, dir.path().join("tools"));
        let system = crate::win::paths::system_dir().unwrap();
        for info in crate::tools::catalog() {
            let volume = info.needs_volume.then_some("C:");
            let plan = runner.plan(&request(info.id, volume)).unwrap();
            assert_eq!(plan.blocked_reason, None, "{plan:?}");
            assert_eq!(
                Path::new(&plan.program),
                system.join(info.program),
                "{plan:?}"
            );
            assert_eq!(plan.title, info.title);
            assert_eq!(plan.cancellable, info.cancellable);
            assert_eq!(
                plan.notes.contains(&RUNS_TO_COMPLETION_NOTE.to_string()),
                !info.cancellable,
                "{plan:?}"
            );
        }
        let check = runner.plan(&request(ToolId::DiskCheck, Some("c"))).unwrap();
        assert_eq!(check.command_line, "chkdsk.exe C:");
        assert_eq!(
            check.notes,
            [
                "C: is in use, so the check runs read-only on a live file system and can report \
              problems that are not real."
            ]
        );
        let hdd = runner
            .plan(&request(ToolId::DriveOptimize, Some("D:")))
            .unwrap();
        assert_eq!(hdd.notes[0], HDD_NOTE);
        let unknown = runner
            .plan(&request(ToolId::DriveRetrim, Some("F:")))
            .unwrap();
        assert_eq!(unknown.notes[0], UNKNOWN_MEDIA_NOTE);
        let restore = runner.plan(&request(ToolId::DismRestore, None)).unwrap();
        assert_eq!(restore.notes, [DOWNLOAD_NOTE, RUNS_TO_COMPLETION_NOTE]);
        assert!(restore.requires_detach && restore.changes_system);
        assert!(!dir.path().join("tools").exists());
        assert!(runner.jobs().is_empty());
    }

    #[test]
    fn not_elevated_is_refused_before_anything() {
        let h = harness_with(Options {
            env: Environment {
                elevated: no,
                ..ENV
            },
            ..DEFAULTS
        });
        let plan = h.runner.plan(&request(ToolId::SfcVerify, None)).unwrap();
        assert_eq!(plan.blocked_reason.as_deref(), Some(NEEDS_ADMIN));
        let err = h.start(ToolId::SfcVerify, None).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert!(h.launcher.launches.lock().is_empty());
        h.nothing_written();
    }

    #[test]
    fn blocked_plans_are_refused() {
        let blocked = |env: Environment, tool: ToolId, volume: Option<&str>| {
            let h = harness_with(Options { env, ..DEFAULTS });
            let plan = h.runner.plan(&request(tool, volume)).unwrap();
            let reason = plan.blocked_reason.clone().expect("blocked");
            let err = h.start(tool, volume).unwrap_err();
            assert_eq!(err.to_string(), reason);
            assert!(h.launcher.launches.lock().is_empty());
            h.nothing_written();
            (reason, plan.notes)
        };

        let (reason, _) = blocked(
            Environment {
                processes: dism_running,
                ..ENV
            },
            ToolId::SfcScan,
            None,
        );
        assert_eq!(
            reason,
            "DISM is already running (started outside Cairn, or before it was last \
             closed). Wait for it to finish."
        );
        let (reason, _) = blocked(
            Environment {
                system_dir: missing_system_dir,
                ..ENV
            },
            ToolId::SfcVerify,
            None,
        );
        assert_eq!(reason, "sfc.exe is not on this PC");
        let (reason, notes) = blocked(
            Environment {
                service_start: disabled,
                ..ENV
            },
            ToolId::DismRestore,
            None,
        );
        assert_eq!(
            reason,
            "The Windows Modules Installer service is disabled; System File Checker and DISM \
             need it."
        );
        assert_eq!(
            notes[0],
            "Windows Update is disabled, so DISM probably can't download repair files."
        );
        let (reason, _) = blocked(
            Environment {
                service_start: disabled,
                ..ENV
            },
            ToolId::DriveRetrim,
            Some("C:"),
        );
        assert_eq!(reason, "The Optimize drives service is disabled.");
        let (reason, _) = blocked(ENV, ToolId::DiskCheck, Some("E:"));
        assert_eq!(reason, "E: is not a fixed drive on this PC");
        let (reason, _) = blocked(ENV, ToolId::DriveRetrim, Some("D:"));
        assert_eq!(reason, "Retrim is for SSDs; this drive is a hard disk");
        let (reason, _) = blocked(
            Environment {
                volumes: unreadable_volumes,
                ..ENV
            },
            ToolId::DriveOptimize,
            Some("C:"),
        );
        assert_eq!(reason, "Could not read the drives: drive list unavailable");

        // The first block wins: a missing program comes before missing rights.
        let dir = tempfile::tempdir().unwrap();
        let env = Environment {
            system_dir: missing_system_dir,
            elevated: no,
            ..ENV
        };
        let runner = ToolRunner::new(Arc::new(PanicLauncher), env, dir.path().join("tools"));
        let plan = runner.plan(&request(ToolId::SfcVerify, None)).unwrap();
        assert_eq!(
            plan.blocked_reason.as_deref(),
            Some("sfc.exe is not on this PC")
        );
    }

    #[test]
    fn drive_tools_wait_for_a_speed_test_on_their_volume() {
        let env = Environment {
            storage_busy_on: speed_test_on_d,
            ..ENV
        };
        for tool in [ToolId::DriveOptimize, ToolId::DiskCheck] {
            let h = harness_with(Options { env, ..DEFAULTS });
            let plan = h.runner.plan(&request(tool, Some("d:"))).unwrap();
            assert_eq!(
                plan.blocked_reason.as_deref(),
                Some("Disk speed test is running on D:; wait for it to finish or stop it."),
                "{tool}"
            );
            let err = h.start(tool, Some("d:")).unwrap_err();
            assert_eq!(err.to_string(), plan.blocked_reason.unwrap());
            assert!(h.launcher.launches.lock().is_empty());
            h.nothing_written();
        }
        // A permanent reason comes first; the speed test is listed after it.
        let dir = tempfile::tempdir().unwrap();
        let runner = ToolRunner::new(Arc::new(PanicLauncher), env, dir.path().join("t"));
        let plan = runner
            .plan(&request(ToolId::DriveRetrim, Some("D:")))
            .unwrap();
        assert_eq!(
            plan.blocked_reason.as_deref(),
            Some("Retrim is for SSDs; this drive is a hard disk")
        );
        // Other volumes and tools without a volume are not affected.
        for (tool, volume) in [
            (ToolId::DriveOptimize, Some("C:")),
            (ToolId::DiskCheck, Some("F:")),
            (ToolId::SfcVerify, None),
        ] {
            let plan = runner.plan(&request(tool, volume)).unwrap();
            assert_eq!(plan.blocked_reason, None, "{tool} {volume:?}");
        }
    }

    #[test]
    fn plan_notes_accumulate() {
        let notes = |env: Environment, tool: ToolId, volume: Option<&str>| {
            let dir = tempfile::tempdir().unwrap();
            let runner = ToolRunner::new(Arc::new(PanicLauncher), env, dir.path().join("t"));
            let plan = runner.plan(&request(tool, volume)).unwrap();
            assert_eq!(plan.blocked_reason, None, "{plan:?}");
            plan.notes
        };
        let unreadable = Environment {
            processes: unreadable_processes,
            service_start: unreadable_service,
            servicing_reboot_pending: reboot_pending,
            ..ENV
        };
        assert_eq!(
            notes(unreadable, ToolId::DismCheck, None),
            [
                PROCESS_LIST_NOTE,
                "Could not check the Windows Modules Installer service.",
                REBOOT_PENDING_NOTE,
                RUNS_TO_COMPLETION_NOTE
            ]
        );
        assert_eq!(
            notes(unreadable, ToolId::DiskCheck, Some("D:")),
            [PROCESS_LIST_NOTE],
            "no reboot note for Check Disk"
        );
        let updating = Environment {
            processes: installing_updates,
            ..ENV
        };
        assert_eq!(
            notes(updating, ToolId::SfcVerify, None),
            [SERVICING_NOTE, RUNS_TO_COMPLETION_NOTE]
        );
        assert_eq!(
            notes(updating, ToolId::DiskCheck, Some("D:")),
            Vec::<String>::new()
        );
        let trim_disabled = Environment {
            delete_notify_disabled: trim_off,
            ..ENV
        };
        assert_eq!(
            notes(trim_disabled, ToolId::DriveRetrim, Some("C:")),
            [TRIM_OFF_NOTE, RUNS_TO_COMPLETION_NOTE]
        );
        assert_eq!(
            notes(trim_disabled, ToolId::DiskCheck, Some("D:")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn only_one_job_at_a_time() {
        let h = harness();
        let (job, script) = h.run(ToolId::SfcVerify, None);
        let busy = "Check system files is already running in Cairn";
        let plan = h
            .runner
            .plan(&request(ToolId::DiskCheck, Some("C:")))
            .unwrap();
        assert_eq!(plan.blocked_reason.as_deref(), Some(busy));
        let err = h.start(ToolId::DiskCheck, Some("C:")).unwrap_err();
        assert_eq!(err.to_string(), busy);
        assert_eq!(h.runner.running().unwrap().id, job.id);
        assert_eq!(h.launcher.launches.lock().len(), 1);

        script.send(Step::Exit(0)).unwrap();
        h.finish(job.id);
        assert!(h.runner.running().is_none());
        let (next, script) = h.run(ToolId::DiskCheck, Some("C:"));
        assert_eq!(next.id, JobId(2));
        script.send(Step::Exit(0)).unwrap();
        h.finish(next.id);
        let ids: Vec<JobId> = h.runner.jobs().iter().map(|j| j.id).collect();
        assert_eq!(ids, [JobId(2), JobId(1)], "newest first");
    }

    #[test]
    fn cancel_stops_cancellable_jobs_only() {
        let h = harness();
        let (job, _script) = h.run(ToolId::DiskCheck, Some("C:"));
        assert!(h.runner.cancel(job.id).unwrap());
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Cancelled);
        assert!(done.cancel_requested);
        assert_eq!(done.exit_code, Some(1));
        assert_eq!(h.tool_ops().last().unwrap().outcome, "cancelled");
        assert!(!h.runner.cancel(job.id).unwrap(), "already finished");

        let (sfc, script) = h.run(ToolId::SfcVerify, None);
        let err = h.runner.cancel(sfc.id).unwrap_err();
        assert!(err.to_string().contains("can't be stopped"), "{err}");
        thread::sleep(Duration::from_millis(30));
        let snapshot = h.runner.snapshot(sfc.id).unwrap();
        assert_eq!(snapshot.state, JobState::Running);
        assert!(!snapshot.cancel_requested);
        script.send(Step::Exit(0)).unwrap();
        assert_eq!(h.finish(sfc.id).state, JobState::Completed);

        let err = h.runner.cancel(JobId(999)).unwrap_err();
        assert_eq!(err.to_string(), "no tool job with id 999");
    }

    #[test]
    fn launch_failure_is_audited_and_not_registered() {
        let h = harness_with(Options {
            fail: Some(DETACH_REFUSED),
            ..DEFAULTS
        });
        let err = h.start(ToolId::SfcScan, None).unwrap_err();
        assert_eq!(err.to_string(), DETACH_REFUSED);
        let ops = h.tool_ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(ops[0].outcome, "started");
        assert_eq!(ops[1].outcome, "failed");
        assert_eq!(
            ops[1].detail.as_deref(),
            Some(format!("could not start: {DETACH_REFUSED}").as_str())
        );
        assert!(h.runner.jobs().is_empty());
        assert!(h.runner.running().is_none());
        let plan = h.runner.plan(&request(ToolId::SfcVerify, None)).unwrap();
        assert_eq!(
            plan.blocked_reason, None,
            "the start reservation was released"
        );

        let log = fs::read_dir(h.log_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "log"))
            .unwrap();
        let text = String::from_utf8(read(&log)).unwrap();
        assert!(text.contains("Not started: could not start: Cairn is running inside"));
    }

    #[test]
    fn lost_process_finishes_failed() {
        let h = harness();
        let (job, script) = h.run(ToolId::SfcVerify, None);
        script
            .send(Step::Lose("the handle is invalid".to_string()))
            .unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(
            done.hint.as_deref(),
            Some("Cairn lost track of the process: the handle is invalid")
        );
        assert_eq!(done.exit_code, None);
        let last = h.tool_ops().pop().unwrap();
        assert_eq!(last.outcome, "failed");
        assert!(last
            .detail
            .unwrap()
            .starts_with("Cairn lost track of the process: the handle is invalid · "));
    }

    #[test]
    fn view_pages_and_reports_skipped() {
        let h = harness();
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        let text: String = (1..=2600).map(|i| format!("line {i}\r\n")).collect();
        script.send(Step::Write(text.into_bytes())).unwrap();
        eventually("every line", || {
            h.runner.snapshot(job.id).unwrap().line_count == 2600
        });

        let view = h.runner.view(job.id, 0, 500).unwrap();
        assert_eq!(
            (view.first, view.next, view.skipped, view.more),
            (Some(601), 1100, 600, true)
        );
        assert_eq!(view.lines.len(), 500);
        assert_eq!(view.lines[0], "line 601");
        assert_eq!(view.job.line_count, 2600);
        let view = h.runner.view(job.id, 1100, 10_000).unwrap();
        assert_eq!(view.lines.len(), MAX_LINES_PER_VIEW);
        assert_eq!((view.first, view.next, view.skipped), (Some(1101), 1600, 0));
        let view = h.runner.view(job.id, 2500, 500).unwrap();
        assert_eq!(view.lines.len(), 100);
        assert_eq!(view.lines.last().unwrap(), "line 2600");
        assert_eq!((view.next, view.more), (2600, false));
        let view = h.runner.view(job.id, 2600, 500).unwrap();
        assert!(view.lines.is_empty());
        assert_eq!((view.first, view.next, view.more), (None, 2600, false));
        assert!(h.runner.view(JobId(77), 0, 500).is_none());
        script.send(Step::Exit(0)).unwrap();
        h.finish(job.id);
        // The transcript keeps every line.
        let transcript = String::from_utf8(read(Path::new(&job.log_path))).unwrap();
        assert!(transcript.contains("\r\nline 1\r\n") && transcript.contains("\r\nline 2600\r\n"));
    }

    #[test]
    fn progress_follows_the_transient_rule() {
        let h = harness();
        let (job, script) = h.run(ToolId::DriveOptimize, Some("C:"));
        let header = "Microsoft Drive Optimizer\r\nCopyright (c) Microsoft Corp.\r\n\r\n\
                      Invoking retrim on Windows (C:)...\r\n\r\n\
                      Pre-Optimization Report:\r\n\r\n\
                      \tVolume Information:\r\n\
                      \t\tVolume size                 = 952.93 GB\r\n\
                      \t\tFree space                  = 611.20 GB\r\n\
                      \t\tTotal fragmented space      = 5%\r";
        script
            .send(Step::Write(header.as_bytes().to_vec()))
            .unwrap();
        eventually("the report lines", || {
            h.runner.snapshot(job.id).unwrap().line_count == 10
        });
        // A report line is not progress, even before its line end arrives.
        thread::sleep(Duration::from_millis(30));
        assert_eq!(h.runner.snapshot(job.id).unwrap().progress, None);
        script
            .send(Step::Write(
                b"\nRetrim:  12% complete...\rRetrim:  17.9% complete...\r".to_vec(),
            ))
            .unwrap();
        // The redraw on screen is the progress, before the one after it begins.
        eventually("the redraw on screen", || {
            h.runner.snapshot(job.id).unwrap().progress == Some(17.9)
        });
        let snapshot = h.runner.snapshot(job.id).unwrap();
        assert_eq!(
            snapshot.progress_line.as_deref(),
            Some("Retrim:  17.9% complete...")
        );
        assert_eq!(snapshot.line_count, 11, "progress is not a line");
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap()["progress"],
            serde_json::json!(17.9),
            "no f32 widening noise"
        );
        script
            .send(Step::Write(b"Retrim:  40% complete...\r".to_vec()))
            .unwrap();
        eventually("the next redraw", || {
            h.runner.snapshot(job.id).unwrap().progress == Some(40.0)
        });
        assert_eq!(
            h.runner.snapshot(job.id).unwrap().progress_line.as_deref(),
            Some("Retrim:  40% complete...")
        );
        script
            .send(Step::Write(b"\r\nFragmented space = 7%\r\n".to_vec()))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.progress, Some(40.0));
        assert_eq!(
            done.progress_line.as_deref(),
            Some("Retrim:  40% complete...")
        );
        let lines = all_lines(&h.runner, job.id);
        assert_eq!(lines[10], "\t\tTotal fragmented space      = 5%");
        assert_eq!(
            lines[11..],
            ["Retrim:  40% complete...", "Fragmented space = 7%"]
        );
    }

    #[test]
    fn shutdown_stops_cancellable_and_records_left_running() {
        // A job that can be stopped is stopped.
        let h = harness();
        let (job, _script) = h.run(ToolId::DiskCheck, Some("C:"));
        assert_eq!(
            h.runner.shutdown(),
            [ShutdownOutcome {
                id: job.id,
                tool: ToolId::DiskCheck,
                action: ShutdownAction::Stopped
            }]
        );
        assert_eq!(
            h.runner.snapshot(job.id).unwrap().state,
            JobState::Cancelled
        );
        assert_eq!(h.tool_ops().last().unwrap().outcome, "cancelled");

        // A detached job keeps running; its row says where its output goes.
        let h = harness();
        let (job, script) = h.run(ToolId::SfcVerify, None);
        let outcomes = h.runner.shutdown();
        assert_eq!(outcomes[0].action, ShutdownAction::LeftRunning);
        let ops = h.tool_ops();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[1].outcome, "left_running");
        assert_eq!(
            ops[1].detail.as_deref(),
            Some(
                format!(
                    "Cairn closed while it ran; its raw output continues in {}",
                    job.raw_log_path
                )
                .as_str()
            )
        );
        assert_eq!(h.runner.snapshot(job.id).unwrap().state, JobState::Running);
        // When it ends before Cairn does, no second final row is written and the
        // transcript gets no footer.
        script
            .send(Step::Write(utf16("Verification 100% complete.\r\r\n")))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Completed);
        assert!(done.logged);
        assert_eq!(h.tool_ops().len(), 2);
        let transcript = String::from_utf8(read(Path::new(&done.log_path))).unwrap();
        assert!(
            transcript.ends_with("\r\n\r\nVerification 100% complete.\r\n"),
            "{transcript:?}"
        );
        assert!(!transcript.contains("Exit code"), "{transcript:?}");

        // A job inside Cairn's job object may end with it.
        let h = harness_with(Options {
            detached: false,
            ..DEFAULTS
        });
        let (job, _script) = h.run(ToolId::DriveOptimize, Some("C:"));
        assert!(!job.detached);
        assert_eq!(h.runner.shutdown()[0].action, ShutdownAction::MayEndWithApp);
        assert_eq!(
            h.tool_ops()[1].detail.as_deref(),
            Some("Cairn closed while it ran; Windows may end it with Cairn")
        );

        // A job that does not stop in time is recorded as left running.
        let mut h = harness_with(Options {
            ignore_kill: true,
            ..DEFAULTS
        });
        h.runner = h.runner.with_stop_wait(Duration::from_millis(50));
        let (job, _script) = h.run(ToolId::DiskCheck, Some("C:"));
        assert_eq!(h.runner.shutdown()[0].action, ShutdownAction::StopTimedOut);
        let last = h.tool_ops().pop().unwrap();
        assert_eq!(last.outcome, "left_running");
        assert!(last.detail.unwrap().ends_with(&job.raw_log_path));
    }

    #[test]
    fn start_after_shutdown_is_refused() {
        let h = harness();
        assert!(h.runner.shutdown().is_empty());
        let _script = h.launcher.script();
        let err = h.start(ToolId::SfcVerify, None).unwrap_err();
        assert_eq!(err.to_string(), CLOSING);
        assert!(h.launcher.launches.lock().is_empty());
        h.nothing_written();
    }

    #[test]
    fn view_and_jobs_do_not_wait_for_a_blocked_launch() {
        let h = harness();
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        *h.launcher.gate.lock() = Some(Gate {
            entered: entered_tx,
            release: release_rx,
        });
        let script = h.launcher.script();
        thread::scope(|scope| {
            let starting = scope.spawn(|| h.start(ToolId::DiskCheck, Some("C:")));
            entered.recv_timeout(Duration::from_secs(10)).unwrap();

            let started = Instant::now();
            assert!(h.runner.view(JobId(1), 0, 500).is_none());
            assert!(h.runner.jobs().is_empty());
            assert!(h.runner.running().is_none());
            assert!(started.elapsed() < Duration::from_millis(50));
            let plan = h.runner.plan(&request(ToolId::SfcVerify, None)).unwrap();
            assert_eq!(
                plan.blocked_reason.as_deref(),
                Some("Check disk (read-only) is already running in Cairn")
            );
            let err = h.start(ToolId::SfcVerify, None).unwrap_err();
            assert!(err.to_string().contains("already running"), "{err}");

            release.send(()).unwrap();
            let job = starting.join().unwrap().unwrap();
            script.send(Step::Exit(0)).unwrap();
            h.finish(job.id);
        });
    }

    #[test]
    fn final_row_is_written_once_when_shutdown_races_finish() {
        // Closing right after the process ended waits for its result instead of recording
        // it as left running, whether or not the watcher has seen the exit yet.
        for round in 0..20 {
            let h = harness();
            let (job, script) = h.run(ToolId::SfcVerify, None);
            script.send(Step::Exit(0)).unwrap();
            if round % 2 == 1 {
                thread::sleep(Duration::from_millis(5));
            }
            let outcomes = h.runner.shutdown();
            assert!(outcomes.is_empty(), "round {round}: {outcomes:?}");
            let done = h.finish(job.id);
            assert_eq!(done.state, JobState::Completed);
            let ops = h.tool_ops();
            assert_eq!(ops.len(), 2, "round {round}: {ops:?}");
            assert_eq!(ops[1].outcome, "completed", "round {round}");
            let transcript = String::from_utf8(read(Path::new(&done.log_path))).unwrap();
            assert!(transcript.contains("\r\nExit code 0 "), "round {round}");
        }
    }

    static SLOW_PROBES: AtomicUsize = AtomicUsize::new(0);

    fn slow_healthy() -> Result<StoreHealth> {
        SLOW_PROBES.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(400));
        Ok(StoreHealth::Healthy)
    }

    #[test]
    fn shutdown_while_a_finished_check_is_judged_records_its_exit() {
        let env = Environment {
            store_health: slow_healthy,
            ..ENV
        };
        // Closing waits for the verdict of a check whose process has ended.
        let h = harness_with(Options { env, ..DEFAULTS });
        let (job, script) = h.run(ToolId::DismCheck, None);
        script.send(Step::Exit(0)).unwrap();
        eventually("the store probe", || {
            SLOW_PROBES.load(Ordering::SeqCst) == 1
        });
        let judged = h.runner.snapshot(job.id).unwrap();
        assert_eq!(judged.state, JobState::Running);
        assert_eq!(
            judged.exit_code,
            Some(0),
            "the exit is known while it is judged"
        );
        assert_eq!(judged.progress_line.as_deref(), Some(READING_STORE));
        assert!(h.runner.shutdown().is_empty(), "it finished on its own");
        let done = h.runner.snapshot(job.id).unwrap();
        assert_eq!(done.state, JobState::Succeeded);
        let outcomes: Vec<String> = h.tool_ops().into_iter().map(|o| o.outcome).collect();
        assert_eq!(outcomes, ["started", "succeeded"]);

        // When the verdict takes longer than closing waits, the exit code is recorded.
        let mut h = harness_with(Options { env, ..DEFAULTS });
        h.runner = h.runner.with_finish_wait(Duration::from_millis(20));
        let (job, script) = h.run(ToolId::DismCheck, None);
        script.send(Step::Exit(0)).unwrap();
        eventually("the store probe", || {
            SLOW_PROBES.load(Ordering::SeqCst) == 2
        });
        assert_eq!(
            h.runner.shutdown(),
            [ShutdownOutcome {
                id: job.id,
                tool: ToolId::DismCheck,
                action: ShutdownAction::Stopped
            }]
        );
        let ops = h.tool_ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(ops[1].outcome, "completed");
        let detail = ops[1].detail.as_deref().unwrap();
        assert!(detail.starts_with("exit 0x00000000 (0) · "), "{detail}");
        assert!(
            detail.ends_with(&format!(" · {STORE_NOT_READ}")),
            "{detail}"
        );
        assert!(h.runner.snapshot(job.id).unwrap().logged);
        // The watcher still finishes the job, without a second row.
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(h.tool_ops().len(), 2);
    }

    #[test]
    fn a_check_that_ends_while_closing_says_its_store_state_was_not_read() {
        let h = harness();
        let (job, script) = h.run(ToolId::DismCheck, None);
        // Cairn begins closing before the watcher sees the exit.
        h.runner.shared.closed.store(true, Ordering::SeqCst);
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Completed);
        let ops = h.tool_ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        let detail = ops[1].detail.as_deref().unwrap();
        assert!(detail.starts_with("exit 0x00000000 (0) · "), "{detail}");
        assert!(
            detail.ends_with(&format!(" · {STORE_NOT_READ}")),
            "{detail}"
        );
        let summary = done.summary.unwrap_or_default();
        assert!(
            summary.ends_with(&format!("{STORE_NOT_READ}.")),
            "{summary}"
        );
    }

    #[test]
    fn a_stop_after_the_check_ended_on_its_own_changes_nothing() {
        let mut h = harness();
        h.runner = h.runner.with_tick(Duration::from_millis(300));
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        // The watcher looks at the process right after the start, then once a tick; the
        // exit and the stop both arrive in between.
        thread::sleep(Duration::from_millis(50));
        script.send(Step::Exit(0)).unwrap();
        assert!(h.runner.cancel(job.id).unwrap());
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(
            done.summary.as_deref(),
            Some("Check Disk found no problems.")
        );
        assert!(done.cancel_requested);
        assert_eq!(h.tool_ops().last().unwrap().outcome, "succeeded");

        // Closing right then does not stop it either.
        let mut h = harness();
        h.runner = h.runner.with_tick(Duration::from_millis(300));
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        thread::sleep(Duration::from_millis(50));
        script.send(Step::Exit(0)).unwrap();
        assert!(h.runner.shutdown().is_empty());
        let done = h.runner.snapshot(job.id).unwrap();
        assert_eq!(done.state, JobState::Succeeded);
        assert!(!done.cancel_requested);
        assert_eq!(h.tool_ops().last().unwrap().outcome, "succeeded");
    }

    fn healthy() -> Result<StoreHealth> {
        Ok(StoreHealth::Healthy)
    }

    #[test]
    fn progress_is_the_bar_on_screen() {
        let h = harness_with(Options {
            env: Environment {
                store_health: healthy,
                ..ENV
            },
            ..DEFAULTS
        });
        let (job, script) = h.run(ToolId::DismScan, None);
        let snapshot = || h.runner.snapshot(job.id).unwrap();
        // DISM's header is far shorter than 256 bytes and shows at once.
        script
            .send(Step::Write(
                b"\r\nDeployment Image Servicing and Management tool\r\nVersion: 10.0.26100.1\r\n\r\n\
                  Image Version: 10.0.26200.6584\r\n\r\n"
                    .to_vec(),
            ))
            .unwrap();
        eventually("the header", || snapshot().line_count == 5);
        // Each bar is drawn after a carriage return and has no end until the next one.
        let bars = [
            ("\r[                 0.0%                 ] ", 0.0),
            ("\r[===              9.9%                 ] ", 9.9),
            ("\r[======          20.3%                 ] ", 20.3),
            ("\r[============    62.3%                 ] ", 62.3),
        ];
        for (bar, value) in bars {
            script.send(Step::Write(bar.as_bytes().to_vec())).unwrap();
            eventually(bar, || snapshot().progress == Some(value));
            assert_eq!(snapshot().progress_line.as_deref(), Some(bar.trim()));
        }
        // Half a bar is not shown; the last whole one stays.
        script
            .send(Step::Write(b"\r[=============   6".to_vec()))
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let partial = snapshot();
        assert_eq!(partial.progress, Some(62.3));
        assert_eq!(partial.progress_line.as_deref(), Some(bars[3].0.trim()));
        script
            .send(Step::Write(b"7.1%                 ] ".to_vec()))
            .unwrap();
        eventually("the rest of the bar", || snapshot().progress == Some(67.1));
        assert_eq!(snapshot().line_count, 5, "progress is not a line");
        script
            .send(Step::Write(
                b"\r[=====================100.0%=====================] \r\n\
                  No component store corruption detected.\r\n"
                    .to_vec(),
            ))
            .unwrap();
        script.send(Step::Exit(0)).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.progress, Some(100.0));
        assert_eq!(
            all_lines(&h.runner, job.id)[5..],
            [
                "[=====================100.0%=====================]",
                "No component store corruption detected."
            ]
        );
    }

    #[test]
    fn finished_jobs_release_their_resources() {
        let h = harness();
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        assert!(Arc::strong_count(&h.journal) > 1);
        script.send(Step::Exit(0)).unwrap();
        h.finish(job.id);
        assert_eq!(
            Arc::strong_count(&h.journal),
            1,
            "the job released the journal"
        );
        assert_eq!(
            h.launcher.dropped.load(Ordering::SeqCst),
            1,
            "process dropped"
        );

        // Two tools, so that at most nine runs of one tool start in the same second.
        for round in 0..11 {
            let (tool, volume) = if round % 2 == 0 {
                (ToolId::SfcVerify, None)
            } else {
                (ToolId::DiskCheck, Some("C:"))
            };
            let (job, script) = h.run(tool, volume);
            script.send(Step::Exit(0)).unwrap();
            h.finish(job.id);
        }
        let ids: Vec<u64> = h.runner.jobs().iter().map(|j| j.id.0).collect();
        assert_eq!(ids, (3..=12).rev().collect::<Vec<u64>>());
        assert_eq!(h.launcher.dropped.load(Ordering::SeqCst), 12);
        assert_eq!(Arc::strong_count(&h.journal), 1);
    }

    #[test]
    fn utf16_bom_is_written_for_sfc_only() {
        let h = harness();
        let (sfc, script) = h.run(ToolId::SfcVerify, None);
        script.send(Step::Write(utf16("ok\r\r\n"))).unwrap();
        script.send(Step::Exit(0)).unwrap();
        h.finish(sfc.id);
        let raw = read(Path::new(&sfc.raw_log_path));
        assert_eq!(raw, [&[0xFF, 0xFE][..], &utf16("ok\r\r\n")].concat());
        assert_eq!(all_lines(&h.runner, sfc.id), ["ok"]);

        let (chkdsk, script) = h.run(ToolId::DiskCheck, Some("C:"));
        script.send(Step::Write(b"ok\r\n".to_vec())).unwrap();
        script.send(Step::Exit(0)).unwrap();
        h.finish(chkdsk.id);
        assert_eq!(read(Path::new(&chkdsk.raw_log_path)), b"ok\r\n");
        assert_eq!(all_lines(&h.runner, chkdsk.id), ["ok"]);
    }

    fn exploding_decoder(_: &[u8]) {
        panic!("decoder exploded");
    }

    #[test]
    fn a_panicking_decoder_finishes_failed() {
        let mut h = harness();
        h.runner = h.runner.with_decode_hook(exploding_decoder);
        let (job, script) = h.run(ToolId::DiskCheck, Some("C:"));
        script.send(Step::Write(b"x\r\n".to_vec())).unwrap();
        let done = h.finish(job.id);
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(
            done.hint.as_deref(),
            Some("internal error: decoder exploded")
        );
        assert!(done.logged);
        let last = h.tool_ops().pop().unwrap();
        assert_eq!(last.outcome, "failed");
        assert!(last
            .detail
            .unwrap()
            .starts_with("internal error: decoder exploded · "));
        assert_eq!(Arc::strong_count(&h.journal), 1);
    }
}
