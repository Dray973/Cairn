//! Independent reads run in parallel under one deadline.
//!
//! Each job runs on its own named thread inside `catch_unwind`. The caller waits until every
//! job has finished or the shared deadline has passed, whichever comes first; a job that is
//! still running then is left to finish on its own and its result is dropped. A job whose
//! thread cannot be spawned runs inline on the calling thread.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One job: a thread name (for example "health-defender") and the work.
pub(crate) type Job<T> = (&'static str, Box<dyn FnOnce() -> T + Send>);

/// Runs every job on its own thread and returns their results in job order. A job that
/// panics, or that has not finished when `deadline` (counted from the call) has passed,
/// gives `None`.
pub(crate) fn run_all<T: Send + 'static>(jobs: Vec<Job<T>>, deadline: Duration) -> Vec<Option<T>> {
    run_all_with(jobs, deadline, &|name, body| {
        thread::Builder::new()
            .name(name.to_string())
            .spawn(body)
            .map(drop)
    })
}

/// Starts a named thread running the body; `Err` when no thread could be started.
type Spawner<'a> = &'a dyn Fn(&'static str, Box<dyn FnOnce() + Send>) -> io::Result<()>;

/// [`run_all`] with the thread start handed to `spawn`.
fn run_all_with<T: Send + 'static>(
    jobs: Vec<Job<T>>,
    deadline: Duration,
    spawn: Spawner<'_>,
) -> Vec<Option<T>> {
    let started = Instant::now();
    let mut receivers = Vec::with_capacity(jobs.len());
    for (name, work) in jobs {
        let (tx, rx) = mpsc::channel::<Option<T>>();
        // The work sits in a shared slot so it can still run inline when the thread does not
        // start: a spawner that fails drops its closure, never the slot's content.
        let slot = Arc::new(Mutex::new(Some(work)));
        let thread_slot = Arc::clone(&slot);
        let body: Box<dyn FnOnce() + Send> = Box::new(move || {
            let work = thread_slot.lock().ok().and_then(|mut s| s.take());
            if let Some(work) = work {
                let _ = tx.send(guarded(work));
            }
        });
        if spawn(name, body).is_err() {
            let work = slot.lock().ok().and_then(|mut s| s.take());
            let result = work.and_then(guarded);
            let (inline_tx, inline_rx) = mpsc::channel();
            let _ = inline_tx.send(result);
            receivers.push(inline_rx);
            continue;
        }
        receivers.push(rx);
    }
    receivers
        .into_iter()
        .map(|rx| {
            let left = deadline.saturating_sub(started.elapsed());
            rx.recv_timeout(left).ok().flatten()
        })
        .collect()
}

/// Runs `work`; a panic gives `None`.
fn guarded<T>(work: Box<dyn FnOnce() -> T + Send>) -> Option<T> {
    panic::catch_unwind(AssertUnwindSafe(work)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job<T: Send + 'static>(
        name: &'static str,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Job<T> {
        (name, Box::new(work))
    }

    #[test]
    fn a_job_that_misses_the_deadline_gives_none() {
        let started = Instant::now();
        let results = run_all(
            vec![
                job("test-fast", || 1),
                job("test-slow", || {
                    thread::sleep(Duration::from_millis(600));
                    2
                }),
                job("test-other", || 3),
            ],
            Duration::from_millis(50),
        );
        assert_eq!(results, vec![Some(1), None, Some(3)]);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_panicking_job_gives_none_and_the_others_still_run() {
        let results = run_all(
            vec![
                job("test-ok", || "a"),
                job("test-panic", || panic!("reader crashed")),
                job("test-ok-2", || "b"),
            ],
            Duration::from_secs(5),
        );
        assert_eq!(results, vec![Some("a"), None, Some("b")]);
    }

    #[test]
    fn jobs_run_on_named_threads() {
        let results = run_all(
            vec![job("health-test-name", || {
                thread::current().name().map(str::to_string)
            })],
            Duration::from_secs(5),
        );
        assert_eq!(results, vec![Some(Some("health-test-name".to_string()))]);
    }

    #[test]
    fn jobs_run_inline_when_no_thread_starts() {
        let caller = thread::current().id();
        let results = run_all_with(
            vec![
                job("test-inline", move || thread::current().id() == caller),
                job("test-inline-panic", || -> bool { panic!("inline crash") }),
            ],
            Duration::from_secs(5),
            &|_, _| Err(io::Error::other("no threads")),
        );
        assert_eq!(results, vec![Some(true), None]);
    }

    #[test]
    fn no_jobs_give_no_results() {
        let results: Vec<Option<u8>> = run_all(Vec::new(), Duration::from_millis(10));
        assert!(results.is_empty());
    }
}
