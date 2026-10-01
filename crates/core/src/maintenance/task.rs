//! The scheduled maintenance task: its XML, its definition as Task Scheduler reports it, the
//! store every change goes through, and its removal.
//!
//! Every Task Scheduler call goes through [`TaskDefinitionStore`], so the whole flow can be
//! tested with [`fake::FakeDefinitions`]. The remover used by rollback only touches Cairn's
//! own objects: a task `\Cairn\Maintenance-<SID>` or a task directly under the self-test
//! folder `\PCOptimizerSelfTest`, and only those two folders.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};

use super::config::{parse_task_arguments, ScheduleConfig, ScheduleDay, ScheduleTime};
use super::{
    EXIT_ATTENTION, EXIT_FAILED, EXIT_REFUSED, EXIT_SKIPPED, EXIT_STOPPED, TASK_FOLDER,
    TASK_NAME_PREFIX,
};
use crate::safety::rollback::{FolderRemoval, TaskDefinitionRemover};
use crate::win::acl::{sddl_security, untrusted_writer, AclRole};
use crate::win::session::sid_string;
use crate::win::task_scheduler::{
    is_valid_task_path, DefinitionSummary, TaskRunState, TaskScheduler,
};
use crate::win::wide;
use crate::{Error, Result};

/// Security of the `\Cairn` folder when Cairn creates it: owner Administrators, the root
/// folder's ACE shapes for Administrators and SYSTEM, protected from inheritance, and read
/// access only for Authenticated Users (the root lets them create tasks). The owner is set
/// explicitly, so the folder passes the folder check whoever Task Scheduler would make its owner.
pub(crate) const FOLDER_SDDL: &str =
    "O:BAD:P(A;CI;FA;;;BA)(A;OI;0x1f019f;;;BA)(A;CI;FA;;;SY)(A;OI;0x1f019f;;;SY)(A;OICI;FR;;;AU)";
/// Security every maintenance task is registered with: owner Administrators, full access for
/// Administrators and SYSTEM, read access for Authenticated Users.
pub(crate) const TASK_SDDL: &str = "O:BAD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FR;;;AU)";
/// Folder of the tasks that sandbox tests register.
pub(crate) const SANDBOX_FOLDER: &str = "PCOptimizerSelfTest";

const DESCRIPTION: &str = "Cairn scheduled maintenance: cleans the selected temporary files and \
                           caches and runs read-only system checks. It never repairs anything. \
                           Turn it off in Cairn to remove it.";
/// `TASK_LOGON_INTERACTIVE_TOKEN`.
pub(crate) const LOGON_INTERACTIVE_TOKEN: i32 = 3;
/// `TASK_RUNLEVEL_HIGHEST`.
pub(crate) const RUNLEVEL_HIGHEST: i32 = 1;

// ───────────────────────────── XML ─────────────────────────────

/// Everything the task's XML is built from.
#[derive(Debug, Clone)]
pub(crate) struct TaskSpec {
    pub(crate) path: String,
    /// `S-1-5-…` of the account the task runs as.
    pub(crate) user_sid: String,
    pub(crate) program: PathBuf,
    pub(crate) arguments: String,
    pub(crate) working_dir: PathBuf,
    /// First run, local wall-clock time.
    pub(crate) start: NaiveDateTime,
    pub(crate) day: ScheduleDay,
    pub(crate) enabled: bool,
}

/// `text` with `& < > " '` replaced by their XML entities.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The task's XML: a weekly trigger on one day, the account's interactive token with the
/// highest rights it has, idle and AC power only, never waking the PC, at most one instance,
/// and a four-hour limit.
pub(crate) fn task_xml(spec: &TaskSpec) -> String {
    let start = spec.start.format("%Y-%m-%dT%H:%M:00");
    format!(
        concat!(
            r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">"#,
            "\r\n",
            "  <RegistrationInfo><Author>Cairn</Author><Description>{description}</Description></RegistrationInfo>\r\n",
            "  <Triggers><CalendarTrigger><StartBoundary>{start}</StartBoundary><Enabled>true</Enabled>",
            "<ScheduleByWeek><DaysOfWeek><{day} /></DaysOfWeek><WeeksInterval>1</WeeksInterval></ScheduleByWeek>",
            "</CalendarTrigger></Triggers>\r\n",
            r#"  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType>"#,
            "<RunLevel>HighestAvailable</RunLevel></Principal></Principals>\r\n",
            "  <Settings>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<AllowHardTerminate>true</AllowHardTerminate>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            "<RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
            "<IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>",
            "<AllowStartOnDemand>true</AllowStartOnDemand>",
            "<Enabled>{enabled}</Enabled>",
            "<Hidden>false</Hidden>",
            "<RunOnlyIfIdle>true</RunOnlyIfIdle>",
            "<WakeToRun>false</WakeToRun>",
            "<ExecutionTimeLimit>PT4H</ExecutionTimeLimit>",
            "<Priority>7</Priority>",
            "</Settings>\r\n",
            r#"  <Actions Context="Author"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments>"#,
            "<WorkingDirectory>{working_dir}</WorkingDirectory></Exec></Actions>\r\n",
            "</Task>\r\n"
        ),
        description = escape(DESCRIPTION),
        start = start,
        day = spec.day.xml_element(),
        user = escape(&spec.user_sid),
        enabled = spec.enabled,
        program = escape(&spec.program.to_string_lossy()),
        arguments = escape(&spec.arguments),
        working_dir = escape(&spec.working_dir.to_string_lossy()),
    )
}

// ───────────────────────────── Registered definition ─────────────────────────────

/// A registered task as Task Scheduler reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredDefinition {
    pub enabled: bool,
    pub state: TaskRunState,
    /// `None` when the task does not have exactly one trigger, a weekly one.
    pub weekly: Option<WeeklyTrigger>,
    /// `None` when the task does not have exactly one action, one that starts a program.
    pub exec: Option<ExecAction>,
    /// The principal's account as a SID (an account name when it cannot be resolved).
    pub user_id: String,
    pub logon_type: i32,
    pub run_level: i32,
    pub disallow_on_batteries: bool,
    pub run_only_if_idle: bool,
    pub wake_to_run: bool,
    pub start_when_available: bool,
    pub last_run_time: Option<NaiveDateTime>,
    pub last_result: i32,
    pub next_run_time: Option<NaiveDateTime>,
    pub missed_runs: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeeklyTrigger {
    /// `DaysOfWeek`: Sunday 1 … Saturday 64.
    pub days_mask: i16,
    pub weeks_interval: i16,
    pub start_boundary: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecAction {
    pub path: String,
    pub arguments: String,
    pub working_dir: String,
}

impl RegisteredDefinition {
    /// The parts of `summary` the checks read, with the account resolved to a SID.
    fn from_summary(
        enabled: bool,
        state: TaskRunState,
        summary: DefinitionSummary,
        resolve_account: impl Fn(&str) -> String,
    ) -> RegisteredDefinition {
        let weekly = match summary.triggers.as_slice() {
            [trigger] => trigger.weekly.map(|(days, weeks)| WeeklyTrigger {
                days_mask: days,
                weeks_interval: weeks,
                start_boundary: trigger.start_boundary.clone(),
                enabled: trigger.enabled,
            }),
            _ => None,
        };
        let exec = match summary.actions.as_slice() {
            [Some(exec)] => Some(ExecAction {
                path: exec.path.clone(),
                arguments: exec.arguments.clone(),
                working_dir: exec.working_dir.clone(),
            }),
            _ => None,
        };
        RegisteredDefinition {
            enabled,
            state,
            weekly,
            exec,
            user_id: resolve_account(&summary.user_id),
            logon_type: summary.logon_type,
            run_level: summary.run_level,
            disallow_on_batteries: summary.disallow_on_batteries,
            run_only_if_idle: summary.run_only_if_idle,
            wake_to_run: summary.wake_to_run,
            start_when_available: summary.start_when_available,
            last_run_time: None,
            last_result: 0,
            next_run_time: None,
            missed_runs: 0,
        }
    }
}

// ───────────────────────────── Store ─────────────────────────────

/// Task Scheduler as scheduled maintenance uses it. Folders are single-level paths such as
/// `\Cairn`; tasks are paths such as `\Cairn\Maintenance-S-1-5-21-…`.
pub(crate) trait TaskDefinitionStore {
    /// Owner and DACL of the folder as SDDL; `None` when it does not exist.
    fn folder_sddl(&self, folder: &str) -> Result<Option<String>>;
    /// Creates the folder with `sddl`; false when it already existed (left unchanged).
    fn create_folder(&self, folder: &str, sddl: &str) -> Result<bool>;
    fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval>;
    fn read(&self, path: &str) -> Result<Option<RegisteredDefinition>>;
    /// Creates or replaces the task from `xml`, always with [`TASK_SDDL`].
    fn register(&self, path: &str, xml: &str) -> Result<()>;
    /// False when the task did not exist.
    fn delete(&self, path: &str) -> Result<bool>;
    /// Owner and DACL of the task as SDDL; `None` when it does not exist.
    fn task_sddl(&self, path: &str) -> Result<Option<String>>;
    /// Starts the task now, ignoring its conditions.
    fn run_now(&self, path: &str) -> Result<()>;
    /// Names of the tasks in the folder; empty when it does not exist.
    fn task_names(&self, folder: &str) -> Result<Vec<String>>;
}

impl<T: TaskDefinitionStore + ?Sized> TaskDefinitionStore for &T {
    fn folder_sddl(&self, folder: &str) -> Result<Option<String>> {
        (**self).folder_sddl(folder)
    }
    fn create_folder(&self, folder: &str, sddl: &str) -> Result<bool> {
        (**self).create_folder(folder, sddl)
    }
    fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval> {
        (**self).delete_folder_if_empty(folder)
    }
    fn read(&self, path: &str) -> Result<Option<RegisteredDefinition>> {
        (**self).read(path)
    }
    fn register(&self, path: &str, xml: &str) -> Result<()> {
        (**self).register(path, xml)
    }
    fn delete(&self, path: &str) -> Result<bool> {
        (**self).delete(path)
    }
    fn task_sddl(&self, path: &str) -> Result<Option<String>> {
        (**self).task_sddl(path)
    }
    fn run_now(&self, path: &str) -> Result<()> {
        (**self).run_now(path)
    }
    fn task_names(&self, folder: &str) -> Result<Vec<String>> {
        (**self).task_names(folder)
    }
}

/// `\Folder\Name` split into its two names.
pub(crate) fn split_task_path(path: &str) -> Result<(&str, &str)> {
    let bad = || Error::Other(format!("not a task path directly under a folder: {path}"));
    if !is_valid_task_path(path) {
        return Err(bad());
    }
    let (folder, name) = path[1..].split_once('\\').ok_or_else(bad)?;
    if name.contains('\\') {
        return Err(bad());
    }
    Ok((folder, name))
}

/// A single-level folder path (`\Cairn`) or bare name, as the name.
fn folder_name(folder: &str) -> Result<&str> {
    let name = folder.strip_prefix('\\').unwrap_or(folder);
    if name.is_empty() || name.contains(['\\', '/', '\0']) {
        Err(Error::Other(format!(
            "not a folder directly under the Task Scheduler root: {folder}"
        )))
    } else {
        Ok(name)
    }
}

/// Task Scheduler of this PC, through one connection used on the thread that opened it.
#[derive(Debug)]
pub(crate) struct LiveDefinitions {
    scheduler: TaskScheduler,
}

impl LiveDefinitions {
    pub(crate) fn connect() -> Result<LiveDefinitions> {
        Ok(LiveDefinitions {
            scheduler: TaskScheduler::connect()?,
        })
    }
}

impl TaskDefinitionStore for LiveDefinitions {
    fn folder_sddl(&self, folder: &str) -> Result<Option<String>> {
        self.scheduler
            .folder_sddl(&format!(r"\{}", folder_name(folder)?))
    }

    fn create_folder(&self, folder: &str, sddl: &str) -> Result<bool> {
        self.scheduler
            .create_folder(folder_name(folder)?, Some(sddl))
    }

    fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval> {
        self.scheduler.delete_folder_if_empty(folder_name(folder)?)
    }

    fn read(&self, path: &str) -> Result<Option<RegisteredDefinition>> {
        split_task_path(path)?;
        let Some(task) = self.scheduler.task(path)? else {
            return Ok(None);
        };
        let xml = task.xml().ok();
        let mut def = RegisteredDefinition::from_summary(
            task.enabled()?,
            task.state()?,
            task.definition_summary()?,
            |reported| principal_account(xml.as_deref(), reported),
        );
        def.last_run_time = task.last_run_time().ok().and_then(ole_date);
        def.last_result = task.last_result().unwrap_or(0);
        def.next_run_time = task.next_run_time().ok().and_then(ole_date);
        def.missed_runs = task.missed_runs().unwrap_or(0);
        Ok(Some(def))
    }

    fn register(&self, path: &str, xml: &str) -> Result<()> {
        let (folder, name) = split_task_path(path)?;
        self.scheduler
            .register_xml(folder, name, xml, Some(TASK_SDDL))
    }

    fn delete(&self, path: &str) -> Result<bool> {
        let (folder, name) = split_task_path(path)?;
        self.scheduler.delete_task(folder, name)
    }

    fn task_sddl(&self, path: &str) -> Result<Option<String>> {
        split_task_path(path)?;
        self.scheduler.task(path)?.map(|t| t.sddl()).transpose()
    }

    fn run_now(&self, path: &str) -> Result<()> {
        split_task_path(path)?;
        let task = self
            .scheduler
            .task(path)?
            .ok_or_else(|| Error::Other(format!("the task {path} does not exist")))?;
        task.run_ignoring_conditions()
    }

    fn task_names(&self, folder: &str) -> Result<Vec<String>> {
        self.scheduler.task_names(folder_name(folder)?)
    }
}

/// `S-1-` followed by numbers separated by single dashes.
fn is_sid(text: &str) -> bool {
    let Some(rest) = text
        .get(..4)
        .filter(|head| head.eq_ignore_ascii_case("S-1-"))
        .and_then(|_| text.get(4..))
    else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The account of the principal in a task's XML when it is written as a SID. Task Scheduler
/// keeps the SID a task was registered with there, while its principal object reports that
/// account as a name without its domain.
fn principal_sid(xml: &str) -> Option<String> {
    let principals = between(xml, "<Principals>", "</Principals>")?;
    let account = between(principals, "<UserId>", "</UserId>")?.trim();
    is_sid(account).then(|| account.to_string())
}

/// The text between the first `open` and the first `close` after it.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let rest = text.get(text.find(open)? + open.len()..)?;
    rest.get(..rest.find(close)?)
}

/// The SID of the account a task runs as: the one its XML holds, otherwise the account its
/// principal object names (`reported`), resolved.
fn principal_account(xml: Option<&str>, reported: &str) -> String {
    xml.and_then(principal_sid)
        .unwrap_or_else(|| resolve_account(reported))
}

/// The SID of the account Task Scheduler names; the name itself when it is already a SID or
/// cannot be resolved.
fn resolve_account(name: &str) -> String {
    let name = name.trim();
    if is_sid(name) {
        return name.to_string();
    }
    account_sid(name).unwrap_or_else(|e| {
        tracing::debug!(account = name, error = %e, "cannot resolve a task's account");
        name.to_string()
    })
}

/// `S-1-…` of the account `name` (`DOMAIN\user` or a bare user name).
fn account_sid(name: &str) -> Result<String> {
    let text = wide(name);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut sid_use = SID_NAME_USE::default();
    // SAFETY: size probe with no buffers; the lengths are valid out pointers.
    let probe = unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(text.as_ptr()),
            None,
            &mut sid_len,
            None,
            &mut domain_len,
            &mut sid_use,
        )
    };
    if sid_len == 0 {
        return Err(match probe {
            Err(e) => e.into(),
            Ok(()) => Error::Other(format!("no SID for the account {name}")),
        });
    }
    let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    domain_len = domain.len() as u32;
    // SAFETY: `sid` holds at least `sid_len` bytes (8-byte aligned) and `domain` holds
    // `domain_len` UTF-16 units; both outlive the call.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(text.as_ptr()),
            Some(PSID(sid.as_mut_ptr() as *mut core::ffi::c_void)),
            &mut sid_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut sid_use,
        )?;
    }
    sid_string(PSID(sid.as_mut_ptr() as *mut core::ffi::c_void))
}

// ───────────────────────────── Own objects and removal ─────────────────────────────

/// `S-1-5-` followed by numbers separated by single dashes.
fn is_account_sid(text: &str) -> bool {
    is_sid(text)
        && text
            .get(..6)
            .is_some_and(|head| head.eq_ignore_ascii_case("S-1-5-"))
}

/// Whether `path` is a task Cairn creates and may delete: `\Cairn\Maintenance-<SID>` or a
/// task directly under `\PCOptimizerSelfTest`. Folder and prefix compare ignoring ASCII case.
pub(crate) fn is_own_task_path(path: &str) -> bool {
    let Ok((folder, name)) = split_task_path(path) else {
        return false;
    };
    if folder.eq_ignore_ascii_case(SANDBOX_FOLDER) {
        return true;
    }
    folder.eq_ignore_ascii_case(TASK_FOLDER)
        && name
            .get(..TASK_NAME_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(TASK_NAME_PREFIX))
        && name
            .get(TASK_NAME_PREFIX.len()..)
            .is_some_and(is_account_sid)
}

/// Whether `folder` (`\Cairn` or a bare name) is a folder Cairn may delete when empty.
pub(crate) fn is_own_folder(folder: &str) -> bool {
    folder_name(folder).is_ok_and(|name| {
        name.eq_ignore_ascii_case(TASK_FOLDER) || name.eq_ignore_ascii_case(SANDBOX_FOLDER)
    })
}

/// Deletes only Cairn's own tasks and folders; anything else is refused before the store is
/// reached.
#[derive(Debug)]
pub(crate) struct OwnRemover<S> {
    store: S,
}

impl<S> OwnRemover<S> {
    pub(crate) fn new(store: S) -> OwnRemover<S> {
        OwnRemover { store }
    }
}

impl<S: TaskDefinitionStore> TaskDefinitionRemover for OwnRemover<S> {
    fn delete(&self, path: &str) -> Result<bool> {
        if !is_own_task_path(path) {
            return Err(Error::Other(format!(
                "{path} is not a task Cairn creates, so it was not deleted"
            )));
        }
        self.store.delete(path)
    }

    fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval> {
        if !is_own_folder(folder) {
            return Err(Error::Other(format!(
                "{folder} is not a Task Scheduler folder Cairn creates, so it was not deleted"
            )));
        }
        self.store.delete_folder_if_empty(folder)
    }
}

/// Task Scheduler access for rolling back task definitions Cairn registered.
pub(crate) fn live_remover() -> Result<Box<dyn TaskDefinitionRemover>> {
    let store = LiveDefinitions::connect()
        .map_err(|e| Error::Other(format!("cannot connect to Task Scheduler: {e}")))?;
    Ok(Box::new(OwnRemover::new(store)))
}

// ───────────────────────────── Security checks ─────────────────────────────

/// Why the task with this security descriptor could be changed by others; `None` when only
/// SYSTEM, Administrators and TrustedInstaller can change it.
pub(crate) fn task_object_problem(sddl: &str) -> Result<Option<String>> {
    Ok(untrusted_writer(&sddl_security(sddl)?, AclRole::TaskObject))
}

/// Why others could add or change tasks in the folder with this security descriptor.
pub(crate) fn task_folder_problem(sddl: &str) -> Result<Option<String>> {
    Ok(untrusted_writer(&sddl_security(sddl)?, AclRole::TaskFolder))
}

// ───────────────────────────── Interpretation ─────────────────────────────

pub(crate) const DRIFT_SCHEDULE: &str = "the schedule was changed outside Cairn";
pub(crate) const DRIFT_COMMAND: &str = "the task's command was changed outside Cairn";
pub(crate) const DRIFT_BATTERY: &str = "Windows may start it on battery power";
pub(crate) const DRIFT_IDLE: &str = "it doesn't wait until the PC is idle";
pub(crate) const DRIFT_WAKE: &str = "it may wake the PC";
pub(crate) const DRIFT_ACCOUNT: &str = "it runs as another account";
pub(crate) const DRIFT_RIGHTS: &str = "it isn't set to run with administrator rights";

/// What the task should look like for this copy of Cairn and this account.
#[derive(Debug, Clone)]
pub(crate) struct Expected {
    pub(crate) program: PathBuf,
    pub(crate) journal: PathBuf,
    pub(crate) user_sid: String,
}

/// A registered task read back: its schedule, how it differs from what Cairn registers, and
/// whether it already is exactly the schedule asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Interpreted {
    /// `None` when the trigger or the command is not one Cairn writes.
    pub(crate) config: Option<ScheduleConfig>,
    pub(crate) drift: Vec<String>,
    pub(crate) matches_spec: bool,
    /// The program the task starts.
    pub(crate) program: Option<String>,
}

/// "HH:MM" of a `StartBoundary` such as `2026-10-04T12:00:00`.
fn boundary_time(boundary: &str) -> Option<ScheduleTime> {
    if boundary.get(10..11) != Some("T") {
        return None;
    }
    ScheduleTime::parse(boundary.get(11..16)?).ok()
}

fn same_path(a: &str, b: &Path) -> bool {
    a.trim().trim_matches('"').to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Reads a registered definition against what Cairn would register (`expected`) and,
/// when given, the schedule the user asks for (`spec`).
pub(crate) fn interpret(
    def: &RegisteredDefinition,
    expected: &Expected,
    spec: Option<&ScheduleConfig>,
) -> Interpreted {
    let mut drift = Vec::new();
    let schedule = def
        .weekly
        .as_ref()
        .filter(|w| w.enabled && w.weeks_interval == 1)
        .and_then(|w| {
            Some((
                ScheduleDay::from_mask(w.days_mask)?,
                boundary_time(&w.start_boundary)?,
            ))
        });
    if schedule.is_none() {
        drift.push(DRIFT_SCHEDULE.to_string());
    }
    let parsed = def
        .exec
        .as_ref()
        .and_then(|e| parse_task_arguments(&e.arguments));
    if parsed.is_none() {
        drift.push(DRIFT_COMMAND.to_string());
    }
    if let Some(exec) = &def.exec {
        if !same_path(&exec.path, &expected.program) {
            drift.push(format!(
                "it runs another copy of Cairn ({})",
                exec.path.trim()
            ));
        }
    }
    if let Some((journal, _)) = &parsed {
        if !same_path(&journal.to_string_lossy(), &expected.journal) {
            drift.push(format!(
                "it writes to another journal ({})",
                journal.display()
            ));
        }
    }
    if !def.disallow_on_batteries {
        drift.push(DRIFT_BATTERY.to_string());
    }
    if !def.run_only_if_idle {
        drift.push(DRIFT_IDLE.to_string());
    }
    if def.wake_to_run {
        drift.push(DRIFT_WAKE.to_string());
    }
    if !def
        .user_id
        .trim()
        .eq_ignore_ascii_case(expected.user_sid.trim())
        || def.logon_type != LOGON_INTERACTIVE_TOKEN
    {
        drift.push(DRIFT_ACCOUNT.to_string());
    }
    if def.run_level != RUNLEVEL_HIGHEST {
        drift.push(DRIFT_RIGHTS.to_string());
    }
    let config = match (schedule, &parsed) {
        (Some((day, time)), Some((_, selection))) => Some(ScheduleConfig {
            day,
            time,
            targets: selection.targets.clone(),
            system_file_check: selection.system_file_check,
            component_store_check: selection.component_store_check,
        }),
        _ => None,
    };
    let matches_spec = def.enabled && drift.is_empty() && spec.is_some() && config.as_ref() == spec;
    Interpreted {
        config,
        drift,
        matches_spec,
        program: def.exec.as_ref().map(|e| e.path.trim().to_string()),
    }
}

// ───────────────────────────── Run history ─────────────────────────────

/// What the last result of the task means; `None` for success.
pub fn task_result_text(code: i32) -> Option<String> {
    let text = match code as u32 {
        0 => return None,
        c if c == EXIT_ATTENTION as u32 => "The last run found something to look at",
        c if c == EXIT_FAILED as u32 => "A step of the last run failed",
        c if c == EXIT_STOPPED as u32 => "The last run stopped early",
        c if c == EXIT_SKIPPED as u32 => "The last run was skipped",
        c if c == EXIT_REFUSED as u32 => {
            "The last run was refused: its settings or data folder were not accepted"
        }
        1 => "The maintenance program reported an error; see the maintenance log",
        0x0004_1301 => "It is running",
        0x0004_1303 => "It hasn't run yet",
        0x0004_1306 => "Task Scheduler or someone stopped it",
        0x8007_02E4 => {
            "Windows couldn't start it with administrator rights; the account may no longer be \
             an administrator"
        }
        0x8007_0002 | 0x8007_0003 => {
            "The program the task runs is missing; reinstall Cairn or turn maintenance off"
        }
        0xC000_0135 => "A file the program needs is missing (Visual C++ runtime); reinstall Cairn",
        0x8004_131F => "An earlier run was still going",
        0x8007_10E0 => "Windows refused to start it",
        0x8007_052E | 0x8007_0569 => "Windows couldn't sign in as your account to start it",
        0x8007_04EC => "A policy on this PC blocks the program",
        0xC000_013A | 0x4001_0004 => "It was stopped (sign-out or shutdown)",
        other => return Some(format!("It ended with code 0x{other:08X}")),
    };
    Some(text.to_string())
}

/// An OLE automation date (days since 1899-12-30, local time) as a date and time; `None`
/// before 2000, which covers Task Scheduler's "never" (1999-11-30).
pub(crate) fn ole_date(value: f64) -> Option<NaiveDateTime> {
    if !value.is_finite() || value < 36_526.0 {
        return None;
    }
    let base = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let seconds = (value * 86_400.0).round();
    if seconds > i64::MAX as f64 / 2.0 {
        return None;
    }
    base.checked_add_signed(TimeDelta::try_seconds(seconds as i64)?)
}

// ───────────────────────────── Test store ─────────────────────────────

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use super::*;

    /// A task the fake holds.
    #[derive(Debug, Clone)]
    pub(crate) struct FakeTask {
        /// The path as registered, case kept.
        pub(crate) path: String,
        pub(crate) xml: String,
        pub(crate) definition: RegisteredDefinition,
        pub(crate) sddl: String,
    }

    /// Called before every write of the fake with its description.
    pub(crate) type WriteHook = Box<dyn Fn(&str)>;

    /// Task Scheduler in memory. Keys compare ignoring ASCII case, like Task Scheduler.
    #[derive(Default)]
    pub(crate) struct FakeDefinitions {
        pub(crate) tasks: RefCell<BTreeMap<String, FakeTask>>,
        /// Folder (`\Cairn`) → SDDL.
        pub(crate) folders: RefCell<BTreeMap<String, String>>,
        /// Every change, in order: `create_folder \Cairn`, `register <path>`, `delete <path>`,
        /// `delete_folder \Cairn`, `run <path>`.
        pub(crate) ops: RefCell<Vec<String>>,
        /// Registrations, deletions and folder changes.
        pub(crate) writes: Cell<usize>,
        pub(crate) runs: Cell<usize>,
        pub(crate) fail_register: Option<String>,
        /// Registration fails with `E_ACCESSDENIED`, as a Task Scheduler policy makes it.
        pub(crate) deny_register: bool,
        pub(crate) fail_delete: Option<String>,
        pub(crate) fail_read: Option<String>,
        pub(crate) fail_run: Option<String>,
        /// Security descriptor a newly registered task reads back with instead of
        /// [`TASK_SDDL`].
        pub(crate) task_sddl_override: Option<String>,
        /// Called before every write with its description.
        pub(crate) before_write: Option<WriteHook>,
    }

    impl std::fmt::Debug for FakeDefinitions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeDefinitions")
                .field("tasks", &self.tasks.borrow().keys().collect::<Vec<_>>())
                .field("folders", &self.folders)
                .field("ops", &self.ops)
                .finish_non_exhaustive()
        }
    }

    fn key(path: &str) -> String {
        path.to_ascii_lowercase()
    }

    fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = xml.find(&open)? + open.len();
        let end = xml[start..].find(&close)? + start;
        Some(&xml[start..end])
    }

    fn unescape(text: &str) -> String {
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    fn flag(xml: &str, tag: &str) -> bool {
        element(xml, tag) == Some("true")
    }

    /// What Task Scheduler would report for a task registered from `xml` by [`task_xml`].
    pub(crate) fn definition_from_xml(xml: &str) -> RegisteredDefinition {
        let settings = element(xml, "Settings").unwrap_or("");
        let weekly = element(xml, "CalendarTrigger").map(|t| WeeklyTrigger {
            days_mask: ScheduleDay::ALL
                .into_iter()
                .filter(|d| t.contains(&format!("<{} />", d.xml_element())))
                .map(ScheduleDay::days_of_week_mask)
                .sum(),
            weeks_interval: element(t, "WeeksInterval")
                .and_then(|w| w.parse().ok())
                .unwrap_or(0),
            start_boundary: element(t, "StartBoundary").unwrap_or("").to_string(),
            enabled: flag(t, "Enabled"),
        });
        let exec = element(xml, "Exec").map(|e| ExecAction {
            path: unescape(element(e, "Command").unwrap_or("")),
            arguments: unescape(element(e, "Arguments").unwrap_or("")),
            working_dir: unescape(element(e, "WorkingDirectory").unwrap_or("")),
        });
        let enabled = flag(settings, "Enabled");
        RegisteredDefinition {
            enabled,
            state: if enabled {
                TaskRunState::Ready
            } else {
                TaskRunState::Disabled
            },
            weekly,
            exec,
            user_id: unescape(element(xml, "UserId").unwrap_or("")),
            logon_type: if element(xml, "LogonType") == Some("InteractiveToken") {
                LOGON_INTERACTIVE_TOKEN
            } else {
                0
            },
            run_level: if element(xml, "RunLevel") == Some("HighestAvailable") {
                RUNLEVEL_HIGHEST
            } else {
                0
            },
            disallow_on_batteries: flag(settings, "DisallowStartIfOnBatteries"),
            run_only_if_idle: flag(settings, "RunOnlyIfIdle"),
            wake_to_run: flag(settings, "WakeToRun"),
            start_when_available: flag(settings, "StartWhenAvailable"),
            last_run_time: None,
            last_result: 0x0004_1303,
            next_run_time: None,
            missed_runs: 0,
        }
    }

    impl FakeDefinitions {
        pub(crate) fn new() -> FakeDefinitions {
            FakeDefinitions::default()
        }

        /// Adds a task registered from `xml` without counting a write.
        pub(crate) fn with_task(self, path: &str, xml: &str) -> FakeDefinitions {
            self.tasks.borrow_mut().insert(
                key(path),
                FakeTask {
                    path: path.to_string(),
                    xml: xml.to_string(),
                    definition: definition_from_xml(xml),
                    sddl: TASK_SDDL.to_string(),
                },
            );
            self
        }

        /// Adds a folder without counting a write.
        pub(crate) fn with_folder(self, folder: &str, sddl: &str) -> FakeDefinitions {
            self.folders
                .borrow_mut()
                .insert(key(folder), sddl.to_string());
            self
        }

        pub(crate) fn task(&self, path: &str) -> Option<FakeTask> {
            self.tasks.borrow().get(&key(path)).cloned()
        }

        pub(crate) fn has_folder(&self, folder: &str) -> bool {
            self.folders.borrow().contains_key(&key(folder))
        }

        fn write(&self, op: String) {
            if let Some(hook) = &self.before_write {
                hook(&op);
            }
            self.writes.set(self.writes.get() + 1);
            self.ops.borrow_mut().push(op);
        }
    }

    impl TaskDefinitionStore for FakeDefinitions {
        fn folder_sddl(&self, folder: &str) -> Result<Option<String>> {
            Ok(self.folders.borrow().get(&key(folder)).cloned())
        }

        fn create_folder(&self, folder: &str, sddl: &str) -> Result<bool> {
            if self.has_folder(folder) {
                return Ok(false);
            }
            self.write(format!("create_folder {folder}"));
            self.folders
                .borrow_mut()
                .insert(key(folder), sddl.to_string());
            Ok(true)
        }

        fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval> {
            if !self.has_folder(folder) {
                return Ok(FolderRemoval::Missing);
            }
            let prefix = format!("{}\\", key(folder));
            if self.tasks.borrow().keys().any(|k| k.starts_with(&prefix)) {
                return Ok(FolderRemoval::NotEmpty);
            }
            self.write(format!("delete_folder {folder}"));
            self.folders.borrow_mut().remove(&key(folder));
            Ok(FolderRemoval::Removed)
        }

        fn read(&self, path: &str) -> Result<Option<RegisteredDefinition>> {
            if let Some(message) = &self.fail_read {
                return Err(Error::Other(message.clone()));
            }
            Ok(self.task(path).map(|t| t.definition))
        }

        fn register(&self, path: &str, xml: &str) -> Result<()> {
            let (folder, _) = split_task_path(path)?;
            if !self.has_folder(&format!(r"\{folder}")) {
                return Err(Error::Other(format!(r"the folder \{folder} is missing")));
            }
            self.write(format!("register {path}"));
            if let Some(message) = &self.fail_register {
                return Err(Error::Other(message.clone()));
            }
            if self.deny_register {
                return Err(Error::Win32(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ACCESSDENIED,
                )));
            }
            self.tasks.borrow_mut().insert(
                key(path),
                FakeTask {
                    path: path.to_string(),
                    xml: xml.to_string(),
                    definition: definition_from_xml(xml),
                    sddl: self
                        .task_sddl_override
                        .clone()
                        .unwrap_or_else(|| TASK_SDDL.to_string()),
                },
            );
            Ok(())
        }

        fn delete(&self, path: &str) -> Result<bool> {
            if self.task(path).is_none() {
                return Ok(false);
            }
            self.write(format!("delete {path}"));
            if let Some(message) = &self.fail_delete {
                return Err(Error::Other(message.clone()));
            }
            self.tasks.borrow_mut().remove(&key(path));
            Ok(true)
        }

        fn task_sddl(&self, path: &str) -> Result<Option<String>> {
            Ok(self.task(path).map(|t| t.sddl))
        }

        fn run_now(&self, path: &str) -> Result<()> {
            if self.task(path).is_none() {
                return Err(Error::Other(format!("the task {path} does not exist")));
            }
            self.write(format!("run {path}"));
            if let Some(message) = &self.fail_run {
                return Err(Error::Other(message.clone()));
            }
            self.runs.set(self.runs.get() + 1);
            Ok(())
        }

        fn task_names(&self, folder: &str) -> Result<Vec<String>> {
            let prefix = format!("{}\\", key(folder));
            Ok(self
                .tasks
                .borrow()
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(_, t)| t.path[prefix.len()..].to_string())
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{definition_from_xml, FakeDefinitions};
    use super::*;
    use crate::maintenance::config::task_arguments;
    use crate::win::task_scheduler::{ExecSummary, TriggerSummary};

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const PATH: &str = r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001";
    const PROGRAM: &str = r"C:\Program Files\Cairn\cairn-maintenance.exe";
    const JOURNAL: &str = r"C:\Users\Test\AppData\Local\PCOptimizer\journal.db";

    fn config() -> ScheduleConfig {
        ScheduleConfig {
            day: ScheduleDay::Sunday,
            time: ScheduleTime::NOON,
            targets: vec!["user_temp".into(), "windows_temp".into()],
            system_file_check: true,
            component_store_check: true,
        }
    }

    fn spec(c: &ScheduleConfig) -> TaskSpec {
        TaskSpec {
            path: PATH.to_string(),
            user_sid: SID.to_string(),
            program: PathBuf::from(PROGRAM),
            arguments: task_arguments(Path::new(JOURNAL), c).unwrap(),
            working_dir: PathBuf::from(r"C:\Windows\System32"),
            start: NaiveDate::from_ymd_opt(2026, 10, 4)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
            day: c.day,
            enabled: true,
        }
    }

    fn expected() -> Expected {
        Expected {
            program: PathBuf::from(PROGRAM),
            journal: PathBuf::from(JOURNAL),
            user_sid: SID.to_string(),
        }
    }

    #[test]
    fn task_xml_matches_the_golden_text() {
        let xml = task_xml(&spec(&config()));
        let golden = concat!(
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n",
            "  <RegistrationInfo><Author>Cairn</Author><Description>Cairn scheduled maintenance: cleans the selected temporary files and caches and runs read-only system checks. It never repairs anything. Turn it off in Cairn to remove it.</Description></RegistrationInfo>\r\n",
            "  <Triggers><CalendarTrigger><StartBoundary>2026-10-04T12:00:00</StartBoundary><Enabled>true</Enabled><ScheduleByWeek><DaysOfWeek><Sunday /></DaysOfWeek><WeeksInterval>1</WeeksInterval></ScheduleByWeek></CalendarTrigger></Triggers>\r\n",
            "  <Principals><Principal id=\"Author\"><UserId>S-1-5-21-1111111111-2222222222-3333333333-1001</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>\r\n",
            "  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><RunOnlyIfIdle>true</RunOnlyIfIdle><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT4H</ExecutionTimeLimit><Priority>7</Priority></Settings>\r\n",
            "  <Actions Context=\"Author\"><Exec><Command>C:\\Program Files\\Cairn\\cairn-maintenance.exe</Command><Arguments>--journal &quot;C:\\Users\\Test\\AppData\\Local\\PCOptimizer\\journal.db&quot; --targets user_temp,windows_temp --sfc --dism</Arguments><WorkingDirectory>C:\\Windows\\System32</WorkingDirectory></Exec></Actions>\r\n",
            "</Task>\r\n",
        );
        assert_eq!(xml, golden);
    }

    #[test]
    fn task_xml_escapes_text_and_carries_the_required_settings() {
        assert_eq!(
            escape(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
        let mut s = spec(&config());
        s.program = PathBuf::from(r"C:\Tools & <Co>\cairn-maintenance.exe");
        s.enabled = false;
        let xml = task_xml(&s);
        assert!(xml.contains(r"<Command>C:\Tools &amp; &lt;Co&gt;\cairn-maintenance.exe</Command>"));
        for required in [
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<RunOnlyIfIdle>true</RunOnlyIfIdle>",
            "<WakeToRun>false</WakeToRun>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<ExecutionTimeLimit>PT4H</ExecutionTimeLimit>",
            "<AllowStartOnDemand>true</AllowStartOnDemand>",
            "<Settings><MultipleInstancesPolicy>",
            "<Enabled>false</Enabled><Hidden>false</Hidden>",
        ] {
            assert!(xml.contains(required), "{required}");
        }
        assert!(!xml.contains("$("));
        let def = definition_from_xml(&xml);
        assert_eq!(
            def.exec.unwrap().path,
            r"C:\Tools & <Co>\cairn-maintenance.exe"
        );
        assert!(!def.enabled);
    }

    #[test]
    fn a_task_cairn_registers_reads_back_as_its_schedule() {
        let c = config();
        let def = definition_from_xml(&task_xml(&spec(&c)));
        let seen = interpret(&def, &expected(), Some(&c));
        assert_eq!(seen.drift, Vec::<String>::new());
        assert_eq!(seen.config.as_ref(), Some(&c));
        assert!(seen.matches_spec);
        assert_eq!(seen.program.as_deref(), Some(PROGRAM));
        let mut other = c.clone();
        other.time = ScheduleTime { hour: 9, minute: 0 };
        assert!(!interpret(&def, &expected(), Some(&other)).matches_spec);
        assert!(!interpret(&def, &expected(), None).matches_spec);
        let mut disabled = def.clone();
        disabled.enabled = false;
        let seen = interpret(&disabled, &expected(), Some(&c));
        assert!(seen.drift.is_empty() && !seen.matches_spec);
    }

    /// A change to a registered definition.
    type DefinitionChange = Box<dyn Fn(&mut RegisteredDefinition)>;

    #[test]
    fn interpret_reports_every_drift() {
        let c = config();
        let good = definition_from_xml(&task_xml(&spec(&c)));
        let cases: Vec<(DefinitionChange, String)> = vec![
            (
                Box::new(|d| d.weekly.as_mut().unwrap().days_mask = 3),
                DRIFT_SCHEDULE.into(),
            ),
            (
                Box::new(|d| d.weekly.as_mut().unwrap().weeks_interval = 2),
                DRIFT_SCHEDULE.into(),
            ),
            (Box::new(|d| d.weekly = None), DRIFT_SCHEDULE.into()),
            (
                Box::new(|d| d.exec.as_mut().unwrap().arguments = "--journal x --scan".into()),
                DRIFT_COMMAND.into(),
            ),
            (Box::new(|d| d.exec = None), DRIFT_COMMAND.into()),
            (
                Box::new(|d| {
                    d.exec.as_mut().unwrap().path = r"D:\Other\cairn-maintenance.exe".into()
                }),
                r"it runs another copy of Cairn (D:\Other\cairn-maintenance.exe)".into(),
            ),
            (
                Box::new(|d| {
                    d.exec.as_mut().unwrap().arguments =
                        r#"--journal "D:\Elsewhere\journal.db" --sfc"#.into()
                }),
                r"it writes to another journal (D:\Elsewhere\journal.db)".into(),
            ),
            (
                Box::new(|d| d.disallow_on_batteries = false),
                DRIFT_BATTERY.into(),
            ),
            (Box::new(|d| d.run_only_if_idle = false), DRIFT_IDLE.into()),
            (Box::new(|d| d.wake_to_run = true), DRIFT_WAKE.into()),
            (
                Box::new(|d| d.user_id = "S-1-5-21-1-2-3-1002".into()),
                DRIFT_ACCOUNT.into(),
            ),
            (Box::new(|d| d.logon_type = 1), DRIFT_ACCOUNT.into()),
            (Box::new(|d| d.run_level = 0), DRIFT_RIGHTS.into()),
        ];
        for (change, drift) in cases {
            let mut def = good.clone();
            change(&mut def);
            let seen = interpret(&def, &expected(), Some(&c));
            assert!(seen.drift.contains(&drift), "{drift}: {:?}", seen.drift);
            assert!(!seen.matches_spec, "{drift}");
        }
        let mut upper = good.clone();
        upper.exec.as_mut().unwrap().path = PROGRAM.to_uppercase();
        upper.user_id = SID.to_lowercase();
        assert!(interpret(&upper, &expected(), Some(&c)).matches_spec);
    }

    #[test]
    fn task_results_read_as_sentences() {
        assert_eq!(task_result_text(0), None);
        assert_eq!(
            task_result_text(0x41303).as_deref(),
            Some("It hasn't run yet")
        );
        assert_eq!(task_result_text(0x41301).as_deref(), Some("It is running"));
        assert_eq!(
            task_result_text(0x8007_0002u32 as i32).as_deref(),
            Some("The program the task runs is missing; reinstall Cairn or turn maintenance off")
        );
        assert_eq!(
            task_result_text(0xC000_013Au32 as i32).as_deref(),
            Some("It was stopped (sign-out or shutdown)")
        );
        for code in [
            EXIT_ATTENTION,
            EXIT_FAILED,
            EXIT_STOPPED,
            EXIT_SKIPPED,
            EXIT_REFUSED,
            1,
        ] {
            assert!(task_result_text(code).is_some(), "{code}");
        }
        assert_eq!(
            task_result_text(0x8000_4005u32 as i32).as_deref(),
            Some("It ended with code 0x80004005")
        );
    }

    #[test]
    fn ole_dates_convert_and_never_is_none() {
        assert_eq!(ole_date(36_494.0), None);
        assert_eq!(ole_date(0.0), None);
        assert_eq!(ole_date(f64::NAN), None);
        assert_eq!(
            ole_date(46_299.5),
            NaiveDate::from_ymd_opt(2026, 10, 4)
                .unwrap()
                .and_hms_opt(12, 0, 0)
        );
        assert_eq!(
            ole_date(36_526.0),
            NaiveDate::from_ymd_opt(2000, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
        );
    }

    #[test]
    fn security_descriptors_are_judged_by_role() {
        assert_eq!(task_object_problem(TASK_SDDL).unwrap(), None);
        let defrag = "O:SYD:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;LS)(A;;FR;;;AU)";
        assert_eq!(task_object_problem(defrag).unwrap(), None);
        let onedrive = format!("O:{SID}D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FA;;;{SID})(A;;FR;;;AU)");
        assert!(task_object_problem(&onedrive).unwrap().is_some());
        let root = "O:SYD:PAI(A;CI;FA;;;BA)(A;OI;0x1f019f;;;BA)(A;CI;FA;;;SY)(A;OI;0x1f019f;;;SY)\
                    (A;CI;FW;;;AU)(A;CI;FW;;;NS)(A;CI;FW;;;LS)(A;OICIIO;FA;;;CO)";
        assert!(task_folder_problem(root).unwrap().is_some());
        assert_eq!(task_folder_problem(FOLDER_SDDL).unwrap(), None);
        let without_owner = FOLDER_SDDL.strip_prefix("O:BA").unwrap();
        assert!(
            task_folder_problem(without_owner).unwrap().is_some(),
            "no owner"
        );
        let user_owned = format!("O:{SID}{without_owner}");
        assert!(task_folder_problem(&user_owned).unwrap().is_some());
    }

    #[test]
    fn only_cairns_own_tasks_and_folders_are_own() {
        for own in [
            PATH,
            r"\cairn\maintenance-S-1-5-18",
            r"\Cairn\Maintenance-s-1-5-21-1-2-3-500",
            r"\PCOptimizerSelfTest\NoSuchMaintenance",
            r"\pcoptimizerselftest\Probe-1-2",
        ] {
            assert!(is_own_task_path(own), "{own}");
        }
        for foreign in [
            r"\Cairn\Maintenance-",
            r"\Cairn\Maintenance-S-1-5-",
            r"\Cairn\Maintenance-S-1-5-21--1",
            r"\Cairn\Maintenance-S-1-5-21-1-",
            r"\Cairn\Maintenance-S-1-1-0",
            r"\Cairn\Maintenance-S-1-51-2",
            r"\Cairn\Maintenance-S-1-5",
            "\\Cairn\\Maintenance-S-1-\u{e9}-5",
            "\\Cairn\\Maintenance-S-1\u{e9}5-18",
            r"\Cairn\Maintenance-S-1-5-21-x",
            r"\Cairn\Other",
            r"\Cairn\Sub\Maintenance-S-1-5-18",
            r"\Microsoft\Windows\Defrag\ScheduledDefrag",
            r"\Maintenance-S-1-5-18",
            r"\PCOptimizerSelfTest\Sub\Task",
            r"Cairn\Maintenance-S-1-5-18",
            "",
        ] {
            assert!(!is_own_task_path(foreign), "{foreign}");
        }
        for own in [r"\Cairn", "Cairn", r"\cairn", r"\PCOptimizerSelfTest"] {
            assert!(is_own_folder(own), "{own}");
        }
        for foreign in [r"\", r"\Microsoft", r"\Cairn\Sub", r"\Cairnx", ""] {
            assert!(!is_own_folder(foreign), "{foreign}");
        }
    }

    #[test]
    fn the_remover_refuses_foreign_objects_before_the_store() {
        let fake = FakeDefinitions::new()
            .with_folder(r"\Microsoft", FOLDER_SDDL)
            .with_task(r"\Microsoft\Other", &task_xml(&spec(&config())))
            .with_folder(r"\Cairn", FOLDER_SDDL)
            .with_task(PATH, &task_xml(&spec(&config())));
        let remover = OwnRemover::new(&fake);
        assert!(remover.delete(r"\Microsoft\Other").is_err());
        assert!(remover.delete_folder_if_empty(r"\Microsoft").is_err());
        assert_eq!(fake.writes.get(), 0);
        assert!(fake.task(r"\Microsoft\Other").is_some());
        assert_eq!(
            remover.delete_folder_if_empty(r"\Cairn").unwrap(),
            FolderRemoval::NotEmpty
        );
        assert!(remover.delete(PATH).unwrap());
        assert!(!remover.delete(PATH).unwrap());
        assert_eq!(
            remover.delete_folder_if_empty(r"\Cairn").unwrap(),
            FolderRemoval::Removed
        );
        assert_eq!(
            remover.delete_folder_if_empty(r"\Cairn").unwrap(),
            FolderRemoval::Missing
        );
    }

    #[test]
    fn task_paths_split_into_folder_and_name() {
        assert_eq!(split_task_path(PATH).unwrap(), ("Cairn", &PATH[7..]));
        for bad in [r"\Cairn", r"\A\B\C", "Cairn\\X", r"\Cairn\"] {
            assert!(split_task_path(bad).is_err(), "{bad}");
        }
        assert_eq!(folder_name(r"\Cairn").unwrap(), "Cairn");
        assert!(folder_name(r"\A\B").is_err());
    }

    #[test]
    fn missing_tasks_read_as_none_through_the_live_store() {
        // Read-only: looks a task up that does not exist.
        let store = LiveDefinitions::connect().unwrap();
        assert!(store
            .read(r"\PCOptimizerSelfTest\NoSuchMaintenance")
            .unwrap()
            .is_none());
        assert!(store
            .folder_sddl(r"\PCOptimizerNoSuchFolder")
            .unwrap()
            .is_none());
        assert!(store
            .task_names(r"\PCOptimizerNoSuchFolder")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn account_names_resolve_to_sids() {
        // Read-only: resolves well-known and own account names.
        assert_eq!(resolve_account("S-1-5-18"), "S-1-5-18");
        assert_eq!(resolve_account(r"NT AUTHORITY\SYSTEM"), "S-1-5-18");
        let own = crate::win::session::process_account_name().unwrap();
        assert_eq!(
            resolve_account(&own),
            crate::win::session::current_user_sid().unwrap()
        );
        assert_eq!(
            resolve_account("no such account 7f3a"),
            "no such account 7f3a"
        );
        // Task Scheduler reports an interactive-token principal as the account name without
        // its domain.
        let bare = own.rsplit('\\').next().unwrap();
        assert_eq!(
            resolve_account(bare),
            crate::win::session::current_user_sid().unwrap()
        );
        // Names no account has whose fourth byte is inside a character.
        for unknown in [
            "Jos\u{e9} 7f3a",
            "Ren\u{e9}e 7f3a",
            "\u{674e}\u{660e} 7f3a",
            "\u{5c71}\u{7530} 7f3a",
            "\u{e2a}\u{e21} 7f3a",
        ] {
            assert_eq!(resolve_account(unknown), unknown);
        }
        // A name that only starts like a SID is looked up like any other name.
        assert_eq!(resolve_account("S-1-7f3a none"), "S-1-7f3a none");
    }

    #[test]
    fn sids_are_told_from_account_names() {
        for sid in [SID, "S-1-5-18", "s-1-5-18", "S-1-1-0", "S-1-12-1-1-2-3-4"] {
            assert!(is_sid(sid), "{sid}");
        }
        for name in [
            "",
            "S-1-",
            "S-1-5-",
            "S-1--5",
            "S-1-5-x",
            "S-2-5-18",
            "Test",
            r"TEST-PC\Test",
            "Jos\u{e9}",
            "\u{674e}\u{660e}",
            "S-1\u{e9}5-18",
        ] {
            assert!(!is_sid(name), "{name}");
        }
    }

    /// A task's XML as Task Scheduler returns it for a task registered with `account`, with
    /// a trigger that names another account before the principal.
    fn registered_xml(account: &str) -> String {
        format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n",
                "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n",
                "  <Triggers>\r\n    <LogonTrigger>\r\n      <UserId>TEST-PC\\Other</UserId>\r\n",
                "    </LogonTrigger>\r\n  </Triggers>\r\n",
                "  <Principals>\r\n    <Principal id=\"Author\">\r\n      <UserId>{}</UserId>\r\n",
                "      <LogonType>InteractiveToken</LogonType>\r\n    </Principal>\r\n  </Principals>\r\n",
                "</Task>"
            ),
            account
        )
    }

    #[test]
    fn the_account_is_the_sid_in_the_tasks_xml() {
        assert_eq!(
            principal_sid(&task_xml(&spec(&config()))).as_deref(),
            Some(SID)
        );
        assert_eq!(principal_sid(&registered_xml(SID)).as_deref(), Some(SID));
        assert_eq!(principal_sid(&registered_xml(r"TEST-PC\Test")), None);
        assert_eq!(principal_sid(&registered_xml("")), None);
        // Without a principal the account of a trigger is not taken for it.
        assert_eq!(
            principal_sid(&format!(
                "<Task><Triggers><UserId>{SID}</UserId></Triggers></Task>"
            )),
            None
        );
        assert_eq!(
            principal_sid("<Principals><Principal><UserId>S-1-5-18"),
            None
        );
        assert_eq!(principal_sid(""), None);

        // The SID of the XML stands, whatever name Task Scheduler reports for it: no name is
        // looked up, so neither an unknown name nor one that another account also has matters.
        for reported in ["Test", "Jos\u{e9}", "\u{674e}\u{660e}", "SYSTEM", ""] {
            assert_eq!(principal_account(Some(&registered_xml(SID)), reported), SID);
        }
        // An XML that names the account, or no XML, leaves the reported account to resolve.
        assert_eq!(
            principal_account(
                Some(&registered_xml(r"NT AUTHORITY\SYSTEM")),
                r"NT AUTHORITY\SYSTEM"
            ),
            "S-1-5-18"
        );
        assert_eq!(principal_account(None, "S-1-5-18"), "S-1-5-18");
        assert_eq!(principal_account(None, "Jos\u{e9} 7f3a"), "Jos\u{e9} 7f3a");
    }

    /// What Task Scheduler's objects report for a task registered from `xml` by [`task_xml`],
    /// with `reported` as the principal's account.
    fn summary(xml: &str, reported: &str) -> DefinitionSummary {
        let def = definition_from_xml(xml);
        let weekly = def.weekly.unwrap();
        let exec = def.exec.unwrap();
        DefinitionSummary {
            triggers: vec![TriggerSummary {
                start_boundary: weekly.start_boundary,
                enabled: weekly.enabled,
                weekly: Some((weekly.days_mask, weekly.weeks_interval)),
            }],
            actions: vec![Some(ExecSummary {
                path: exec.path,
                arguments: exec.arguments,
                working_dir: exec.working_dir,
            })],
            user_id: reported.to_string(),
            logon_type: def.logon_type,
            run_level: def.run_level,
            disallow_on_batteries: def.disallow_on_batteries,
            run_only_if_idle: def.run_only_if_idle,
            wake_to_run: def.wake_to_run,
            start_when_available: def.start_when_available,
        }
    }

    #[test]
    fn a_task_whose_account_is_reported_by_name_reads_back_without_drift() {
        let c = config();
        let xml = task_xml(&spec(&c));
        for reported in ["Test", "Jos\u{e9}", "Ren\u{e9}e", "\u{674e}\u{660e}"] {
            let def = RegisteredDefinition::from_summary(
                true,
                TaskRunState::Ready,
                summary(&xml, reported),
                |name| principal_account(Some(&xml), name),
            );
            assert_eq!(def.user_id, SID, "{reported}");
            let seen = interpret(&def, &expected(), Some(&c));
            assert_eq!(seen.drift, Vec::<String>::new(), "{reported}");
            assert!(seen.matches_spec, "{reported}");
        }
        // Without the XML a name no account has stays a name: the task reads as another
        // account's.
        let def = RegisteredDefinition::from_summary(
            true,
            TaskRunState::Ready,
            summary(&xml, "Jos\u{e9} 7f3a"),
            |name| principal_account(None, name),
        );
        assert_eq!(def.user_id, "Jos\u{e9} 7f3a");
        assert_eq!(
            interpret(&def, &expected(), Some(&c)).drift,
            [DRIFT_ACCOUNT]
        );
    }

    #[test]
    fn a_system_tasks_xml_holds_the_sid_of_its_account() {
        // Read-only: reads a task Windows installs, which runs as SYSTEM and whose principal
        // Task Scheduler reports by name.
        let scheduler = TaskScheduler::connect().unwrap();
        let Some(task) = scheduler
            .task(r"\Microsoft\Windows\Defrag\ScheduledDefrag")
            .unwrap()
        else {
            eprintln!("skipped: this PC has no ScheduledDefrag task");
            return;
        };
        let xml = task.xml().unwrap();
        assert_eq!(principal_sid(&xml).as_deref(), Some("S-1-5-18"), "{xml}");
        let reported = task.definition_summary().unwrap().user_id;
        assert_eq!(principal_account(Some(&xml), &reported), "S-1-5-18");
        assert_eq!(resolve_account(&reported), "S-1-5-18", "{reported}");
    }

    /// Runs `body`, then deletes the task at `path` and its folder when that is empty, also
    /// when `body` panics (the panic continues afterwards), so a failed assertion leaves
    /// nothing registered. Returns what `body` returned and what the two removals reported.
    fn then_remove<S: TaskDefinitionStore, T>(
        store: &S,
        path: &str,
        folder: &str,
        body: impl FnOnce() -> T,
    ) -> (T, Result<bool>, Result<FolderRemoval>) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let deleted = store.delete(path);
        let folder_removed = store.delete_folder_if_empty(folder);
        match outcome {
            Ok(value) => (value, deleted, folder_removed),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    #[test]
    fn a_failed_sandbox_check_still_removes_the_task_and_the_folder() {
        let folder = format!(r"\{SANDBOX_FOLDER}");
        let path = format!(r"{folder}\Maintenance-Probe-1");
        let xml = task_xml(&spec(&config()));

        let fake = FakeDefinitions::new();
        fake.create_folder(&folder, FOLDER_SDDL).unwrap();
        let (value, deleted, folder_removed) = then_remove(&fake, &path, &folder, || {
            fake.register(&path, &xml).unwrap();
            7
        });
        assert_eq!(value, 7);
        assert!(deleted.unwrap());
        assert_eq!(folder_removed.unwrap(), FolderRemoval::Removed);
        assert!(fake.task(&path).is_none() && !fake.has_folder(&folder));

        let fake = FakeDefinitions::new();
        fake.create_folder(&folder, FOLDER_SDDL).unwrap();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            then_remove(&fake, &path, &folder, || -> () {
                fake.register(&path, &xml).unwrap();
                panic!("a check of the registered task failed");
            })
        }));
        let message = failed.expect_err("the failure of the body is passed on");
        assert_eq!(
            message.downcast_ref::<&str>().copied(),
            Some("a check of the registered task failed")
        );
        assert!(fake.task(&path).is_none(), "the task is left registered");
        assert!(!fake.has_folder(&folder), "the folder is left behind");
    }

    /// Registers a disabled task at `\PCOptimizerSelfTest\Maintenance-Probe-<pid>` in a folder
    /// created with [`FOLDER_SDDL`], reads it back, and deletes both, also when a check fails.
    /// Needs an elevated process; run by exact name only.
    #[test]
    #[ignore = "registers a task in \\PCOptimizerSelfTest; run elevated by exact name"]
    fn sandbox_maintenance_task_registers_reads_back_and_deletes() {
        let store = LiveDefinitions::connect().unwrap();
        let folder = format!(r"\{SANDBOX_FOLDER}");
        let path = format!(r"{folder}\Maintenance-Probe-{}", std::process::id());
        let sid = crate::win::session::current_user_sid().unwrap();
        let system = crate::win::paths::system_dir().unwrap();
        let created = store.create_folder(&folder, FOLDER_SDDL).unwrap();
        let spec = TaskSpec {
            path: path.clone(),
            user_sid: sid.clone(),
            program: system.join("cmd.exe"),
            arguments: "/c exit 0".into(),
            working_dir: system.clone(),
            start: NaiveDate::from_ymd_opt(2099, 1, 4)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
            day: ScheduleDay::Sunday,
            enabled: false,
        };
        let check = || -> Result<()> {
            store.register(&path, &task_xml(&spec))?;
            let def = store.read(&path)?.expect("the task reads back");
            assert!(!def.enabled);
            assert_eq!(def.user_id.to_ascii_uppercase(), sid.to_ascii_uppercase());
            assert_eq!(def.logon_type, LOGON_INTERACTIVE_TOKEN);
            assert_eq!(def.run_level, RUNLEVEL_HIGHEST);
            assert!(def.disallow_on_batteries && def.run_only_if_idle && def.start_when_available);
            assert!(!def.wake_to_run);
            let weekly = def.weekly.expect("one weekly trigger");
            assert_eq!(weekly.days_mask, 1);
            assert_eq!(weekly.weeks_interval, 1);
            assert!(weekly.start_boundary.starts_with("2099-01-04T12:00"));
            let exec = def.exec.expect("one exec action");
            assert!(exec
                .path
                .eq_ignore_ascii_case(&system.join("cmd.exe").to_string_lossy()));
            assert_eq!(exec.arguments, "/c exit 0");
            let sddl = store.task_sddl(&path)?.expect("the task has security");
            eprintln!("task security: {sddl}");
            assert_eq!(task_object_problem(&sddl)?, None);
            let folder_sddl = store.folder_sddl(&folder)?.expect("the folder exists");
            eprintln!("folder security: {folder_sddl}");
            if created {
                assert_eq!(task_folder_problem(&folder_sddl)?, None);
            }
            Ok(())
        };
        let (result, deleted, folder_removed) = then_remove(&store, &path, &folder, check);
        result.unwrap();
        assert!(deleted.unwrap());
        assert!(store.read(&path).unwrap().is_none());
        eprintln!("folder: {:?}", folder_removed.unwrap());
    }

    /// Registers an enabled task at `\PCOptimizerSelfTest\Maintenance-Run-<pid>` that runs
    /// `cmd.exe /c exit 0` with the account's interactive token and highest rights, starts
    /// it on demand, waits for its result and deletes it, also when a check fails. Needs an
    /// elevated process; run by exact name only.
    #[test]
    #[ignore = "runs a task in \\PCOptimizerSelfTest; run elevated by exact name"]
    fn sandbox_interactive_highest_task_runs_on_demand() {
        let store = LiveDefinitions::connect().unwrap();
        let folder = format!(r"\{SANDBOX_FOLDER}");
        let path = format!(r"{folder}\Maintenance-Run-{}", std::process::id());
        let system = crate::win::paths::system_dir().unwrap();
        store.create_folder(&folder, FOLDER_SDDL).unwrap();
        let spec = TaskSpec {
            path: path.clone(),
            user_sid: crate::win::session::current_user_sid().unwrap(),
            program: system.join("cmd.exe"),
            arguments: "/c exit 0".into(),
            working_dir: system,
            start: NaiveDate::from_ymd_opt(2099, 1, 4)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
            day: ScheduleDay::Sunday,
            enabled: true,
        };
        let run = || -> Result<i32> {
            store.register(&path, &task_xml(&spec))?;
            store.run_now(&path)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let def = store.read(&path)?.expect("the task exists");
                if def.last_result == 0 && def.last_run_time.is_some() {
                    return Ok(0);
                }
                if std::time::Instant::now() > deadline {
                    return Ok(def.last_result);
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        };
        let (result, deleted, _) = then_remove(&store, &path, &folder, run);
        assert_eq!(result.unwrap(), 0, "the task did not finish with 0 in 30 s");
        assert!(deleted.unwrap());
        assert!(store.read(&path).unwrap().is_none());
    }
}
