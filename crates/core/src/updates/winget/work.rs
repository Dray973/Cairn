//! The work of a winget job, run on the job's thread: the update check (version, installed
//! apps, upgrade listing) and the update and install batches.
//!
//! Each step starts winget with its output going to a capture file in the job's step folder
//! and reads that file every tick. The step folder is `steps\<run stem>` in the lane's log
//! folder; for an elevated Cairn it grants access to SYSTEM and Administrators only, so a
//! standard user cannot plant the file an elevated winget writes. A capture file is deleted
//! when its process ends; one left running keeps its file, named in its audit row.
//!
//! A batch writes an app's "started" row before it starts that app's winget, and exactly one
//! final row after it, from this thread only. No row means no start. An app's winget is
//! never stopped: a stop request ends the batch after the current app, and when Cairn closes
//! the running app is recorded as left running and the rest are not started.
//!
//! The transcript marks where each step of a check begins ("== winget export =="), and in a
//! batch where each app begins and, from its final row, how it ended ("→ failed: exit …").

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};

use super::codes::{item_outcome, retry_may_help, scan_outcome, ItemState, ScanCode};
use super::export::{parse_export, Inventory, MAX_EXPORT_BYTES};
use super::progress::{step_fraction, step_sizes};
use super::table::{parse_upgrade_table, TableRow};
use super::{
    export_args, outdated_text, upgrade_list_args, version_args, BatchResult, ItemResult, Limits,
    ScanError, ScanResult, UpdateItem, UpdatesKind, UpdatesRequest, UpgradeRow, WingetEnv,
    WingetVersion, APP_INSTALLER_ID, APP_INSTALLER_NOTE, EXPLICIT_NOTE, MIN_VERSION, OP_INSTALL,
    OP_UPGRADE, STORE_NOTE, TRUNCATED_NOTE,
};
use crate::jobs::{JobContext, JobState, Work, WorkEnd};
use crate::tools::exit_code_hex;
use crate::tools::launch::{CommandSpec, DetachPolicy, Launcher};
use crate::tools::logs::refuse_links;
use crate::tools::runner::exit_detail;
use crate::win::console_text::{oem_code_page, OutputDecoder, OutputEvent, TextEncoding};
use crate::win::fs::{create_private_dir, read_small_regular_file};
use crate::{Error, Result};

/// Most output bytes read from a capture file per tick.
const READ_PER_TICK: usize = 1024 * 1024;
/// Most output lines a step keeps for parsing.
const MAX_KEPT_LINES: usize = 20_000;
/// Step folders older than this are deleted when a job starts.
const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
/// Folder of the step folders, inside the lane's log folder.
const STEPS_DIR: &str = "steps";

/// What a job needs besides its request.
pub(crate) struct WorkDeps {
    pub launcher: Arc<dyn Launcher>,
    /// winget.exe as located when the job was planned.
    pub program: PathBuf,
    pub env: WingetEnv,
    pub limits: Limits,
    /// Make the step folder administrator-only (Cairn runs elevated).
    pub private_steps: bool,
}

/// The work of `request`'s job.
pub(crate) fn work(deps: WorkDeps, request: UpdatesRequest) -> Work {
    Box::new(move |ctx: &JobContext| {
        let mut run = Run {
            ctx,
            program: deps.program.clone(),
            deps,
            relocated: false,
            dir: None,
            next_step: 1,
        };
        let end = match request.kind() {
            UpdatesKind::Scan => run.scan(),
            kind => run.batch(kind, request.items()),
        };
        run.finish();
        end
    })
}

/// How one winget step is run.
#[derive(Debug, Clone, Copy)]
struct StepOptions {
    deadline: Duration,
    keep_lines: bool,
    kill_on_cancel: bool,
    kill_on_close: bool,
}

/// How one winget step ended.
#[derive(Debug)]
enum StepEnd {
    Exited {
        code: i32,
        lines: Vec<String>,
        elapsed: Duration,
    },
    /// Cairn is closing; the process was stopped when the step allows it, else it continues.
    Closing {
        capture: PathBuf,
    },
    /// It ran past its deadline; stopped when the step allows it, else it continues.
    TimedOut {
        capture: PathBuf,
    },
    /// Stopped on request.
    Stopped,
    NotStarted {
        reason: String,
        not_found: bool,
    },
    /// Waiting on the process failed; it may still run.
    Lost {
        reason: String,
        capture: PathBuf,
    },
}

/// A launch that failed because the program is missing (App Installer was updated and the
/// versioned folder changed).
fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::Io(io) if matches!(io.kind(), io::ErrorKind::NotFound))
        || matches!(e, Error::Io(io) if io.raw_os_error() == Some(3))
}

/// Keeps the PC from sleeping while an update or install batch runs.
struct KeepAwake;

impl KeepAwake {
    fn start() -> KeepAwake {
        // SAFETY: plain flags; affects only this thread's execution state.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        KeepAwake
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: as above; resets this thread's request.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// `YYYYMMDD-HHMMSS-<kind>` with an optional `-N` suffix: the name of a run's step folder.
fn is_run_stem(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 17
        || !bytes[..8].iter().all(u8::is_ascii_digit)
        || bytes[8] != b'-'
        || !bytes[9..15].iter().all(u8::is_ascii_digit)
        || bytes[15] != b'-'
    {
        return false;
    }
    let kind = &name[16..];
    let kind = match kind.rsplit_once('-') {
        Some((head, n)) if n.len() == 1 && n.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => kind,
    };
    !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// Deletes the step folders in `root` last changed before `now - STALE_AFTER`. Links and
/// entries not named like a run are left alone; errors are ignored.
pub(super) fn sweep(root: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_run_stem) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 || !meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            if let Err(e) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(path = %entry.path().display(), error = %e, "cannot delete an old winget step folder");
            }
        }
    }
}

/// The version winget prints: the first word starting with 'v' that parses.
fn version_from(lines: &[String]) -> Option<WingetVersion> {
    lines
        .iter()
        .flat_map(|l| l.split_whitespace())
        .filter(|w| w.starts_with(['v', 'V']))
        .find_map(WingetVersion::parse)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// "60 min" for deadlines of a minute or more, else seconds.
fn deadline_text(deadline: Duration) -> String {
    if deadline.as_secs() >= 60 {
        format!("{} min", deadline.as_secs() / 60)
    } else {
        format!("{} s", deadline.as_secs())
    }
}

/// How reading the installed apps ended.
enum ExportEnd {
    Read(Inventory),
    Failed(String),
    Cancelled,
}

struct Run<'a> {
    ctx: &'a JobContext,
    deps: WorkDeps,
    program: PathBuf,
    /// winget was located again after a launch found its program missing.
    relocated: bool,
    /// This run's step folder, once created.
    dir: Option<PathBuf>,
    next_step: u32,
}

impl Run<'_> {
    /// Creates this run's step folder after deleting stale ones.
    fn prepare(&mut self) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        let lane = self.ctx.dir();
        let root = lane.join(STEPS_DIR);
        refuse_links(lane)?;
        refuse_links(&root)?;
        fs::create_dir_all(&root)?;
        refuse_links(&root)?;
        sweep(&root, SystemTime::now());
        let stem = self
            .ctx
            .stem()
            .ok_or_else(|| Error::Other("this job keeps no files".into()))?;
        let dir = root.join(stem);
        if self.deps.private_steps {
            create_private_dir(&dir)?;
        } else {
            fs::create_dir(&dir)?;
        }
        self.dir = Some(dir.clone());
        Ok(dir)
    }

    /// Removes the step folder when nothing is left in it.
    fn finish(&self) {
        if let Some(dir) = &self.dir {
            let _ = fs::remove_dir(dir);
        }
    }

    /// Reads up to `limit` new bytes of the capture file and passes the decoded events on.
    #[allow(clippy::too_many_arguments)]
    fn pump(
        &self,
        reader: &mut File,
        decoder: &mut OutputDecoder,
        buf: &mut [u8],
        limit: usize,
        lines: &mut Vec<String>,
        keep: bool,
        on_progress: &mut dyn FnMut(Option<f64>, &str),
    ) {
        let mut read = 0usize;
        let mut events = Vec::new();
        while read < limit {
            let n = match reader.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read winget's output");
                    break;
                }
            };
            read += n;
            decoder.push(&buf[..n], &mut events);
        }
        self.apply(events, decoder, lines, keep, on_progress);
    }

    fn apply(
        &self,
        events: Vec<OutputEvent>,
        decoder: &OutputDecoder,
        lines: &mut Vec<String>,
        keep: bool,
        on_progress: &mut dyn FnMut(Option<f64>, &str),
    ) {
        for event in events {
            match event {
                OutputEvent::Line(text) => {
                    self.ctx.line(&text);
                    if keep && lines.len() < MAX_KEPT_LINES {
                        lines.push(text);
                    }
                }
                OutputEvent::Progress(text) => on_progress(step_fraction(&text), &text),
            }
        }
        if let Some(text) = decoder.pending_progress() {
            on_progress(step_fraction(&text), &text);
        }
    }

    /// Runs winget with `args` and follows it until it ends, the deadline passes, a stop is
    /// requested (when the step may be stopped) or Cairn closes. `on_launch` learns, once the
    /// process runs, whether it runs outside Cairn's job object (and so outlives Cairn).
    fn run_step(
        &mut self,
        args: &[String],
        opts: StepOptions,
        on_progress: &mut dyn FnMut(Option<f64>, &str),
        on_launch: &mut dyn FnMut(bool),
    ) -> StepEnd {
        if let Some(flag) = super::forbidden_flag(args) {
            return StepEnd::NotStarted {
                reason: format!("Cairn never passes {flag} to winget"),
                not_found: false,
            };
        }
        let dir = match self.prepare() {
            Ok(dir) => dir,
            Err(e) => {
                return StepEnd::NotStarted {
                    reason: format!("its work folder could not be created: {e}"),
                    not_found: false,
                }
            }
        };
        let capture = dir.join(format!("s{}.out", self.next_step));
        self.next_step += 1;
        let output = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&capture)
        {
            Ok(file) => file,
            Err(e) => {
                return StepEnd::NotStarted {
                    reason: format!("its output file could not be created: {e}"),
                    not_found: false,
                }
            }
        };
        let mut reader = match File::open(&capture) {
            Ok(file) => file,
            Err(e) => {
                drop(output);
                let _ = fs::remove_file(&capture);
                return StepEnd::NotStarted {
                    reason: format!("its output file could not be read: {e}"),
                    not_found: false,
                };
            }
        };
        let spec = CommandSpec {
            program: self.program.clone(),
            args: args.to_vec(),
            detach: DetachPolicy::Prefer,
        };
        let started = Instant::now();
        let mut launched = match self.deps.launcher.launch(&spec, output) {
            Ok(launched) => launched,
            Err(e) => {
                drop(reader);
                let _ = fs::remove_file(&capture);
                return StepEnd::NotStarted {
                    not_found: is_not_found(&e),
                    reason: e.to_string(),
                };
            }
        };
        self.ctx.set_detached(launched.detached);
        on_launch(launched.detached);
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(oem_code_page()));
        let mut buf = vec![0u8; 64 * 1024];
        let mut lines = Vec::new();
        let mut stopping = false;
        let end = loop {
            self.pump(
                &mut reader,
                &mut decoder,
                &mut buf,
                READ_PER_TICK,
                &mut lines,
                opts.keep_lines,
                on_progress,
            );
            match launched.process.try_wait() {
                Ok(Some(code)) => {
                    self.pump(
                        &mut reader,
                        &mut decoder,
                        &mut buf,
                        usize::MAX,
                        &mut lines,
                        opts.keep_lines,
                        on_progress,
                    );
                    let mut events = Vec::new();
                    decoder.finish(&mut events);
                    self.apply(events, &decoder, &mut lines, opts.keep_lines, on_progress);
                    drop(reader);
                    let _ = fs::remove_file(&capture);
                    break if stopping {
                        StepEnd::Stopped
                    } else {
                        StepEnd::Exited {
                            code,
                            lines,
                            elapsed: started.elapsed(),
                        }
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    break StepEnd::Lost {
                        reason: e.to_string(),
                        capture,
                    }
                }
            }
            if self.ctx.closing() {
                if opts.kill_on_close {
                    let _ = launched.process.kill();
                }
                break StepEnd::Closing { capture };
            }
            if !stopping && opts.kill_on_cancel && self.ctx.cancel_requested() {
                stopping = true;
                let _ = launched.process.kill();
                continue;
            }
            if started.elapsed() > opts.deadline {
                if opts.kill_on_cancel {
                    let _ = launched.process.kill();
                }
                break StepEnd::TimedOut { capture };
            }
            thread::sleep(self.ctx.tick());
        };
        self.ctx.set_detached(false);
        end
    }

    /// [`Run::run_step`], locating winget again and retrying once when its program is
    /// missing.
    fn run_located(
        &mut self,
        args: &[String],
        opts: StepOptions,
        on_progress: &mut dyn FnMut(Option<f64>, &str),
        on_launch: &mut dyn FnMut(bool),
    ) -> StepEnd {
        let end = self.run_step(args, opts, on_progress, on_launch);
        if !matches!(
            end,
            StepEnd::NotStarted {
                not_found: true,
                ..
            }
        ) || self.relocated
        {
            return end;
        }
        self.relocated = true;
        match (self.deps.env.locate)() {
            Ok(Some(location)) if location.path != self.program => {
                self.ctx
                    .line(&format!("winget moved to {}", location.path.display()));
                self.program = location.path;
                self.run_step(args, opts, on_progress, on_launch)
            }
            _ => end,
        }
    }

    fn list_options(&self) -> StepOptions {
        StepOptions {
            deadline: self.deps.limits.list,
            keep_lines: true,
            kill_on_cancel: true,
            kill_on_close: true,
        }
    }

    /// Marks in the transcript where the step that runs `winget <what>` begins.
    fn step_marker(&self, what: &str) {
        self.ctx.line(&format!("== winget {what} =="));
    }

    /// Reads the installed apps with `winget export` into this run's step folder.
    fn export_inventory(&mut self) -> ExportEnd {
        self.step_marker("export");
        let dir = match self.prepare() {
            Ok(dir) => dir,
            Err(e) => {
                return ExportEnd::Failed(format!("its work folder could not be created: {e}"))
            }
        };
        let file = dir.join(format!("inventory-{}.json", super::super::random_hex(8)));
        let mut opts = self.list_options();
        opts.keep_lines = false;
        let end = self.run_located(&export_args(&file), opts, &mut |_, _| {}, &mut |_| {});
        let outcome = match end {
            StepEnd::Exited { code, .. } => {
                match read_small_regular_file(&file, MAX_EXPORT_BYTES) {
                    Ok(Some(bytes)) => match parse_export(&bytes) {
                        Ok(inventory) => ExportEnd::Read(inventory),
                        Err(e) => ExportEnd::Failed(format!(
                            "winget's list of installed apps could not be read: {e}"
                        )),
                    },
                    Ok(None) => ExportEnd::Failed(format!(
                        "winget export ended with {} and wrote no list",
                        exit_code_hex(code)
                    )),
                    Err(e) => ExportEnd::Failed(format!(
                        "winget's list of installed apps could not be read: {e}"
                    )),
                }
            }
            StepEnd::Stopped | StepEnd::Closing { .. } => ExportEnd::Cancelled,
            StepEnd::TimedOut { .. } => ExportEnd::Failed(format!(
                "winget export did not finish within {}",
                deadline_text(self.deps.limits.list)
            )),
            StepEnd::NotStarted { reason, .. } => {
                ExportEnd::Failed(format!("winget export could not be started: {reason}"))
            }
            StepEnd::Lost { reason, .. } => {
                ExportEnd::Failed(format!("Cairn lost track of winget export: {reason}"))
            }
        };
        let _ = fs::remove_file(&file);
        outcome
    }

    // ───────────────────────────── update check ─────────────────────────────

    fn publish_scan(&self, result: &ScanResult) {
        match serde_json::to_value(result) {
            Ok(value) => self.ctx.publish(value),
            Err(e) => tracing::error!(error = %e, "cannot publish the update check"),
        }
    }

    fn scan_failed(&self, mut result: ScanResult, error: ScanError) -> WorkEnd {
        let summary = error.message.clone();
        result.error = Some(error);
        result.checked_at = Some((self.deps.env.now)().to_rfc3339());
        self.publish_scan(&result);
        WorkEnd {
            state: JobState::Failed,
            summary,
            hint: None,
            restart_required: false,
            audit_detail: None,
        }
    }

    fn scan_stopped(&self, result: &ScanResult) -> WorkEnd {
        self.publish_scan(result);
        let summary = if self.ctx.closing() {
            "The check stopped because Cairn closed."
        } else {
            "The check was stopped."
        };
        WorkEnd {
            state: JobState::Cancelled,
            summary: summary.to_string(),
            hint: None,
            restart_required: false,
            audit_detail: None,
        }
    }

    fn error(message: String) -> ScanError {
        ScanError {
            code: None,
            message,
            availability: None,
            outdated: false,
        }
    }

    fn scan(&mut self) -> WorkEnd {
        let mut result = ScanResult::empty();
        self.ctx.progress(None, Some("Starting winget…"));
        let version_options = StepOptions {
            deadline: self.deps.limits.version,
            keep_lines: true,
            kill_on_cancel: true,
            kill_on_close: true,
        };
        self.step_marker("--version");
        match self.run_located(
            &version_args(),
            version_options,
            &mut |_, _| {},
            &mut |_| {},
        ) {
            StepEnd::Exited { lines, .. } => match version_from(&lines) {
                Some(version) if version < MIN_VERSION => {
                    result.winget_version = Some(version.to_string());
                    let error = ScanError {
                        code: None,
                        message: outdated_text(&version.to_string()),
                        availability: None,
                        outdated: true,
                    };
                    return self.scan_failed(result, error);
                }
                Some(version) => result.winget_version = Some(version.to_string()),
                None => result
                    .warnings
                    .push("winget's version could not be read.".to_string()),
            },
            StepEnd::Stopped | StepEnd::Closing { .. } => return self.scan_stopped(&result),
            StepEnd::TimedOut { .. } => {
                let message = format!(
                    "winget didn't answer within {}.",
                    deadline_text(self.deps.limits.version)
                );
                return self.scan_failed(result, Self::error(message));
            }
            StepEnd::NotStarted { reason, .. } => {
                return self.scan_failed(
                    result,
                    Self::error(format!("winget couldn't be started: {reason}")),
                )
            }
            StepEnd::Lost { reason, .. } => {
                return self.scan_failed(
                    result,
                    Self::error(format!("Cairn lost track of winget: {reason}")),
                )
            }
        }
        if self.ctx.cancel_requested() {
            return self.scan_stopped(&result);
        }

        self.ctx.progress(None, Some("Reading installed apps…"));
        let inventory = match self.export_inventory() {
            ExportEnd::Read(inventory) => Some(inventory),
            ExportEnd::Failed(reason) => {
                self.ctx.line(&reason);
                result.inventory_complete = false;
                result.warnings.push(reason);
                None
            }
            ExportEnd::Cancelled => return self.scan_stopped(&result),
        };
        if let Some(inventory) = &inventory {
            result.installed = inventory.ids();
        }
        if self.ctx.cancel_requested() {
            return self.scan_stopped(&result);
        }

        self.ctx.progress(None, Some("Looking for updates…"));
        let list_options = self.list_options();
        self.step_marker("upgrade");
        let (code, lines) = match self.run_located(
            &upgrade_list_args(),
            list_options,
            &mut |_, _| {},
            &mut |_| {},
        ) {
            StepEnd::Exited { code, lines, .. } => (code, lines),
            StepEnd::Stopped | StepEnd::Closing { .. } => return self.scan_stopped(&result),
            StepEnd::TimedOut { .. } => {
                let message = format!(
                    "winget didn't finish listing updates within {}.",
                    deadline_text(self.deps.limits.list)
                );
                return self.scan_failed(result, Self::error(message));
            }
            StepEnd::NotStarted { reason, .. } => {
                return self.scan_failed(
                    result,
                    Self::error(format!("winget couldn't be started: {reason}")),
                )
            }
            StepEnd::Lost { reason, .. } => {
                return self.scan_failed(
                    result,
                    Self::error(format!("Cairn lost track of winget: {reason}")),
                )
            }
        };
        let table = parse_upgrade_table(&lines, inventory.as_ref());
        result.unparsed_rows = table.unparsed;
        result.upgrades = table.rows.iter().map(upgrade_row).collect();
        match scan_outcome(code) {
            ScanCode::Ok | ScanCode::Empty => {}
            ScanCode::Partial(text) if !result.upgrades.is_empty() => {
                result.warnings.push(text.to_string())
            }
            ScanCode::Partial(text) => {
                let error = ScanError {
                    code: Some(exit_code_hex(code)),
                    ..Self::error(text.to_string())
                };
                return self.scan_failed(result, error);
            }
            ScanCode::Error(failure) => {
                let error = ScanError {
                    code: Some(exit_code_hex(code)),
                    ..Self::error(failure.message())
                };
                return self.scan_failed(result, error);
            }
        }
        result.checked_at = Some((self.deps.env.now)().to_rfc3339());
        self.publish_scan(&result);
        let count = result.upgrades.len();
        let summary = if count == 0 {
            "All apps are up to date".to_string()
        } else {
            format!("{} available", plural(count, "update", "updates"))
        };
        self.ctx.progress(Some(100.0), Some(&summary));
        WorkEnd {
            state: JobState::Succeeded,
            summary,
            hint: None,
            restart_required: false,
            audit_detail: None,
        }
    }

    // ───────────────────────────── batches ─────────────────────────────

    fn publish_batch(&self, batch: &mut BatchResult) {
        publish_batch(self.ctx, batch);
    }

    /// Writes an app's final audit row, and its outcome and detail as a transcript line; a
    /// failed write of the row is logged and noted.
    fn final_row(&self, op: &str, id: &str, outcome: &str, detail: &str) {
        if let Err(e) = self.ctx.audit(op, id, outcome, Some(detail)) {
            tracing::error!(op, id, outcome, error = %e, "cannot write an app's final audit row");
            self.ctx.note(&format!(
                "The final History row for {id} could not be written: {e}"
            ));
        }
        self.ctx.line(&format!("→ {outcome}: {detail}"));
    }

    fn batch(&mut self, kind: UpdatesKind, items: &[UpdateItem]) -> WorkEnd {
        let _awake = KeepAwake::start();
        let install = kind == UpdatesKind::Install;
        let op = if install { OP_INSTALL } else { OP_UPGRADE };
        let verb = if install { "Installing" } else { "Updating" };
        let total = items.len();
        let mut batch = BatchResult {
            kind,
            items: items.iter().map(ItemResult::queued).collect(),
            current: None,
            done: 0,
            total,
            stopping: false,
            restart_required: false,
        };
        self.publish_batch(&mut batch);

        if install {
            self.ctx
                .progress(Some(0.0), Some("Checking which apps are installed…"));
            match self.export_inventory() {
                ExportEnd::Read(inventory) => {
                    for item in batch.items.iter_mut() {
                        if inventory.find(&item.id).is_some() {
                            item.state = ItemState::AlreadyInstalled;
                            item.message = Some(super::codes::ALREADY_INSTALLED_TEXT.to_string());
                        }
                    }
                }
                ExportEnd::Failed(reason) => {
                    self.ctx.line(&reason);
                    self.ctx.note(&format!(
                        "Cairn couldn't check which apps are installed ({reason}); winget skips \
                         apps that are already installed."
                    ));
                }
                ExportEnd::Cancelled => {
                    for item in batch.items.iter_mut() {
                        if item.state == ItemState::Queued {
                            item.state = ItemState::NotStarted;
                        }
                    }
                    self.publish_batch(&mut batch);
                    return self.batch_end(kind, &batch, false, false, false);
                }
            }
            self.publish_batch(&mut batch);
        }

        let mut launched_any = false;
        let mut audit_failed = false;
        let mut stopped = false;
        for (index, item) in items.iter().enumerate() {
            if batch.items[index].state != ItemState::Queued {
                continue;
            }
            if self.ctx.cancel_requested() {
                not_started_from(&mut batch, index);
                stopped = true;
                break;
            }
            let position = format!("({} of {total})", index + 1);
            batch.current = Some(index);
            batch.items[index].state = ItemState::Running;
            self.publish_batch(&mut batch);
            let done = batch.done;
            let line = format!("{verb} {} {position}", item.name);
            self.ctx.set_detail(item_detail(index, None));
            self.ctx
                .progress(Some(100.0 * done as f64 / total as f64), Some(&line));
            self.ctx.set_shutdown_note(&format!(
                "{} was still running when Cairn closed; it continues on its own.",
                item.name
            ));

            let log = self
                .ctx
                .run_file(".log")
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let detail = if install {
                format!("{}  ·  source {}  ·  log {log}", item.name, item.source)
            } else {
                format!(
                    "{}  ·  {} → {}  ·  source {}  ·  log {log}",
                    item.name,
                    item.from.as_deref().unwrap_or("?"),
                    item.to.as_deref().unwrap_or("?"),
                    item.source
                )
            };
            if let Err(e) = self.ctx.audit(op, &item.id, "started", Some(&detail)) {
                tracing::error!(op, id = %item.id, error = %e, "cannot write an app's audit row");
                let failed = &mut batch.items[index];
                failed.state = ItemState::Failed;
                failed.message =
                    Some("Couldn't write the audit row, so it wasn't started.".to_string());
                not_started_from(&mut batch, index + 1);
                audit_failed = true;
                self.ctx.set_detail(serde_json::Value::Null);
                break;
            }
            let action = if install {
                "installing".to_string()
            } else {
                format!(
                    "updating {} → {}",
                    item.from.as_deref().unwrap_or("?"),
                    item.to.as_deref().unwrap_or("?")
                )
            };
            self.ctx.line(&format!(
                "== {} ({}): {action} {position} ==",
                item.name, item.id
            ));

            let options = StepOptions {
                deadline: self.deps.limits.item,
                keep_lines: false,
                kill_on_cancel: false,
                kill_on_close: false,
            };
            let ctx = self.ctx;
            // A download shows its sizes; any other display with a fraction its percentage.
            // The detail goes first, so a progress line never belongs to an older detail.
            let mut on_progress = |fraction: Option<f64>, display: &str| {
                let within = fraction.unwrap_or(0.0);
                let overall = 100.0 * (done as f64 + within) / total as f64;
                let shown =
                    step_sizes(display).or_else(|| fraction.map(|f| format!("{:.0}%", f * 100.0)));
                let text = match &shown {
                    Some(shown) => format!("{line}  ·  {shown}"),
                    None => line.clone(),
                };
                ctx.set_detail(item_detail(index, shown.as_deref()));
                ctx.progress(Some(overall), Some(&text));
            };
            // The close dialog reads from the running app whether its winget outlives Cairn.
            let mut on_launch = |detached: bool| {
                batch.items[index].detached = detached;
                publish_batch(ctx, &mut batch);
            };
            let args = super::item_args(kind, item);
            let started = Instant::now();
            let end = self.run_located(&args, options, &mut on_progress, &mut on_launch);
            self.ctx.set_detail(serde_json::Value::Null);
            let result = &mut batch.items[index];
            result.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match end {
                StepEnd::Exited { code, elapsed, .. } => {
                    launched_any = true;
                    let (state, message) = item_outcome(kind, code);
                    result.state = state;
                    result.exit_code = Some(code);
                    result.exit_code_hex = Some(exit_code_hex(code));
                    result.message = message.clone();
                    result.retry = retry_may_help(code);
                    let mut detail = exit_detail(code, elapsed);
                    if let Some(message) = &message {
                        detail.push_str(&format!(" · {message}"));
                    }
                    self.final_row(op, &item.id, state.audit_outcome(), &detail);
                }
                StepEnd::Closing { capture } => {
                    result.state = ItemState::LeftRunning;
                    result.message = Some("It continues after Cairn closed.".to_string());
                    self.final_row(
                        op,
                        &item.id,
                        ItemState::LeftRunning.audit_outcome(),
                        &format!(
                            "Cairn closed while this was running; it continues on its own. \
                             Its output: {}",
                            capture.display()
                        ),
                    );
                    not_started_from(&mut batch, index + 1);
                    batch.current = None;
                    self.publish_batch(&mut batch);
                    return WorkEnd {
                        state: JobState::Cancelled,
                        summary: format!(
                            "Cairn closed while {} was running; it continues on its own.",
                            item.name
                        ),
                        hint: None,
                        restart_required: false,
                        audit_detail: None,
                    };
                }
                StepEnd::TimedOut { capture } => {
                    launched_any = true;
                    let waited = deadline_text(self.deps.limits.item);
                    result.state = ItemState::TimedOut;
                    result.message = Some(format!("Still running after {waited}."));
                    self.final_row(
                        op,
                        &item.id,
                        ItemState::TimedOut.audit_outcome(),
                        &format!(
                            "Still running after {waited}; Cairn stopped waiting. Its output: {}",
                            capture.display()
                        ),
                    );
                    not_started_from(&mut batch, index + 1);
                    batch.current = None;
                    self.publish_batch(&mut batch);
                    break;
                }
                StepEnd::NotStarted { reason, .. } => {
                    result.state = ItemState::Failed;
                    result.message = Some(format!("winget couldn't be started: {reason}"));
                    self.final_row(op, &item.id, "failed", &reason);
                }
                StepEnd::Stopped => {
                    launched_any = true;
                    result.state = ItemState::Failed;
                    result.message = Some("winget was stopped.".to_string());
                    self.final_row(op, &item.id, "failed", "winget was stopped");
                }
                StepEnd::Lost { reason, capture } => {
                    launched_any = true;
                    result.state = ItemState::Failed;
                    result.message = Some(format!("Cairn lost track of winget: {reason}"));
                    self.final_row(
                        op,
                        &item.id,
                        "failed",
                        &format!(
                            "Cairn lost track of winget ({reason}); it may still run. Its output: {}",
                            capture.display()
                        ),
                    );
                    not_started_from(&mut batch, index + 1);
                    batch.current = None;
                    self.publish_batch(&mut batch);
                    break;
                }
            }
            batch.current = None;
            self.publish_batch(&mut batch);
        }
        batch.current = None;
        self.publish_batch(&mut batch);
        self.batch_end(kind, &batch, launched_any, audit_failed, stopped)
    }

    fn batch_end(
        &self,
        kind: UpdatesKind,
        batch: &BatchResult,
        launched_any: bool,
        audit_failed: bool,
        stopped: bool,
    ) -> WorkEnd {
        let count = |state: ItemState| batch.items.iter().filter(|i| i.state == state).count();
        let succeeded = count(ItemState::Succeeded);
        let failed = count(ItemState::Failed) + count(ItemState::TimedOut);
        let restart = count(ItemState::RestartRequired);
        let current = count(ItemState::AlreadyCurrent);
        let installed = count(ItemState::AlreadyInstalled);
        let not_started = count(ItemState::NotStarted);
        let done_word = if kind == UpdatesKind::Install {
            "installed"
        } else {
            "updated"
        };
        let mut parts = Vec::new();
        if succeeded > 0 {
            parts.push(format!("{succeeded} {done_word}"));
        }
        if current > 0 {
            parts.push(format!("{current} already up to date"));
        }
        if installed > 0 {
            parts.push(format!("{installed} already installed"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if restart > 0 {
            parts.push(format!(
                "{restart} {}",
                if restart == 1 {
                    "needs a restart"
                } else {
                    "need a restart"
                }
            ));
        }
        if not_started > 0 {
            parts.push(format!("{not_started} not started"));
        }
        let summary = parts.join(" · ");
        let state = if audit_failed || (!launched_any && failed > 0) {
            JobState::Failed
        } else if failed > 0 || restart > 0 {
            JobState::Attention
        } else if not_started > 0 || stopped {
            JobState::Cancelled
        } else {
            JobState::Succeeded
        };
        let hint = audit_failed.then(|| {
            "Cairn couldn't write to its change history, so it started nothing more.".to_string()
        });
        WorkEnd {
            state,
            summary,
            hint,
            restart_required: restart > 0,
            audit_detail: None,
        }
    }
}

/// Publishes `batch` with its counters brought up to date.
fn publish_batch(ctx: &JobContext, batch: &mut BatchResult) {
    batch.done = batch.items.iter().filter(|i| i.state.is_finished()).count();
    batch.stopping = ctx.cancel_requested();
    batch.restart_required = batch
        .items
        .iter()
        .any(|i| i.state == ItemState::RestartRequired);
    match serde_json::to_value(&*batch) {
        Ok(value) => ctx.publish(value),
        Err(e) => tracing::error!(error = %e, "cannot publish the batch"),
    }
}

/// The job's live detail while app `index` of a batch runs: `{"item": index, "progress":
/// "12.0 MB / 32.5 MB" | "45%" | null}`, the download sizes or the percentage winget shows.
fn item_detail(index: usize, progress: Option<&str>) -> serde_json::Value {
    serde_json::json!({ "item": index, "progress": progress })
}

/// Marks every queued item from `index` on as not started.
fn not_started_from(batch: &mut BatchResult, index: usize) {
    for item in batch.items.iter_mut().skip(index) {
        if item.state == ItemState::Queued {
            item.state = ItemState::NotStarted;
        }
    }
}

/// The published row of an app with an update, with whether it can be picked and why not.
fn upgrade_row(row: &TableRow) -> UpgradeRow {
    let mut notes: Vec<String> = Vec::new();
    let mut selectable = true;
    if row.id.eq_ignore_ascii_case(APP_INSTALLER_ID) {
        selectable = false;
        notes.push(APP_INSTALLER_NOTE.to_string());
    } else if row.id_truncated {
        selectable = false;
        notes.push(TRUNCATED_NOTE.to_string());
    } else {
        if row.explicit_only {
            notes.push(EXPLICIT_NOTE.to_string());
        }
        if row.source.eq_ignore_ascii_case("msstore") {
            notes.push(STORE_NOTE.to_string());
        }
    }
    if let Some(note) = &row.note {
        notes.push(note.clone());
    }
    UpgradeRow {
        id: row.id.clone(),
        name: row.name.clone(),
        installed: row.installed.clone(),
        available: row.available.clone(),
        source: row.source.clone(),
        explicit_only: row.explicit_only,
        selectable,
        note: (!notes.is_empty()).then(|| notes.join("  ·  ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::runner::fmt_duration;

    #[test]
    fn run_stems_are_recognised() {
        assert!(is_run_stem("20260925-100000-winget_scan"));
        assert!(is_run_stem("20260925-100000-winget_upgrade-2"));
        assert!(!is_run_stem("20260925-100000-"));
        assert!(!is_run_stem("notes.txt"));
        assert!(!is_run_stem("2026092-100000-winget_scan"));
        assert!(!is_run_stem("20260925-100000-Winget"));
    }

    #[test]
    fn versions_are_read_from_the_first_v_word() {
        let lines = vec!["".to_string(), "v1.29.380".to_string()];
        assert_eq!(
            version_from(&lines),
            Some(WingetVersion {
                major: 1,
                minor: 29,
                patch: 380
            })
        );
        assert_eq!(version_from(&["winget 1.2".to_string()]), None);
        assert_eq!(
            version_from(&["value v1.5.0 more".to_string()]),
            Some(WingetVersion {
                major: 1,
                minor: 5,
                patch: 0
            })
        );
    }

    #[test]
    fn deadlines_read_in_minutes_or_seconds() {
        assert_eq!(deadline_text(Duration::from_secs(3600)), "60 min");
        assert_eq!(deadline_text(Duration::from_secs(300)), "5 min");
        assert_eq!(deadline_text(Duration::from_secs(30)), "30 s");
        assert_eq!(fmt_duration(Duration::from_secs(61)), "1 min 1 s");
    }

    #[test]
    fn rows_say_why_they_cannot_be_picked() {
        let row = |id: &str, source: &str, explicit_only: bool, id_truncated: bool| TableRow {
            name: "App".into(),
            id: id.into(),
            installed: "1.0".into(),
            available: "2.0".into(),
            source: source.into(),
            explicit_only,
            id_truncated,
            note: None,
        };
        let installer = upgrade_row(&row("microsoft.appinstaller", "winget", false, false));
        assert!(!installer.selectable);
        assert_eq!(installer.note.as_deref(), Some(APP_INSTALLER_NOTE));
        let cut = upgrade_row(&row("Contoso.Long…", "winget", false, true));
        assert!(!cut.selectable);
        assert_eq!(cut.note.as_deref(), Some(TRUNCATED_NOTE));
        let explicit = upgrade_row(&row("Contoso.Editor", "winget", true, false));
        assert!(explicit.selectable && explicit.explicit_only);
        assert_eq!(explicit.note.as_deref(), Some(EXPLICIT_NOTE));
        let store = upgrade_row(&row("9NTAILSPIN0001", "msstore", false, false));
        assert!(store.selectable);
        assert_eq!(store.note.as_deref(), Some(STORE_NOTE));
        let mut twice = row("Contoso.Editor", "winget", false, false);
        twice.note = Some("2 installations".into());
        assert_eq!(upgrade_row(&twice).note.as_deref(), Some("2 installations"));
        assert_eq!(
            upgrade_row(&row("Contoso.Editor", "winget", false, false)).note,
            None
        );
    }
}
