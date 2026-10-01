//! Engine-owned background jobs polled by the UI.
//!
//! A [`JobHost`] runs one job at a time on a thread of its own and keeps its state in memory
//! for polling: a snapshot, its newest output lines, a small live `detail` value and a
//! published result (JSON, and optionally a typed value for the engine). One host serves one
//! lane of work (`winget`, `storage`); job ids are unique across every host in the process.
//!
//! - A job may write a transcript (`<log dir>\<stem>.log`) and audit rows: with an
//!   [`AuditSpec`] the host writes the "started" row before the work starts and exactly one
//!   final row, or a `stop_timed_out` / `left_running` row when Cairn closes first.
//! - Locks guard in-memory state only; no lock is held across file, SQLite, process or
//!   thread-spawn I/O. [`JobHost::view`], [`JobHost::snapshot`], [`JobHost::jobs`],
//!   [`JobHost::running`], [`JobHost::cancel`], [`JobHost::result`] and [`JobHost::typed`]
//!   are microsecond reads, safe on the UI thread.
//! - The work runs on its own thread; work that starts processes launches them itself
//!   through [`crate::tools::launch`].

pub(crate) mod lines;
#[cfg(test)]
pub(crate) mod testing;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use self::lines::Lines;
use crate::safety::state_log::Journal;
use crate::tools::logs;
use crate::tools::runner::{fmt_duration, MAX_LINES_PER_VIEW};
use crate::{Error, Result};

pub use crate::tools::runner::JobState;

/// Longest command line kept for a job; longer ones are cut with "…".
const MAX_COMMAND_LINE: usize = 200;
/// Shown as the state of a job whose work returned without a final state.
const NO_FINAL_STATE: &str = "internal error: the job ended without a result";
/// Detail of the row a job gets when Cairn closes before it ends and it set no note.
const DEFAULT_SHUTDOWN_NOTE: &str = "Cairn closed while it ran";
/// How often [`JobHost::wait`] and the shutdown waits look at a job.
const POLL: Duration = Duration::from_millis(5);

/// Ids of every host's jobs; the first job is 1.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Identifies a job of any host for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostJobId(pub u64);

impl fmt::Display for HostJobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a host runs, logs and keeps its jobs.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// `[a-z_]+`, for example "winget" or "storage".
    pub lane: &'static str,
    /// Transcripts of jobs that log, normally `<data dir>\jobs\<lane>`.
    pub log_dir: PathBuf,
    /// Transcript stems kept in `log_dir`.
    pub keep_logs: usize,
    /// How often work is expected to look at [`JobContext::cancel_requested`].
    pub tick: Duration,
    /// How long [`JobHost::shutdown`] waits for a start in progress.
    pub settle_wait: Duration,
    /// How long [`JobHost::shutdown`] waits for the running work to end.
    pub stop_wait: Duration,
    /// Finished jobs kept in the registry besides the newest finished job of each kind
    /// (always kept).
    pub keep_finished: usize,
    /// Published and typed results are kept only for the newest N finished jobs of each
    /// kind; older finished jobs keep their snapshot and lose their result. `None`: results
    /// live as long as the job.
    pub keep_results_per_kind: Option<usize>,
}

/// Audit rows of a job: "started" before the work, one final row after it.
#[derive(Debug, Clone)]
pub struct AuditSpec {
    pub op: &'static str,
    pub target: String,
    pub started_detail: String,
}

/// What to start.
#[derive(Debug, Clone)]
pub struct JobSpec {
    /// `[a-z_]+`, unique across lanes; used in the transcript's stem.
    pub kind: &'static str,
    pub title: String,
    /// Shown, and used as the transcript's header; at most 200 characters are kept.
    pub command_line: String,
    pub cancellable: bool,
    /// `Some`: a "started" row before the thread starts, exactly one final row, and a
    /// `stop_timed_out` or `left_running` row on close. `None`: the host writes no rows (the
    /// work may, through [`JobContext::audit`]).
    pub audit: Option<AuditSpec>,
    /// Open the journal even without `audit` (work that writes its own rows).
    pub needs_journal: bool,
    /// `false`: no files; lines are kept in memory only.
    pub log: bool,
}

/// How a job's work ended.
#[derive(Debug)]
pub struct WorkEnd {
    /// A final state; `Running` is taken as `Failed`.
    pub state: JobState,
    pub summary: String,
    pub hint: Option<String>,
    pub restart_required: bool,
    /// Detail of the final audit row; the summary when `None`.
    pub audit_detail: Option<String>,
}

/// The work of a job, run once on the job's thread.
pub type Work = Box<dyn FnOnce(&JobContext) -> WorkEnd + Send + 'static>;

/// The state of a job at one moment (exactly these 25 keys in JSON).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostJobSnapshot {
    pub id: HostJobId,
    pub lane: &'static str,
    pub kind: &'static str,
    pub title: String,
    pub command_line: String,
    pub state: JobState,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub finished_at: Option<String>,
    pub elapsed_ms: u64,
    /// Time since the last line, progress or detail (until the job finished).
    pub idle_ms: u64,
    /// Percent, rounded to one decimal.
    pub progress: Option<f64>,
    pub progress_line: Option<String>,
    pub cancellable: bool,
    pub cancel_requested: bool,
    pub detached: bool,
    pub restart_required: bool,
    pub summary: Option<String>,
    pub hint: Option<String>,
    pub notes: Vec<String>,
    /// Small kind-specific live status.
    pub detail: Option<serde_json::Value>,
    /// The transcript; `None` for a job that does not log.
    pub log_path: Option<String>,
    /// Output lines so far.
    pub line_count: u64,
    /// The job's audit rows were written; false for a job without audit rows and when a
    /// row could not be written.
    pub logged: bool,
    /// A published or typed result is held.
    pub has_result: bool,
    /// Revision of the published result; 0 before the first.
    pub result_revision: u64,
}

/// A snapshot and a page of output lines (exactly these 30 keys in JSON).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostJobView {
    #[serde(flatten)]
    pub job: HostJobSnapshot,
    /// Output lines after the requested one, oldest first.
    pub lines: Vec<String>,
    /// Number of the first line in `lines` (lines are numbered from 1); `None` when empty.
    pub first: Option<u64>,
    /// Number of the last line delivered; pass it as `after` to continue.
    pub next: u64,
    /// Lines after the requested one that are no longer kept in memory.
    pub skipped: u64,
    /// More lines are waiting after this page.
    pub more: bool,
}

/// What closing did with the job that was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostShutdownAction {
    /// It ended within the wait.
    Stopped,
    /// It was asked to stop and was still running when the wait ended.
    StopTimedOut,
    /// It cannot be stopped and was left to finish.
    LeftRunning,
}

impl HostShutdownAction {
    /// Outcome of the row a job still running at shutdown gets.
    fn audit_outcome(self) -> &'static str {
        match self {
            HostShutdownAction::Stopped => "stopped",
            HostShutdownAction::StopTimedOut => "stop_timed_out",
            HostShutdownAction::LeftRunning => "left_running",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostShutdownOutcome {
    pub id: HostJobId,
    pub kind: &'static str,
    pub action: HostShutdownAction,
}

// ───────────────────────────── Jobs ─────────────────────────────

/// The parts of a job that change while it runs.
#[derive(Debug)]
struct Status {
    state: JobState,
    finished_at: Option<String>,
    finished: Option<Instant>,
    last_activity: Instant,
    progress: Option<f64>,
    progress_line: Option<String>,
    restart_required: bool,
    summary: Option<String>,
    hint: Option<String>,
    notes: Vec<String>,
    detail: Option<serde_json::Value>,
    detached: bool,
    logged: bool,
    shutdown_note: Option<String>,
}

/// A job's published results.
#[derive(Default)]
struct Results {
    value: Option<Arc<serde_json::Value>>,
    typed: Option<Arc<dyn Any + Send + Sync>>,
    revision: u64,
}

impl Results {
    fn held(&self) -> bool {
        self.value.is_some() || self.typed.is_some()
    }
}

struct Job {
    id: HostJobId,
    lane: &'static str,
    spec: JobSpec,
    started: Instant,
    started_at: String,
    stem: Option<String>,
    /// The transcript. Every handle to it appends, so the work's lines, its footer and a
    /// footer written while Cairn closes all land at its end.
    log_path: Option<PathBuf>,
    /// Released when the job finishes; cloned out before any write.
    journal: Mutex<Option<Arc<Journal>>>,
    /// When both are held, `status` is locked first.
    status: Mutex<Status>,
    lines: Mutex<Lines>,
    results: Mutex<Results>,
    cancel: Arc<AtomicBool>,
    closing: AtomicBool,
    /// Claimed by whoever writes the final audit row and footer, so exactly one is written.
    claimed: AtomicBool,
    /// The state is final.
    done: AtomicBool,
    /// The state is final and the host pruned its jobs afterwards.
    settled: AtomicBool,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("lane", &self.lane)
            .field("kind", &self.spec.kind)
            .field("done", &self.done.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Job {
    fn is_running(&self) -> bool {
        !self.done.load(Ordering::SeqCst)
    }

    fn claim(&self) -> bool {
        self.claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn snapshot(&self) -> HostJobSnapshot {
        let status = self.status.lock();
        // Counted after the state is read: a job's last lines are pushed before its state
        // becomes final, so a finished snapshot counts every line.
        let line_count = self.lines.lock().total;
        let (has_result, result_revision) = {
            let results = self.results.lock();
            (results.held(), results.revision)
        };
        let end = status.finished.unwrap_or_else(Instant::now);
        HostJobSnapshot {
            id: self.id,
            lane: self.lane,
            kind: self.spec.kind,
            title: self.spec.title.clone(),
            command_line: self.spec.command_line.clone(),
            state: status.state,
            started_at: self.started_at.clone(),
            finished_at: status.finished_at.clone(),
            elapsed_ms: millis(end.saturating_duration_since(self.started)),
            idle_ms: millis(end.saturating_duration_since(status.last_activity)),
            progress: status.progress.map(|p| (p * 10.0).round() / 10.0),
            progress_line: status.progress_line.clone(),
            cancellable: self.spec.cancellable,
            cancel_requested: self.cancel.load(Ordering::SeqCst),
            detached: status.detached,
            restart_required: status.restart_required,
            summary: status.summary.clone(),
            hint: status.hint.clone(),
            notes: status.notes.clone(),
            detail: status.detail.clone(),
            log_path: self.log_path.as_ref().map(|p| p.display().to_string()),
            line_count,
            logged: status.logged,
            has_result,
            result_revision,
        }
    }

    /// Writes an audit row of this job's op and target to `journal`; false (and an error
    /// trace) when it fails or there is no journal. Callers clone the journal out of the job
    /// before they claim the row, so a finish that loses the claim cannot release it first.
    fn audit_row(&self, journal: Option<&Journal>, outcome: &str, detail: &str) -> bool {
        let (Some(journal), Some(audit)) = (journal, &self.spec.audit) else {
            return false;
        };
        match journal.log_op(None, audit.op, &audit.target, outcome, Some(detail)) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    job = self.id.0,
                    kind = self.spec.kind,
                    outcome,
                    error = %e,
                    "cannot write the job's audit row"
                );
                false
            }
        }
    }

    /// Appends the closing block of the transcript through a handle of its own.
    fn footer(&self, text: &str) {
        let Some(path) = &self.log_path else {
            return;
        };
        let mut block = String::from("\r\n");
        for line in text.lines() {
            block.push_str(line);
            block.push_str("\r\n");
        }
        let written = OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut log| log.write_all(block.as_bytes()));
        if let Err(e) = written {
            tracing::warn!(job = self.id.0, error = %e, "cannot finish the job's transcript");
        }
    }

    /// Records how the work ended: the final audit row and footer when this call wins the
    /// claim, then the final state.
    fn finish(&self, end: WorkEnd) {
        let state = if end.state == JobState::Running {
            JobState::Failed
        } else {
            end.state
        };
        let summary = if end.state == JobState::Running && end.summary.is_empty() {
            NO_FINAL_STATE.to_string()
        } else {
            end.summary
        };
        let elapsed = self.started.elapsed();
        let text = format!(
            "{} · {}",
            end.audit_detail.as_deref().unwrap_or(&summary),
            fmt_duration(elapsed)
        );
        let mut logged = None;
        let journal = self.journal.lock().clone();
        if self.claim() {
            if self.spec.audit.is_some() {
                logged = Some(self.audit_row(journal.as_deref(), state.audit_outcome(), &text));
            }
            self.footer(&text);
        }
        drop(journal);
        let mut status = self.status.lock();
        status.state = state;
        status.summary = (!summary.is_empty()).then_some(summary);
        status.hint = end.hint;
        status.restart_required = end.restart_required;
        status.finished_at = Some(utc_now());
        status.finished = Some(Instant::now());
        if let Some(logged) = logged {
            status.logged = status.logged && logged;
        }
        drop(status);
        let journal = self.journal.lock().take();
        self.done.store(true, Ordering::SeqCst);
        drop(journal);
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// `[a-z_]+`.
fn is_kind(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// `text` cut to [`MAX_COMMAND_LINE`] characters, the last one "…" when cut.
fn bounded_command_line(text: &str) -> String {
    if text.chars().count() <= MAX_COMMAND_LINE {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_COMMAND_LINE - 1).collect();
    cut.push('…');
    cut
}

/// Text of a panic payload.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "the job panicked".to_string()
    }
}

fn wait_done(job: &Job, timeout: Duration) -> bool {
    wait_for(|| !job.is_running(), timeout)
}

/// Polls `ready` every [`POLL`] until it is true (true) or `timeout` passed (false).
fn wait_for(ready: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

// ───────────────────────────── Context ─────────────────────────────

/// Handed to the work; every method is cheap and never waits for the UI.
pub struct JobContext {
    job: Arc<Job>,
    journal: Option<Arc<Journal>>,
    /// The transcript, opened for appending; written without a lock.
    log: Option<File>,
    dir: PathBuf,
    tick: Duration,
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext")
            .field("job", &self.job)
            .field("dir", &self.dir)
            .field("logs", &self.log.is_some())
            .finish_non_exhaustive()
    }
}

impl JobContext {
    pub fn id(&self) -> HostJobId {
        self.job.id
    }

    /// Appends a line to the transcript (when the job logs) and to the lines kept in memory
    /// (the newest 2000).
    pub fn line(&self, text: &str) {
        if let Some(mut log) = self.log.as_ref() {
            if let Err(e) = log.write_all(format!("{text}\r\n").as_bytes()) {
                tracing::warn!(job = self.job.id.0, error = %e, "cannot write the job's transcript");
            }
        }
        self.job.status.lock().last_activity = Instant::now();
        self.job.lines.lock().push_all(vec![text.to_string()]);
    }

    /// Replaces the progress percent and the progress text.
    pub fn progress(&self, percent: Option<f64>, text: Option<&str>) {
        let mut status = self.job.status.lock();
        status.progress = percent
            .filter(|p| p.is_finite())
            .map(|p| p.clamp(0.0, 100.0));
        status.progress_line = text.map(str::to_string);
        status.last_activity = Instant::now();
    }

    /// Replaces the small kind-specific live status.
    pub fn set_detail(&self, detail: serde_json::Value) {
        let mut status = self.job.status.lock();
        status.detail = Some(detail);
        status.last_activity = Instant::now();
    }

    /// Replaces the published result; its revision grows by one.
    pub fn publish(&self, value: serde_json::Value) {
        let mut results = self.job.results.lock();
        results.value = Some(Arc::new(value));
        results.revision += 1;
    }

    /// Replaces the typed result, read with [`JobHost::typed`].
    pub fn publish_typed(&self, value: Arc<dyn Any + Send + Sync>) {
        self.job.results.lock().typed = Some(value);
    }

    /// Adds a note to the snapshot.
    pub fn note(&self, text: &str) {
        self.job.status.lock().notes.push(text.to_string());
    }

    /// Whether the work runs outside this process's job object and outlives it.
    pub fn set_detached(&self, detached: bool) {
        self.job.status.lock().detached = detached;
    }

    /// Detail of the `left_running` or `stop_timed_out` row written when Cairn closes before
    /// the job ends.
    pub fn set_shutdown_note(&self, note: &str) {
        self.job.status.lock().shutdown_note = Some(note.to_string());
    }

    /// A stop was requested ([`JobHost::cancel`] or closing).
    pub fn cancel_requested(&self) -> bool {
        self.job.cancel.load(Ordering::SeqCst)
    }

    /// Cairn is closing.
    pub fn closing(&self) -> bool {
        self.job.closing.load(Ordering::SeqCst)
    }

    /// The flag behind [`JobContext::cancel_requested`], for code that polls it elsewhere.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.job.cancel)
    }

    /// How often the work is expected to look at [`JobContext::cancel_requested`].
    pub fn tick(&self) -> Duration {
        self.tick
    }

    /// The journal, when the job has one (audited or `needs_journal`).
    pub fn journal(&self) -> Option<&Arc<Journal>> {
        self.journal.as_ref()
    }

    /// Writes an ops_log row with no session. Fails when the job has no journal or the write
    /// failed.
    pub fn audit(&self, op: &str, target: &str, outcome: &str, detail: Option<&str>) -> Result<()> {
        let journal = self
            .journal
            .as_ref()
            .ok_or_else(|| Error::Other("this job has no journal".to_string()))?;
        journal.log_op(None, op, target, outcome, detail)
    }

    /// The lane's log folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The transcript's stem; `None` when the job does not log.
    pub fn stem(&self) -> Option<&str> {
        self.job.stem.as_deref()
    }

    /// `<log dir>\<stem><suffix>`, for files that belong to this run; fails when the job
    /// does not log.
    pub fn run_file(&self, suffix: &str) -> Result<PathBuf> {
        let stem = self
            .stem()
            .ok_or_else(|| Error::Other("this job keeps no files".to_string()))?;
        Ok(self.dir.join(format!("{stem}{suffix}")))
    }
}

// ───────────────────────────── Host ─────────────────────────────

#[derive(Debug, Default)]
struct Registry {
    jobs: Vec<Arc<Job>>,
    /// Title of a job between its checks and its registration.
    starting: Option<String>,
}

#[derive(Debug)]
struct Shared {
    config: HostConfig,
    closed: AtomicBool,
    registry: Mutex<Registry>,
}

impl Shared {
    fn find(&self, id: HostJobId) -> Option<Arc<Job>> {
        self.registry
            .lock()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    fn running_job(&self) -> Option<Arc<Job>> {
        self.registry
            .lock()
            .jobs
            .iter()
            .find(|j| j.is_running())
            .cloned()
    }

    /// Keeps the running job, the newest finished job of every kind and the
    /// `keep_finished` newest finished jobs; then drops the results of finished jobs that
    /// are not among the `keep_results_per_kind` newest of their kind. Dropped jobs and
    /// results are released after the lock.
    fn prune(&self) {
        let mut released_jobs: Vec<Arc<Job>> = Vec::new();
        let mut released_results: Vec<Results> = Vec::new();
        {
            let mut registry = self.registry.lock();
            let mut finished: Vec<Arc<Job>> = registry
                .jobs
                .iter()
                .filter(|j| !j.is_running())
                .cloned()
                .collect();
            finished.sort_unstable_by_key(|j| std::cmp::Reverse(j.id));
            let mut keep: HashSet<HostJobId> = finished
                .iter()
                .take(self.config.keep_finished)
                .map(|j| j.id)
                .collect();
            let mut kinds = HashSet::new();
            for job in &finished {
                if kinds.insert(job.spec.kind) {
                    keep.insert(job.id);
                }
            }
            let (kept, dropped): (Vec<Arc<Job>>, Vec<Arc<Job>>) = registry
                .jobs
                .drain(..)
                .partition(|j| j.is_running() || keep.contains(&j.id));
            registry.jobs = kept;
            released_jobs.extend(dropped);

            if let Some(limit) = self.config.keep_results_per_kind {
                let mut per_kind: HashMap<&'static str, usize> = HashMap::new();
                for job in finished.iter().filter(|j| keep.contains(&j.id)) {
                    let seen = per_kind.entry(job.spec.kind).or_insert(0);
                    if *seen >= limit {
                        let mut results = job.results.lock();
                        if results.held() {
                            released_results.push(Results {
                                value: results.value.take(),
                                typed: results.typed.take(),
                                revision: 0,
                            });
                        }
                    }
                    *seen += 1;
                }
            }
        }
        drop(released_results);
        drop(released_jobs);
    }
}

/// Clears the start reservation when a start ends early.
struct Reservation<'a> {
    shared: &'a Shared,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.registry.lock().starting = None;
        }
    }
}

/// Runs one job at a time for one lane; see the module docs.
pub struct JobHost {
    shared: Arc<Shared>,
}

impl fmt::Debug for JobHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobHost")
            .field("lane", &self.shared.config.lane)
            .field("closed", &self.shared.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl JobHost {
    pub fn new(config: HostConfig) -> JobHost {
        debug_assert!(is_kind(config.lane), "lane {:?}", config.lane);
        JobHost {
            shared: Arc::new(Shared {
                config,
                closed: AtomicBool::new(false),
                registry: Mutex::new(Registry::default()),
            }),
        }
    }

    pub fn config(&self) -> &HostConfig {
        &self.shared.config
    }

    /// Starts `work` as this lane's job and returns its first snapshot.
    ///
    /// In order: refuses when the host is closed or a job of it runs or is starting; when
    /// `spec.log` is set, creates the transcript (the log folder is checked for links before
    /// and after it is created) and prunes old ones; when `spec.audit` is set or
    /// `spec.needs_journal`, opens the journal (`open_journal` is never called otherwise) and
    /// writes the "started" row, deleting the new transcript when either fails; refuses when
    /// the host closed meanwhile (writing the final row); then runs `work` on a thread named
    /// `job-<lane>-<id>`. A panic in the work finishes the job as failed with the summary
    /// "internal error: <message>"; so does a thread that cannot be started, and the error
    /// is returned.
    pub fn start(
        &self,
        spec: JobSpec,
        open_journal: impl FnOnce() -> Result<Arc<Journal>>,
        work: Work,
    ) -> Result<HostJobSnapshot> {
        if !is_kind(spec.kind) {
            return Err(Error::Other(format!(
                "not a job kind: {:?}; expected lowercase letters and underscores",
                spec.kind
            )));
        }
        let config = &self.shared.config;
        {
            let mut registry = self.shared.registry.lock();
            if self.shared.closed.load(Ordering::SeqCst) {
                return Err(Error::Other(closing_text()));
            }
            let busy = registry
                .jobs
                .iter()
                .find(|j| j.is_running())
                .map(|j| j.spec.title.clone())
                .or_else(|| registry.starting.clone());
            if let Some(title) = busy {
                return Err(Error::Other(format!(
                    "{title} is running; wait for it to finish or stop it."
                )));
            }
            registry.starting = Some(spec.title.clone());
        }
        let mut reservation = Reservation {
            shared: &self.shared,
            armed: true,
        };

        let mut spec = spec;
        spec.command_line = bounded_command_line(&spec.command_line);

        let transcript = if spec.log {
            let header = format!(
                "{} {}  ·  {}",
                crate::APP_NAME,
                crate::VERSION,
                spec.command_line
            );
            let (stem, path, file) =
                logs::create_transcript(&config.log_dir, spec.kind, &header, Local::now())?;
            // Reopened for appending, so writes from any thread land at the end.
            drop(file);
            let log = match OpenOptions::new().append(true).open(&path) {
                Ok(log) => log,
                Err(e) => {
                    let _ = fs::remove_file(&path);
                    return Err(e.into());
                }
            };
            logs::prune(
                &config.log_dir,
                config.keep_logs,
                std::slice::from_ref(&stem),
            );
            Some((stem, path, log))
        } else {
            None
        };
        let discard = |transcript: Option<(String, PathBuf, File)>| {
            if let Some((_, path, log)) = transcript {
                drop(log);
                let _ = fs::remove_file(&path);
            }
        };

        let journal = if spec.audit.is_some() || spec.needs_journal {
            match open_journal() {
                Ok(journal) => Some(journal),
                Err(e) => {
                    discard(transcript);
                    return Err(e);
                }
            }
        } else {
            None
        };
        if let (Some(audit), Some(journal)) = (&spec.audit, &journal) {
            if let Err(e) = journal.log_op(
                None,
                audit.op,
                &audit.target,
                "started",
                Some(&audit.started_detail),
            ) {
                discard(transcript);
                return Err(e);
            }
        }

        let id = HostJobId(NEXT_ID.fetch_add(1, Ordering::SeqCst));
        let now = Instant::now();
        let audited = spec.audit.is_some();
        let (stem, log_path, log) = match transcript {
            Some((stem, path, log)) => (Some(stem), Some(path), Some(log)),
            None => (None, None, None),
        };
        let job = Arc::new(Job {
            id,
            lane: config.lane,
            spec,
            started: now,
            started_at: utc_now(),
            stem,
            log_path,
            journal: Mutex::new(journal.clone()),
            status: Mutex::new(Status {
                state: JobState::Running,
                finished_at: None,
                finished: None,
                last_activity: now,
                progress: None,
                progress_line: None,
                restart_required: false,
                summary: None,
                hint: None,
                notes: Vec::new(),
                detail: None,
                detached: false,
                logged: audited,
                shutdown_note: None,
            }),
            lines: Mutex::new(Lines::default()),
            results: Mutex::new(Results::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            closing: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            done: AtomicBool::new(false),
            settled: AtomicBool::new(false),
        });

        {
            // Checked under the registry lock: shutdown closes the host before it waits for
            // the reservation to clear, so the job is either registered before shutdown looks
            // for it or refused here.
            let mut registry = self.shared.registry.lock();
            if self.shared.closed.load(Ordering::SeqCst) {
                drop(registry);
                job.finish(failed(closing_text()));
                return Err(Error::Other(closing_text()));
            }
            registry.jobs.push(Arc::clone(&job));
            registry.starting = None;
        }
        reservation.armed = false;

        let context = JobContext {
            job: Arc::clone(&job),
            journal,
            log,
            dir: config.log_dir.clone(),
            tick: config.tick,
        };
        let shared = Arc::clone(&self.shared);
        let spawned = thread::Builder::new()
            .name(format!("job-{}-{id}", config.lane))
            .spawn(move || {
                let end = panic::catch_unwind(AssertUnwindSafe(|| work(&context))).unwrap_or_else(
                    |payload| {
                        let message = panic_message(payload.as_ref());
                        tracing::error!(job = context.job.id.0, %message, "a job panicked");
                        failed(format!("internal error: {message}"))
                    },
                );
                context.job.finish(end);
                let job = Arc::clone(&context.job);
                drop(context);
                shared.prune();
                job.settled.store(true, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            let reason = format!("internal error: the job's thread could not start: {e}");
            job.finish(failed(reason.clone()));
            self.shared.prune();
            job.settled.store(true, Ordering::SeqCst);
            return Err(Error::Other(reason));
        }
        Ok(job.snapshot())
    }

    /// The job's snapshot and up to `max_lines` output lines numbered after `after` (at most
    /// 500); `None` for an unknown id.
    pub fn view(&self, id: HostJobId, after: u64, max_lines: usize) -> Option<HostJobView> {
        let job = self.shared.find(id)?;
        let max = max_lines.clamp(1, MAX_LINES_PER_VIEW);
        // A job's last lines are pushed before its state becomes final, so a page read after
        // a finished snapshot holds every line (or reports `more`).
        let snapshot = job.snapshot();
        let (lines, first, next, skipped, more) = job.lines.lock().page(after, max);
        Some(HostJobView {
            job: snapshot,
            lines,
            first,
            next,
            skipped,
            more,
        })
    }

    pub fn snapshot(&self, id: HostJobId) -> Option<HostJobSnapshot> {
        self.shared.find(id).map(|j| j.snapshot())
    }

    /// The running job and the retained finished ones, newest first.
    pub fn jobs(&self) -> Vec<HostJobSnapshot> {
        let mut jobs = self.shared.registry.lock().jobs.clone();
        jobs.sort_unstable_by_key(|j| std::cmp::Reverse(j.id));
        jobs.iter().map(|j| j.snapshot()).collect()
    }

    /// The running job, if any.
    pub fn running(&self) -> Option<HostJobSnapshot> {
        self.shared.running_job().map(|j| j.snapshot())
    }

    /// Asks the running job to stop. `Ok(false)` when it already finished. Fails for an
    /// unknown id and for a job that cannot be stopped.
    pub fn cancel(&self, id: HostJobId) -> Result<bool> {
        let job = self.shared.find(id).ok_or_else(|| {
            Error::Other(format!("no {} job with id {id}", self.shared.config.lane))
        })?;
        if !job.spec.cancellable {
            return Err(Error::Other(format!(
                "{} can't be stopped; it runs to completion",
                job.spec.title
            )));
        }
        if !job.is_running() {
            return Ok(false);
        }
        job.cancel.store(true, Ordering::SeqCst);
        Ok(true)
    }

    /// The published result and its revision, only when the revision is newer than `since`.
    pub fn result(&self, id: HostJobId, since: u64) -> Option<(u64, Arc<serde_json::Value>)> {
        let job = self.shared.find(id)?;
        let results = job.results.lock();
        match &results.value {
            Some(value) if results.revision > since => Some((results.revision, Arc::clone(value))),
            _ => None,
        }
    }

    /// The typed result, when one of type `T` is held.
    pub fn typed<T: Any + Send + Sync>(&self, id: HostJobId) -> Option<Arc<T>> {
        let job = self.shared.find(id)?;
        let typed = job.results.lock().typed.clone()?;
        typed.downcast::<T>().ok()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Refuses new starts and ends this lane's work. Closes the host first, so a start that
    /// has not registered its job yet fails, then waits up to `settle_wait` for a start in
    /// progress to register or fail, asks the running job to stop and waits up to
    /// `stop_wait` without holding a lock: a job that ended is `Stopped`; one still running
    /// is `StopTimedOut` when it can be stopped, else `LeftRunning`, and an audited one gets
    /// that row with its shutdown note instead of a final row. A job that finished on its own
    /// before this was called is not listed.
    pub fn shutdown(&self) -> Vec<HostShutdownOutcome> {
        let config = &self.shared.config;
        self.shared.closed.store(true, Ordering::SeqCst);
        let settle = Instant::now() + config.settle_wait;
        while self.shared.registry.lock().starting.is_some() && Instant::now() < settle {
            thread::sleep(POLL);
        }
        let Some(job) = self.shared.running_job() else {
            return Vec::new();
        };
        job.closing.store(true, Ordering::SeqCst);
        job.cancel.store(true, Ordering::SeqCst);
        // Taken before the claim: a finish that loses the claim drops only its own copy.
        let journal = job.journal.lock().clone();
        // A job whose finish won the claim has ended as far as its rows go.
        let action = if wait_done(&job, config.stop_wait) || !job.claim() {
            HostShutdownAction::Stopped
        } else {
            let action = if job.spec.cancellable {
                HostShutdownAction::StopTimedOut
            } else {
                HostShutdownAction::LeftRunning
            };
            let note = job
                .status
                .lock()
                .shutdown_note
                .clone()
                .unwrap_or_else(|| DEFAULT_SHUTDOWN_NOTE.to_string());
            if job.spec.audit.is_some() {
                let logged = job.audit_row(journal.as_deref(), action.audit_outcome(), &note);
                let mut status = job.status.lock();
                status.logged = status.logged && logged;
            }
            job.footer(&note);
            action
        };
        drop(journal);
        vec![HostShutdownOutcome {
            id: job.id,
            kind: job.spec.kind,
            action,
        }]
    }

    /// Opens the job's transcript in Notepad.
    pub fn open_log(&self, id: HostJobId) -> Result<()> {
        let job = self.shared.find(id).ok_or_else(|| {
            Error::Other(format!("no {} job with id {id}", self.shared.config.lane))
        })?;
        let path = job
            .log_path
            .as_ref()
            .ok_or_else(|| Error::Other(format!("{} keeps no log", job.spec.title)))?;
        let mut command = crate::win::process::system_command("notepad.exe")?;
        command.arg(path);
        command.spawn()?;
        Ok(())
    }

    /// Waits until the job finished and the host pruned its finished jobs afterwards, or
    /// `timeout` passed; the latest snapshot, `None` for an unknown id.
    pub fn wait(&self, id: HostJobId, timeout: Duration) -> Option<HostJobSnapshot> {
        let job = self.shared.find(id)?;
        wait_for(|| job.settled.load(Ordering::SeqCst), timeout);
        Some(job.snapshot())
    }
}

fn closing_text() -> String {
    format!("{} is closing; nothing was started.", crate::APP_NAME)
}

fn failed(summary: String) -> WorkEnd {
    WorkEnd {
        state: JobState::Failed,
        summary,
        hint: None,
        restart_required: false,
        audit_detail: None,
    }
}

#[cfg(test)]
mod tests;
