//! Disk speed test: sequential 1 MiB and random 4 KiB reads and writes at a deep and a
//! shallow queue, as CrystalDiskMark measures them, on a test file Cairn creates and deletes.
//!
//! Needs administrator rights (the test folder at the volume root gets an admin-only DACL)
//! and is audited: the `disk_speed_test` "started" row is written before any folder, file or
//! thread exists, and exactly one final row after the file is gone. Throughput is in decimal
//! MB/s; each result is the best of the runs. The file is opened with
//! FILE_FLAG_DELETE_ON_CLOSE, so Windows deletes it even when the process is killed.

pub(crate) mod history;
pub(crate) mod io;
pub(crate) mod place;

use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileAllocationInfo, FileBasicInfo, FileEndOfFileInfo, FileStorageInfo,
    GetFileAttributesW, GetFileInformationByHandleEx, RemoveDirectoryW, SetFileInformationByHandle,
    CREATE_NEW, DELETE, FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_BASIC_INFO,
    FILE_END_OF_FILE_INFO, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_MODE, FILE_STORAGE_INFO, INVALID_FILE_ATTRIBUTES,
};

pub use history::{speed_history, HISTORY_FILE, HISTORY_LIMIT};
pub use place::{
    check_folder_name, is_folder_name, remove_leftover, remove_leftover_opening, Leftover,
    LeftoverRemoval, FILE_NAME, FOLDER_PREFIX, OP_LEFTOVER, ROOT_REMOVAL_FORBIDDEN,
};

use self::io::{
    measure, prepare, AlignedBuf, IoOp, MeasureSpec, OverlappedTarget, Rng, RunStats,
    DISK_FULL_MARK,
};
use self::place::{create_test_folder, find_leftovers, random_hex, remove_leftover_in};
use super::files::{
    wide_path, ATTR_COMPRESSED, ATTR_ENCRYPTED, ATTR_INTEGRITY_STREAM, ATTR_SPARSE,
};
use super::volumes::{self, root_of, StorageVolume};
use super::{
    detail, drive_tests_forbidden, size_text, speed_title, utc_now, AwakeGuard, KIND_SPEED_TEST,
};
use crate::jobs::{AuditSpec, HostJobSnapshot, JobContext, JobHost, JobSpec, JobState, WorkEnd};
use crate::safety::state_log::Journal;
use crate::tools::catalog::{normalize_volume, ToolId};
use crate::tools::runner::fmt_duration;
use crate::win::error_mode::ErrorModeGuard;
use crate::win::handle::OwnedHandle;
use crate::win::storage::MediaKind;
use crate::{Error, Result};

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
/// Test file sizes the UI offers.
pub const SIZE_CHOICES: [u64; 5] = [64 * MIB, 256 * MIB, GIB, 4 * GIB, 16 * GIB];
pub const DEFAULT_SIZE: u64 = GIB;
pub const MIN_SIZE: u64 = MIB;
pub const MAX_SIZE: u64 = 64 * GIB;
/// Run counts the UI offers.
pub const RUN_CHOICES: [u32; 3] = [1, 3, 5];
pub const DEFAULT_RUNS: u32 = 3;
pub const MAX_RUNS: u32 = 9;
/// Length of one read measurement (a write measurement also ends after one pass).
pub const MEASURE_TIME: Duration = Duration::from_secs(5);
/// Pause between measurements, which lets an SSD's cache settle.
pub const PAUSE: Duration = Duration::from_secs(1);
/// ops_log name of a speed test.
pub const OP: &str = "disk_speed_test";
/// Detail of the row written when Cairn closes before the test stopped.
pub const SHUTDOWN_NOTE: &str =
    "Cairn closed before the test stopped; the test file is deleted when Cairn's process ends";
/// Smallest and largest pool of random write data.
const POOL_MIN: u64 = 8 * MIB;
const POOL_MAX: u64 = 64 * MIB;
/// I/O slots of the target: the deepest queue of the tests.
const SLOTS: usize = 32;
/// Read buffer of the target: the most bytes in flight in any read test (8 × 1 MiB).
const READ_BUFFER: usize = 8 << 20;
/// A write measurement shorter than this gets a note about the test size.
const SHORT_WRITE: Duration = Duration::from_millis(500);
/// How often the live speed is published.
const LIVE_PERIOD: Duration = Duration::from_millis(200);
/// Attempts, 200 ms apart, to remove the test folder.
const FOLDER_REMOVE_TRIES: u32 = 5;
/// Processes whose work lowers the results.
const MAINTENANCE_PROCESSES: [&str; 5] = [
    "defrag.exe",
    "chkdsk.exe",
    "sfc.exe",
    "dism.exe",
    "tiworker.exe",
];

pub const NOT_ELEVATED_TEXT: &str = "Needs administrator rights; restart Cairn as administrator.";
pub const FORBIDDEN_TEXT: &str =
    "Speed tests are turned off in this environment (OPTIMIZER_FORBID_DRIVE_TESTS)";

/// One of the four measurements, each done as a read and as a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedTestId {
    Seq1mQ8t1,
    Seq1mQ1t1,
    Rnd4kQ32t1,
    Rnd4kQ1t1,
}

impl SpeedTestId {
    pub const ALL: [SpeedTestId; 4] = [
        SpeedTestId::Seq1mQ8t1,
        SpeedTestId::Seq1mQ1t1,
        SpeedTestId::Rnd4kQ32t1,
        SpeedTestId::Rnd4kQ1t1,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SpeedTestId::Seq1mQ8t1 => "SEQ1M Q8T1",
            SpeedTestId::Seq1mQ1t1 => "SEQ1M Q1T1",
            SpeedTestId::Rnd4kQ32t1 => "RND4K Q32T1",
            SpeedTestId::Rnd4kQ1t1 => "RND4K Q1T1",
        }
    }

    pub fn block_bytes(self) -> u32 {
        if self.random() {
            4096
        } else {
            1 << 20
        }
    }

    pub fn queue_depth(self) -> u32 {
        match self {
            SpeedTestId::Seq1mQ8t1 => 8,
            SpeedTestId::Seq1mQ1t1 => 1,
            SpeedTestId::Rnd4kQ32t1 => 32,
            SpeedTestId::Rnd4kQ1t1 => 1,
        }
    }

    pub fn random(self) -> bool {
        matches!(self, SpeedTestId::Rnd4kQ32t1 | SpeedTestId::Rnd4kQ1t1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Read,
    Write,
}

impl Direction {
    fn word(self) -> &'static str {
        match self {
            Direction::Read => "read",
            Direction::Write => "write",
        }
    }
}

/// A test as the plan lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestSpec {
    pub id: SpeedTestId,
    pub label: &'static str,
    pub block_bytes: u32,
    pub queue_depth: u32,
    pub random: bool,
}

/// What to test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedTestRequest {
    /// "C:".
    pub volume: String,
    pub size_bytes: u64,
    pub runs: u32,
    pub measure: Duration,
    pub pause: Duration,
    /// Folder the test folder is created in instead of the volume root (tests, diagnostics).
    pub place: Option<PathBuf>,
    /// Folder of the results history instead of the storage data folder.
    pub history_dir: Option<PathBuf>,
}

impl SpeedTestRequest {
    /// Validates the volume ("c", "C:", "c:\"), the size (a whole number of MiB from
    /// [`MIN_SIZE`] to [`MAX_SIZE`]) and the runs (1 to [`MAX_RUNS`]).
    pub fn new(volume: &str, size_bytes: u64, runs: u32) -> Result<SpeedTestRequest> {
        let volume = normalize_volume(volume)?;
        if size_bytes % MIB != 0 || !(MIN_SIZE..=MAX_SIZE).contains(&size_bytes) {
            return Err(Error::Other(format!(
                "the test size must be a whole number of MB from 1 MB to 64 GB, got {size_bytes} bytes"
            )));
        }
        if !(1..=MAX_RUNS).contains(&runs) {
            return Err(Error::Other(format!(
                "the number of runs must be 1 to {MAX_RUNS}, got {runs}"
            )));
        }
        Ok(SpeedTestRequest {
            volume,
            size_bytes,
            runs,
            measure: MEASURE_TIME,
            pause: PAUSE,
            place: None,
            history_dir: None,
        })
    }

    /// Creates the test folder inside `dir` instead of the volume root; leftovers are looked
    /// for and removed only there.
    pub fn with_place(self, dir: impl Into<PathBuf>) -> SpeedTestRequest {
        SpeedTestRequest {
            place: Some(dir.into()),
            ..self
        }
    }

    pub fn with_timing(self, measure: Duration, pause: Duration) -> SpeedTestRequest {
        SpeedTestRequest {
            measure,
            pause,
            ..self
        }
    }

    /// Keeps the results history in `dir` instead of the storage data folder.
    pub fn with_history_dir(self, dir: impl Into<PathBuf>) -> SpeedTestRequest {
        SpeedTestRequest {
            history_dir: Some(dir.into()),
            ..self
        }
    }

    /// Bytes a test may write: the test file, then up to one pass in each write measurement.
    pub fn max_write_bytes(&self) -> u64 {
        self.size_bytes * (1 + 4 * u64::from(self.runs))
    }
}

/// What a speed test would do, and why it cannot start now, if it cannot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeedTestPlan {
    pub volume: String,
    pub size_bytes: u64,
    pub runs: u32,
    pub tests: Vec<TestSpec>,
    /// size × (1 + 4 × runs).
    pub max_write_bytes: u64,
    pub estimated_seconds: u64,
    /// "C:\CairnSpeedTest-…".
    pub folder_pattern: String,
    pub requires_admin: bool,
    pub media: MediaKind,
    /// Test folders left on this volume (or in the test's place); removed before the test.
    pub leftovers: Vec<Leftover>,
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

/// The best run of one test in one direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub test: SpeedTestId,
    pub label: String,
    pub direction: Direction,
    pub block_bytes: u32,
    pub queue_depth: u32,
    pub threads: u32,
    pub mb_s: f64,
    pub iops: f64,
    pub latency_us: f64,
    /// Runs completed.
    pub runs: u32,
    pub duration_ms: u64,
    pub bytes: u64,
}

/// A finished, stopped or failed speed test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedTestResult {
    pub volume: String,
    pub label: String,
    pub model: Option<String>,
    pub bus: Option<String>,
    pub media: MediaKind,
    pub file_system: String,
    pub size_bytes: u64,
    pub runs: u32,
    pub started_at: String,
    pub finished_at: String,
    pub elapsed_ms: u64,
    /// Every measurement ran.
    pub completed: bool,
    /// Why a failed test ended early; `None` for a finished or stopped test (and left out of
    /// the JSON then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub measurements: Vec<Measurement>,
    pub skipped: Vec<SpeedTestId>,
    pub bytes_written: u64,
    pub notes: Vec<String>,
    pub app_version: String,
    /// The result is in the history of earlier results.
    #[serde(default)]
    pub history_saved: bool,
}

impl SpeedTestResult {
    /// How a test that did not finish ended, as lists of earlier results show it: "failed"
    /// when an error ended it early, "stopped" when it was stopped; `None` for a finished test.
    pub fn end_word(&self) -> Option<&'static str> {
        if self.error.is_some() {
            Some("failed")
        } else if !self.completed {
            Some("stopped")
        } else {
            None
        }
    }
}

/// Live state of a running speed test (the `speed` block of the job's detail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeedProgress {
    pub test: Option<SpeedTestId>,
    pub direction: Option<Direction>,
    pub run: u32,
    pub runs: u32,
    pub step: u32,
    /// 1 + 8 × runs: preparing, then every run of every measurement.
    pub steps: u32,
    pub live_mb_s: Option<f64>,
    pub bytes_written: u64,
    /// Best result so far of each measurement that ran.
    pub done: Vec<Measurement>,
}

/// A maintenance tool that is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningTool {
    pub title: String,
    pub volume: Option<String>,
    /// Optimize Drives, Retrim or Check Disk.
    pub drive_tool: bool,
}

/// Everything the speed test reads from the system besides the drive it tests.
#[derive(Clone, Copy)]
pub struct SpeedEnv {
    pub elevated: fn() -> bool,
    pub volumes: fn() -> Result<Vec<StorageVolume>>,
    /// Bytes available to the caller on the volume of the path.
    pub free_bytes: fn(&Path) -> Result<u64>,
    pub on_battery: fn() -> Option<bool>,
    pub running_tool: fn() -> Option<RunningTool>,
    /// Lowercase executable names of the running processes; `None` when unreadable.
    pub processes: fn() -> Option<HashSet<String>>,
    /// Give the test folder the admin-only DACL on volumes that keep ACLs.
    pub protect_folder: bool,
}

impl fmt::Debug for SpeedEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeedEnv")
            .field("protect_folder", &self.protect_folder)
            .finish_non_exhaustive()
    }
}

impl SpeedEnv {
    /// This PC.
    pub const SYSTEM: SpeedEnv = SpeedEnv {
        elevated: crate::is_elevated,
        volumes: volumes::volumes,
        free_bytes: volumes::free_bytes,
        on_battery: system_on_battery,
        running_tool: system_running_tool,
        processes: crate::win::process::running_process_names,
        protect_folder: true,
    };
}

fn system_on_battery() -> Option<bool> {
    crate::win::power::power_source().on_battery
}

fn system_running_tool() -> Option<RunningTool> {
    let job = crate::tools::runner().running()?;
    Some(RunningTool {
        title: job.title,
        volume: job.volume,
        drive_tool: matches!(
            job.tool,
            ToolId::DriveOptimize | ToolId::DriveRetrim | ToolId::DiskCheck
        ),
    })
}

// ───────────────────────────── Plan ─────────────────────────────

/// Why a start is refused; `NotElevated` becomes [`Error::NotElevated`].
enum Block {
    NotElevated,
    Other(String),
}

/// A plan with what its start needs.
struct Planned {
    plan: SpeedTestPlan,
    block: Option<Block>,
    volume: Option<StorageVolume>,
    base: PathBuf,
}

fn plan(host: &JobHost, env: &SpeedEnv, request: &SpeedTestRequest) -> Result<Planned> {
    let letter = request.volume.clone();
    let base = request.place.clone().unwrap_or_else(|| root_of(&letter));
    let listed = (env.volumes)()?;
    let volume = listed
        .into_iter()
        .find(|v| v.letter.eq_ignore_ascii_case(&letter));
    let leftovers = match (&request.place, &volume) {
        (Some(place), _) => find_leftovers(place),
        (None, Some(v)) => v.leftovers.clone(),
        (None, None) => Vec::new(),
    };
    let measure = request.measure.as_secs_f64();
    let pause = request.pause.as_secs_f64();
    let media = volume.as_ref().map_or(MediaKind::Unknown, |v| v.media);
    let prepare_rate = if media == MediaKind::Hdd {
        100e6
    } else {
        400e6
    };
    let estimated = 8.0 * f64::from(request.runs) * (measure + pause)
        + request.size_bytes as f64 / prepare_rate;
    let mut plan = SpeedTestPlan {
        volume: letter.clone(),
        size_bytes: request.size_bytes,
        runs: request.runs,
        tests: SpeedTestId::ALL
            .iter()
            .map(|&id| TestSpec {
                id,
                label: id.label(),
                block_bytes: id.block_bytes(),
                queue_depth: id.queue_depth(),
                random: id.random(),
            })
            .collect(),
        max_write_bytes: request.max_write_bytes(),
        estimated_seconds: estimated.ceil() as u64,
        folder_pattern: base.join(format!("{FOLDER_PREFIX}…")).display().to_string(),
        requires_admin: true,
        media,
        leftovers,
        blocked_reason: None,
        notes: Vec::new(),
    };

    let running_tool = (env.running_tool)();
    let block = first_block(
        host,
        env,
        request,
        volume.as_ref(),
        &plan,
        running_tool.as_ref(),
        &base,
    );
    plan.blocked_reason = block.as_ref().map(|b| match b {
        Block::NotElevated => NOT_ELEVATED_TEXT.to_string(),
        Block::Other(text) => text.clone(),
    });

    if (env.on_battery)() == Some(true) {
        plan.notes.push(
            "The PC is on battery power; Windows may slow the drive to save energy, so results can be lower."
                .to_string(),
        );
    }
    if volume.as_ref().is_some_and(|v| v.system) {
        plan.notes.push(format!(
            "Other programs use {letter} while the test runs, which can lower the results."
        ));
    }
    if media == MediaKind::Hdd {
        plan.notes.push(
            "On a hard disk a small test file shows better random speeds than the whole disk would."
                .to_string(),
        );
    }
    if let Some(tool) = running_tool.as_ref().filter(|t| !drive_tool_on(t, &letter)) {
        plan.notes.push(format!(
            "{} is running, so results can be lower.",
            tool.title
        ));
    }
    if let Some(names) = (env.processes)() {
        let found: Vec<&str> = MAINTENANCE_PROCESSES
            .iter()
            .copied()
            .filter(|p| names.contains(*p))
            .collect();
        if !found.is_empty() {
            plan.notes.push(format!(
                "Windows maintenance is running ({}), so results can be lower.",
                found.join(", ")
            ));
        }
    }
    for leftover in plan.leftovers.iter().filter(|l| !l.in_use) {
        plan.notes.push(format!(
            "An earlier test file on {letter} ({}) is removed first.",
            leftover
                .bytes
                .map_or_else(|| "size unknown".to_string(), size_text)
        ));
    }
    Ok(Planned {
        plan,
        block,
        volume,
        base,
    })
}

fn drive_tool_on(tool: &RunningTool, letter: &str) -> bool {
    tool.drive_tool
        && tool
            .volume
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case(letter))
}

/// The first reason the test cannot start, in the documented order.
fn first_block(
    host: &JobHost,
    env: &SpeedEnv,
    request: &SpeedTestRequest,
    volume: Option<&StorageVolume>,
    plan: &SpeedTestPlan,
    running_tool: Option<&RunningTool>,
    base: &Path,
) -> Option<Block> {
    let letter = &request.volume;
    let Some(volume) = volume else {
        return Some(Block::Other(format!(
            "{letter} is not a fixed drive on this PC"
        )));
    };
    if let Some(error) = &volume.error {
        return Some(Block::Other(error.clone()));
    }
    if volume.not_responding {
        return Some(Block::Other(format!("{letter} isn't responding")));
    }
    if volume.read_only {
        return Some(Block::Other(format!("{letter} is read-only")));
    }
    if !(env.elevated)() {
        return Some(Block::NotElevated);
    }
    if request.place.is_none() && drive_tests_forbidden() {
        return Some(Block::Other(FORBIDDEN_TEXT.to_string()));
    }
    if let Some(job) = host.running() {
        return Some(Block::Other(format!(
            "{} is running; wait for it to finish or stop it.",
            job.title
        )));
    }
    if plan.leftovers.iter().any(|l| l.in_use) {
        return Some(Block::Other(format!(
            "Another Cairn window is testing {letter}."
        )));
    }
    if let Some(tool) = running_tool.filter(|t| drive_tool_on(t, letter)) {
        return Some(Block::Other(format!(
            "{} is running on {letter}; wait for it to finish.",
            tool.title
        )));
    }
    if volume.file_system.eq_ignore_ascii_case("FAT32") && request.size_bytes >= 4 * GIB {
        return Some(Block::Other(
            "FAT32 can't hold a file of 4 GB or more; choose a smaller test size.".to_string(),
        ));
    }
    let reserve = volume
        .size_bytes
        .map_or(2 * GIB, |size| (size / 20).clamp(2 * GIB, 20 * GIB));
    match (env.free_bytes)(base) {
        Ok(free) if free < request.size_bytes.saturating_add(reserve) => {
            Some(Block::Other(format!(
                "{letter} has {} free; the test needs {} plus {} kept free.",
                size_text(free),
                size_text(request.size_bytes),
                size_text(reserve)
            )))
        }
        Ok(_) => None,
        Err(e) => Some(Block::Other(format!(
            "The free space on {letter} can't be read: {e}"
        ))),
    }
}

// ───────────────────────────── Start ─────────────────────────────

/// Sees each phase of a test as it begins, with the job's stop flag.
pub(crate) type PhaseHook = Box<dyn Fn(&str, &AtomicBool) + Send>;

/// Sees each run of a measurement before it starts; `Some(error)` ends the run with that
/// error instead of measuring.
pub(crate) type MeasureHook = dyn Fn(SpeedTestId, Direction) -> Option<String> + Send;

/// Test seams of the work.
#[derive(Default)]
pub(crate) struct Hooks {
    /// Runs on the job's thread just before the test folder is created.
    pub before_folder: Option<Box<dyn FnOnce() + Send>>,
    /// Sees each phase as it begins, with the job's stop flag.
    pub on_phase: Option<PhaseHook>,
    /// Sees each run of a measurement before it starts and can fail it.
    pub before_measure: Option<Box<MeasureHook>>,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

/// Plans the test on `request.volume`, and unless `dry_run` starts it as a job of `host`.
///
/// A start refuses a blocked plan (not elevated: [`Error::NotElevated`]) before anything is
/// opened; then the host writes the `disk_speed_test` "started" row (calling
/// `open_journal`) and runs the test on its own thread. A dry run never calls `open_journal`
/// and creates nothing.
pub fn plan_or_start_speed_test(
    host: &JobHost,
    env: &SpeedEnv,
    request: &SpeedTestRequest,
    dry_run: bool,
    open_journal: impl FnOnce() -> Result<Arc<Journal>>,
) -> Result<(SpeedTestPlan, Option<HostJobSnapshot>)> {
    plan_or_start_with(host, env, request, dry_run, open_journal, Hooks::default())
}

pub(crate) fn plan_or_start_with(
    host: &JobHost,
    env: &SpeedEnv,
    request: &SpeedTestRequest,
    dry_run: bool,
    open_journal: impl FnOnce() -> Result<Arc<Journal>>,
    hooks: Hooks,
) -> Result<(SpeedTestPlan, Option<HostJobSnapshot>)> {
    let planned = plan(host, env, request)?;
    if dry_run {
        return Ok((planned.plan, None));
    }
    match planned.block {
        Some(Block::NotElevated) => return Err(Error::NotElevated),
        Some(Block::Other(reason)) => return Err(Error::Other(reason)),
        None => {}
    }
    let Some(volume) = planned.volume else {
        return Err(Error::Other(format!(
            "{} is not a fixed drive on this PC",
            request.volume
        )));
    };
    let plan = planned.plan;
    let hex = random_hex()?;
    let folder = planned.base.join(format!("{FOLDER_PREFIX}{hex}"));
    let letter = request.volume.clone();
    let started_detail = format!(
        "{} test file, {} run{}, writes up to {}; folder {}",
        size_text(request.size_bytes),
        request.runs,
        if request.runs == 1 { "" } else { "s" },
        size_text(plan.max_write_bytes),
        folder.display()
    );
    let spec = JobSpec {
        kind: KIND_SPEED_TEST,
        title: speed_title(&letter),
        command_line: format!(
            "optctl storage speed {letter} --size {}M --runs {}",
            request.size_bytes / MIB,
            request.runs
        ),
        cancellable: true,
        audit: Some(AuditSpec {
            op: OP,
            target: letter.clone(),
            started_detail,
        }),
        needs_journal: false,
        log: false,
    };
    let work = SpeedWork {
        env: *env,
        request: request.clone(),
        volume,
        base: planned.base,
        hex,
        leftovers: plan
            .leftovers
            .iter()
            .filter(|l| !l.in_use)
            .cloned()
            .collect(),
        history_dir: request.history_dir.clone().unwrap_or_else(super::data_dir),
        notes: plan.notes.clone(),
        hooks,
    };
    let job = host.start(spec, open_journal, Box::new(move |ctx| work.run(ctx)))?;
    Ok((plan, Some(job)))
}

// ───────────────────────────── Work ─────────────────────────────

struct SpeedWork {
    env: SpeedEnv,
    request: SpeedTestRequest,
    volume: StorageVolume,
    base: PathBuf,
    hex: String,
    leftovers: Vec<Leftover>,
    history_dir: PathBuf,
    notes: Vec<String>,
    hooks: Hooks,
}

/// Publishes the phase, the progress block and the percent of a running test.
struct Tracker<'a> {
    ctx: &'a JobContext,
    on_phase: Option<PhaseHook>,
    phase: &'static str,
    progress: SpeedProgress,
    /// Share of the current step that is done, 0 to 1.
    within: f64,
    last_live: Instant,
}

impl Tracker<'_> {
    fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
        if let Some(hook) = &self.on_phase {
            hook(phase, &self.ctx.cancel_flag());
        }
        self.publish();
    }

    fn publish(&mut self) {
        let steps = f64::from(self.progress.steps.max(1));
        let done = f64::from(self.progress.step.saturating_sub(1)) + self.within.clamp(0.0, 1.0);
        let percent = (done / steps * 100.0).min(100.0);
        let line = self.line(percent);
        self.ctx.progress(Some(percent), Some(&line));
        self.publish_detail();
    }

    fn publish_detail(&self) {
        self.ctx.set_detail(detail(
            self.phase,
            serde_json::to_value(&self.progress).ok(),
            None,
            None,
        ));
    }

    /// The test ended, whichever way: the phase becomes "done" with the last progress block;
    /// the percent and the progress line keep what the last phase showed.
    fn end(&mut self) {
        self.phase = "done";
        self.publish_detail();
    }

    /// Publishes at most every 200 ms.
    fn publish_live(&mut self) {
        if self.last_live.elapsed() >= LIVE_PERIOD {
            self.last_live = Instant::now();
            self.publish();
        }
    }

    fn line(&self, percent: f64) -> String {
        match self.phase {
            "preparing" => format!("Preparing the test file…  {percent:.0} %"),
            "pausing" => "Pausing between measurements…".to_string(),
            "cleaning_up" => "Deleting the test file…".to_string(),
            "saving" => "Saving the result…".to_string(),
            _ => {
                let mut line = match (self.progress.test, self.progress.direction) {
                    (Some(test), Some(direction)) => format!(
                        "{}  ·  {}  ·  run {} of {}",
                        test.label(),
                        direction.word(),
                        self.progress.run,
                        self.progress.runs
                    ),
                    _ => "Measuring…".to_string(),
                };
                if let Some(mb_s) = self.progress.live_mb_s {
                    line.push_str(&format!("  ·  {mb_s:.1} MB/s"));
                }
                line
            }
        }
    }
}

/// What the measurements produced.
#[derive(Default)]
struct Measured {
    measurements: Vec<Measurement>,
    skipped: Vec<SpeedTestId>,
    bytes_written: u64,
    error: Option<String>,
}

impl SpeedWork {
    fn run(self, ctx: &JobContext) -> WorkEnd {
        let SpeedWork {
            env,
            request,
            volume,
            base,
            hex,
            leftovers,
            history_dir,
            mut notes,
            hooks,
        } = self;
        let started = Instant::now();
        let started_at = utc_now();
        let _awake = AwakeGuard::new();
        let _mode = ErrorModeGuard::new();
        ctx.set_shutdown_note(SHUTDOWN_NOTE);
        let letter = request.volume.clone();
        let mut tracker = Tracker {
            ctx,
            on_phase: hooks.on_phase,
            phase: "preparing",
            progress: SpeedProgress {
                test: None,
                direction: None,
                run: 0,
                runs: request.runs,
                step: 1,
                steps: 1 + 8 * request.runs,
                live_mb_s: None,
                bytes_written: 0,
                done: Vec::new(),
            },
            within: 0.0,
            last_live: Instant::now(),
        };
        tracker.publish();

        // Folders of earlier tests that did not finish, each with its own rows.
        if let Some(journal) = ctx.journal() {
            for leftover in &leftovers {
                let folder = Path::new(&leftover.path);
                match remove_leftover_in(&env, journal, folder, request.place.as_deref()) {
                    Ok(done) if !done.removed => notes.push(format!(
                        "The earlier test folder {} was not removed: {}",
                        leftover.path, done.detail
                    )),
                    Ok(_) => {}
                    Err(e) => notes.push(format!(
                        "The earlier test folder {} was not removed: {e}",
                        leftover.path
                    )),
                }
            }
        }

        if let Some(hook) = hooks.before_folder {
            hook();
        }
        let protect = env.protect_folder && volume.persistent_acls;
        let running = match create_test_folder(&base, &hex, protect) {
            Ok(running) => running,
            Err(e) => {
                let text = format!("the test folder can't be created: {e}");
                tracker.end();
                return WorkEnd {
                    state: JobState::Failed,
                    summary: text.clone(),
                    hint: None,
                    restart_required: false,
                    audit_detail: Some(format!("{text}; nothing was written")),
                };
            }
        };
        let folder = running.path().to_path_buf();

        let measured = measure_in(
            &folder,
            &request,
            ctx,
            &mut tracker,
            &mut notes,
            hooks.before_measure.as_deref(),
        );

        tracker.set_phase("cleaning_up");
        let file_left = file_still_there(&folder.join(FILE_NAME));
        if let Err(e) = remove_folder(&folder) {
            notes.push(format!(
                "The test folder {} couldn't be removed: {e}; Cairn removes it the next time the Storage page opens.",
                folder.display()
            ));
        }
        // A folder that could not be removed is a leftover from here on.
        drop(running);

        let cancelled = ctx.cancel_requested() && measured.error.is_none();
        let completed = !cancelled && measured.error.is_none();
        let mut result = SpeedTestResult {
            volume: letter.clone(),
            label: volume.label.clone(),
            model: volume.model.clone(),
            bus: volume.bus.clone(),
            media: volume.media,
            file_system: volume.file_system.clone(),
            size_bytes: request.size_bytes,
            runs: request.runs,
            started_at,
            finished_at: utc_now(),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            completed,
            error: measured.error.clone(),
            measurements: measured.measurements.clone(),
            skipped: measured.skipped.clone(),
            bytes_written: measured.bytes_written,
            notes: Vec::new(),
            app_version: crate::VERSION.to_string(),
            history_saved: false,
        };
        if completed || !result.measurements.is_empty() {
            tracker.set_phase("saving");
            result.notes = notes.clone();
            // The entry written to the history says that it is saved there.
            result.history_saved = true;
            if let Err(e) = history::append_history(&history_dir, &result) {
                result.history_saved = false;
                notes.push(format!(
                    "The result couldn't be saved to the earlier results: {e}"
                ));
            }
        }
        result.notes = notes;
        result.finished_at = utc_now();
        result.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut published = serde_json::to_value(&result).unwrap_or_else(|_| json!({}));
        if let Some(map) = published.as_object_mut() {
            map.insert("kind".to_string(), json!("speed"));
        }
        ctx.publish(published);
        tracker.end();

        let wrote = size_text(result.bytes_written);
        let deleted = match &file_left {
            None => "the test file was deleted".to_string(),
            Some(e) => format!("the test file could not be deleted: {e}"),
        };
        if let Some(error) = measured.error {
            return WorkEnd {
                state: JobState::Failed,
                summary: error.clone(),
                hint: None,
                restart_required: false,
                audit_detail: Some(format!("{error}; {deleted}")),
            };
        }
        if cancelled {
            let reason = if ctx.closing() {
                "stopped because Cairn was closing"
            } else {
                "stopped"
            };
            return WorkEnd {
                state: JobState::Cancelled,
                summary: format!(
                    "Stopped after {}; {deleted}.",
                    fmt_duration(started.elapsed())
                ),
                hint: None,
                restart_required: false,
                audit_detail: Some(format!("{reason}; wrote {wrote}; {deleted}")),
            };
        }
        let detail_text = result_detail(&result);
        WorkEnd {
            state: JobState::Succeeded,
            summary: detail_text.clone(),
            hint: None,
            restart_required: false,
            audit_detail: Some(detail_text),
        }
    }
}

/// "SEQ1M Q8T1 7012/6345 MB/s · … · wrote 9.4 GB" (the host adds the duration).
pub(crate) fn result_detail(result: &SpeedTestResult) -> String {
    let mut parts: Vec<String> = SpeedTestId::ALL
        .iter()
        .map(|&id| {
            if result.skipped.contains(&id) {
                return format!("{} skipped", id.label());
            }
            let speed = |direction: Direction| {
                result
                    .measurements
                    .iter()
                    .find(|m| m.test == id && m.direction == direction)
                    .map_or_else(|| "–".to_string(), |m| format!("{:.0}", m.mb_s))
            };
            format!(
                "{} {}/{} MB/s",
                id.label(),
                speed(Direction::Read),
                speed(Direction::Write)
            )
        })
        .collect();
    parts.push(format!("wrote {}", size_text(result.bytes_written)));
    parts.join(" · ")
}

/// Creates the test file in `folder`, writes it once and runs every measurement. The file is
/// closed, and therefore deleted, before this returns.
fn measure_in(
    folder: &Path,
    request: &SpeedTestRequest,
    ctx: &JobContext,
    tracker: &mut Tracker<'_>,
    notes: &mut Vec<String>,
    before_measure: Option<&MeasureHook>,
) -> Measured {
    let mut measured = Measured::default();
    let letter = &request.volume;
    let file = match create_test_file(&folder.join(FILE_NAME), request.size_bytes) {
        Ok(file) => file,
        Err(e) => {
            measured.error = Some(if e.win32_code() == Some(112) {
                format!("{letter} ran out of space while the test file was written")
            } else {
                format!("the test file can't be created: {e}")
            });
            return measured;
        }
    };
    let (attrs, sector) = match file_facts(&file) {
        Ok(facts) => facts,
        Err(e) => {
            measured.error = Some(format!("the test file can't be read: {e}"));
            return measured;
        }
    };
    if attrs & (ATTR_COMPRESSED | ATTR_SPARSE) != 0 {
        measured.error = Some(format!(
            "The test file can't be created uncompressed on {letter}."
        ));
        return measured;
    }
    if attrs & ATTR_ENCRYPTED != 0 {
        notes.push("The folder is encrypted with EFS; results include the encryption.".to_string());
    }
    if attrs & ATTR_INTEGRITY_STREAM != 0 {
        notes.push("Integrity streams are on here; writes include checksums.".to_string());
    }
    if sector > 4096 {
        measured.skipped = vec![SpeedTestId::Rnd4kQ32t1, SpeedTestId::Rnd4kQ1t1];
        notes.push(
            "The drive's sectors are larger than 4 KB, so the 4 KB tests can't run unbuffered."
                .to_string(),
        );
    }
    let align = sector.max(4096) as usize;
    let pool_len = request.size_bytes.clamp(POOL_MIN, POOL_MAX) as usize;
    let pool = match AlignedBuf::random(pool_len, align) {
        Ok(pool) => Arc::new(pool),
        Err(e) => {
            measured.error = Some(format!("the test data can't be prepared: {e}"));
            return measured;
        }
    };
    let mut rng = match Rng::from_system() {
        Ok(rng) => rng,
        Err(e) => {
            measured.error = Some(format!("the test data can't be prepared: {e}"));
            return measured;
        }
    };
    let mut target = match OverlappedTarget::new(file, SLOTS, READ_BUFFER, align, pool) {
        Ok(target) => target,
        Err(e) => {
            measured.error = Some(format!("the test file can't be opened for testing: {e}"));
            return measured;
        }
    };
    let cancel = ctx.cancel_flag();
    let size = request.size_bytes;

    tracker.set_phase("preparing");
    let prepared = {
        let mut report = |written: u64| {
            tracker.within = written as f64 / size as f64;
            tracker.progress.bytes_written = written;
            tracker.publish_live();
        };
        prepare(
            &mut target,
            size,
            &mut rng,
            pool_len,
            sector,
            &cancel,
            &mut report,
        )
    };
    match prepared {
        Ok(written) => {
            measured.bytes_written = written;
            tracker.progress.bytes_written = written;
        }
        Err(e) => {
            measured.error = Some(write_error(letter, &e));
            return measured;
        }
    }

    let mut step = 1u32;
    let mut first = true;
    let mut short_write_noted = false;
    'tests: for direction in [Direction::Read, Direction::Write] {
        for id in SpeedTestId::ALL {
            if measured.skipped.contains(&id) {
                step += request.runs;
                continue;
            }
            let mut best: Option<RunStats> = None;
            let mut runs_done = 0u32;
            // A stop ends the runs; the best of the runs that finished is still kept.
            let mut stopped = false;
            for run in 1..=request.runs {
                step += 1;
                if cancel.load(Ordering::SeqCst) {
                    stopped = true;
                    break;
                }
                if !first && !request.pause.is_zero() {
                    tracker.set_phase("pausing");
                    pause(request.pause, &cancel);
                    if cancel.load(Ordering::SeqCst) {
                        stopped = true;
                        break;
                    }
                }
                first = false;
                tracker.progress.test = Some(id);
                tracker.progress.direction = Some(direction);
                tracker.progress.run = run;
                tracker.progress.step = step;
                tracker.progress.live_mb_s = None;
                tracker.within = 0.0;
                tracker.set_phase("measuring");
                let spec = MeasureSpec {
                    op: match direction {
                        Direction::Read => IoOp::Read,
                        Direction::Write => IoOp::Write,
                    },
                    block: id.block_bytes(),
                    qd: id.queue_depth(),
                    random: id.random(),
                    file_len: size,
                    duration: request.measure,
                    byte_limit: (direction == Direction::Write).then_some(size),
                    align: sector,
                };
                let measure_time = request.measure.as_secs_f64().max(0.001);
                let base_written = measured.bytes_written;
                let stats = match before_measure.and_then(|hook| hook(id, direction)) {
                    Some(error) => Err(Error::Other(error)),
                    None => {
                        let mut live = |s: &RunStats| {
                            tracker.within = (s.elapsed.as_secs_f64() / measure_time).min(1.0);
                            tracker.progress.live_mb_s = Some(s.mb_s());
                            if direction == Direction::Write {
                                tracker.progress.bytes_written = base_written + s.bytes;
                            }
                            tracker.publish_live();
                        };
                        measure(&mut target, &spec, &mut rng, pool_len, &cancel, &mut live)
                    }
                };
                let stats = match stats {
                    Ok(stats) => stats,
                    Err(e) => {
                        measured.error = Some(match direction {
                            Direction::Write => write_error(letter, &e),
                            Direction::Read => e.to_string(),
                        });
                        return measured;
                    }
                };
                if direction == Direction::Write {
                    measured.bytes_written += stats.bytes;
                    tracker.progress.bytes_written = measured.bytes_written;
                }
                if cancel.load(Ordering::SeqCst) {
                    stopped = true;
                    break;
                }
                if direction == Direction::Write
                    && !id.random()
                    && stats.elapsed < SHORT_WRITE
                    && !short_write_noted
                {
                    short_write_noted = true;
                    notes.push(
                        "Sequential writes finished in under half a second; a larger test size gives steadier write results on this drive."
                            .to_string(),
                    );
                }
                runs_done += 1;
                if best.map_or(true, |b| stats.mb_s() > b.mb_s()) {
                    best = Some(stats);
                }
                if let Some(best) = best {
                    let measurement = to_measurement(id, direction, &best, runs_done);
                    upsert(&mut tracker.progress.done, measurement);
                    tracker.publish();
                }
            }
            if let Some(best) = best {
                measured
                    .measurements
                    .push(to_measurement(id, direction, &best, runs_done));
            }
            if stopped {
                break 'tests;
            }
        }
    }
    drop(target);
    measured
}

fn upsert(done: &mut Vec<Measurement>, measurement: Measurement) {
    match done
        .iter_mut()
        .find(|m| m.test == measurement.test && m.direction == measurement.direction)
    {
        Some(existing) => *existing = measurement,
        None => done.push(measurement),
    }
}

fn to_measurement(
    id: SpeedTestId,
    direction: Direction,
    best: &RunStats,
    runs: u32,
) -> Measurement {
    Measurement {
        test: id,
        label: id.label().to_string(),
        direction,
        block_bytes: id.block_bytes(),
        queue_depth: id.queue_depth(),
        threads: 1,
        mb_s: best.mb_s(),
        iops: best.iops(),
        latency_us: best.latency_us(),
        runs,
        duration_ms: u64::try_from(best.elapsed.as_millis()).unwrap_or(u64::MAX),
        bytes: best.bytes,
    }
}

fn write_error(letter: &str, e: &Error) -> String {
    let text = e.to_string();
    if e.win32_code() == Some(112) || text.contains(DISK_FULL_MARK) {
        format!("{letter} ran out of space while the test file was written")
    } else {
        text
    }
}

/// Sleeps `length` in short slices, returning early after a stop.
fn pause(length: Duration, cancel: &AtomicBool) {
    let end = Instant::now() + length;
    while !cancel.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        thread::sleep((end - now).min(Duration::from_millis(20)));
    }
}

/// Creates the test file: exclusive, unbuffered, written through, overlapped and deleted on
/// close; its space is allocated first (a full disk fails here) and its length set.
pub(crate) fn create_test_file(path: &Path, size: u64) -> Result<OwnedHandle> {
    let wide = wide_path(path);
    let flags = FILE_FLAG_OVERLAPPED
        | FILE_FLAG_NO_BUFFERING
        | FILE_FLAG_WRITE_THROUGH
        | FILE_FLAG_DELETE_ON_CLOSE
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    // SAFETY: `wide` is NUL-terminated; no security attributes or template are passed.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0,
            FILE_SHARE_MODE(0),
            None,
            CREATE_NEW,
            flags,
            None,
        )
    }?;
    let file = OwnedHandle::new(handle);
    let size =
        i64::try_from(size).map_err(|_| Error::Other("the test size is too large".to_string()))?;
    let allocation = FILE_ALLOCATION_INFO {
        AllocationSize: size,
    };
    // SAFETY: `allocation` is a FILE_ALLOCATION_INFO that outlives the call; sizes match.
    unsafe {
        SetFileInformationByHandle(
            file.raw(),
            FileAllocationInfo,
            &allocation as *const FILE_ALLOCATION_INFO as *const c_void,
            size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    }?;
    let end = FILE_END_OF_FILE_INFO { EndOfFile: size };
    // SAFETY: `end` is a FILE_END_OF_FILE_INFO that outlives the call; sizes match.
    unsafe {
        SetFileInformationByHandle(
            file.raw(),
            FileEndOfFileInfo,
            &end as *const FILE_END_OF_FILE_INFO as *const c_void,
            size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    }?;
    Ok(file)
}

/// (attributes, logical sector size of at least 512 bytes) of the open test file.
fn file_facts(file: &OwnedHandle) -> Result<(u32, u32)> {
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: `basic` is a FILE_BASIC_INFO that outlives the call; the size matches it.
    unsafe {
        GetFileInformationByHandleEx(
            file.raw(),
            FileBasicInfo,
            &mut basic as *mut FILE_BASIC_INFO as *mut c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }?;
    let mut storage = FILE_STORAGE_INFO::default();
    // SAFETY: `storage` is a FILE_STORAGE_INFO that outlives the call; the size matches it.
    let sector = match unsafe {
        GetFileInformationByHandleEx(
            file.raw(),
            FileStorageInfo,
            &mut storage as *mut FILE_STORAGE_INFO as *mut c_void,
            size_of::<FILE_STORAGE_INFO>() as u32,
        )
    } {
        Ok(()) => storage.LogicalBytesPerSector.max(512),
        Err(_) => 512,
    };
    Ok((basic.FileAttributes, sector))
}

/// `None` when the file is gone (or being deleted); otherwise why it is still there.
fn file_still_there(path: &Path) -> Option<String> {
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    (attrs != INVALID_FILE_ATTRIBUTES).then(|| format!("{} is still there", path.display()))
}

/// Removes the (empty) test folder, retrying while a file in it is still being deleted.
fn remove_folder(folder: &Path) -> Result<()> {
    let wide = wide_path(folder);
    let mut last = None;
    for attempt in 0..FOLDER_REMOVE_TRIES {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(200));
        }
        // SAFETY: `wide` is NUL-terminated.
        match unsafe { RemoveDirectoryW(PCWSTR(wide.as_ptr())) } {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(
        || Error::Other("the folder could not be removed".to_string()),
        Error::from,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::jobs::HostConfig;
    use crate::safety::state_log::OpLogEntry;
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    fn yes() -> bool {
        true
    }
    fn no() -> bool {
        false
    }
    fn none_bool() -> Option<bool> {
        None
    }
    fn no_tool() -> Option<RunningTool> {
        None
    }
    fn no_processes() -> Option<HashSet<String>> {
        None
    }
    fn plenty(_: &Path) -> Result<u64> {
        Ok(1 << 50)
    }

    /// The letter of the temporary folder's volume, where tests place their files.
    pub(crate) fn temp_letter() -> String {
        std::env::temp_dir().to_string_lossy()[..2].to_ascii_uppercase()
    }

    pub(crate) fn fixed_volume(letter: &str) -> StorageVolume {
        StorageVolume {
            letter: letter.to_string(),
            label: "Windows".to_string(),
            file_system: "NTFS".to_string(),
            size_bytes: Some(952 * GIB),
            free_bytes: Some(611 * GIB),
            media: MediaKind::Ssd,
            bus: Some("NVMe".to_string()),
            model: Some("Test NVMe SSD".to_string()),
            system: false,
            read_only: false,
            persistent_acls: true,
            not_responding: false,
            error: None,
            speed_test_blocked: None,
            scan_blocked: None,
            leftovers: Vec::new(),
        }
    }

    fn temp_volumes() -> Result<Vec<StorageVolume>> {
        Ok(vec![fixed_volume(&temp_letter())])
    }

    /// Elevated, the temporary folder's volume as the only fixed volume, nothing running,
    /// plenty of space, no DACL on the test folder.
    pub(crate) const TEST_ENV: SpeedEnv = SpeedEnv {
        elevated: yes,
        volumes: temp_volumes,
        free_bytes: plenty,
        on_battery: none_bool,
        running_tool: no_tool,
        processes: no_processes,
        protect_folder: false,
    };

    pub(crate) fn sample_result() -> SpeedTestResult {
        SpeedTestResult {
            volume: "C:".to_string(),
            label: "Windows".to_string(),
            model: Some("Test NVMe SSD".to_string()),
            bus: Some("NVMe".to_string()),
            media: MediaKind::Ssd,
            file_system: "NTFS".to_string(),
            size_bytes: GIB,
            runs: 3,
            started_at: "2026-09-28T12:00:00Z".to_string(),
            finished_at: "2026-09-28T12:02:41Z".to_string(),
            elapsed_ms: 161_000,
            completed: true,
            error: None,
            measurements: Vec::new(),
            skipped: Vec::new(),
            bytes_written: 13 * GIB,
            notes: Vec::new(),
            app_version: "0.2.0".to_string(),
            history_saved: true,
        }
    }

    pub(crate) fn test_host(dir: &Path) -> JobHost {
        JobHost::new(HostConfig {
            tick: Duration::from_millis(5),
            settle_wait: Duration::from_millis(500),
            stop_wait: Duration::from_millis(500),
            log_dir: dir.join("jobs"),
            ..crate::storage::lane_config()
        })
    }

    fn request(place: &Path) -> SpeedTestRequest {
        SpeedTestRequest::new(&temp_letter(), 4 * MIB, 1)
            .unwrap()
            .with_place(place)
            .with_timing(Duration::from_millis(30), Duration::ZERO)
            .with_history_dir(place.join("history"))
    }

    fn rows(journal: &Journal) -> Vec<OpLogEntry> {
        let mut rows = journal.ops(100).unwrap();
        rows.reverse();
        rows
    }

    fn never_open() -> Result<Arc<Journal>> {
        panic!("the journal must not be opened")
    }

    /// The keys of a JSON object, sorted.
    fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    fn blocked(env: &SpeedEnv, request: &SpeedTestRequest) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let (plan, job) = plan_or_start_speed_test(&host, env, request, true, never_open).unwrap();
        assert!(job.is_none());
        plan.blocked_reason
    }

    #[test]
    fn requests_are_validated() {
        let ok = SpeedTestRequest::new("c", GIB, 3).unwrap();
        assert_eq!(ok.volume, "C:");
        assert_eq!(ok.measure, MEASURE_TIME);
        assert_eq!(ok.max_write_bytes(), 13 * GIB);
        assert!(SpeedTestRequest::new("CC", GIB, 3).is_err());
        assert!(SpeedTestRequest::new("C:", GIB + 1, 3).is_err());
        assert!(SpeedTestRequest::new("C:", 0, 3).is_err());
        assert!(SpeedTestRequest::new("C:", 65 * GIB, 3).is_err());
        assert!(SpeedTestRequest::new("C:", MIB, 3).is_ok());
        assert!(SpeedTestRequest::new("C:", GIB, 0).is_err());
        assert!(SpeedTestRequest::new("C:", GIB, 10).is_err());
        assert_eq!(SpeedTestId::Rnd4kQ32t1.label(), "RND4K Q32T1");
        assert_eq!(SpeedTestId::Seq1mQ8t1.block_bytes(), 1 << 20);
        assert_eq!(SpeedTestId::Rnd4kQ1t1.queue_depth(), 1);
        assert_eq!(
            serde_json::to_value(SpeedTestId::Seq1mQ1t1).unwrap(),
            json!("seq1m_q1t1")
        );
    }

    #[test]
    fn the_plan_states_writes_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let request = SpeedTestRequest::new(&temp_letter(), GIB, 3)
            .unwrap()
            .with_place(dir.path());
        let (plan, job) =
            plan_or_start_speed_test(&host, &TEST_ENV, &request, true, never_open).unwrap();
        assert!(job.is_none());
        assert_eq!(plan.max_write_bytes, 13 * GIB);
        // 8 × 3 × (5 + 1) s, plus 1 GiB at 400 MB/s.
        assert_eq!(plan.estimated_seconds, 147);
        assert_eq!(plan.tests.len(), 4);
        assert!(plan.requires_admin);
        assert_eq!(plan.blocked_reason, None);
        assert!(plan.folder_pattern.ends_with("CairnSpeedTest-…"));
        let json = serde_json::to_value(&plan).unwrap();
        for key in [
            "volume",
            "size_bytes",
            "runs",
            "tests",
            "max_write_bytes",
            "estimated_seconds",
            "folder_pattern",
            "requires_admin",
            "media",
            "leftovers",
            "blocked_reason",
            "notes",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        // Nothing was created by the dry run.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    fn not_listed() -> Result<Vec<StorageVolume>> {
        Ok(Vec::new())
    }
    fn errored() -> Result<Vec<StorageVolume>> {
        let mut v = fixed_volume(&temp_letter());
        v.error = Some("cannot read the drive: locked".to_string());
        v.not_responding = true;
        Ok(vec![v])
    }
    fn hung() -> Result<Vec<StorageVolume>> {
        let mut v = fixed_volume(&temp_letter());
        v.not_responding = true;
        v.read_only = true;
        Ok(vec![v])
    }
    fn read_only() -> Result<Vec<StorageVolume>> {
        let mut v = fixed_volume(&temp_letter());
        v.read_only = true;
        Ok(vec![v])
    }
    fn fat32() -> Result<Vec<StorageVolume>> {
        let mut v = fixed_volume(&temp_letter());
        v.file_system = "FAT32".to_string();
        Ok(vec![v])
    }
    fn hdd_system() -> Result<Vec<StorageVolume>> {
        let mut v = fixed_volume(&temp_letter());
        v.media = MediaKind::Hdd;
        v.system = true;
        Ok(vec![v])
    }
    fn drive_tool() -> Option<RunningTool> {
        Some(RunningTool {
            title: "Optimize Drives".to_string(),
            volume: Some(temp_letter()),
            drive_tool: true,
        })
    }
    fn other_tool() -> Option<RunningTool> {
        Some(RunningTool {
            title: "System File Checker".to_string(),
            volume: None,
            drive_tool: false,
        })
    }
    fn small_free(_: &Path) -> Result<u64> {
        Ok(3 * GIB)
    }
    fn on_battery() -> Option<bool> {
        Some(true)
    }
    fn maintenance() -> Option<HashSet<String>> {
        Some(["tiworker.exe".to_string(), "explorer.exe".to_string()].into())
    }

    #[test]
    fn blocks_come_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let letter = temp_letter();
        let placed = SpeedTestRequest::new(&letter, GIB, 3)
            .unwrap()
            .with_place(dir.path());
        let env = |f: fn(&mut SpeedEnv)| {
            let mut env = TEST_ENV;
            f(&mut env);
            env
        };
        assert_eq!(
            blocked(&env(|e| e.volumes = not_listed), &placed).unwrap(),
            format!("{letter} is not a fixed drive on this PC")
        );
        assert_eq!(
            blocked(&env(|e| e.volumes = errored), &placed).unwrap(),
            "cannot read the drive: locked"
        );
        assert_eq!(
            blocked(&env(|e| e.volumes = hung), &placed).unwrap(),
            format!("{letter} isn't responding")
        );
        assert_eq!(
            blocked(&env(|e| e.volumes = read_only), &placed).unwrap(),
            format!("{letter} is read-only")
        );
        assert_eq!(
            blocked(&env(|e| e.elevated = no), &placed).unwrap(),
            NOT_ELEVATED_TEXT
        );
        // Unit tests always forbid a test at the volume root.
        let at_root = SpeedTestRequest::new(&letter, GIB, 3).unwrap();
        assert_eq!(blocked(&TEST_ENV, &at_root).unwrap(), FORBIDDEN_TEXT);
        assert_eq!(
            blocked(&env(|e| e.running_tool = drive_tool), &placed).unwrap(),
            format!("Optimize Drives is running on {letter}; wait for it to finish.")
        );
        let big = SpeedTestRequest::new(&letter, 4 * GIB, 1)
            .unwrap()
            .with_place(dir.path());
        assert_eq!(
            blocked(&env(|e| e.volumes = fat32), &big).unwrap(),
            "FAT32 can't hold a file of 4 GB or more; choose a smaller test size."
        );
        // 952 GiB / 20 = 47.6 GiB, kept at 20 GiB.
        assert_eq!(
            blocked(&env(|e| e.free_bytes = small_free), &placed).unwrap(),
            format!("{letter} has 3 GB free; the test needs 1 GB plus 20 GB kept free.")
        );
        assert_eq!(blocked(&TEST_ENV, &placed), None);
    }

    #[test]
    fn a_running_job_and_a_leftover_in_use_block() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        host.start(
            JobSpec {
                kind: KIND_SPEED_TEST,
                title: "Speed test of Z:".to_string(),
                command_line: String::new(),
                cancellable: true,
                audit: None,
                needs_journal: false,
                log: false,
            },
            never_open,
            Box::new(move |_| {
                let _ = wait.recv();
                WorkEnd {
                    state: JobState::Succeeded,
                    summary: String::new(),
                    hint: None,
                    restart_required: false,
                    audit_detail: None,
                }
            }),
        )
        .unwrap();
        let request = request(dir.path());
        let (plan, _) =
            plan_or_start_speed_test(&host, &TEST_ENV, &request, true, never_open).unwrap();
        assert_eq!(
            plan.blocked_reason.as_deref(),
            Some("Speed test of Z: is running; wait for it to finish or stop it.")
        );
        release.send(()).unwrap();

        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let live = place.join("CairnSpeedTest-aaaa1111");
        std::fs::create_dir(&live).unwrap();
        let _handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(0x0400_0000)
            .open(live.join(FILE_NAME))
            .unwrap();
        let fresh = test_host(dir.path());
        let (plan, _) = plan_or_start_speed_test(
            &fresh,
            &TEST_ENV,
            &request.clone().with_place(&place),
            true,
            never_open,
        )
        .unwrap();
        assert_eq!(
            plan.blocked_reason,
            Some(format!(
                "Another Cairn window is testing {}.",
                temp_letter()
            ))
        );
    }

    #[test]
    fn notes_say_what_lowers_the_results() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        std::fs::create_dir(dir.path().join("CairnSpeedTest-0123abcd")).unwrap();
        std::fs::write(
            dir.path().join("CairnSpeedTest-0123abcd").join(FILE_NAME),
            vec![0u8; 2048],
        )
        .unwrap();
        let env = SpeedEnv {
            volumes: hdd_system,
            on_battery,
            running_tool: other_tool,
            processes: maintenance,
            ..TEST_ENV
        };
        let letter = temp_letter();
        let (plan, _) =
            plan_or_start_speed_test(&host, &env, &request(dir.path()), true, never_open).unwrap();
        assert_eq!(plan.blocked_reason, None);
        assert_eq!(
            plan.notes,
            vec![
                "The PC is on battery power; Windows may slow the drive to save energy, so results can be lower.".to_string(),
                format!("Other programs use {letter} while the test runs, which can lower the results."),
                "On a hard disk a small test file shows better random speeds than the whole disk would.".to_string(),
                "System File Checker is running, so results can be lower.".to_string(),
                "Windows maintenance is running (tiworker.exe), so results can be lower.".to_string(),
                format!("An earlier test file on {letter} (2 KB) is removed first."),
            ]
        );
        assert_eq!(plan.leftovers.len(), 1);
        assert_eq!(plan.media, MediaKind::Hdd);
    }

    #[test]
    fn a_refused_start_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let env = SpeedEnv {
            elevated: no,
            ..TEST_ENV
        };
        let err = plan_or_start_speed_test(&host, &env, &request(dir.path()), false, never_open)
            .unwrap_err();
        assert!(matches!(err, Error::NotElevated));
        let env = SpeedEnv {
            free_bytes: small_free,
            ..TEST_ENV
        };
        let err = plan_or_start_speed_test(&host, &env, &request(dir.path()), false, never_open)
            .unwrap_err();
        assert!(err.to_string().contains("kept free"), "{err}");
        assert!(host.jobs().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_test_in_a_temporary_folder_measures_everything_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let seen: Arc<Mutex<Option<Vec<OpLogEntry>>>> = Arc::default();
        let hooks = Hooks {
            before_folder: Some(Box::new({
                let journal = Arc::clone(&journal);
                let seen = Arc::clone(&seen);
                move || *seen.lock().unwrap() = Some(rows(&journal))
            })),
            on_phase: None,
            before_measure: None,
        };
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (plan, job) =
            plan_or_start_with(&host, &TEST_ENV, &request(&place), false, open, hooks).unwrap();
        assert_eq!(plan.blocked_reason, None);
        let job = job.unwrap();
        assert_eq!(job.kind, KIND_SPEED_TEST);
        let end = host.wait(job.id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        assert!(end.logged);
        let (_, result) = host.result(job.id, 0).unwrap();
        assert_eq!(result["kind"], "speed");
        let measurements = result["measurements"].as_array().unwrap();
        assert_eq!(measurements.len(), 8, "{result}");
        for m in measurements {
            assert!(m["mb_s"].as_f64().unwrap() > 0.0, "{m}");
            assert_eq!(m["runs"], 1);
        }
        assert_eq!(result["completed"], true);
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["history_saved"], true);
        assert!(result["bytes_written"].as_u64().unwrap() >= 4 * MIB);
        assert!(result["bytes_written"].as_u64().unwrap() <= request(&place).max_write_bytes());
        // The folder and file are gone; only the history is left in the place.
        let left: Vec<_> = std::fs::read_dir(&place)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, ["history"]);
        // The copy in the history says so too.
        let history = speed_history(&place.join("history")).unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].history_saved);
        assert_eq!(history[0].end_word(), None);
        // Rows: started (already there before the folder was created), then completed.
        let before = seen.lock().unwrap().take().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].outcome, "started");
        let rows = rows(&journal);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].op, OP);
        assert_eq!(rows[0].target, temp_letter());
        assert!(rows[0]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("4 MB test file, 1 run, writes up to 20 MB; folder "));
        assert_eq!(rows[1].outcome, "succeeded");
        assert!(rows[1]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("SEQ1M Q8T1 "));
        // The detail and its speed block have exactly the documented keys.
        let detail = end.detail.unwrap();
        assert_eq!(detail["phase"], "done");
        assert!(detail["scan"].is_null() && detail["duplicates"].is_null());
        assert_eq!(
            sorted_keys(&detail),
            ["duplicates", "phase", "scan", "speed"]
        );
        let speed = &detail["speed"];
        assert_eq!(
            sorted_keys(speed),
            [
                "bytes_written",
                "direction",
                "done",
                "live_mb_s",
                "run",
                "runs",
                "step",
                "steps",
                "test"
            ]
        );
        assert_eq!(speed["steps"], 9);
        assert_eq!(speed["done"].as_array().unwrap().len(), 8);
        assert_eq!(
            sorted_keys(&speed["done"][0]),
            [
                "block_bytes",
                "bytes",
                "direction",
                "duration_ms",
                "iops",
                "label",
                "latency_us",
                "mb_s",
                "queue_depth",
                "runs",
                "test",
                "threads"
            ]
        );
    }

    #[test]
    fn a_stop_during_a_later_run_keeps_the_best_run_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let measuring = AtomicU32::new(0);
        let hooks = Hooks {
            before_folder: None,
            // Stops as the second run of the first measurement begins.
            on_phase: Some(Box::new(move |phase, cancel| {
                if phase == "measuring" && measuring.fetch_add(1, Ordering::SeqCst) == 1 {
                    cancel.store(true, Ordering::SeqCst);
                }
            })),
            before_measure: None,
        };
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let request = SpeedTestRequest::new(&temp_letter(), 4 * MIB, 2)
            .unwrap()
            .with_place(&place)
            .with_timing(Duration::from_millis(30), Duration::ZERO)
            .with_history_dir(place.join("history"));
        let (_, job) = plan_or_start_with(&host, &TEST_ENV, &request, false, open, hooks).unwrap();
        let job = job.unwrap();
        let end = host.wait(job.id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Cancelled, "{end:?}");
        let (_, result) = host.result(job.id, 0).unwrap();
        assert_eq!(result["completed"], false);
        let measurements = result["measurements"].as_array().unwrap();
        assert_eq!(measurements.len(), 1, "{result}");
        assert_eq!(measurements[0]["test"], "seq1m_q8t1");
        assert_eq!(measurements[0]["direction"], "read");
        assert_eq!(measurements[0]["runs"], 1);
        assert!(measurements[0]["mb_s"].as_f64().unwrap() > 0.0);
        // The result holds what the progress block showed, and the history keeps it.
        let detail = end.detail.unwrap();
        assert_eq!(detail["speed"]["done"], result["measurements"]);
        assert_eq!(result["history_saved"], true);
        assert!(result.get("error").is_none(), "{result}");
        let history = speed_history(&place.join("history")).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].measurements.len(), 1);
        assert_eq!(history[0].measurements[0].runs, 1);
        assert!(!history[0].completed);
        assert!(history[0].history_saved);
        assert_eq!(history[0].end_word(), Some("stopped"));
    }

    #[test]
    fn a_failure_after_a_measurement_is_saved_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let error = "the drive reported an I/O error: a scripted failure";
        let hooks = Hooks {
            // The reads are measured; the first write fails.
            before_measure: Some(Box::new(move |_, direction| {
                (direction == Direction::Write).then(|| error.to_string())
            })),
            ..Hooks::default()
        };
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (_, job) =
            plan_or_start_with(&host, &TEST_ENV, &request(&place), false, open, hooks).unwrap();
        let end = host.wait(job.unwrap().id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Failed, "{end:?}");
        assert_eq!(end.summary.as_deref(), Some(error));
        let (_, result) = host.result(end.id, 0).unwrap();
        assert_eq!(result["completed"], false);
        assert_eq!(result["error"], error);
        let measurements = result["measurements"].as_array().unwrap();
        assert!(!measurements.is_empty(), "{result}");
        assert!(
            measurements.iter().all(|m| m["direction"] == "read"),
            "{result}"
        );
        // The history keeps the measurements as a failed test, not as a stopped one.
        let history = speed_history(&place.join("history")).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].measurements.len(), measurements.len());
        assert!(!history[0].completed);
        assert_eq!(history[0].error.as_deref(), Some(error));
        assert_eq!(history[0].end_word(), Some("failed"));
        assert!(history[0].history_saved);
        let left: Vec<_> = std::fs::read_dir(&place)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, ["history"]);
        let rows = rows(&journal);
        assert_eq!(
            rows.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
            ["started", "failed"]
        );
        let detail = rows[1].detail.as_deref().unwrap();
        assert!(
            detail.starts_with(&format!("{error}; the test file was deleted")),
            "{detail}"
        );
    }

    #[test]
    fn only_a_failed_result_carries_an_error() {
        // A finished result has no "error" key, so entries saved before the field existed
        // read back the same.
        let finished = serde_json::to_value(sample_result()).unwrap();
        assert!(finished.get("error").is_none(), "{finished}");
        let read: SpeedTestResult = serde_json::from_value(finished).unwrap();
        assert_eq!(read, sample_result());
        assert_eq!(read.end_word(), None);
        let stopped = SpeedTestResult {
            completed: false,
            ..sample_result()
        };
        assert!(serde_json::to_value(&stopped)
            .unwrap()
            .get("error")
            .is_none());
        assert_eq!(stopped.end_word(), Some("stopped"));
        let failed = SpeedTestResult {
            completed: false,
            error: Some("the drive reported an I/O error: a scripted failure".to_string()),
            ..sample_result()
        };
        assert_eq!(failed.end_word(), Some("failed"));
        let dir = tempfile::tempdir().unwrap();
        history::append_history(dir.path(), &failed).unwrap();
        assert_eq!(speed_history(dir.path()).unwrap(), vec![failed]);
    }

    #[test]
    fn a_stop_while_preparing_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let hooks = Hooks {
            before_folder: None,
            on_phase: Some(Box::new(|phase, cancel| {
                if phase == "preparing" {
                    cancel.store(true, Ordering::SeqCst);
                }
            })),
            before_measure: None,
        };
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (_, job) =
            plan_or_start_with(&host, &TEST_ENV, &request(&place), false, open, hooks).unwrap();
        let end = host.wait(job.unwrap().id, Duration::from_secs(30)).unwrap();
        assert_eq!(end.state, JobState::Cancelled, "{end:?}");
        assert_eq!(std::fs::read_dir(&place).unwrap().count(), 0);
        let rows = rows(&journal);
        assert_eq!(
            rows.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
            ["started", "cancelled"]
        );
        let detail = rows[1].detail.as_deref().unwrap();
        assert!(
            detail.starts_with("stopped; wrote ") && detail.contains("the test file was deleted"),
            "{detail}"
        );
    }

    #[test]
    fn a_folder_that_cannot_be_created_ends_the_test_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        // The place does not exist, so no test folder can be created in it.
        let place = dir.path().join("missing");
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (plan, job) = plan_or_start_with(
            &host,
            &TEST_ENV,
            &request(&place),
            false,
            open,
            Hooks::default(),
        )
        .unwrap();
        assert_eq!(plan.blocked_reason, None);
        let end = host.wait(job.unwrap().id, Duration::from_secs(30)).unwrap();
        assert_eq!(end.state, JobState::Failed, "{end:?}");
        let summary = end.summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("the test folder can't be created: "),
            "{summary}"
        );
        assert!(!end.has_result);
        // Like every other end of a storage job, the detail says "done".
        let detail = end.detail.unwrap();
        assert_eq!(detail["phase"], "done", "{detail}");
        assert_eq!(
            sorted_keys(&detail),
            ["duplicates", "phase", "scan", "speed"]
        );
        assert_eq!(detail["speed"]["bytes_written"], 0);
        let rows = rows(&journal);
        assert_eq!(
            rows.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
            ["started", "failed"]
        );
        let detail = rows[1].detail.as_deref().unwrap();
        assert!(
            detail.starts_with("the test folder can't be created: ")
                && detail.contains("; nothing was written"),
            "{detail}"
        );
        assert!(!place.exists());
    }

    #[test]
    fn a_running_test_is_not_listed_as_a_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        // What the place holds and what is listed as a leftover there while the test measures.
        type Seen = Option<(Vec<String>, Vec<Leftover>)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let hooks = Hooks {
            before_folder: None,
            on_phase: Some(Box::new({
                let place = place.clone();
                let seen = Arc::clone(&seen);
                move |phase, _| {
                    let mut seen = seen.lock().unwrap();
                    if phase == "measuring" && seen.is_none() {
                        let names = std::fs::read_dir(&place)
                            .unwrap()
                            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                            .collect();
                        *seen = Some((names, find_leftovers(&place)));
                    }
                }
            })),
            before_measure: None,
        };
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (_, job) =
            plan_or_start_with(&host, &TEST_ENV, &request(&place), false, open, hooks).unwrap();
        let end = host.wait(job.unwrap().id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        let (names, listed) = seen.lock().unwrap().take().expect("the test measured");
        // The test's folder was there, its file held open by the test,
        assert_eq!(
            names.iter().filter(|n| is_folder_name(n)).count(),
            1,
            "{names:?}"
        );
        // but it is this process's own test, not a leftover of another one.
        assert!(listed.is_empty(), "{listed:?}");
        assert!(find_leftovers(&place).is_empty());
    }

    #[test]
    fn leftovers_in_the_place_are_removed_first() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path());
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        let old = place.join("CairnSpeedTest-0123abcd");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join(FILE_NAME), vec![1u8; 1000]).unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (plan, job) =
            plan_or_start_speed_test(&host, &TEST_ENV, &request(&place), false, open).unwrap();
        assert_eq!(plan.leftovers.len(), 1);
        let end = host.wait(job.unwrap().id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        assert!(!old.exists());
        let ops: Vec<(String, String)> = rows(&journal)
            .into_iter()
            .map(|r| (r.op, r.outcome))
            .collect();
        assert_eq!(
            ops,
            [
                (OP.to_string(), "started".to_string()),
                (OP_LEFTOVER.to_string(), "started".to_string()),
                (OP_LEFTOVER.to_string(), "deleted".to_string()),
                (OP.to_string(), "succeeded".to_string()),
            ]
        );
    }

    /// A folder standing in for a volume root that holds a leftover.
    static FAKE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

    fn volume_with_root_leftovers() -> Result<Vec<StorageVolume>> {
        let root = FAKE_ROOT.lock().unwrap().clone().expect("fake root");
        let mut volume = fixed_volume(&temp_letter());
        volume.leftovers = find_leftovers(&root);
        Ok(vec![volume])
    }

    #[test]
    fn place_hides_the_volume_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let old = root.join("CairnSpeedTest-0123abcd");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(FILE_NAME), vec![7u8; 3000]).unwrap();
        let place = dir.path().join("place");
        std::fs::create_dir(&place).unwrap();
        *FAKE_ROOT.lock().unwrap() = Some(root.clone());
        let env = SpeedEnv {
            volumes: volume_with_root_leftovers,
            ..TEST_ENV
        };
        assert_eq!(volume_with_root_leftovers().unwrap()[0].leftovers.len(), 1);

        let host = test_host(dir.path());
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let open = {
            let journal = Arc::clone(&journal);
            move || Ok(journal)
        };
        let (plan, job) =
            plan_or_start_speed_test(&host, &env, &request(&place), false, open).unwrap();
        assert!(plan.leftovers.is_empty(), "{:?}", plan.leftovers);
        assert!(!plan.notes.iter().any(|n| n.contains("earlier test file")));
        let end = host.wait(job.unwrap().id, Duration::from_secs(60)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        // The root's folder and file are untouched, and no row names them.
        assert_eq!(std::fs::read(old.join(FILE_NAME)).unwrap(), vec![7u8; 3000]);
        let rows = rows(&journal);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.op == OP));
        let old_text = old.display().to_string();
        assert!(rows.iter().all(|r| !r.target.contains(&old_text)
            && !r.detail.as_deref().unwrap_or("").contains(&old_text)));
        *FAKE_ROOT.lock().unwrap() = None;
    }

    #[test]
    fn dropping_the_target_deletes_the_test_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let file = create_test_file(&path, 4 * MIB).unwrap();
        assert!(path.exists());
        let (attrs, sector) = file_facts(&file).unwrap();
        assert_eq!(attrs & (ATTR_COMPRESSED | ATTR_SPARSE), 0);
        assert!(sector >= 512);
        let pool = Arc::new(AlignedBuf::random(8 << 20, 4096).unwrap());
        let target = OverlappedTarget::new(file, 4, 1 << 20, 4096, pool).unwrap();
        drop(target);
        assert!(!path.exists());
        // A handle that is simply closed deletes the file too.
        let file = create_test_file(&path, MIB).unwrap();
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn the_result_detail_lists_every_test() {
        let mut result = sample_result();
        result.measurements = vec![to_measurement(
            SpeedTestId::Seq1mQ8t1,
            Direction::Read,
            &RunStats {
                bytes: 7_012_000_000,
                ios: 6687,
                elapsed: Duration::from_secs(1),
                latency_sum: Duration::from_secs(1),
            },
            3,
        )];
        result.skipped = vec![SpeedTestId::Rnd4kQ1t1];
        result.bytes_written = 9 * GIB + 400 * MIB;
        assert_eq!(
            result_detail(&result),
            "SEQ1M Q8T1 7012/– MB/s · SEQ1M Q1T1 –/– MB/s · RND4K Q32T1 –/– MB/s · RND4K Q1T1 skipped · wrote 9.4 GB"
        );
    }
}
