//! Tests of the job host: temporary journals and log folders, a 5 ms tick, no processes.

use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

use serde_json::json;

use super::*;
use crate::safety::state_log::OpLogEntry;
use crate::tools::runner::MAX_JOB_LINES;

const TICK: Duration = Duration::from_millis(5);
const LONG: Duration = Duration::from_secs(10);

struct Fixture {
    dir: tempfile::TempDir,
    journal: Arc<Journal>,
    host: JobHost,
}

fn config(dir: &Path) -> HostConfig {
    HostConfig {
        lane: "test",
        log_dir: dir.join(r"jobs\test"),
        keep_logs: 20,
        tick: TICK,
        settle_wait: Duration::from_secs(2),
        stop_wait: Duration::from_millis(200),
        keep_finished: 10,
        keep_results_per_kind: None,
    }
}

fn fixture_with(edit: impl FnOnce(&mut HostConfig)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    let mut config = config(dir.path());
    edit(&mut config);
    Fixture {
        host: JobHost::new(config),
        dir,
        journal,
    }
}

fn fixture() -> Fixture {
    fixture_with(|_| {})
}

fn spec(kind: &'static str) -> JobSpec {
    JobSpec {
        kind,
        title: format!("Job {kind}"),
        command_line: format!("{kind} --run"),
        cancellable: true,
        audit: None,
        needs_journal: false,
        log: false,
    }
}

fn audited(kind: &'static str) -> JobSpec {
    JobSpec {
        audit: Some(AuditSpec {
            op: "test_op",
            target: "C:".to_string(),
            started_detail: "size 1 MB".to_string(),
        }),
        log: true,
        ..spec(kind)
    }
}

fn end(state: JobState, summary: &str) -> WorkEnd {
    WorkEnd {
        state,
        summary: summary.to_string(),
        hint: None,
        restart_required: false,
        audit_detail: None,
    }
}

fn done(summary: &'static str) -> Work {
    Box::new(move |_| end(JobState::Succeeded, summary))
}

impl Fixture {
    fn open(&self) -> impl FnOnce() -> Result<Arc<Journal>> {
        let journal = Arc::clone(&self.journal);
        move || Ok(journal)
    }

    fn start(&self, spec: JobSpec, work: Work) -> HostJobSnapshot {
        self.host.start(spec, self.open(), work).unwrap()
    }

    fn finish(&self, id: HostJobId) -> HostJobSnapshot {
        let snapshot = self.host.wait(id, LONG).unwrap();
        assert!(snapshot.state.is_finished(), "{snapshot:?}");
        snapshot
    }

    /// Rows of `op`, oldest first.
    fn rows(&self, op: &str) -> Vec<OpLogEntry> {
        let mut ops = self.journal.ops(10_000).unwrap();
        ops.retain(|o| o.op == op);
        ops.reverse();
        ops
    }

    fn log_dir(&self) -> PathBuf {
        self.dir.path().join(r"jobs\test")
    }
}

/// Starts a job whose work blocks until the returned sender sends (or is dropped).
fn blocked(f: &Fixture, spec: JobSpec) -> (HostJobSnapshot, mpsc::Sender<()>) {
    let (release, wait) = mpsc::channel::<()>();
    let job = f.start(
        spec,
        Box::new(move |_| {
            let _ = wait.recv();
            end(JobState::Succeeded, "released")
        }),
    );
    (job, release)
}

fn never_open() -> Result<Arc<Journal>> {
    panic!("the journal must not be opened for this job")
}

// ───────────────────────────── contract ─────────────────────────────

#[test]
fn snapshot_and_view_keys_match_the_contract() {
    let f = fixture();
    let (job, release) = blocked(&f, audited("scan"));
    let snapshot = serde_json::to_value(f.host.snapshot(job.id).unwrap()).unwrap();
    let keys: Vec<&str> = snapshot
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected = [
        "id",
        "lane",
        "kind",
        "title",
        "command_line",
        "state",
        "started_at",
        "finished_at",
        "elapsed_ms",
        "idle_ms",
        "progress",
        "progress_line",
        "cancellable",
        "cancel_requested",
        "detached",
        "restart_required",
        "summary",
        "hint",
        "notes",
        "detail",
        "log_path",
        "line_count",
        "logged",
        "has_result",
        "result_revision",
    ];
    let mut sorted_keys = keys.clone();
    sorted_keys.sort_unstable();
    let mut sorted_expected = expected.to_vec();
    sorted_expected.sort_unstable();
    assert_eq!(sorted_keys, sorted_expected);
    assert_eq!(keys.len(), 25);

    let view = serde_json::to_value(f.host.view(job.id, 0, 10).unwrap()).unwrap();
    let view_keys = view.as_object().unwrap();
    assert_eq!(view_keys.len(), 30);
    for key in expected
        .iter()
        .chain(["lines", "first", "next", "skipped", "more"].iter())
    {
        assert!(view_keys.contains_key(*key), "{key}");
    }
    assert_eq!(snapshot["state"], "running");
    assert_eq!(snapshot["lane"], "test");
    assert_eq!(snapshot["kind"], "scan");
    drop(release);
    f.finish(job.id);
}

#[test]
fn ids_are_unique_across_hosts() {
    let a = fixture();
    let b = fixture();
    let first = a.start(spec("one"), done("a"));
    let second = b.start(spec("one"), done("b"));
    let third = a.start(spec("two"), {
        a.finish(first.id);
        done("c")
    });
    assert!(first.id < second.id && second.id < third.id);
    assert_ne!(first.id, second.id);
    b.finish(second.id);
    a.finish(third.id);
}

// ───────────────────────────── start ─────────────────────────────

#[test]
fn started_row_precedes_the_work() {
    let f = fixture();
    let journal = Arc::clone(&f.journal);
    let job = f.start(
        audited("speed_test"),
        Box::new(move |ctx| {
            let last = journal.ops(1).unwrap();
            assert_eq!(
                (
                    last[0].op.as_str(),
                    last[0].target.as_str(),
                    last[0].outcome.as_str()
                ),
                ("test_op", "C:", "started")
            );
            assert_eq!(last[0].detail.as_deref(), Some("size 1 MB"));
            assert!(ctx.journal().is_some());
            ctx.line("measuring");
            WorkEnd {
                audit_detail: Some("read 1000 MB/s".to_string()),
                ..end(JobState::Succeeded, "Finished")
            }
        }),
    );
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Succeeded);
    assert_eq!(finished.summary.as_deref(), Some("Finished"));
    assert!(finished.logged);
    let rows = f.rows("test_op");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].outcome, "succeeded");
    let detail = rows[1].detail.as_deref().unwrap();
    assert!(detail.starts_with("read 1000 MB/s · "), "{detail}");
    assert!(rows.iter().all(|r| r.session_id.is_none()));

    // The transcript holds the header, the line and the footer.
    let path = PathBuf::from(finished.log_path.unwrap());
    assert!(path.starts_with(f.log_dir()));
    let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
    assert!(text.starts_with('\u{feff}'), "{text:?}");
    assert!(
        text.contains(&format!(
            "Cairn {}  ·  speed_test --run\r\n",
            crate::VERSION
        )),
        "{text:?}"
    );
    assert!(text.contains("\r\nmeasuring\r\n"), "{text:?}");
    assert!(text.ends_with(&format!("\r\n\r\n{detail}\r\n")), "{text:?}");
}

#[test]
fn an_unaudited_job_without_needs_journal_never_opens_the_journal() {
    let f = fixture();
    let job = f
        .host
        .start(
            spec("scan"),
            never_open,
            Box::new(|ctx| {
                assert!(ctx.journal().is_none());
                assert!(ctx.audit("op", "t", "started", None).is_err());
                end(JobState::Completed, "scanned")
            }),
        )
        .unwrap();
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Completed);
    assert!(!finished.logged);
    assert!(f.journal.ops(10).unwrap().is_empty());

    // needs_journal opens it without writing rows; the work may write its own.
    let journal = Arc::clone(&f.journal);
    let job = f.start(
        JobSpec {
            needs_journal: true,
            ..spec("upgrade")
        },
        Box::new(move |ctx| {
            assert!(Arc::ptr_eq(ctx.journal().unwrap(), &journal));
            ctx.audit("app_upgrade", "Contoso.App", "started", Some("1.0 -> 2.0"))
                .unwrap();
            end(JobState::Succeeded, "updated")
        }),
    );
    f.finish(job.id);
    let rows = f.journal.ops(10).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].op, "app_upgrade");
}

#[test]
fn a_job_without_a_log_creates_no_files_and_has_no_run_file() {
    let f = fixture();
    let job = f.start(
        spec("scan"),
        Box::new(|ctx| {
            assert_eq!(ctx.stem(), None);
            assert!(ctx.run_file(".json").is_err());
            ctx.line("kept in memory only");
            end(JobState::Succeeded, "ok")
        }),
    );
    let finished = f.finish(job.id);
    assert_eq!(finished.log_path, None);
    assert_eq!(finished.line_count, 1);
    assert!(!f.log_dir().exists(), "a log folder was created");
    assert!(f.host.open_log(job.id).is_err());
}

#[test]
fn run_files_sit_next_to_the_transcript() {
    let f = fixture();
    let job = f.start(
        JobSpec {
            log: true,
            ..spec("winget_scan")
        },
        Box::new(|ctx| {
            let stem = ctx.stem().unwrap().to_string();
            assert!(stem.ends_with("-winget_scan"), "{stem}");
            let file = ctx.run_file(".steps").unwrap();
            assert_eq!(file, ctx.dir().join(format!("{stem}.steps")));
            end(JobState::Succeeded, "ok")
        }),
    );
    f.finish(job.id);
}

#[test]
fn a_panicking_work_finishes_failed_with_one_final_row() {
    let f = fixture();
    let job = f.start(
        audited("speed_test"),
        Box::new(|_| panic!("the test file vanished")),
    );
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Failed);
    assert_eq!(
        finished.summary.as_deref(),
        Some("internal error: the test file vanished")
    );
    let rows = f.rows("test_op");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].outcome, "failed");
    // The host still takes new jobs.
    let next = f.start(spec("scan"), done("ok"));
    assert_eq!(f.finish(next.id).state, JobState::Succeeded);
}

#[test]
fn a_work_that_returns_running_is_a_failure() {
    let f = fixture();
    let job = f.start(spec("scan"), Box::new(|_| end(JobState::Running, "")));
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Failed);
    assert_eq!(finished.summary.as_deref(), Some(NO_FINAL_STATE));
}

#[test]
fn a_busy_host_refuses_and_writes_nothing() {
    let f = fixture();
    let (job, release) = blocked(&f, spec("scan"));
    let err = f
        .host
        .start(audited("speed_test"), never_open, done("never"))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Job scan is running; wait for it to finish or stop it."
    );
    assert!(f.journal.ops(10).unwrap().is_empty());
    assert!(!f.log_dir().exists());
    assert_eq!(f.host.jobs().len(), 1);
    drop(release);
    f.finish(job.id);
}

#[test]
fn a_closed_host_refuses_and_writes_nothing() {
    let f = fixture();
    assert!(f.host.shutdown().is_empty());
    assert!(f.host.is_closed());
    let err = f
        .host
        .start(audited("speed_test"), never_open, done("never"))
        .unwrap_err();
    assert_eq!(err.to_string(), "Cairn is closing; nothing was started.");
    assert!(f.journal.ops(10).unwrap().is_empty());
    assert!(!f.log_dir().exists());
    assert!(f.host.jobs().is_empty());
}

#[test]
fn a_failing_journal_deletes_the_new_transcript() {
    let f = fixture();
    let err = f
        .host
        .start(
            audited("speed_test"),
            || Err(Error::Other("journal locked".into())),
            done("never"),
        )
        .unwrap_err();
    assert_eq!(err.to_string(), "journal locked");
    let files: Vec<_> = fs::read_dir(f.log_dir()).unwrap().collect();
    assert!(files.is_empty(), "{files:?}");
    assert!(f.host.jobs().is_empty());
    // The reservation was released.
    let job = f.start(spec("scan"), done("ok"));
    f.finish(job.id);
}

#[test]
fn invalid_kinds_are_refused_before_anything() {
    let f = fixture();
    for kind in ["", "Scan", "speed-test", "scan1"] {
        assert!(f.host.start(spec(kind), never_open, done("never")).is_err());
    }
    assert!(f.host.jobs().is_empty());
}

#[test]
fn long_command_lines_are_cut() {
    let f = fixture();
    let long = "x".repeat(300);
    let job = f.start(
        JobSpec {
            command_line: long,
            ..spec("scan")
        },
        done("ok"),
    );
    assert_eq!(job.command_line.chars().count(), 200);
    assert!(job.command_line.ends_with('…'));
    f.finish(job.id);
}

// ───────────────────────────── polling ─────────────────────────────

#[test]
fn views_page_through_lines_and_keep_the_newest_2000() {
    let f = fixture();
    let (go, wait) = mpsc::channel::<()>();
    let job = f.start(
        spec("scan"),
        Box::new(move |ctx| {
            for i in 1..=5 {
                ctx.line(&format!("line {i}"));
            }
            let _ = wait.recv();
            for i in 6..=MAX_JOB_LINES + 10 {
                ctx.line(&format!("line {i}"));
            }
            end(JobState::Succeeded, "ok")
        }),
    );
    let deadline = Instant::now() + LONG;
    while f.host.snapshot(job.id).unwrap().line_count < 5 {
        assert!(Instant::now() < deadline);
        thread::sleep(TICK);
    }
    let view = f.host.view(job.id, 0, 2).unwrap();
    assert_eq!(view.lines, ["line 1", "line 2"]);
    assert_eq!(
        (view.first, view.next, view.skipped, view.more),
        (Some(1), 2, 0, true)
    );
    let view = f.host.view(job.id, 2, 10).unwrap();
    assert_eq!(
        (view.first, view.next, view.skipped, view.more),
        (Some(3), 5, 0, false)
    );
    let view = f.host.view(job.id, 5, 10).unwrap();
    assert_eq!((view.first, view.next, view.lines.len()), (None, 5, 0));
    go.send(()).unwrap();
    let finished = f.finish(job.id);
    assert_eq!(finished.line_count, MAX_JOB_LINES as u64 + 10);
    let view = f.host.view(job.id, 3, 1).unwrap();
    assert_eq!(
        (view.first, view.next, view.skipped, view.more),
        (Some(11), 11, 7, true)
    );
    assert_eq!(view.lines, ["line 11"]);
    // At most 500 lines per view.
    let view = f.host.view(job.id, 0, 10_000).unwrap();
    assert_eq!(view.lines.len(), MAX_LINES_PER_VIEW);
    assert!(view.more);
    assert!(f.host.view(HostJobId(u64::MAX), 0, 10).is_none());
}

#[test]
fn progress_detail_notes_and_flags_reach_the_snapshot() {
    let f = fixture();
    let (go, wait) = mpsc::channel::<()>();
    let (ready, seen) = mpsc::channel::<()>();
    let job = f.start(
        spec("speed_test"),
        Box::new(move |ctx| {
            ctx.progress(Some(17.94), Some("Writing 1 of 4"));
            ctx.set_detail(json!({"phase": "write"}));
            ctx.note("The drive is busy.");
            ctx.set_detached(true);
            ready.send(()).unwrap();
            let _ = wait.recv();
            WorkEnd {
                hint: Some("Close other programs".to_string()),
                restart_required: true,
                ..end(JobState::Attention, "Slower than expected")
            }
        }),
    );
    seen.recv().unwrap();
    let running = f.host.running().unwrap();
    assert_eq!(running.id, job.id);
    assert_eq!(running.progress, Some(17.9));
    assert_eq!(running.progress_line.as_deref(), Some("Writing 1 of 4"));
    assert_eq!(running.detail, Some(json!({"phase": "write"})));
    assert_eq!(running.notes, ["The drive is busy."]);
    assert!(running.detached);
    assert_eq!(running.finished_at, None);
    go.send(()).unwrap();
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Attention);
    assert_eq!(finished.hint.as_deref(), Some("Close other programs"));
    assert!(finished.restart_required);
    assert!(finished.finished_at.is_some());
    assert!(f.host.running().is_none());
}

#[test]
fn view_and_jobs_do_not_wait_for_a_blocked_work() {
    let f = fixture();
    let (release, wait) = mpsc::channel::<()>();
    let (entered, inside) = mpsc::channel::<()>();
    let job = f.start(
        audited("scan"),
        Box::new(move |ctx| {
            ctx.line("before");
            entered.send(()).unwrap();
            let _ = wait.recv();
            end(JobState::Succeeded, "ok")
        }),
    );
    inside.recv().unwrap();
    let started = Instant::now();
    for _ in 0..100 {
        assert!(f.host.view(job.id, 0, 10).is_some());
        assert_eq!(f.host.jobs().len(), 1);
        assert!(f.host.snapshot(job.id).is_some());
        assert!(f.host.running().is_some());
        assert!(f.host.result(job.id, 0).is_none());
    }
    assert!(f.host.cancel(job.id).unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    release.send(()).unwrap();
    f.finish(job.id);
}

// ───────────────────────────── cancel ─────────────────────────────

#[test]
fn cancel_reaches_the_work_and_reports_errors() {
    let f = fixture();
    let job = f.start(
        spec("scan"),
        Box::new(|ctx| {
            let flag = ctx.cancel_flag();
            while !ctx.cancel_requested() {
                thread::sleep(ctx.tick());
            }
            assert!(flag.load(Ordering::SeqCst));
            assert!(!ctx.closing());
            end(JobState::Cancelled, "Stopped")
        }),
    );
    assert!(f.host.cancel(job.id).unwrap());
    let finished = f.finish(job.id);
    assert_eq!(finished.state, JobState::Cancelled);
    assert!(finished.cancel_requested);
    assert!(!f.host.cancel(job.id).unwrap(), "already finished");
    assert!(f
        .host
        .cancel(HostJobId(u64::MAX))
        .unwrap_err()
        .to_string()
        .contains("no test job"));

    let (job, release) = blocked(
        &f,
        JobSpec {
            cancellable: false,
            ..spec("upgrade")
        },
    );
    let err = f.host.cancel(job.id).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Job upgrade can't be stopped; it runs to completion"
    );
    drop(release);
    f.finish(job.id);
}

// ───────────────────────────── results ─────────────────────────────

#[test]
fn results_have_revisions_and_since_filters_them() {
    let f = fixture();
    let (go, wait) = mpsc::channel::<()>();
    let (ready, seen) = mpsc::channel::<()>();
    let job = f.start(
        spec("scan"),
        Box::new(move |ctx| {
            ctx.publish(json!({"files": 1}));
            ready.send(()).unwrap();
            let _ = wait.recv();
            ctx.publish(json!({"files": 2}));
            end(JobState::Succeeded, "ok")
        }),
    );
    seen.recv().unwrap();
    let (revision, value) = f.host.result(job.id, 0).unwrap();
    assert_eq!((revision, value.as_ref()), (1, &json!({"files": 1})));
    assert!(f.host.result(job.id, 1).is_none(), "nothing newer than 1");
    go.send(()).unwrap();
    let finished = f.finish(job.id);
    assert!(finished.has_result);
    assert_eq!(finished.result_revision, 2);
    let (revision, value) = f.host.result(job.id, 1).unwrap();
    assert_eq!((revision, value.as_ref()), (2, &json!({"files": 2})));
    assert!(f.host.result(HostJobId(u64::MAX), 0).is_none());
}

#[derive(Debug, PartialEq)]
struct Tree {
    folders: usize,
}

#[test]
fn typed_results_downcast_to_their_type_only() {
    let f = fixture();
    let job = f.start(
        spec("space_scan"),
        Box::new(|ctx| {
            ctx.publish_typed(Arc::new(Tree { folders: 3 }));
            end(JobState::Succeeded, "ok")
        }),
    );
    let finished = f.finish(job.id);
    assert!(finished.has_result);
    assert_eq!(
        finished.result_revision, 0,
        "typed results carry no revision"
    );
    assert_eq!(
        f.host.typed::<Tree>(job.id).as_deref(),
        Some(&Tree { folders: 3 })
    );
    assert!(f.host.typed::<String>(job.id).is_none());
    assert!(f.host.result(job.id, 0).is_none());
}

// ───────────────────────────── prune ─────────────────────────────

fn scan_result(f: &Fixture, kind: &'static str, folders: usize) -> HostJobId {
    let job = f.start(
        spec(kind),
        Box::new(move |ctx| {
            ctx.publish(json!({ "folders": folders }));
            ctx.publish_typed(Arc::new(Tree { folders }));
            end(JobState::Succeeded, "ok")
        }),
    );
    // `wait` returns once the prune after the final state ran.
    f.finish(job.id);
    job.id
}

#[test]
fn prune_keeps_the_newest_result_of_each_kind() {
    let f = fixture_with(|c| {
        c.keep_finished = 2;
        c.keep_results_per_kind = Some(1);
    });
    let old_scan = scan_result(&f, "space_scan", 1);
    let new_scan = scan_result(&f, "space_scan", 2);
    let old = f.host.snapshot(old_scan).expect("still listed");
    assert!(!old.has_result, "{old:?}");
    assert!(f.host.typed::<Tree>(old_scan).is_none());
    assert!(f.host.result(old_scan, 0).is_none());
    assert!(f.host.snapshot(new_scan).unwrap().has_result);

    let tests: Vec<HostJobId> = (0..3).map(|i| scan_result(&f, "speed_test", i)).collect();
    let listed: Vec<HostJobId> = f.host.jobs().iter().map(|j| j.id).collect();
    // The two newest overall plus the newest scan, newest first.
    assert_eq!(listed, [tests[2], tests[1], new_scan]);
    assert_eq!(
        f.host.typed::<Tree>(new_scan).as_deref(),
        Some(&Tree { folders: 2 })
    );
    assert!(f.host.snapshot(tests[2]).unwrap().has_result);
    assert!(!f.host.snapshot(tests[1]).unwrap().has_result);
    assert!(f.host.snapshot(old_scan).is_none());
}

#[test]
fn prune_without_a_result_limit_keeps_results() {
    let f = fixture_with(|c| c.keep_finished = 3);
    let ids: Vec<HostJobId> = (0..5).map(|i| scan_result(&f, "winget_scan", i)).collect();
    let listed: Vec<HostJobId> = f.host.jobs().iter().map(|j| j.id).collect();
    assert_eq!(listed, [ids[4], ids[3], ids[2]]);
    for id in &ids[2..] {
        assert!(f.host.result(*id, 0).is_some(), "{id}");
    }
}

#[test]
fn a_result_cloned_by_a_running_job_outlives_the_prune() {
    let f = fixture_with(|c| {
        c.keep_finished = 1;
        c.keep_results_per_kind = Some(1);
    });
    let scan = scan_result(&f, "space_scan", 7);
    let tree = f.host.typed::<Tree>(scan).unwrap();
    scan_result(&f, "space_scan", 8);
    assert!(f.host.typed::<Tree>(scan).is_none());
    assert_eq!(tree.folders, 7, "the clone stays valid");
}

#[test]
fn transcripts_are_pruned_to_keep_logs_and_the_new_one_is_protected() {
    for (keep, left) in [(20usize, 19usize), (0, 0)] {
        let f = fixture_with(|c| c.keep_logs = keep);
        fs::create_dir_all(f.log_dir()).unwrap();
        for i in 0..25 {
            fs::write(
                f.log_dir()
                    .join(format!("20200101-0000{i:02}-speed_test.log")),
                b"x",
            )
            .unwrap();
        }
        let job = f.start(audited("speed_test"), done("ok"));
        let finished = f.finish(job.id);
        let new = PathBuf::from(finished.log_path.unwrap());
        assert!(new.is_file(), "the new transcript is kept");
        let old = fs::read_dir(f.log_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.path() != new)
            .count();
        assert_eq!(old, left, "keep {keep}");
    }
}

// ───────────────────────────── shutdown ─────────────────────────────

#[test]
fn shutdown_stops_a_job_that_listens() {
    let f = fixture();
    let job = f.start(
        audited("speed_test"),
        Box::new(|ctx| {
            while !ctx.cancel_requested() {
                thread::sleep(ctx.tick());
            }
            assert!(ctx.closing());
            end(JobState::Cancelled, "Stopped because Cairn closed")
        }),
    );
    let outcomes = f.host.shutdown();
    assert_eq!(
        outcomes,
        [HostShutdownOutcome {
            id: job.id,
            kind: "speed_test",
            action: HostShutdownAction::Stopped,
        }]
    );
    let rows = f.rows("test_op");
    assert_eq!(
        rows.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
        ["started", "cancelled"]
    );
    // Closed afterwards.
    assert!(f.host.start(spec("scan"), never_open, done("x")).is_err());
}

#[test]
fn shutdown_records_a_job_that_does_not_stop_in_time() {
    let f = fixture_with(|c| c.stop_wait = Duration::from_millis(30));
    let (release, wait) = mpsc::channel::<()>();
    let (noted, note_set) = mpsc::channel::<()>();
    let job = f.start(
        audited("speed_test"),
        Box::new(move |ctx| {
            ctx.set_shutdown_note("the test file is deleted when Cairn's process ends");
            noted.send(()).unwrap();
            let _ = wait.recv();
            end(JobState::Succeeded, "late")
        }),
    );
    note_set.recv().unwrap();
    let outcomes = f.host.shutdown();
    assert_eq!(outcomes[0].action, HostShutdownAction::StopTimedOut);
    release.send(()).unwrap();
    f.finish(job.id);
    let rows = f.rows("test_op");
    assert_eq!(rows.len(), 2, "never a second final row: {rows:?}");
    assert_eq!(rows[1].outcome, "stop_timed_out");
    assert_eq!(
        rows[1].detail.as_deref(),
        Some("the test file is deleted when Cairn's process ends")
    );
    let finished = f.host.snapshot(job.id).unwrap();
    assert_eq!(finished.state, JobState::Succeeded);
    let text = fs::read_to_string(finished.log_path.unwrap()).unwrap();
    assert_eq!(
        text.matches("the test file is deleted").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("late ·"), "{text}");
}

#[test]
fn shutdown_leaves_a_job_that_cannot_be_stopped() {
    let f = fixture_with(|c| c.stop_wait = Duration::from_millis(30));
    let (release, wait) = mpsc::channel::<()>();
    let job = f.start(
        JobSpec {
            cancellable: false,
            ..audited("app_install")
        },
        Box::new(move |_| {
            let _ = wait.recv();
            end(JobState::Succeeded, "installed")
        }),
    );
    let outcomes = f.host.shutdown();
    assert_eq!(outcomes[0].action, HostShutdownAction::LeftRunning);
    let rows = f.rows("test_op");
    assert_eq!(rows[1].outcome, "left_running");
    assert_eq!(rows[1].detail.as_deref(), Some(DEFAULT_SHUTDOWN_NOTE));
    release.send(()).unwrap();
    f.finish(job.id);
    assert_eq!(f.rows("test_op").len(), 2);
}

#[test]
fn shutdown_without_a_running_job_lists_nothing() {
    let f = fixture();
    let job = f.start(spec("scan"), done("ok"));
    f.finish(job.id);
    assert!(f.host.shutdown().is_empty());
}

#[test]
fn final_row_is_written_once_when_shutdown_races_finish() {
    let finals = Arc::new(AtomicUsize::new(0));
    for round in 0..20u64 {
        let f = fixture_with(|c| c.stop_wait = Duration::from_millis(10));
        let delay = Duration::from_millis(5 + round % 10);
        let job = f.start(
            audited("speed_test"),
            Box::new(move |ctx| {
                while !ctx.closing() {
                    thread::sleep(Duration::from_millis(1));
                }
                // Ends around the time the shutdown wait runs out.
                thread::sleep(delay);
                end(JobState::Cancelled, "stopped")
            }),
        );
        let outcomes = f.host.shutdown();
        f.finish(job.id);
        let rows = f.rows("test_op");
        assert_eq!(rows.len(), 2, "round {round}: {rows:?}");
        let expected = match outcomes[0].action {
            HostShutdownAction::Stopped => "cancelled",
            HostShutdownAction::StopTimedOut => "stop_timed_out",
            HostShutdownAction::LeftRunning => unreachable!(),
        };
        assert_eq!(rows[1].outcome, expected, "round {round}");
        finals.fetch_add(1, Ordering::SeqCst);
    }
    assert_eq!(finals.load(Ordering::SeqCst), 20);
}

#[test]
fn wait_returns_the_latest_snapshot_after_the_timeout() {
    let f = fixture();
    let (job, release) = blocked(&f, spec("scan"));
    let snapshot = f.host.wait(job.id, Duration::from_millis(20)).unwrap();
    assert_eq!(snapshot.state, JobState::Running);
    assert!(f.host.wait(HostJobId(u64::MAX), TICK).is_none());
    drop(release);
    f.finish(job.id);
}
