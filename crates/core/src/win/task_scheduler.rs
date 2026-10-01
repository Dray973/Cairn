//! Task Scheduler 2.0 through ITaskService.
//!
//! Reads a registered task's enabled flag and run state, and sets the enabled flag. For the
//! scheduled maintenance task it also creates and deletes a folder directly under the root,
//! registers a task from XML with an explicit security descriptor, deletes it, reads its
//! definition, security descriptor and run history, and starts it on demand. Nothing here
//! journals state; the debloat engine and scheduled maintenance record the baseline before
//! they change a task.

use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    VARIANT_BOOL,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::TaskScheduler::{
    IExecAction, IRegisteredTask, ITaskFolder, ITaskService, IWeeklyTrigger,
    TaskScheduler as TASK_SCHEDULER_CLSID, TASK_ACTION_EXEC, TASK_ACTION_TYPE,
    TASK_CREATE_OR_UPDATE, TASK_ENUM_HIDDEN, TASK_LOGON_INTERACTIVE_TOKEN, TASK_LOGON_TYPE,
    TASK_RUNLEVEL_TYPE, TASK_RUN_IGNORE_CONSTRAINTS, TASK_STATE, TASK_TRIGGER_TYPE2,
    TASK_TRIGGER_WEEKLY,
};
use windows::Win32::System::Variant::VARIANT;

use super::com::ComApartment;
use super::is_win32;
use crate::safety::rollback::FolderRemoval;
use crate::{Error, Result};

/// `OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION`, as `GetSecurityDescriptor` of
/// a task or folder takes them.
const OWNER_AND_DACL: i32 = 0x1 | 0x4;

/// A connection to the local Task Scheduler service as the account this process runs as.
/// It belongs to the thread that opened it.
pub struct TaskScheduler {
    root: ITaskFolder,
    /// Kept for the lifetime of the connection that `root` was opened through.
    _service: ITaskService,
    /// Declared last, so COM stays initialized until both interfaces are released.
    _com: ComApartment,
}

impl TaskScheduler {
    /// Connects to the Task Scheduler service of this PC.
    pub fn connect() -> Result<TaskScheduler> {
        let com = ComApartment::enter();
        // SAFETY: COM is initialized on this thread while `com` lives, and the interface is
        // stored next to it, so it is released before COM is uninitialized.
        let service: ITaskService =
            unsafe { CoCreateInstance(&TASK_SCHEDULER_CLSID, None, CLSCTX_INPROC_SERVER) }?;
        let empty = VARIANT::default();
        // SAFETY: the four arguments are empty VARIANTs that outlive the call; empty means
        // this PC and the account this process runs as.
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }?;
        // SAFETY: the BSTR outlives the call; `service` is connected.
        let root = unsafe { service.GetFolder(&BSTR::from("\\")) }?;
        Ok(TaskScheduler {
            root,
            _service: service,
            _com: com,
        })
    }

    /// The registered task at `path` (for example `\Microsoft\Windows\Autochk\Proxy`), or
    /// `None` when the task or one of its folders does not exist. Fails for a path that
    /// [`is_valid_task_path`] rejects.
    pub fn task(&self, path: &str) -> Result<Option<RegisteredTask<'_>>> {
        if !is_valid_task_path(path) {
            return Err(Error::Other(format!("not a scheduled task path: {path}")));
        }
        // SAFETY: the BSTR outlives the call; `root` is a live interface of this connection.
        match unsafe { self.root.GetTask(&BSTR::from(path)) } {
            Ok(task) => Ok(Some(RegisteredTask {
                task,
                path: path.to_string(),
                _scheduler: PhantomData,
            })),
            Err(e) if is_missing(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The folder at `path` (for example `\Cairn`), or `None` when it does not exist.
    fn folder(&self, path: &str) -> Result<Option<ITaskFolder>> {
        if !is_valid_task_path(path) {
            return Err(Error::Other(format!(
                "not a Task Scheduler folder path: {path}"
            )));
        }
        // SAFETY: the BSTR outlives the call; `root` is a live interface of this connection.
        match unsafe { self.root.GetFolder(&BSTR::from(path)) } {
            Ok(folder) => Ok(Some(folder)),
            Err(e) if is_missing(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Owner and DACL of the folder at `path` as SDDL, or `None` when it does not exist.
    pub(crate) fn folder_sddl(&self, path: &str) -> Result<Option<String>> {
        let Some(folder) = self.folder(path)? else {
            return Ok(None);
        };
        // SAFETY: `folder` is a live interface; the call only returns a string.
        let sddl = unsafe { folder.GetSecurityDescriptor(OWNER_AND_DACL) }?;
        Ok(Some(sddl.to_string()))
    }

    /// Creates the folder `name` directly under the root with the security descriptor
    /// `sddl` (`None`: the one it inherits). Returns false when it already existed, in which
    /// case its security is left as it is.
    pub(crate) fn create_folder(&self, name: &str, sddl: Option<&str>) -> Result<bool> {
        check_folder_name(name)?;
        let security = sddl.map(VARIANT::from).unwrap_or_default();
        // SAFETY: the BSTR and the VARIANT outlive the call; `root` is a live interface.
        match unsafe { self.root.CreateFolder(&BSTR::from(name), &security) } {
            Ok(_) => Ok(true),
            Err(e) if is_win32(&e, ERROR_ALREADY_EXISTS) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Deletes the folder `name` directly under the root when it holds no task and no
    /// subfolder.
    pub(crate) fn delete_folder_if_empty(&self, name: &str) -> Result<FolderRemoval> {
        check_folder_name(name)?;
        // SAFETY: the BSTR outlives the call; `root` is a live interface.
        match unsafe { self.root.DeleteFolder(&BSTR::from(name), 0) } {
            Ok(()) => Ok(FolderRemoval::Removed),
            Err(e) if is_win32(&e, ERROR_DIR_NOT_EMPTY) => Ok(FolderRemoval::NotEmpty),
            Err(e) if is_missing(&e) => Ok(FolderRemoval::Missing),
            Err(e) => Err(e.into()),
        }
    }

    /// Registers (creates or replaces) the task `name` in the existing folder `\<folder>`
    /// from `xml`, to run with the account's interactive token, with the security descriptor
    /// `sddl` (`None`: the one it inherits).
    pub(crate) fn register_xml(
        &self,
        folder: &str,
        name: &str,
        xml: &str,
        sddl: Option<&str>,
    ) -> Result<()> {
        check_folder_name(name)?;
        let folder = self.folder(&format!(r"\{folder}"))?.ok_or_else(|| {
            Error::Other(format!(r"the Task Scheduler folder \{folder} is missing"))
        })?;
        let empty = VARIANT::default();
        let security = sddl.map(VARIANT::from).unwrap_or_default();
        // SAFETY: every BSTR and VARIANT argument outlives the call; the empty VARIANTs take
        // the account and password from the XML's principal (an interactive token needs no
        // password); `folder` is a live interface.
        unsafe {
            folder.RegisterTask(
                &BSTR::from(name),
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &security,
            )
        }?;
        Ok(())
    }

    /// Deletes the task `name` in the folder `\<folder>`. Returns false when the task or the
    /// folder does not exist.
    pub(crate) fn delete_task(&self, folder: &str, name: &str) -> Result<bool> {
        check_folder_name(name)?;
        let Some(folder) = self.folder(&format!(r"\{folder}"))? else {
            return Ok(false);
        };
        // SAFETY: the BSTR outlives the call; `folder` is a live interface.
        match unsafe { folder.DeleteTask(&BSTR::from(name), 0) } {
            Ok(()) => Ok(true),
            Err(e) if is_missing(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Names of the tasks in the folder `\<folder>`, hidden ones included; empty when the
    /// folder does not exist.
    pub(crate) fn task_names(&self, folder: &str) -> Result<Vec<String>> {
        let Some(folder) = self.folder(&format!(r"\{folder}"))? else {
            return Ok(Vec::new());
        };
        // SAFETY: `folder` is a live interface; the collection is released when dropped.
        let tasks = unsafe { folder.GetTasks(TASK_ENUM_HIDDEN.0) }?;
        // SAFETY: `tasks` is a live collection; the call only returns a value.
        let count = unsafe { tasks.Count() }?;
        let mut names = Vec::new();
        for index in 1..=count {
            // SAFETY: indexes are 1-based and within the count read above; the VARIANT
            // outlives the call.
            let task = unsafe { tasks.get_Item(&VARIANT::from(index)) }?;
            // SAFETY: `task` is a live interface; the call only returns a string.
            names.push(unsafe { task.Name() }?.to_string());
        }
        Ok(names)
    }
}

/// A folder or task name directly under a folder: non-empty, without `\`, `/` or NUL.
fn check_folder_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains(['\\', '/', '\0']) {
        Err(Error::Other(format!("not a Task Scheduler name: {name:?}")))
    } else {
        Ok(())
    }
}

impl fmt::Debug for TaskScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskScheduler").finish_non_exhaustive()
    }
}

/// One registered task, valid while the [`TaskScheduler`] it came from is.
pub struct RegisteredTask<'a> {
    task: IRegisteredTask,
    path: String,
    _scheduler: PhantomData<&'a TaskScheduler>,
}

impl RegisteredTask<'_> {
    /// The path the task was looked up by.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the task is enabled, so its triggers can start it.
    pub fn enabled(&self) -> Result<bool> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(unsafe { self.task.Enabled() }?.as_bool())
    }

    /// The task's current run state.
    pub fn state(&self) -> Result<TaskRunState> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(TaskRunState::from_raw(unsafe { self.task.State() }?))
    }

    /// Enables or disables the task. A run already in progress is not stopped; disabling
    /// only keeps later runs from starting.
    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        // SAFETY: `task` is a live interface; the flag is passed by value.
        unsafe { self.task.SetEnabled(VARIANT_BOOL::from(enabled)) }?;
        Ok(())
    }

    /// The task's owner and DACL as SDDL.
    pub(crate) fn sddl(&self) -> Result<String> {
        // SAFETY: `task` is a live interface; the call only returns a string.
        Ok(unsafe { self.task.GetSecurityDescriptor(OWNER_AND_DACL) }?.to_string())
    }

    /// The task's definition as XML. It holds the principal's account as it was registered
    /// (a SID stays a SID), which the principal object reports as an account name.
    pub(crate) fn xml(&self) -> Result<String> {
        // SAFETY: `task` is a live interface; the call only returns a string.
        Ok(unsafe { self.task.Xml() }?.to_string())
    }

    /// When the task last ran, as an OLE automation date (local time); Task Scheduler
    /// reports 1999-11-30 when it never ran.
    pub(crate) fn last_run_time(&self) -> Result<f64> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(unsafe { self.task.LastRunTime() }?)
    }

    /// The exit code or Task Scheduler status of the last run.
    pub(crate) fn last_result(&self) -> Result<i32> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(unsafe { self.task.LastTaskResult() }?)
    }

    /// When the task runs next, as an OLE automation date (local time).
    pub(crate) fn next_run_time(&self) -> Result<f64> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(unsafe { self.task.NextRunTime() }?)
    }

    /// Runs Task Scheduler missed since the last one.
    pub(crate) fn missed_runs(&self) -> Result<i32> {
        // SAFETY: `task` is a live interface; the call only returns a value.
        Ok(unsafe { self.task.NumberOfMissedRuns() }?)
    }

    /// The triggers, actions, principal and the settings the maintenance checks read.
    pub(crate) fn definition_summary(&self) -> Result<DefinitionSummary> {
        // SAFETY: `task` is a live interface; every object read below is a live interface
        // returned by the call before it, and every out pointer is a valid local.
        unsafe {
            let definition = self.task.Definition()?;
            let triggers = definition.Triggers()?;
            let mut count = 0i32;
            triggers.Count(&mut count)?;
            let mut trigger_list = Vec::new();
            for index in 1..=count {
                let trigger = triggers.get_Item(index)?;
                let mut kind = TASK_TRIGGER_TYPE2::default();
                trigger.Type(&mut kind)?;
                let mut start = BSTR::new();
                trigger.StartBoundary(&mut start)?;
                let mut enabled = VARIANT_BOOL::default();
                trigger.Enabled(&mut enabled)?;
                let weekly = if kind == TASK_TRIGGER_WEEKLY {
                    let weekly: IWeeklyTrigger = trigger.cast()?;
                    let mut days = 0i16;
                    weekly.DaysOfWeek(&mut days)?;
                    let mut weeks = 0i16;
                    weekly.WeeksInterval(&mut weeks)?;
                    Some((days, weeks))
                } else {
                    None
                };
                trigger_list.push(TriggerSummary {
                    start_boundary: start.to_string(),
                    enabled: enabled.as_bool(),
                    weekly,
                });
            }
            let actions = definition.Actions()?;
            let mut count = 0i32;
            actions.Count(&mut count)?;
            let mut action_list = Vec::new();
            for index in 1..=count {
                let action = actions.get_Item(index)?;
                let mut kind = TASK_ACTION_TYPE::default();
                action.Type(&mut kind)?;
                let exec = if kind == TASK_ACTION_EXEC {
                    let exec: IExecAction = action.cast()?;
                    let mut path = BSTR::new();
                    exec.Path(&mut path)?;
                    let mut arguments = BSTR::new();
                    exec.Arguments(&mut arguments)?;
                    let mut working_dir = BSTR::new();
                    exec.WorkingDirectory(&mut working_dir)?;
                    Some(ExecSummary {
                        path: path.to_string(),
                        arguments: arguments.to_string(),
                        working_dir: working_dir.to_string(),
                    })
                } else {
                    None
                };
                action_list.push(exec);
            }
            let principal = definition.Principal()?;
            let mut user_id = BSTR::new();
            principal.UserId(&mut user_id)?;
            let mut logon_type = TASK_LOGON_TYPE::default();
            principal.LogonType(&mut logon_type)?;
            let mut run_level = TASK_RUNLEVEL_TYPE::default();
            principal.RunLevel(&mut run_level)?;
            let settings = definition.Settings()?;
            let flag = |read: &dyn Fn(*mut VARIANT_BOOL) -> windows::core::Result<()>| {
                let mut value = VARIANT_BOOL::default();
                read(&mut value).map(|()| value.as_bool())
            };
            Ok(DefinitionSummary {
                triggers: trigger_list,
                actions: action_list,
                user_id: user_id.to_string(),
                logon_type: logon_type.0,
                run_level: run_level.0,
                disallow_on_batteries: flag(&|p| settings.DisallowStartIfOnBatteries(p))?,
                run_only_if_idle: flag(&|p| settings.RunOnlyIfIdle(p))?,
                wake_to_run: flag(&|p| settings.WakeToRun(p))?,
                start_when_available: flag(&|p| settings.StartWhenAvailable(p))?,
            })
        }
    }

    /// Starts the task now, ignoring its conditions (idle, power), as its own account;
    /// returns once Task Scheduler accepted the request.
    pub(crate) fn run_ignoring_conditions(&self) -> Result<()> {
        // SAFETY: `task` is a live interface; the empty VARIANT passes no parameters, the
        // empty BSTR names no other user, and the returned running task is released at once.
        unsafe {
            self.task.RunEx(
                &VARIANT::default(),
                TASK_RUN_IGNORE_CONSTRAINTS.0,
                0,
                &BSTR::new(),
            )
        }?;
        Ok(())
    }
}

/// A task's definition as the maintenance checks read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionSummary {
    pub(crate) triggers: Vec<TriggerSummary>,
    /// One entry per action; `None` for an action that does not start a program.
    pub(crate) actions: Vec<Option<ExecSummary>>,
    /// The principal's account: a SID or an account name, as Task Scheduler reports it.
    pub(crate) user_id: String,
    /// `TASK_LOGON_TYPE` (3: interactive token).
    pub(crate) logon_type: i32,
    /// `TASK_RUNLEVEL_TYPE` (1: highest available).
    pub(crate) run_level: i32,
    pub(crate) disallow_on_batteries: bool,
    pub(crate) run_only_if_idle: bool,
    pub(crate) wake_to_run: bool,
    pub(crate) start_when_available: bool,
}

/// One trigger of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerSummary {
    pub(crate) start_boundary: String,
    pub(crate) enabled: bool,
    /// `(DaysOfWeek, WeeksInterval)` of a weekly trigger; `None` for other kinds.
    pub(crate) weekly: Option<(i16, i16)>,
}

/// An action that starts a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecSummary {
    pub(crate) path: String,
    pub(crate) arguments: String,
    pub(crate) working_dir: String,
}

impl fmt::Debug for RegisteredTask<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTask")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Run state of a registered task (`TASK_STATE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunState {
    Unknown,
    Disabled,
    Queued,
    Ready,
    Running,
}

impl TaskRunState {
    /// Maps `TASK_STATE` values 0 to 4; anything else is [`TaskRunState::Unknown`].
    pub fn from_raw(state: TASK_STATE) -> TaskRunState {
        match state.0 {
            1 => TaskRunState::Disabled,
            2 => TaskRunState::Queued,
            3 => TaskRunState::Ready,
            4 => TaskRunState::Running,
            _ => TaskRunState::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskRunState::Unknown => "unknown",
            TaskRunState::Disabled => "disabled",
            TaskRunState::Queued => "queued",
            TaskRunState::Ready => "ready",
            TaskRunState::Running => "running",
        }
    }
}

/// A full task path: `\` followed by one or more non-empty names separated by `\`, with no
/// trailing `\`, no `/` and no NUL.
pub fn is_valid_task_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('\\') else {
        return false;
    };
    !rest.is_empty() && !path.contains(['/', '\0']) && rest.split('\\').all(|name| !name.is_empty())
}

/// True for the errors Task Scheduler returns when a task or folder does not exist.
fn is_missing(e: &windows::core::Error) -> bool {
    is_win32(e, ERROR_FILE_NOT_FOUND) || is_win32(e, ERROR_PATH_NOT_FOUND)
}

/// A throwaway task under `\PCOptimizerSelfTest\` for tests that change a real task.
#[cfg(test)]
pub(crate) mod probe {
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing::warn;
    use windows::core::BSTR;
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY};
    use windows::Win32::System::TaskScheduler::{
        ITaskFolder, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
    };
    use windows::Win32::System::Variant::VARIANT;

    use super::{is_missing, TaskScheduler};
    use crate::win::is_win32;
    use crate::Result;

    /// Name of the probe folder, directly under the root folder.
    pub(crate) const FOLDER: &str = "PCOptimizerSelfTest";

    /// An enabled, hidden task without triggers that cannot be started on demand, so it
    /// never runs.
    const PROBE_XML: &str = r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Cairn self-test probe. It has no triggers and never runs.</Description></RegistrationInfo>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><Enabled>true</Enabled><AllowStartOnDemand>false</AllowStartOnDemand><Hidden>true</Hidden></Settings>
  <Actions Context="Author"><Exec><Command>%SystemRoot%\System32\cmd.exe</Command><Arguments>/c exit 0</Arguments></Exec></Actions>
</Task>"#;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A registered probe task, deleted together with its folder when dropped.
    pub(crate) struct ProbeTask {
        folder: ITaskFolder,
        name: String,
        path: String,
        /// Declared last: the folder is released before the connection and COM go away.
        scheduler: TaskScheduler,
    }

    impl ProbeTask {
        /// Creates `\PCOptimizerSelfTest` if needed and registers `Probe-<pid>-<n>` in it.
        pub(crate) fn register() -> Result<ProbeTask> {
            let scheduler = TaskScheduler::connect()?;
            let empty = VARIANT::default();
            let folder_name = BSTR::from(FOLDER);
            // SAFETY: the BSTR and the empty VARIANT (default security) outlive the call.
            let folder = match unsafe { scheduler.root.CreateFolder(&folder_name, &empty) } {
                Ok(folder) => folder,
                Err(e) if is_win32(&e, ERROR_ALREADY_EXISTS) => {
                    // SAFETY: the BSTR outlives the call.
                    unsafe { scheduler.root.GetFolder(&folder_name) }?
                }
                Err(e) => return Err(e.into()),
            };
            let name = format!(
                "Probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            // Built before registering, so a failed registration still removes the folder.
            let probe = ProbeTask {
                folder,
                path: format!(r"\{FOLDER}\{name}"),
                name,
                scheduler,
            };
            // SAFETY: every BSTR and VARIANT argument outlives the call; the empty VARIANTs
            // register the task for the account this process runs as.
            unsafe {
                probe.folder.RegisterTask(
                    &BSTR::from(probe.name.as_str()),
                    &BSTR::from(PROBE_XML),
                    TASK_CREATE_OR_UPDATE.0,
                    &empty,
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )
            }?;
            Ok(probe)
        }

        /// Full path of the probe task.
        pub(crate) fn path(&self) -> &str {
            &self.path
        }
    }

    impl fmt::Debug for ProbeTask {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ProbeTask")
                .field("path", &self.path)
                .finish_non_exhaustive()
        }
    }

    impl Drop for ProbeTask {
        fn drop(&mut self) {
            // SAFETY: the BSTR outlives the call; `folder` is a live interface.
            if let Err(e) = unsafe { self.folder.DeleteTask(&BSTR::from(self.name.as_str()), 0) } {
                if !is_missing(&e) {
                    warn!(task = %self.path, error = %e, "cannot delete the probe task");
                }
            }
            // SAFETY: the BSTR outlives the call; `root` is a live interface.
            if let Err(e) = unsafe { self.scheduler.root.DeleteFolder(&BSTR::from(FOLDER), 0) } {
                if !is_missing(&e) && !is_win32(&e, ERROR_DIR_NOT_EMPTY) {
                    warn!(folder = FOLDER, error = %e, "cannot delete the probe folder");
                }
            }
        }
    }

    /// Whether the folder at `path` (for example `\PCOptimizerSelfTest`) exists.
    pub(crate) fn folder_exists(scheduler: &TaskScheduler, path: &str) -> Result<bool> {
        // SAFETY: the BSTR outlives the call; `root` is a live interface.
        match unsafe { scheduler.root.GetFolder(&BSTR::from(path)) } {
            Ok(_) => Ok(true),
            Err(e) if is_missing(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_paths_are_validated() {
        for path in [
            r"\Microsoft\Windows\Autochk\Proxy",
            r"\Top",
            r"\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser",
        ] {
            assert!(is_valid_task_path(path), "{path}");
        }
        for path in [
            "",
            "\\",
            "\\\\",
            r"Microsoft\Windows\Autochk\Proxy",
            r"\Microsoft\Windows\",
            r"\Microsoft\\Windows",
            "/Microsoft/Windows",
            r"\Microsoft/Windows",
            "\\Microsoft\0Windows",
        ] {
            assert!(!is_valid_task_path(path), "{path:?}");
        }
    }

    #[test]
    fn task_run_state_maps_raw_values() {
        let states: Vec<TaskRunState> = (0..=5)
            .map(|n| TaskRunState::from_raw(TASK_STATE(n)))
            .collect();
        assert_eq!(
            states,
            [
                TaskRunState::Unknown,
                TaskRunState::Disabled,
                TaskRunState::Queued,
                TaskRunState::Ready,
                TaskRunState::Running,
                TaskRunState::Unknown,
            ]
        );
        assert_eq!(
            TaskRunState::from_raw(TASK_STATE(-1)),
            TaskRunState::Unknown
        );
        assert_eq!(TaskRunState::Running.label(), "running");
        assert_eq!(
            serde_json::to_value(TaskRunState::Ready).unwrap(),
            serde_json::json!("ready")
        );
    }

    #[test]
    fn missing_tasks_and_folders_read_as_none() {
        // Read-only: only looks tasks up.
        let scheduler = TaskScheduler::connect().unwrap();
        assert!(scheduler
            .task(r"\PCOptimizerSelfTest\NoSuchTask")
            .unwrap()
            .is_none());
        assert!(scheduler
            .task(r"\PCOptimizerNoSuchFolder\X")
            .unwrap()
            .is_none());
        assert!(scheduler.task(r"PCOptimizerSelfTest\NoSuchTask").is_err());
        assert!(scheduler.task(r"\PCOptimizerSelfTest\").is_err());
    }
}
