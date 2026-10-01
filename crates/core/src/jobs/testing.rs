//! Scripted stand-ins for tool and job processes, shared by the tests of every module that
//! launches processes through [`Launcher`]. Nothing here starts a real process.

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::tools::launch::{CommandSpec, DetachPolicy, Launched, Launcher, RunningProcess};
use crate::{Error, Result};

/// One step of a scripted process, sent by the test.
#[derive(Debug)]
pub(crate) enum Step {
    /// Appends bytes to the process's output file.
    Write(Vec<u8>),
    /// The process ends with this exit code.
    Exit(i32),
    /// Waiting on the process fails with this message.
    Lose(String),
}

/// A process whose output and exit follow the [`Step`]s a test sends.
#[derive(Debug)]
pub(crate) struct ScriptedProcess {
    pub(crate) output: File,
    pub(crate) steps: Receiver<Step>,
    pub(crate) exit: Option<i32>,
    /// A stop seems to work but the process keeps running.
    pub(crate) ignore_kill: bool,
    /// Counts dropped processes.
    pub(crate) dropped: Arc<AtomicUsize>,
}

impl RunningProcess for ScriptedProcess {
    fn id(&self) -> u32 {
        4242
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.exit {
            return Ok(Some(code));
        }
        loop {
            match self.steps.try_recv() {
                Ok(Step::Write(bytes)) => self.output.write_all(&bytes)?,
                Ok(Step::Exit(code)) => {
                    self.exit = Some(code);
                    return Ok(Some(code));
                }
                Ok(Step::Lose(message)) => return Err(Error::Other(message)),
                Err(TryRecvError::Empty) => return Ok(None),
                // The test is over: the process ends with it.
                Err(TryRecvError::Disconnected) => {
                    self.exit = Some(-1);
                    return Ok(Some(-1));
                }
            }
        }
    }

    /// Like a real process, one that has ended cannot be ended again. With `ignore_kill`
    /// the stop seems to work but the process keeps running.
    fn kill(&mut self) -> Result<bool> {
        if self.try_wait()?.is_some() {
            return Ok(false);
        }
        if !self.ignore_kill {
            self.exit = Some(1);
        }
        Ok(true)
    }
}

impl Drop for ScriptedProcess {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A launch that waits until the test lets it continue.
#[derive(Debug)]
pub(crate) struct Gate {
    /// Signalled when a launch reaches the gate.
    pub(crate) entered: Sender<()>,
    /// The launch continues once this receives.
    pub(crate) release: Receiver<()>,
}

/// Starts scripted processes; each launch takes the next script queued with
/// [`ScriptedLauncher::script`]. A launch records its command, runs `before_launch` (tests
/// assert there what must already have happened, such as an audit row), waits at the gate
/// when one is set, then fails with `fail` or starts the next script.
pub(crate) struct ScriptedLauncher {
    /// Launches of a detaching command report `detached`.
    pub(crate) detached: bool,
    pub(crate) ignore_kill: bool,
    pub(crate) fail: Option<String>,
    pub(crate) gate: Mutex<Option<Gate>>,
    pub(crate) scripts: Mutex<VecDeque<Receiver<Step>>>,
    /// Every launched command, in order; shared so a `before_launch` hook can read the
    /// command being launched (the last one).
    pub(crate) launches: Arc<Mutex<Vec<CommandSpec>>>,
    pub(crate) dropped: Arc<AtomicUsize>,
    pub(crate) before_launch: Option<Box<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for ScriptedLauncher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedLauncher")
            .field("detached", &self.detached)
            .field("ignore_kill", &self.ignore_kill)
            .field("fail", &self.fail)
            .field("launches", &self.launches.lock().len())
            .finish_non_exhaustive()
    }
}

impl Default for ScriptedLauncher {
    fn default() -> Self {
        ScriptedLauncher::new()
    }
}

impl ScriptedLauncher {
    /// Launches that stay attached, can be stopped, never fail and check nothing first.
    pub(crate) fn new() -> ScriptedLauncher {
        ScriptedLauncher {
            detached: false,
            ignore_kill: false,
            fail: None,
            gate: Mutex::new(None),
            scripts: Mutex::new(VecDeque::new()),
            launches: Arc::new(Mutex::new(Vec::new())),
            dropped: Arc::new(AtomicUsize::new(0)),
            before_launch: None,
        }
    }

    /// Queues the script of the next launch; the test drives it through the sender.
    pub(crate) fn script(&self) -> Sender<Step> {
        let (sender, receiver) = mpsc::channel();
        self.scripts.lock().push_back(receiver);
        sender
    }

    /// Commands launched so far.
    pub(crate) fn launched(&self) -> Vec<CommandSpec> {
        self.launches.lock().clone()
    }
}

impl Launcher for ScriptedLauncher {
    fn launch(&self, command: &CommandSpec, output: File) -> Result<Launched> {
        self.launches.lock().push(command.clone());
        if let Some(hook) = &self.before_launch {
            hook();
        }
        let gate = self.gate.lock().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.release.recv().unwrap();
        }
        if let Some(message) = &self.fail {
            return Err(Error::Other(message.clone()));
        }
        let steps = self
            .scripts
            .lock()
            .pop_front()
            .expect("a script for every launch");
        Ok(Launched {
            process: Box::new(ScriptedProcess {
                output,
                steps,
                exit: None,
                ignore_kill: self.ignore_kill,
                dropped: Arc::clone(&self.dropped),
            }),
            detached: self.detached && command.detach != DetachPolicy::None,
        })
    }
}

/// Fails the test when anything is launched.
#[derive(Debug)]
pub(crate) struct PanicLauncher;

impl Launcher for PanicLauncher {
    fn launch(&self, command: &CommandSpec, _output: File) -> Result<Launched> {
        panic!("nothing may be launched here: {command:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> CommandSpec {
        CommandSpec {
            program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec!["/c".into(), "exit".into()],
            detach: DetachPolicy::Prefer,
        }
    }

    fn output() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("out.raw")).unwrap();
        (dir, file)
    }

    #[test]
    fn a_scripted_process_writes_exits_and_counts_its_drop() {
        let mut launcher = ScriptedLauncher::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let launches = Arc::clone(&launcher.launches);
        launcher.before_launch = Some(Box::new(move || {
            assert_eq!(launches.lock().len(), 1, "the command is recorded first");
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        launcher.detached = true;
        let script = launcher.script();
        let (dir, file) = output();
        let mut launched = launcher.launch(&command(), file).unwrap();
        assert!(launched.detached);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        script.send(Step::Write(b"hello".to_vec())).unwrap();
        assert_eq!(launched.process.try_wait().unwrap(), None);
        script.send(Step::Exit(3)).unwrap();
        assert_eq!(launched.process.try_wait().unwrap(), Some(3));
        assert!(!launched.process.kill().unwrap());
        drop(launched);
        assert_eq!(launcher.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(dir.path().join("out.raw")).unwrap(), b"hello");
        assert_eq!(launcher.launched(), [command()]);
    }

    #[test]
    fn a_failing_launch_and_a_lost_process_report_errors() {
        let mut launcher = ScriptedLauncher::new();
        launcher.fail = Some("cannot start".into());
        let (_dir, file) = output();
        assert_eq!(
            launcher.launch(&command(), file).unwrap_err().to_string(),
            "cannot start"
        );

        let launcher = ScriptedLauncher::new();
        let script = launcher.script();
        let (_dir, file) = output();
        let mut launched = launcher.launch(&command(), file).unwrap();
        assert!(!launched.detached);
        script
            .send(Step::Lose("the handle is invalid".into()))
            .unwrap();
        assert!(launched.process.try_wait().is_err());
        drop(script);
        assert_eq!(launched.process.try_wait().unwrap(), Some(-1));
    }

    #[test]
    #[should_panic(expected = "nothing may be launched here")]
    fn the_panic_launcher_panics() {
        let (_dir, file) = output();
        let _ = PanicLauncher.launch(&command(), file);
    }
}
