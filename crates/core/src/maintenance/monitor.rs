//! Follows maintenance runs from the app without blocking it.
//!
//! Runs happen in the task's own process. An engine-owned thread ("maintenance-monitor")
//! reads the newest run row and who holds the run lock, every second while a run is in
//! progress or expected to start and every 15 seconds otherwise, and keeps the latest
//! observation. The UI reads it with [`RunMonitor::latest`], a lock and a clone: no I/O and
//! no waiting. The source is read without the state lock held.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;

use super::report::MaintenanceRun;
use super::run::RunState;
use super::RUN_MUTEX;
use crate::safety::state_log::Journal;
use crate::win::mutex::{self, MutexPresence};
use crate::Result;

/// Poll interval while a run is in progress or expected.
const FAST: Duration = Duration::from_secs(1);
/// Poll interval otherwise.
const SLOW: Duration = Duration::from_secs(15);
/// How long a run started with Run now may take to appear.
const START_WAIT: Duration = Duration::from_secs(60);

/// What the monitor saw last.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunObservation {
    /// A run is in progress: an administrator holds the run lock and the newest run is running.
    pub running: bool,
    /// Run now was requested and its run has not appeared yet.
    pub waiting_for_start: bool,
    /// Run now was requested, but no run appeared within a minute.
    pub start_timed_out: bool,
    /// The newest run.
    pub run: Option<MaintenanceRun>,
    /// RFC 3339, UTC.
    pub observed_at: String,
    /// Why the last read failed; the run is the one read before.
    pub error: Option<String>,
}

/// Where the monitor reads runs from.
pub(crate) trait MonitorSource: Send + Sync {
    /// The newest run and who holds the run lock.
    fn read(&self) -> Result<(Option<MaintenanceRun>, MutexPresence)>;
}

/// The account's journal (opened on the first read and kept) and the run lock of this PC.
#[derive(Default)]
struct LiveSource {
    /// Only the monitor thread reads it.
    journal: Mutex<Option<Journal>>,
}

impl MonitorSource for LiveSource {
    fn read(&self) -> Result<(Option<MaintenanceRun>, MutexPresence)> {
        let presence = mutex::presence(RUN_MUTEX);
        let mut journal = self.journal.lock();
        if journal.is_none() {
            *journal = Some(Journal::open_default()?);
        }
        let rows = match journal.as_ref() {
            Some(journal) => journal.maintenance_runs(1)?,
            None => Vec::new(),
        };
        Ok((
            rows.first()
                .map(|row| MaintenanceRun::from_row(row, presence)),
            presence,
        ))
    }
}

#[derive(Debug, Default)]
struct MonitorState {
    started: bool,
    /// Threads started, for tests.
    threads: usize,
    stop: bool,
    /// Wake the thread now.
    poke: bool,
    /// Run now was requested; a run with a larger id than `expect_after` must appear by then.
    expect_until: Option<Instant>,
    expect_after: i64,
    timed_out: bool,
    latest: Option<RunObservation>,
}

/// Follows maintenance runs on its own thread; see the module documentation.
pub struct RunMonitor {
    source: Box<dyn MonitorSource>,
    state: Mutex<MonitorState>,
    wake: Condvar,
    fast: Duration,
    slow: Duration,
    start_wait: Duration,
}

impl std::fmt::Debug for RunMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunMonitor")
            .field("state", &*self.state.lock())
            .field("fast", &self.fast)
            .field("slow", &self.slow)
            .finish_non_exhaustive()
    }
}

/// The process-wide monitor of this account's journal.
pub fn monitor() -> &'static RunMonitor {
    static MONITOR: OnceLock<RunMonitor> = OnceLock::new();
    MONITOR.get_or_init(|| RunMonitor::new(Box::new(LiveSource::default()), FAST, SLOW, START_WAIT))
}

impl RunMonitor {
    fn new(
        source: Box<dyn MonitorSource>,
        fast: Duration,
        slow: Duration,
        start_wait: Duration,
    ) -> RunMonitor {
        RunMonitor {
            source,
            state: Mutex::new(MonitorState::default()),
            wake: Condvar::new(),
            fast,
            slow,
            start_wait,
        }
    }

    /// Starts the monitor's thread once and wakes it. With `expect_start`, a run is expected
    /// within a minute (after Run now): the monitor polls every second and reports
    /// `waiting_for_start` until it appears, then `start_timed_out` if it does not. Returns at
    /// once.
    pub fn watch(&'static self, expect_start: bool) {
        let spawn = {
            let mut state = self.state.lock();
            if expect_start {
                state.expect_until = Some(Instant::now() + self.start_wait);
                state.expect_after = state
                    .latest
                    .as_ref()
                    .and_then(|o| o.run.as_ref())
                    .map_or(0, |run| run.id);
                state.timed_out = false;
                if let Some(latest) = state.latest.as_mut() {
                    latest.waiting_for_start = true;
                    latest.start_timed_out = false;
                }
            }
            state.poke = true;
            let spawn = !state.started;
            if spawn {
                state.started = true;
                state.threads += 1;
            }
            spawn
        };
        self.wake.notify_all();
        if spawn {
            let spawned = std::thread::Builder::new()
                .name("maintenance-monitor".into())
                .spawn(move || self.run_loop());
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "cannot start the maintenance monitor");
                self.state.lock().started = false;
            }
        }
    }

    /// The latest observation; `None` before the first read. A lock and a clone only.
    pub fn latest(&self) -> Option<RunObservation> {
        self.state.lock().latest.clone()
    }

    fn run_loop(&self) {
        loop {
            let read = self.source.read();
            let mut state = self.state.lock();
            if state.stop {
                return;
            }
            let observation = observe(&mut state, read, Instant::now());
            let busy = observation.running || observation.waiting_for_start;
            state.latest = Some(observation);
            state.poke = false;
            let deadline = Instant::now() + if busy { self.fast } else { self.slow };
            while !state.poke && !state.stop {
                if self.wake.wait_until(&mut state, deadline).timed_out() {
                    break;
                }
            }
            if state.stop {
                return;
            }
        }
    }

    /// Ends the monitor's thread.
    #[cfg(test)]
    fn stop(&self) {
        self.state.lock().stop = true;
        self.wake.notify_all();
    }
}

/// A monitor over `source` that polls every `fast` (and every `4 × fast` when idle) and waits
/// `10 × fast` for an expected run. It lives for the rest of the test process.
#[cfg(test)]
pub(crate) fn test_monitor(source: Box<dyn MonitorSource>, fast: Duration) -> &'static RunMonitor {
    Box::leak(Box::new(RunMonitor::new(source, fast, fast * 4, fast * 10)))
}

/// Turns a read into an observation and advances the expected-start state.
fn observe(
    state: &mut MonitorState,
    read: Result<(Option<MaintenanceRun>, MutexPresence)>,
    now: Instant,
) -> RunObservation {
    let (run, presence, error) = match read {
        Ok((run, presence)) => (run, presence, None),
        Err(e) => (
            state.latest.as_ref().and_then(|o| o.run.clone()),
            MutexPresence::Absent,
            Some(e.to_string()),
        ),
    };
    let running = presence == MutexPresence::Admin
        && run.as_ref().is_some_and(|r| r.state == RunState::Running);
    let appeared = run.as_ref().is_some_and(|r| r.id > state.expect_after);
    let mut waiting = false;
    if let Some(until) = state.expect_until {
        if running || appeared {
            state.expect_until = None;
        } else if now >= until {
            state.expect_until = None;
            state.timed_out = true;
        } else {
            waiting = true;
        }
    }
    if appeared {
        state.timed_out = false;
    }
    RunObservation {
        running,
        waiting_for_start: waiting,
        start_timed_out: state.timed_out,
        run,
        observed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        error,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    use super::*;
    use crate::maintenance::run::{RunOrigin, RunRequest};

    fn run(id: i64, state: RunState) -> MaintenanceRun {
        MaintenanceRun {
            id,
            origin: RunOrigin::Task,
            state,
            started_at: "2026-09-27T12:00:00Z".into(),
            ended_at: None,
            request: RunRequest {
                targets: vec![],
                system_file_check: true,
                component_store_check: false,
                origin: RunOrigin::Task,
            },
            progress: None,
            report: None,
            log_path: None,
            acknowledged: false,
            stale: false,
        }
    }

    /// Answers with whatever the test set last.
    struct Scripted {
        answer: Mutex<Result<(Option<MaintenanceRun>, MutexPresence)>>,
        reads: AtomicUsize,
    }

    impl Scripted {
        fn new(run: Option<MaintenanceRun>, presence: MutexPresence) -> Arc<Scripted> {
            Arc::new(Scripted {
                answer: Mutex::new(Ok((run, presence))),
                reads: AtomicUsize::new(0),
            })
        }

        fn set(&self, answer: Result<(Option<MaintenanceRun>, MutexPresence)>) {
            *self.answer.lock() = answer;
        }
    }

    impl MonitorSource for Arc<Scripted> {
        fn read(&self) -> Result<(Option<MaintenanceRun>, MutexPresence)> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            match &*self.answer.lock() {
                Ok(answer) => Ok(answer.clone()),
                Err(e) => Err(crate::Error::Other(e.to_string())),
            }
        }
    }

    fn wait_for(monitor: &RunMonitor, what: impl Fn(&RunObservation) -> bool) -> RunObservation {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(o) = monitor.latest() {
                if what(&o) {
                    return o;
                }
            }
            assert!(Instant::now() < deadline, "{:?}", monitor.latest());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn latest_follows_the_source() {
        let source = Scripted::new(Some(run(1, RunState::Completed)), MutexPresence::Absent);
        let monitor = test_monitor(Box::new(Arc::clone(&source)), Duration::from_millis(5));
        assert_eq!(monitor.latest(), None);
        monitor.watch(false);
        let first = wait_for(monitor, |o| o.run.is_some());
        assert!(!first.running && !first.waiting_for_start && first.error.is_none());
        source.set(Ok((Some(run(2, RunState::Running)), MutexPresence::Admin)));
        let busy = wait_for(monitor, |o| o.running);
        assert_eq!(busy.run.unwrap().id, 2);
        source.set(Ok((Some(run(2, RunState::Running)), MutexPresence::Other)));
        let foreign = wait_for(monitor, |o| !o.running);
        assert_eq!(foreign.run.unwrap().id, 2);
        source.set(Err(crate::Error::Other("journal locked".into())));
        let failed = wait_for(monitor, |o| o.error.is_some());
        assert_eq!(failed.error.as_deref(), Some("journal locked"));
        assert_eq!(failed.run.unwrap().id, 2, "the last run is kept");
        monitor.stop();
    }

    #[test]
    fn an_expected_start_waits_and_then_times_out() {
        let source = Scripted::new(Some(run(4, RunState::Completed)), MutexPresence::Absent);
        let monitor = test_monitor(Box::new(Arc::clone(&source)), Duration::from_millis(5));
        monitor.watch(false);
        wait_for(monitor, |o| o.run.is_some());
        monitor.watch(true);
        let waiting = monitor.latest().unwrap();
        assert!(waiting.waiting_for_start, "shown at once");
        let timed_out = wait_for(monitor, |o| o.start_timed_out);
        assert!(!timed_out.waiting_for_start);
        // A new request starts the wait again; the run appearing ends it.
        monitor.watch(true);
        assert!(!monitor.latest().unwrap().start_timed_out);
        source.set(Ok((Some(run(5, RunState::Running)), MutexPresence::Admin)));
        let started = wait_for(monitor, |o| o.running);
        assert!(!started.waiting_for_start && !started.start_timed_out);
        monitor.stop();
    }

    #[test]
    fn a_run_that_finished_between_polls_counts_as_started() {
        let source = Scripted::new(Some(run(7, RunState::Completed)), MutexPresence::Absent);
        let monitor = test_monitor(Box::new(Arc::clone(&source)), Duration::from_millis(5));
        monitor.watch(false);
        wait_for(monitor, |o| o.run.is_some());
        monitor.watch(true);
        source.set(Ok((
            Some(run(8, RunState::Attention)),
            MutexPresence::Absent,
        )));
        let seen = wait_for(monitor, |o| o.run.as_ref().is_some_and(|r| r.id == 8));
        assert!(!seen.waiting_for_start && !seen.start_timed_out && !seen.running);
        monitor.stop();
    }

    #[test]
    fn watching_twice_starts_one_thread() {
        let source = Scripted::new(None, MutexPresence::Absent);
        let monitor = test_monitor(Box::new(Arc::clone(&source)), Duration::from_millis(5));
        monitor.watch(false);
        monitor.watch(false);
        monitor.watch(true);
        wait_for(monitor, |_| true);
        assert_eq!(monitor.state.lock().threads, 1);
        monitor.stop();
    }

    /// Blocks every read until the test lets it go.
    struct Blocked {
        gate: Mutex<mpsc::Receiver<()>>,
        entered: AtomicUsize,
    }

    impl MonitorSource for Arc<Blocked> {
        fn read(&self) -> Result<(Option<MaintenanceRun>, MutexPresence)> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let _ = self.gate.lock().recv();
            Ok((None, MutexPresence::Absent))
        }
    }

    #[test]
    fn watch_and_latest_do_not_wait_for_a_blocked_source() {
        let (release, gate) = mpsc::channel();
        let source = Arc::new(Blocked {
            gate: Mutex::new(gate),
            entered: AtomicUsize::new(0),
        });
        let monitor = test_monitor(Box::new(Arc::clone(&source)), Duration::from_millis(5));
        monitor.watch(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while source.entered.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let started = Instant::now();
        monitor.watch(true);
        assert_eq!(monitor.latest(), None);
        assert!(started.elapsed() < Duration::from_millis(500));
        monitor.stop();
        release.send(()).unwrap();
    }

    #[test]
    fn the_observation_keys_match_the_contract() {
        let observation = RunObservation {
            running: false,
            waiting_for_start: false,
            start_timed_out: false,
            run: Some(run(1, RunState::Completed)),
            observed_at: "2026-09-27T12:00:00Z".into(),
            error: None,
        };
        let json = serde_json::to_value(&observation).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "error",
                "observed_at",
                "run",
                "running",
                "start_timed_out",
                "waiting_for_start"
            ]
        );
    }
}
