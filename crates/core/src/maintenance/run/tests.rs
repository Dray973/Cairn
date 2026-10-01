use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};

use super::*;
use crate::cleanup::TargetResult;
use crate::maintenance::sfc::{NO_VIOLATIONS, VIOLATIONS};
use crate::safety::state_log::OpLogEntry;
use crate::tools::JobId;

#[derive(Debug)]
struct FakeLock;

impl RunLock for FakeLock {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lock {
    Free,
    Busy,
    Foreign,
    Broken,
}

#[derive(Debug, Clone)]
enum FakeCheck {
    Blocked(&'static str),
    Finishes {
        lines: Vec<String>,
        state: JobState,
        exit: i32,
        summary: Option<&'static str>,
        hint: Option<&'static str>,
    },
    /// Never ends on its own.
    Never,
    StartFails,
}

/// Called with the journal when the fake cleanup starts.
type CleanHook = Box<dyn Fn(&Journal)>;

struct Fake {
    elevated: bool,
    lock: Lock,
    /// Answers of `on_battery`, one per call; false once empty.
    battery: RefCell<VecDeque<bool>>,
    per_user: Option<String>,
    processes: Option<HashSet<String>>,
    reboot_pending: bool,
    clean_calls: RefCell<Vec<Vec<String>>>,
    clean_error: Option<String>,
    /// Target ids whose cleanup fails with an error and deletes nothing.
    failing_targets: Vec<&'static str>,
    /// Files every other target deletes.
    files_per_target: u64,
    before_clean: Option<CleanHook>,
    sfc: FakeCheck,
    dism: FakeCheck,
    started: RefCell<Vec<ToolId>>,
    abandoned: Cell<usize>,
    run_limit: Duration,
    messages: SfcMessages,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            elevated: true,
            lock: Lock::Free,
            battery: RefCell::new(VecDeque::new()),
            per_user: None,
            processes: Some(HashSet::new()),
            reboot_pending: false,
            clean_calls: RefCell::new(Vec::new()),
            clean_error: None,
            failing_targets: Vec::new(),
            files_per_target: 3,
            before_clean: None,
            sfc: sfc_finishing(&["Verification 100% complete.", ENGLISH_OK]),
            dism: dism_finishing(JobState::Succeeded),
            started: RefCell::new(Vec::new()),
            abandoned: Cell::new(0),
            run_limit: Duration::from_secs(3600),
            messages: SfcMessages::from_pairs(&[
                (NO_VIOLATIONS, ENGLISH_OK),
                (VIOLATIONS, ENGLISH_VIOLATIONS),
                (NO_VIOLATIONS, GERMAN_OK),
            ]),
        }
    }
}

const ENGLISH_OK: &str = "Windows Resource Protection did not find any integrity violations.";
const ENGLISH_VIOLATIONS: &str = "Windows Resource Protection found integrity violations.";
const GERMAN_OK: &str = "Der Windows-Ressourcenschutz hat keine Integritätsverletzungen gefunden.";

fn sfc_finishing(lines: &[&str]) -> FakeCheck {
    FakeCheck::Finishes {
        lines: lines.iter().map(|l| l.to_string()).collect(),
        state: JobState::Completed,
        exit: 0,
        summary: None,
        hint: None,
    }
}

fn dism_finishing(state: JobState) -> FakeCheck {
    FakeCheck::Finishes {
        lines: vec!["The component store is repairable.".into()],
        state,
        exit: 0,
        summary: Some("No component store damage was found."),
        hint: Some("run Repair the component store"),
    }
}

fn snapshot(
    tool: ToolId,
    state: JobState,
    exit: i32,
    summary: Option<&str>,
    hint: Option<&str>,
) -> JobSnapshot {
    JobSnapshot {
        id: JobId(1),
        tool,
        title: tool.info().title.to_string(),
        command_line: command_line(tool),
        volume: None,
        state,
        started_at: "2026-09-27T12:00:00Z".into(),
        finished_at: Some("2026-09-27T12:10:00Z".into()),
        elapsed_ms: 600_000,
        idle_ms: 0,
        progress: Some(100.0),
        progress_line: None,
        exit_code: Some(exit),
        exit_code_hex: Some(crate::tools::exit_code_hex(exit)),
        cancellable: false,
        cancel_requested: false,
        detached: false,
        restart_required: false,
        hint: hint.map(str::to_string),
        summary: summary.map(str::to_string),
        log_path: r"C:\Data\maintenance\tools\x.log".into(),
        raw_log_path: r"C:\Data\maintenance\tools\x.raw".into(),
        line_count: 1,
        logged: true,
    }
}

struct FakeJob<'a> {
    fake: &'a Fake,
    tool: ToolId,
    plan: FakeCheck,
    polls: u32,
}

impl CheckJob for FakeJob<'_> {
    fn poll(&mut self) -> CheckPoll {
        self.polls += 1;
        match &self.plan {
            FakeCheck::Finishes {
                lines,
                state,
                exit,
                summary,
                hint,
            } => {
                if self.polls == 1 {
                    CheckPoll {
                        lines: vec!["Beginning verification phase of system scan.".into()],
                        percent: Some(45.0),
                        finished: None,
                    }
                } else {
                    CheckPoll {
                        lines: lines.clone(),
                        percent: Some(100.0),
                        finished: Some(snapshot(self.tool, *state, *exit, *summary, *hint)),
                    }
                }
            }
            _ => CheckPoll {
                lines: vec![format!("still running {}", self.polls)],
                percent: None,
                finished: None,
            },
        }
    }

    fn abandon(&mut self) {
        self.fake.abandoned.set(self.fake.abandoned.get() + 1);
    }

    fn log_path(&self) -> Option<String> {
        Some(format!(r"C:\Data\maintenance\tools\{}.log", self.tool))
    }
}

impl MaintenanceSystem for Fake {
    fn elevated(&self) -> bool {
        self.elevated
    }

    fn acquire_lock(&self) -> Result<LockState> {
        match self.lock {
            Lock::Free => Ok(LockState::Held(Box::new(FakeLock))),
            Lock::Busy => Ok(LockState::Busy),
            Lock::Foreign => Ok(LockState::Foreign),
            Lock::Broken => Err(Error::Other("the lock is broken".into())),
        }
    }

    fn on_battery(&self) -> bool {
        self.battery.borrow_mut().pop_front().unwrap_or(false)
    }

    fn per_user_refusal(&self) -> Option<String> {
        self.per_user.clone()
    }

    fn processes(&self) -> Option<HashSet<String>> {
        self.processes.clone()
    }

    fn servicing_reboot_pending(&self) -> bool {
        self.reboot_pending
    }

    fn clean(&self, journal: &Journal, ids: &[String]) -> Result<CleanupReport> {
        if let Some(hook) = &self.before_clean {
            hook(journal);
        }
        self.clean_calls.borrow_mut().push(ids.to_vec());
        if let Some(message) = &self.clean_error {
            return Err(Error::Other(message.clone()));
        }
        let results: Vec<TargetResult> = ids
            .iter()
            .map(|id| {
                let fails = self.failing_targets.contains(&id.as_str());
                TargetResult {
                    id: id.clone(),
                    freed_bytes: if fails { 0 } else { 1024 * 1024 },
                    deleted_files: if fails { 0 } else { self.files_per_target },
                    skipped_files: 1,
                    skipped_reason: None,
                    errors: if fails {
                        vec!["access denied".into()]
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect();
        Ok(CleanupReport {
            freed_bytes: results.iter().map(|r| r.freed_bytes).sum(),
            results,
            duration_ms: 5,
        })
    }

    fn start_check<'a>(&'a self, _journal: Arc<Journal>, tool: ToolId) -> Result<CheckStart<'a>> {
        self.started.borrow_mut().push(tool);
        let plan = if tool == ToolId::SfcVerify {
            self.sfc.clone()
        } else {
            self.dism.clone()
        };
        match plan {
            FakeCheck::Blocked(reason) => Ok(CheckStart::Blocked(reason.to_string())),
            FakeCheck::StartFails => Err(Error::Other("could not start".into())),
            plan => Ok(CheckStart::Started(Box::new(FakeJob {
                fake: self,
                tool,
                plan,
                polls: 0,
            }))),
        }
    }

    fn sfc_messages(&self) -> SfcMessages {
        self.messages.clone()
    }

    fn now(&self) -> DateTime<Local> {
        Local::now()
    }

    fn run_limit(&self) -> Duration {
        self.run_limit
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn progress_interval(&self) -> Duration {
        Duration::ZERO
    }
}

struct Setup {
    dir: tempfile::TempDir,
    journal: Arc<Journal>,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    Setup { dir, journal }
}

fn request(targets: &[&str], sfc: bool, dism: bool) -> RunRequest {
    RunRequest {
        targets: targets.iter().map(|t| t.to_string()).collect(),
        system_file_check: sfc,
        component_store_check: dism,
        origin: RunOrigin::Task,
    }
}

fn run(fake: &Fake, s: &Setup, req: &RunRequest) -> Result<MaintenanceReport> {
    run_with(fake, Arc::clone(&s.journal), s.dir.path(), req)
}

/// Audit rows of the run lane, oldest first.
fn ops(journal: &Journal) -> Vec<OpLogEntry> {
    let mut rows: Vec<OpLogEntry> = journal
        .ops(100)
        .unwrap()
        .into_iter()
        .filter(|r| r.op == OP_RUN)
        .collect();
    rows.reverse();
    rows
}

fn outcomes(journal: &Journal) -> Vec<String> {
    ops(journal).into_iter().map(|r| r.outcome).collect()
}

#[test]
fn the_started_row_exists_before_anything_is_cleaned() {
    let s = setup();
    let mut fake = Fake::new();
    fake.before_clean = Some(Box::new(|journal: &Journal| {
        let rows = ops(journal);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outcome, "started");
        assert!(rows[0].session_id.is_none());
        let run = &journal.maintenance_runs(1).unwrap()[0];
        assert_eq!(run.state, "running");
        assert_eq!(rows[0].target, format!("maintenance run {}", run.id));
    }));
    let report = run(
        &fake,
        &s,
        &request(&["user_temp", "windows_temp"], true, true),
    )
    .unwrap();
    assert_eq!(fake.clean_calls.borrow().len(), 1);
    assert_eq!(report.state, RunState::Completed);
    assert_eq!(outcomes(&s.journal), ["started", "completed"]);
    let rows = ops(&s.journal);
    assert_eq!(
        rows[0].detail.as_deref(),
        Some(
            "task; cleanup: user_temp, windows_temp; checks: sfc.exe /verifyonly, dism.exe \
             /Online /Cleanup-Image /CheckHealth"
        )
    );
    let detail = rows[1].detail.as_deref().unwrap();
    assert!(
        detail.starts_with("Freed 2.0 MB  ·  no system file problems  ·  component store OK · "),
        "{detail}"
    );
    assert!(detail.contains(" · log "), "{detail}");
    let stored = &s.journal.maintenance_runs(1).unwrap()[0];
    assert_eq!(stored.state, "completed");
    assert!(stored.ended_at.is_some());
    let back: MaintenanceReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    assert_eq!(back, report);
    assert_eq!(stored.log_path, report.log_path);
    let progress: RunProgress =
        serde_json::from_str(stored.progress_json.as_deref().unwrap()).unwrap();
    assert_eq!(progress.step, "finishing");
    assert_eq!(progress.count, 3);
    assert_eq!(report.exit_code_state(), 0);
}

impl MaintenanceReport {
    fn exit_code_state(&self) -> i32 {
        self.state.exit_code()
    }
}

#[test]
fn a_busy_lock_writes_a_skipped_row_and_no_run_row() {
    let s = setup();
    let mut fake = Fake::new();
    fake.lock = Lock::Busy;
    let report = run(&fake, &s, &request(&["user_temp"], true, false)).unwrap();
    assert_eq!(report.state, RunState::Skipped);
    assert_eq!(report.run_id, 0);
    assert_eq!(report.state.exit_code(), EXIT_SKIPPED);
    assert!(fake.clean_calls.borrow().is_empty());
    assert!(fake.started.borrow().is_empty());
    assert!(s.journal.maintenance_runs(10).unwrap().is_empty());
    let rows = ops(&s.journal);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "skipped");
    assert_eq!(rows[0].target, "maintenance");
    assert_eq!(rows[0].detail.as_deref(), Some(BUSY_DETAIL));
}

#[test]
fn a_foreign_lock_skips_with_its_text() {
    let s = setup();
    let mut fake = Fake::new();
    fake.lock = Lock::Foreign;
    let report = run(&fake, &s, &request(&["user_temp"], false, false)).unwrap();
    assert_eq!(report.state, RunState::Skipped);
    assert_eq!(report.stopped_reason.as_deref(), Some(FOREIGN_LOCK_TEXT));
    assert!(fake.clean_calls.borrow().is_empty());
    assert!(s.journal.maintenance_runs(10).unwrap().is_empty());
    let rows = ops(&s.journal);
    assert_eq!(rows[0].detail.as_deref(), Some(FOREIGN_LOCK_TEXT));
}

#[test]
fn refusals_write_one_row_and_run_nothing() {
    let s = setup();
    let mut fake = Fake::new();
    fake.elevated = false;
    let err = run(&fake, &s, &request(&["user_temp"], true, true)).unwrap_err();
    assert!(matches!(err, Error::NotElevated));
    assert_eq!(outcomes(&s.journal), ["skipped"]);
    assert_eq!(
        ops(&s.journal)[0].detail.as_deref(),
        Some(NEEDS_ADMIN_DETAIL)
    );
    let mut broken = Fake::new();
    broken.lock = Lock::Broken;
    assert!(run(&broken, &s, &request(&["user_temp"], false, false)).is_err());
    assert!(fake.clean_calls.borrow().is_empty() && broken.clean_calls.borrow().is_empty());
    assert!(s.journal.maintenance_runs(10).unwrap().is_empty());
    // A request that names nothing to do or a foreign target writes nothing at all.
    let before = s.journal.ops(100).unwrap().len();
    assert!(run(&Fake::new(), &s, &request(&[], false, false)).is_err());
    assert!(run(&Fake::new(), &s, &request(&["recycle_bin"], true, false)).is_err());
    assert_eq!(s.journal.ops(100).unwrap().len(), before);
}

#[test]
fn battery_at_start_skips_every_step() {
    let s = setup();
    let fake = Fake::new();
    fake.battery.borrow_mut().push_back(true);
    let report = run(&fake, &s, &request(&["user_temp"], true, true)).unwrap();
    assert_eq!(report.state, RunState::Skipped);
    assert_eq!(report.stopped_reason.as_deref(), Some(BATTERY_AT_START));
    assert!(fake.clean_calls.borrow().is_empty());
    assert!(fake.started.borrow().is_empty());
    assert_eq!(
        report.cleanup.as_ref().unwrap().outcome,
        StepOutcome::NotRun
    );
    assert!(report
        .checks
        .iter()
        .all(|c| c.outcome == StepOutcome::NotRun));
    assert_eq!(report.headline, "Skipped: the PC runs on battery power");
    assert_eq!(outcomes(&s.journal), ["started", "skipped"]);
    assert_eq!(s.journal.maintenance_runs(1).unwrap()[0].state, "skipped");
}

#[test]
fn battery_after_the_cleanup_stops_the_checks() {
    let s = setup();
    let fake = Fake::new();
    fake.battery.borrow_mut().extend([false, true]);
    let report = run(&fake, &s, &request(&["user_temp"], true, true)).unwrap();
    assert_eq!(report.state, RunState::Stopped);
    assert_eq!(report.state.exit_code(), EXIT_STOPPED);
    assert_eq!(report.stopped_reason.as_deref(), Some(BATTERY_LATER));
    assert_eq!(fake.clean_calls.borrow().len(), 1);
    assert!(fake.started.borrow().is_empty());
    assert_eq!(report.checks.len(), 2);
    assert!(report
        .checks
        .iter()
        .all(|c| c.outcome == StepOutcome::NotRun));
    assert_eq!(report.headline, "Freed 1.0 MB");
    assert_eq!(outcomes(&s.journal), ["started", "stopped"]);
}

#[test]
fn a_per_user_refusal_skips_only_per_user_targets() {
    let s = setup();
    let mut fake = Fake::new();
    fake.per_user = Some("another account".into());
    let report = run(
        &fake,
        &s,
        &request(&["user_temp", "windows_temp", "browser_edge"], false, false),
    )
    .unwrap();
    assert_eq!(
        *fake.clean_calls.borrow(),
        [vec!["windows_temp".to_string()]]
    );
    let cleanup = report.cleanup.unwrap();
    let skipped: Vec<(&str, &str)> = cleanup
        .targets
        .iter()
        .filter(|t| t.outcome == "skipped")
        .map(|t| (t.id.as_str(), t.reason.as_deref().unwrap()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("user_temp", "another account"),
            ("browser_edge", "another account")
        ]
    );
    assert_eq!(
        cleanup
            .targets
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["user_temp", "windows_temp", "browser_edge"]
    );
    assert_eq!(cleanup.outcome, StepOutcome::Ok);
    assert_eq!(report.state, RunState::Completed);
}

#[test]
fn the_servicing_guard_skips_windows_update_targets() {
    let busy: HashSet<String> = ["tiworker.exe".to_string()].into_iter().collect();
    for (processes, reboot) in [
        (Some(busy), false),
        (None, false),
        (Some(HashSet::new()), true),
    ] {
        let s = setup();
        let mut fake = Fake::new();
        fake.processes = processes.clone();
        fake.reboot_pending = reboot;
        let report = run(
            &fake,
            &s,
            &request(
                &["user_temp", "update_cache", "delivery_optimization"],
                false,
                false,
            ),
        )
        .unwrap();
        assert_eq!(
            *fake.clean_calls.borrow(),
            [vec!["user_temp".to_string()]],
            "{processes:?} {reboot}"
        );
        let cleanup = report.cleanup.unwrap();
        let guarded = cleanup
            .targets
            .iter()
            .filter(|t| t.reason.as_deref() == Some(SERVICING_TEXT))
            .count();
        assert_eq!(guarded, 2);
    }
    // Only guarded targets: every one skipped, the step counts as skipped.
    let s = setup();
    let mut fake = Fake::new();
    fake.reboot_pending = true;
    let report = run(&fake, &s, &request(&["update_cache"], false, false)).unwrap();
    assert!(fake.clean_calls.borrow().is_empty());
    assert_eq!(report.cleanup.unwrap().outcome, StepOutcome::Skipped);
    assert_eq!(report.headline, "Cleanup skipped");
}

#[test]
fn stale_running_rows_are_interrupted_with_rows() {
    let s = setup();
    let stale = s
        .journal
        .insert_maintenance_run(&NewMaintenanceRun {
            origin: "task".into(),
            state: "running".into(),
            request_json: "{}".into(),
        })
        .unwrap();
    let fake = Fake::new();
    run(&fake, &s, &request(&["user_temp"], false, false)).unwrap();
    let rows = ops(&s.journal);
    assert_eq!(rows[0].outcome, "interrupted");
    assert_eq!(rows[0].target, format!("maintenance run {stale}"));
    assert_eq!(rows[0].detail.as_deref(), Some(INTERRUPTED_DETAIL));
    let runs = s.journal.maintenance_runs(10).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].state, "interrupted");
}

#[test]
fn the_sfc_verdict_comes_from_its_messages_in_any_language() {
    for (lines, outcome, part) in [
        (vec![ENGLISH_OK], StepOutcome::Ok, "no system file problems"),
        (vec![GERMAN_OK], StepOutcome::Ok, "no system file problems"),
        (
            vec![ENGLISH_OK, ENGLISH_VIOLATIONS],
            StepOutcome::Attention,
            "damaged system files found",
        ),
    ] {
        let s = setup();
        let mut fake = Fake::new();
        fake.sfc = sfc_finishing(&lines);
        let report = run(&fake, &s, &request(&[], true, false)).unwrap();
        let check = &report.checks[0];
        assert_eq!(check.outcome, outcome, "{lines:?}");
        assert_eq!(check.windows_message.as_deref(), lines.last().copied());
        assert_eq!(report.headline.to_lowercase(), part.to_lowercase());
        assert_eq!(check.exit_code_hex.as_deref(), Some("0x00000000"));
        assert!(check
            .log_path
            .as_deref()
            .unwrap()
            .ends_with("sfc_verify.log"));
        if outcome == StepOutcome::Attention {
            assert_eq!(report.state, RunState::Attention);
            assert_eq!(report.state.exit_code(), EXIT_ATTENTION);
            assert_eq!(
                report.attention,
                ["Windows found damaged system files. Open Tools and run Repair system files."]
            );
            assert_eq!(outcomes(&s.journal).last().unwrap(), "problems_found");
        }
    }
}

#[test]
fn dism_states_map_to_step_outcomes() {
    for (state, outcome, run_state) in [
        (JobState::Succeeded, StepOutcome::Ok, RunState::Completed),
        (
            JobState::Attention,
            StepOutcome::Attention,
            RunState::Attention,
        ),
        (JobState::Failed, StepOutcome::Failed, RunState::Failed),
        (
            JobState::Completed,
            StepOutcome::Unknown,
            RunState::Completed,
        ),
        (JobState::Cancelled, StepOutcome::Failed, RunState::Failed),
    ] {
        let s = setup();
        let mut fake = Fake::new();
        fake.dism = dism_finishing(state);
        let report = run(&fake, &s, &request(&[], false, true)).unwrap();
        assert_eq!(report.checks[0].outcome, outcome, "{state:?}");
        assert_eq!(report.state, run_state, "{state:?}");
        if outcome == StepOutcome::Attention {
            assert_eq!(
                report.checks[0].hint.as_deref(),
                Some("run Repair the component store")
            );
        }
    }
}

#[test]
fn blocked_and_failing_checks_are_reported() {
    let s = setup();
    let mut fake = Fake::new();
    fake.sfc = FakeCheck::Blocked("System File Checker is already running.");
    fake.dism = FakeCheck::StartFails;
    let report = run(&fake, &s, &request(&[], true, true)).unwrap();
    assert_eq!(report.checks[0].outcome, StepOutcome::Skipped);
    assert_eq!(
        report.checks[0].text,
        "Not checked: System File Checker is already running."
    );
    assert_eq!(report.checks[1].outcome, StepOutcome::Failed);
    assert_eq!(report.state, RunState::Failed);
    assert_eq!(report.state.exit_code(), EXIT_FAILED);
    assert_eq!(
        report.headline,
        "System files not checked  ·  component store check failed"
    );
    assert_eq!(report.attention.len(), 1);
}

#[test]
fn cleanup_failures_are_reported() {
    let s = setup();
    let mut fake = Fake::new();
    fake.failing_targets = vec!["windows_temp"];
    let report = run(
        &fake,
        &s,
        &request(&["user_temp", "windows_temp"], false, false),
    )
    .unwrap();
    let cleanup = report.cleanup.as_ref().unwrap();
    assert_eq!(cleanup.outcome, StepOutcome::Attention);
    assert_eq!(report.state, RunState::Attention);
    assert_eq!(
        report.attention,
        ["Windows temp folder couldn't be cleaned: access denied"]
    );
    assert_eq!(cleanup.skipped_files, 2);
    let s = setup();
    let mut fake = Fake::new();
    fake.clean_error = Some("the journal is locked".into());
    let report = run(&fake, &s, &request(&["user_temp"], true, false)).unwrap();
    assert_eq!(report.state, RunState::Failed);
    assert_eq!(
        report.cleanup.as_ref().unwrap().outcome,
        StepOutcome::Failed
    );
    assert_eq!(
        report.attention,
        ["The cleanup failed: the journal is locked"]
    );
    assert!(report.headline.starts_with("Cleanup failed  ·  "));
    assert_eq!(fake.started.borrow().as_slice(), [ToolId::SfcVerify]);
}

#[test]
fn the_time_limit_abandons_a_running_check() {
    let s = setup();
    let mut fake = Fake::new();
    fake.sfc = FakeCheck::Never;
    fake.run_limit = Duration::from_millis(30);
    let report = run(&fake, &s, &request(&[], true, true)).unwrap();
    assert_eq!(fake.abandoned.get(), 1);
    assert_eq!(report.checks[0].outcome, StepOutcome::LeftRunning);
    assert_eq!(report.checks[1].outcome, StepOutcome::NotRun);
    assert_eq!(report.state, RunState::Stopped);
    assert_eq!(report.stopped_reason.as_deref(), Some(LEFT_RUNNING));
    assert_eq!(fake.started.borrow().as_slice(), [ToolId::SfcVerify]);
    assert_eq!(report.headline, "System file check still running");
}

#[test]
fn states_have_exit_codes_and_audit_outcomes() {
    let table = [
        (RunState::Completed, 0, "completed"),
        (RunState::Attention, 10, "problems_found"),
        (RunState::Failed, 11, "failed"),
        (RunState::Stopped, 12, "stopped"),
        (RunState::Skipped, 13, "skipped"),
        (RunState::Interrupted, 11, "interrupted"),
    ];
    for (state, code, outcome) in table {
        assert_eq!(state.exit_code(), code, "{state:?}");
        assert_eq!(state.audit_outcome(), outcome);
        assert_eq!(RunState::parse(state.as_str()), Some(state));
        assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
    }
    assert_eq!(RunOrigin::parse("task"), Some(RunOrigin::Task));
    assert_eq!(RunOrigin::parse("cli"), Some(RunOrigin::Cli));
    assert_eq!(RunOrigin::parse("app"), None);
    assert_eq!(
        serde_json::to_value(StepOutcome::LeftRunning).unwrap(),
        "left_running"
    );
}

#[test]
fn the_transcript_records_the_run_and_old_ones_are_pruned() {
    let s = setup();
    let folder = s.dir.path().join(MAINTENANCE_DIR);
    std::fs::create_dir_all(&folder).unwrap();
    for n in 0..25 {
        std::fs::write(
            folder.join(format!("20200101-0000{n:02}-maintenance.log")),
            b"old",
        )
        .unwrap();
    }
    let fake = Fake::new();
    let report = run(&fake, &s, &request(&["user_temp"], true, false)).unwrap();
    let path = PathBuf::from(report.log_path.as_deref().unwrap());
    assert_eq!(path.parent().unwrap(), folder);
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.trim_start_matches('\u{feff}');
    assert!(
        text.starts_with(&format!(
            "Cairn {VERSION}  ·  maintenance run {} (task)\r\n",
            report.run_id
        )),
        "{text}"
    );
    assert!(
        text.contains("Clean up:\r\n  Temporary files: freed 1.0 MB (3 files)"),
        "{text}"
    );
    assert!(text.contains(&format!("  | {ENGLISH_OK}")), "{text}");
    assert!(
        text.contains("Finished: Freed 1.0 MB  ·  no system file problems"),
        "{text}"
    );
    let logs = std::fs::read_dir(&folder)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "log")
        })
        .count();
    assert_eq!(logs, crate::tools::logs::KEEP_RUNS);
    assert!(path.exists());
}

#[test]
fn the_transcript_counts_files_in_the_singular_and_the_plural() {
    let cleaned_line = |files: u64| {
        let s = setup();
        let fake = Fake {
            files_per_target: files,
            ..Fake::new()
        };
        let report = run(&fake, &s, &request(&["user_temp"], false, false)).unwrap();
        let text = std::fs::read_to_string(report.log_path.as_deref().unwrap()).unwrap();
        text.split("\r\n")
            .find(|line| line.starts_with("  Temporary files: "))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("no line for the target in {text}"))
    };
    assert_eq!(cleaned_line(1), "  Temporary files: freed 1.0 MB (1 file)");
    assert_eq!(cleaned_line(2), "  Temporary files: freed 1.0 MB (2 files)");
    assert_eq!(cleaned_line(0), "  Temporary files: freed 1.0 MB (0 files)");
}

#[test]
fn a_run_without_a_transcript_still_finishes() {
    let s = setup();
    // A file where the folder would go makes the transcript impossible.
    std::fs::write(s.dir.path().join(MAINTENANCE_DIR), b"").unwrap();
    let report = run(&Fake::new(), &s, &request(&["user_temp"], false, false)).unwrap();
    assert_eq!(report.log_path, None);
    assert_eq!(report.state, RunState::Completed);
    assert!(ops(&s.journal)[1]
        .detail
        .as_deref()
        .unwrap()
        .ends_with("· no log"));
}

#[test]
fn the_echo_gets_the_transcript_line_by_line() {
    let s = setup();
    let echoed = RefCell::new(Vec::<String>::new());
    let echo = |line: &str| echoed.borrow_mut().push(line.to_string());
    let report = run_echoing_with(
        &Fake::new(),
        Arc::clone(&s.journal),
        s.dir.path(),
        &request(&["user_temp"], true, true),
        Some(&echo),
    )
    .unwrap();
    let text = std::fs::read_to_string(report.log_path.as_deref().unwrap()).unwrap();
    let written: Vec<&str> = text
        .trim_start_matches('\u{feff}')
        .trim_end_matches("\r\n")
        .split("\r\n")
        .collect();
    assert_eq!(*echoed.borrow(), written);
    assert!(echoed.borrow()[0].starts_with("Cairn "), "{echoed:?}");
    assert!(echoed.borrow().contains(&format!("  | {ENGLISH_OK}")));

    // Without a transcript file the lines are still echoed.
    let s = setup();
    std::fs::write(s.dir.path().join(MAINTENANCE_DIR), b"").unwrap();
    echoed.borrow_mut().clear();
    let report = run_echoing_with(
        &Fake::new(),
        Arc::clone(&s.journal),
        s.dir.path(),
        &request(&["user_temp"], false, false),
        Some(&echo),
    )
    .unwrap();
    assert_eq!(report.log_path, None);
    let lines = echoed.borrow();
    assert!(lines.contains(&"Clean up:".to_string()), "{lines:?}");
    assert!(lines.last().unwrap().starts_with("Finished "), "{lines:?}");
}

#[test]
fn run_rows_are_pruned_to_the_newest() {
    let s = setup();
    for _ in 0..KEEP_RUN_ROWS + 5 {
        let id = s
            .journal
            .insert_maintenance_run(&NewMaintenanceRun {
                origin: "task".into(),
                state: "running".into(),
                request_json: "{}".into(),
            })
            .unwrap();
        s.journal
            .finish_maintenance_run(id, "completed", "{}", None)
            .unwrap();
    }
    let report = run(&Fake::new(), &s, &request(&["user_temp"], false, false)).unwrap();
    let rows = s.journal.maintenance_runs(1000).unwrap();
    assert_eq!(rows.len(), KEEP_RUN_ROWS);
    assert_eq!(rows[0].id, report.run_id);
}

#[test]
fn the_report_keys_match_the_contract() {
    let s = setup();
    let report = run(&Fake::new(), &s, &request(&["user_temp"], true, false)).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&json),
        [
            "attention",
            "checks",
            "cleanup",
            "duration_ms",
            "ended_at",
            "headline",
            "log_path",
            "origin",
            "request",
            "run_id",
            "started_at",
            "state",
            "stopped_reason"
        ]
    );
    assert_eq!(
        keys(&json["cleanup"]),
        [
            "deleted_files",
            "error",
            "freed_bytes",
            "outcome",
            "skipped_files",
            "targets"
        ]
    );
    assert_eq!(
        keys(&json["cleanup"]["targets"][0]),
        [
            "deleted_files",
            "freed_bytes",
            "id",
            "outcome",
            "reason",
            "title"
        ]
    );
    assert_eq!(
        keys(&json["checks"][0]),
        [
            "command_line",
            "elapsed_ms",
            "exit_code_hex",
            "hint",
            "log_path",
            "outcome",
            "restart_required",
            "text",
            "title",
            "tool",
            "windows_message"
        ]
    );
    assert_eq!(
        keys(&json["request"]),
        [
            "component_store_check",
            "origin",
            "system_file_check",
            "targets"
        ]
    );
    let row = &s.journal.maintenance_runs(1).unwrap()[0];
    let run = crate::maintenance::report::MaintenanceRun::from_row(
        row,
        crate::win::mutex::MutexPresence::Absent,
    );
    assert_eq!(
        keys(&serde_json::to_value(&run).unwrap()),
        [
            "acknowledged",
            "ended_at",
            "id",
            "log_path",
            "origin",
            "progress",
            "report",
            "request",
            "stale",
            "started_at",
            "state"
        ]
    );
    assert_eq!(
        keys(&serde_json::to_value(run.progress.unwrap()).unwrap()),
        ["count", "index", "percent", "step", "title", "updated_at"]
    );
}

#[test]
fn the_live_system_writes_into_the_journals_folder() {
    let dir = tempfile::tempdir().unwrap();
    let live = LiveMaintenance::new(dir.path());
    assert_eq!(
        live.tools_dir(),
        dir.path().join("maintenance").join("tools")
    );
    assert!(
        !dir.path().join("maintenance").exists(),
        "nothing is created up front"
    );
}

#[test]
fn per_user_refusals_follow_the_account_checks() {
    let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    assert_eq!(per_user_refusal_for(Ok(sid.into()), Ok(false)), None);
    assert!(per_user_refusal_for(Ok(sid.into()), Ok(true)).is_some());
    assert!(per_user_refusal_for(Ok(sid.into()), Err(Error::Other("x".into()))).is_some());
    assert!(per_user_refusal_for(Ok("S-1-5-18".into()), Ok(false)).is_some());
    assert!(per_user_refusal_for(Err(Error::Other("x".into())), Ok(false)).is_some());
}

#[test]
fn a_plan_lists_the_steps_and_what_blocks_them() {
    let plan = plan_with(
        &request(&["user_temp", "error_reports"], true, true),
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        plan.steps
            .iter()
            .map(|s| s.step.as_str())
            .collect::<Vec<_>>(),
        ["cleanup", "system_files", "component_store"]
    );
    assert_eq!(
        plan.steps[0].detail,
        "deletes files permanently in: Temporary files, Error reports"
    );
    assert_eq!(plan.steps[1].detail, "sfc.exe /verifyonly");
    assert_eq!(plan.blocked_reason, None);
    let plan = plan_with(&request(&[], true, false), false, true).unwrap();
    assert!(plan.blocked_reason.is_some());
    assert_eq!(plan.notes.len(), 1);
    assert!(plan_with(&request(&["recycle_bin"], false, false), true, false).is_err());
}
