//! Starting a tool's process with its output going to a file, outside Cairn's job object
//! when the tool must not end with Cairn.
//!
//! A process started with `CREATE_BREAKAWAY_FROM_JOB` is not a member of the job Cairn runs
//! in (a terminal, an IDE or a launcher may put it in one), so closing that job cannot kill
//! it. The flag is ignored when Cairn is in no job, and
//! `CreateProcess` fails with `ERROR_ACCESS_DENIED` when the job does not allow breaking
//! away; whether breaking away worked is read from that result.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::System::Threading::{TerminateProcess, CREATE_BREAKAWAY_FROM_JOB};

use super::catalog::ToolInfo;
use crate::win::paths::system_dir;
use crate::win::process::{hardened_command, CREATE_NO_WINDOW};
use crate::{Error, Result};

/// Whether a tool's process is started outside Cairn's job object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachPolicy {
    /// Stays in the job: a tool that can be stopped may end with Cairn.
    None,
    /// Breaks away when the job allows it, else starts inside it.
    Prefer,
    /// Breaks away, or does not start at all.
    Require,
}

impl DetachPolicy {
    /// Tools that can be stopped stay in the job; tools that require detaching refuse to
    /// start inside it; every other tool prefers to break away.
    pub fn for_tool(info: &ToolInfo) -> DetachPolicy {
        if info.cancellable {
            DetachPolicy::None
        } else if info.requires_detach {
            DetachPolicy::Require
        } else {
            DetachPolicy::Prefer
        }
    }
}

/// What to start: an absolute program path in System32, its arguments, and the detach
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub detach: DetachPolicy,
}

/// A started process.
#[derive(Debug)]
pub struct Launched {
    pub process: Box<dyn RunningProcess>,
    /// Started with `CREATE_BREAKAWAY_FROM_JOB`: it keeps running when Cairn ends.
    pub detached: bool,
}

/// Starts tool processes. [`SystemLauncher`] starts real ones; tests substitute scripted
/// launchers.
pub trait Launcher: Send + Sync + fmt::Debug {
    /// Starts `command` with stdout and stderr going to `output`.
    fn launch(&self, command: &CommandSpec, output: File) -> Result<Launched>;
}

/// A running tool process.
pub trait RunningProcess: Send + fmt::Debug {
    fn id(&self) -> u32;
    /// The exit code once the process has ended; never blocks.
    fn try_wait(&mut self) -> Result<Option<i32>>;
    /// Ends the process with exit code 1. `Ok(true)` when this ended a process that was
    /// still running, `Ok(false)` when it had already ended on its own.
    fn kill(&mut self) -> Result<bool>;
}

/// Exit code of a process [`RunningProcess::kill`] ends.
const KILLED_EXIT_CODE: u32 = 1;

impl RunningProcess for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(Child::try_wait(self)?.map(|status| status.code().unwrap_or(-1)))
    }

    fn kill(&mut self) -> Result<bool> {
        if Child::try_wait(self)?.is_some() {
            return Ok(false);
        }
        // Called directly rather than through `Child::kill`, which also succeeds for a
        // process that has ended: TerminateProcess fails for a process that has ended, so
        // success means this call ended it.
        // SAFETY: the handle is owned by `self`, which outlives the call.
        match unsafe { TerminateProcess(HANDLE(self.as_raw_handle()), KILLED_EXIT_CODE) } {
            Ok(()) => Ok(true),
            Err(_) if matches!(Child::try_wait(self), Ok(Some(_))) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The refusal of a tool that must break away when the job does not allow it.
pub const DETACH_REFUSED: &str = "Cairn is running inside a Windows job that would \
     stop this repair if Cairn closed or crashed. Start Cairn from the Start \
     menu or a desktop shortcut and try again.";

/// Starts a prepared command with the given creation flags.
pub(crate) type Spawner = fn(&mut Command, u32) -> io::Result<Box<dyn RunningProcess>>;

fn spawn_child(command: &mut Command, flags: u32) -> io::Result<Box<dyn RunningProcess>> {
    command.creation_flags(flags);
    Ok(Box::new(command.spawn()?))
}

/// The folder a [`SystemLauncher`] starts programs from; programs anywhere else are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramRoot {
    /// System32, as `GetSystemDirectoryW` reports it.
    SystemDir,
    /// Another folder, such as a package's install folder.
    Dir(PathBuf),
}

impl ProgramRoot {
    fn path(&self) -> Result<PathBuf> {
        match self {
            ProgramRoot::SystemDir => system_dir(),
            ProgramRoot::Dir(dir) => Ok(dir.clone()),
        }
    }
}

/// Starts real processes from one folder (System32 unless chosen otherwise) through
/// [`hardened_command`], without a console window, with stdout and stderr going to the
/// output file.
#[derive(Debug, Clone)]
pub struct SystemLauncher {
    root: ProgramRoot,
    spawn: Spawner,
}

impl SystemLauncher {
    /// Starts programs in System32; the same as `within(ProgramRoot::SystemDir)`.
    pub fn new() -> SystemLauncher {
        SystemLauncher::within(ProgramRoot::SystemDir)
    }

    /// Starts programs below `root` only.
    pub fn within(root: ProgramRoot) -> SystemLauncher {
        SystemLauncher {
            root,
            spawn: spawn_child,
        }
    }

    /// A System32 launcher that hands the prepared command and its creation flags to
    /// `spawn` instead of starting it.
    #[cfg(test)]
    pub(crate) fn with_spawner(spawn: Spawner) -> SystemLauncher {
        SystemLauncher::spawner_for_tests(ProgramRoot::SystemDir, spawn)
    }

    /// A launcher for programs below `root` that hands the prepared command and its
    /// creation flags to `spawn` instead of starting it.
    #[cfg(test)]
    pub(crate) fn spawner_for_tests(root: ProgramRoot, spawn: Spawner) -> SystemLauncher {
        SystemLauncher { root, spawn }
    }
}

impl Default for SystemLauncher {
    fn default() -> Self {
        SystemLauncher::new()
    }
}

impl Launcher for SystemLauncher {
    fn launch(&self, spec: &CommandSpec, output: File) -> Result<Launched> {
        let root = self.root.path()?;
        if !spec.program.starts_with(&root) {
            return Err(Error::Other(format!(
                "{} is not a program in {}",
                spec.program.display(),
                root.display()
            )));
        }
        let mut command = hardened_command(&spec.program)?;
        // The child writes through these handles; this process never reads through them,
        // because they share the child's file position.
        command
            .args(&spec.args)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        if spec.detach == DetachPolicy::None {
            let process = (self.spawn)(&mut command, CREATE_NO_WINDOW)?;
            return Ok(Launched {
                process,
                detached: false,
            });
        }
        let breakaway = CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB.0;
        match (self.spawn)(&mut command, breakaway) {
            Ok(process) => Ok(Launched {
                process,
                detached: true,
            }),
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
                if spec.detach == DetachPolicy::Require {
                    return Err(Error::Other(DETACH_REFUSED.into()));
                }
                tracing::info!(
                    program = %spec.program.display(),
                    "this job does not allow breaking away; the tool runs inside it"
                );
                let process = (self.spawn)(&mut command, CREATE_NO_WINDOW)?;
                Ok(Launched {
                    process,
                    detached: false,
                })
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::tools::catalog::{catalog, ToolId};

    /// A process that was never started.
    #[derive(Debug)]
    struct NotStarted;

    impl RunningProcess for NotStarted {
        fn id(&self) -> u32 {
            0
        }

        fn try_wait(&mut self) -> Result<Option<i32>> {
            Ok(Some(0))
        }

        fn kill(&mut self) -> Result<bool> {
            Ok(false)
        }
    }

    thread_local! {
        /// Creation flags of every spawn request on this thread.
        static SPAWNS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    }

    /// Refuses every request that breaks away, as a job without breakaway rights does, and
    /// never starts anything.
    fn job_without_breakaway(_: &mut Command, flags: u32) -> io::Result<Box<dyn RunningProcess>> {
        SPAWNS.with(|s| s.borrow_mut().push(flags));
        if flags & CREATE_BREAKAWAY_FROM_JOB.0 != 0 {
            return Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32));
        }
        Ok(Box::new(NotStarted))
    }

    fn take_spawns() -> Vec<u32> {
        SPAWNS.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn spec(detach: DetachPolicy) -> CommandSpec {
        CommandSpec {
            program: system_dir().unwrap().join("sfc.exe"),
            args: vec!["/scannow".into()],
            detach,
        }
    }

    fn output() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("out.raw")).unwrap();
        (dir, file)
    }

    #[test]
    fn require_detach_refuses_when_breakaway_is_denied() {
        let launcher = SystemLauncher::with_spawner(job_without_breakaway);
        let (_dir, file) = output();
        take_spawns();
        let err = launcher
            .launch(&spec(DetachPolicy::Require), file)
            .unwrap_err();
        assert_eq!(err.to_string(), DETACH_REFUSED);
        let spawns = take_spawns();
        assert_eq!(spawns.len(), 1, "no retry inside the job: {spawns:x?}");
        assert_ne!(spawns[0] & CREATE_BREAKAWAY_FROM_JOB.0, 0);
        assert_ne!(spawns[0] & CREATE_NO_WINDOW, 0);
    }

    #[test]
    fn prefer_detach_falls_back_and_reports_not_detached() {
        let launcher = SystemLauncher::with_spawner(job_without_breakaway);
        let (_dir, file) = output();
        take_spawns();
        let launched = launcher.launch(&spec(DetachPolicy::Prefer), file).unwrap();
        assert!(!launched.detached);
        let spawns = take_spawns();
        assert_eq!(spawns.len(), 2, "{spawns:x?}");
        assert_ne!(spawns[0] & CREATE_BREAKAWAY_FROM_JOB.0, 0);
        assert_eq!(spawns[1], CREATE_NO_WINDOW);
    }

    #[test]
    fn allowed_breakaway_reports_detached_and_none_never_asks() {
        fn allow(_: &mut Command, flags: u32) -> io::Result<Box<dyn RunningProcess>> {
            SPAWNS.with(|s| s.borrow_mut().push(flags));
            Ok(Box::new(NotStarted))
        }
        let launcher = SystemLauncher::with_spawner(allow);
        take_spawns();
        let (_dir, file) = output();
        assert!(
            launcher
                .launch(&spec(DetachPolicy::Require), file)
                .unwrap()
                .detached
        );
        let (_dir, file) = output();
        assert!(
            !launcher
                .launch(&spec(DetachPolicy::None), file)
                .unwrap()
                .detached
        );
        assert_eq!(
            take_spawns(),
            [
                CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB.0,
                CREATE_NO_WINDOW
            ]
        );
    }

    #[test]
    fn programs_outside_system32_are_refused() {
        let launcher = SystemLauncher::with_spawner(job_without_breakaway);
        let (dir, file) = output();
        take_spawns();
        let outside = CommandSpec {
            program: dir.path().join("sfc.exe"),
            args: Vec::new(),
            detach: DetachPolicy::None,
        };
        assert!(launcher.launch(&outside, file).is_err());
        assert!(take_spawns().is_empty());
    }

    #[test]
    fn a_launcher_within_a_folder_refuses_system32_programs() {
        let (dir, file) = output();
        let launcher = SystemLauncher::spawner_for_tests(
            ProgramRoot::Dir(dir.path().to_path_buf()),
            job_without_breakaway,
        );
        take_spawns();
        let err = launcher
            .launch(&spec(DetachPolicy::None), file)
            .unwrap_err();
        assert!(err.to_string().contains("is not a program in"), "{err}");
        assert!(take_spawns().is_empty());

        let inside = CommandSpec {
            program: dir.path().join("winget.exe"),
            args: vec!["--info".into()],
            detach: DetachPolicy::None,
        };
        let file = File::create(dir.path().join("out2.raw")).unwrap();
        let launched = launcher.launch(&inside, file).unwrap();
        assert!(!launched.detached);
        assert_eq!(take_spawns(), [CREATE_NO_WINDOW]);

        // The real constructor refuses the same way without starting anything.
        let real = SystemLauncher::within(ProgramRoot::Dir(dir.path().to_path_buf()));
        let file = File::create(dir.path().join("out3.raw")).unwrap();
        assert!(real.launch(&spec(DetachPolicy::None), file).is_err());
        assert_eq!(
            SystemLauncher::new().root,
            SystemLauncher::within(ProgramRoot::SystemDir).root
        );
    }

    #[test]
    fn policy_per_tool() {
        for info in catalog() {
            let expected = match info.id {
                ToolId::DiskCheck => DetachPolicy::None,
                ToolId::SfcScan | ToolId::DismRestore => DetachPolicy::Require,
                _ => DetachPolicy::Prefer,
            };
            assert_eq!(DetachPolicy::for_tool(info), expected, "{}", info.id);
        }
    }
}
