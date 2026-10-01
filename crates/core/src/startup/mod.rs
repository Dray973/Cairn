//! Startup apps: the Run keys, the Startup folders, packaged-app startup tasks and the
//! Group Policy Run keys, enabled and disabled the way Task Manager does it.
//!
//! Disabling never deletes the entry; it writes a registry value through the safety layer,
//! so every change is journaled and reverts like any other registry change.
//!
//! Run keys and Startup folders are switched through `Explorer\StartupApproved` values. A
//! StartupApproved value is REG_BINARY named after the Run value (or the Startup folder
//! file). An even first byte (`0x02`, `0x06`) or a missing value means enabled; an odd first
//! byte (`0x03`, `0x07`) means disabled. Task Manager writes `02 00 00 00` followed by eight
//! zero bytes to enable, and `03 00 00 00` followed by the current FILETIME to disable.
//!
//! Packaged-app startup tasks are switched through their `State` DWORD under
//! `HKCU\Software\Classes\Local Settings\...\AppModel\SystemAppData\<family>\<task>`:
//! 1 (disabled by the user) or 2 (enabled). Tasks whose state was set by Group Policy (3, 4)
//! are listed but not toggleable. Entries of the `Policies\Explorer\Run` keys are set by
//! Group Policy, are not covered by StartupApproved and are listed read-only.
//!
//! Per-user entries are refused while this process runs as a different account than the
//! signed-in user (UAC elevation with another administrator's credentials): HKCU and the
//! per-user folders would then belong to that other account. They are also refused when
//! the two accounts cannot be compared.

mod command;
mod folder;
mod manifest;
pub(crate) mod packaged;
mod publisher;
mod run_key;

#[cfg(test)]
mod tests;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::safety::rollback::RegistryTarget;
use crate::safety::{MutationOutcome, Safety};
use crate::win::filetime::now as filetime_now;
use crate::win::registry::{Hive, Key, RegValue};
use crate::win::session;
use crate::{Error, Result};

use self::packaged::{Package, TaskState};

const USER_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const MACHINE_RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
const MACHINE_RUN32_KEY: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const USER_POLICY_RUN_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer\Run";
const MACHINE_POLICY_RUN_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\Explorer\Run";
/// Parent of the `Run`, `Run32` and `StartupFolder` subkeys, in HKCU and in HKLM.
const APPROVED_ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

const POLICY_NOTE: &str = "Set by Group Policy; it can only be changed through that policy.";
const OTHER_USER_NOTE: &str = "Belongs to the account this window runs as, not to the \
     signed-in user, so it cannot be changed from this window. Reopen Cairn as \
     yourself, without another account's credentials, to change your own startup apps.";
const UNKNOWN_USER_NOTE: &str = "Cannot confirm that this window runs as the signed-in user, \
     so per-user startup apps cannot be changed from here. If this window was opened with \
     administrator rights, reopen Cairn without them to change your own startup apps.";

/// Whether this process runs as the signed-in user, who owns the per-user entries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Account {
    /// The signed-in user, or no signed-in user at all: per-user entries can be changed.
    SignedIn,
    /// Another account than the signed-in user: per-user entries belong to that account.
    Other,
    /// The accounts could not be compared, for the given reason.
    Unknown(String),
}

impl Account {
    /// Account state from the result of [`session::elevated_as_other_user`].
    fn from_check(check: Result<bool>) -> Account {
        match check {
            Ok(false) => Account::SignedIn,
            Ok(true) => Account::Other,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "cannot compare this process's account with the signed-in user"
                );
                Account::Unknown(e.to_string())
            }
        }
    }

    fn current() -> Account {
        Account::from_check(session::elevated_as_other_user())
    }

    /// Why per-user entries cannot be toggled; `None` when they can.
    fn per_user_note(&self) -> Option<&'static str> {
        match self {
            Account::SignedIn => None,
            Account::Other => Some(OTHER_USER_NOTE),
            Account::Unknown(_) => Some(UNKNOWN_USER_NOTE),
        }
    }

    /// Refusal for a per-user change; `Ok` when the change would reach the signed-in user.
    fn ensure_signed_in(&self) -> Result<()> {
        match self {
            Account::SignedIn => Ok(()),
            Account::Other => Err(Error::Other(
                "this window runs as a different account than the signed-in user (it was \
                 opened or elevated with that account's credentials), so the per-user startup \
                 apps it sees belong to that account; nothing was changed. Reopen Cairn \
                 as yourself, without another account's credentials, to change your own \
                 startup apps, or sign in as that account to change its startup apps"
                    .to_string(),
            )),
            Account::Unknown(reason) => Err(Error::Other(format!(
                "cannot confirm that this window runs as the signed-in user ({reason}); \
                 nothing was changed. If this window was opened with administrator rights, \
                 reopen Cairn without them to change your own startup apps"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupSource {
    /// HKCU\Software\Microsoft\Windows\CurrentVersion\Run
    UserRun,
    /// HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run
    MachineRun,
    /// HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run
    MachineRun32,
    /// %APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup
    UserFolder,
    /// %ProgramData%\Microsoft\Windows\Start Menu\Programs\Startup
    CommonFolder,
    /// Startup tasks of packaged (MSIX / Microsoft Store) apps installed for the current
    /// user, under HKCU\Software\Classes\Local Settings\...\AppModel\SystemAppData.
    PackagedTask,
    /// HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer\Run (read-only).
    PolicyUserRun,
    /// HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\Explorer\Run (read-only).
    PolicyMachineRun,
}

impl StartupSource {
    const ALL: [StartupSource; 8] = [
        StartupSource::UserRun,
        StartupSource::MachineRun,
        StartupSource::MachineRun32,
        StartupSource::UserFolder,
        StartupSource::CommonFolder,
        StartupSource::PackagedTask,
        StartupSource::PolicyUserRun,
        StartupSource::PolicyMachineRun,
    ];

    /// Source named by the prefix of an entry id (`user_run:Discord` → `UserRun`).
    pub fn of_id(id: &str) -> Option<StartupSource> {
        parse_id(id).map(|(source, _)| source)
    }

    /// Entries of this source are machine-wide: changing them, where they can be changed
    /// at all, needs an elevated process.
    pub fn requires_admin(self) -> bool {
        !self.is_per_user()
    }

    /// Entries of this source belong to the account the process runs as (HKCU or the
    /// per-user profile).
    pub fn is_per_user(self) -> bool {
        matches!(
            self,
            StartupSource::UserRun
                | StartupSource::UserFolder
                | StartupSource::PackagedTask
                | StartupSource::PolicyUserRun
        )
    }

    /// Entries of this source are set by Group Policy and are never toggled here.
    pub fn is_policy(self) -> bool {
        matches!(
            self,
            StartupSource::PolicyUserRun | StartupSource::PolicyMachineRun
        )
    }

    /// Id prefix; identical to the serde name.
    fn key(self) -> &'static str {
        match self {
            StartupSource::UserRun => "user_run",
            StartupSource::MachineRun => "machine_run",
            StartupSource::MachineRun32 => "machine_run32",
            StartupSource::UserFolder => "user_folder",
            StartupSource::CommonFolder => "common_folder",
            StartupSource::PackagedTask => "packaged_task",
            StartupSource::PolicyUserRun => "policy_user_run",
            StartupSource::PolicyMachineRun => "policy_machine_run",
        }
    }

    fn from_key(key: &str) -> Option<StartupSource> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }

    fn location(self) -> &'static str {
        match self {
            StartupSource::UserRun => "HKCU Run",
            StartupSource::MachineRun => "HKLM Run",
            StartupSource::MachineRun32 => "HKLM Run (32-bit)",
            StartupSource::UserFolder => "Startup folder",
            StartupSource::CommonFolder => "Startup folder (all users)",
            StartupSource::PackagedTask => "Packaged app",
            StartupSource::PolicyUserRun => "HKCU Run (Group Policy)",
            StartupSource::PolicyMachineRun => "HKLM Run (Group Policy)",
        }
    }

    fn is_folder(self) -> bool {
        matches!(
            self,
            StartupSource::UserFolder | StartupSource::CommonFolder
        )
    }

    /// Hive holding this source's StartupApproved values.
    fn approved_hive(self) -> Hive {
        if self.requires_admin() {
            Hive::LocalMachine
        } else {
            Hive::CurrentUser
        }
    }

    /// Subkey of the StartupApproved root holding this source's values; `None` for sources
    /// StartupApproved does not cover.
    fn approved_subkey(self) -> Option<&'static str> {
        match self {
            StartupSource::UserRun | StartupSource::MachineRun => Some("Run"),
            StartupSource::MachineRun32 => Some("Run32"),
            StartupSource::UserFolder | StartupSource::CommonFolder => Some("StartupFolder"),
            StartupSource::PackagedTask
            | StartupSource::PolicyUserRun
            | StartupSource::PolicyMachineRun => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupEntry {
    /// Stable id: `<source>:<key>`. The key is the Run value name, the Startup folder file
    /// name, or `<PackageFamilyName>\<TaskId>` for packaged tasks; for example
    /// `user_run:Discord` or `packaged_task:Microsoft.WindowsTerminal_8wekyb3d8bbwe\StartTerminalOnLoginTask`.
    pub id: String,
    /// Value name (Run keys), file name (Startup folders) or display name (packaged tasks).
    pub name: String,
    pub source: StartupSource,
    /// Human-readable location, for example `HKCU Run` or `Startup folder (all users)`.
    pub location: String,
    /// Raw command line (Run keys), shortcut target with arguments (Startup folders) or the
    /// quoted executable (packaged tasks).
    pub command: String,
    /// Executable the entry starts, with environment variables expanded; empty if unknown.
    pub path: String,
    /// CompanyName from the executable's version resource, else the package's publisher
    /// display name for packaged tasks; empty if unavailable.
    pub publisher: String,
    /// Whether `path` exists on disk.
    pub exists: bool,
    pub enabled: bool,
    /// The entry is machine-wide: changing it needs an elevated process.
    pub requires_admin: bool,
    /// False when `set_enabled` refuses this entry whatever the elevation: entries set by
    /// Group Policy, packaged tasks in an unrecognized state, and per-user entries while
    /// this process runs as a different account than the signed-in user or that cannot be
    /// confirmed. `note` says why.
    #[serde(default = "default_true")]
    pub can_toggle: bool,
    /// Why the entry cannot be toggled; empty when it can.
    #[serde(default)]
    pub note: String,
}

fn default_true() -> bool {
    true
}

/// Startup entries of the current user and the machine, sorted by name: the Run keys
/// (HKCU, HKLM, HKLM 32-bit), the Startup folders (per-user and all users), the startup
/// tasks of packaged apps installed for the current user that have a recorded state, and
/// the `Policies\Explorer\Run` keys (HKCU and HKLM, never toggleable). RunOnce keys,
/// services, scheduled tasks and Winlogon entries are not included. Read-only.
///
/// Per-user entries are listed with `can_toggle` false and an explaining `note` while this
/// process runs as a different account than the signed-in user, or when that cannot be
/// determined.
pub fn list() -> Result<Vec<StartupEntry>> {
    SYSTEM.list(&Account::current())
}

/// Enables or disables one entry by writing its StartupApproved value (Run keys and Startup
/// folders) or its startup task state (packaged tasks) through `safety` (journaled,
/// revertible). Returns `AlreadyInDesiredState` when nothing changes. Group Policy entries
/// are refused, and so are per-user entries while this process runs as a different account
/// than the signed-in user or when that cannot be determined.
pub fn set_enabled(safety: &Safety, id: &str, enabled: bool) -> Result<MutationOutcome> {
    SYSTEM.set_enabled(safety, id, enabled, &session::elevated_as_other_user)
}

/// Fails, with the message [`set_enabled`] would give, when per-user entries cannot be
/// changed from this process: it runs as a different account than the signed-in user, or
/// the two accounts cannot be compared.
pub fn ensure_per_user_changes_allowed() -> Result<()> {
    Account::current().ensure_signed_in()
}

/// Registry value [`set_enabled`] writes for entry `id`: its StartupApproved value (Run keys
/// and Startup folders) or its startup task's `State` (packaged tasks). `None` for Group
/// Policy sources and malformed ids. Pure: nothing is read, and the entry need not exist.
pub fn registry_target(id: &str) -> Option<RegistryTarget> {
    SYSTEM.registry_target(id)
}

// ───────────────────────────── ids ─────────────────────────────

fn entry_id(source: StartupSource, key: &str) -> String {
    format!("{}:{key}", source.key())
}

/// Splits `<source>:<key>`. The key may itself contain colons.
fn parse_id(id: &str) -> Option<(StartupSource, &str)> {
    let (source, key) = id.split_once(':')?;
    if key.is_empty() {
        return None;
    }
    Some((StartupSource::from_key(source)?, key))
}

/// Id key of a packaged task.
fn packaged_key(family: &str, task_id: &str) -> String {
    format!(r"{family}\{task_id}")
}

// ───────────────────────────── StartupApproved values ─────────────────────────────

/// Enabled state encoded by a StartupApproved value. A missing or empty value is enabled.
fn approved_is_enabled(data: Option<&[u8]>) -> bool {
    match data.and_then(|d| d.first()) {
        Some(flags) => flags & 1 == 0,
        None => true,
    }
}

/// Value Task Manager writes to enable an entry.
fn approved_enabled_value() -> Vec<u8> {
    let mut data = vec![0u8; 12];
    data[0] = 0x02;
    data
}

/// Value Task Manager writes to disable an entry: flags, then the time it was disabled.
fn approved_disabled_value(filetime: u64) -> Vec<u8> {
    let mut data = vec![0x03, 0, 0, 0];
    data.extend_from_slice(&filetime.to_le_bytes());
    data
}

// ───────────────────────────── layout ─────────────────────────────

/// Registry locations the scan reads and `set_enabled` writes. [`SYSTEM`] is the real
/// configuration; another layout points the same logic at different keys.
#[derive(Debug, Clone, Copy)]
struct Layout<'a> {
    /// Run keys as `(source, hive, key path)`, Group Policy Run keys included.
    run_keys: &'a [(StartupSource, Hive, &'a str)],
    /// Parent of the StartupApproved subkeys, opened in each source's approved hive.
    approved_root: &'a str,
    /// Whether the Startup folders are scanned.
    folders: bool,
    /// HKCU key holding one subkey per package family with one subkey per startup task;
    /// `None` leaves packaged tasks out.
    packaged_root: Option<&'a str>,
    /// Installed packages of a family: `Some(empty)` when none is installed, `None` when
    /// that cannot be determined.
    packages: fn(&str) -> Option<Vec<Package>>,
}

const SYSTEM: Layout<'static> = Layout {
    run_keys: &[
        (StartupSource::UserRun, Hive::CurrentUser, USER_RUN_KEY),
        (
            StartupSource::MachineRun,
            Hive::LocalMachine,
            MACHINE_RUN_KEY,
        ),
        (
            StartupSource::MachineRun32,
            Hive::LocalMachine,
            MACHINE_RUN32_KEY,
        ),
        (
            StartupSource::PolicyUserRun,
            Hive::CurrentUser,
            USER_POLICY_RUN_KEY,
        ),
        (
            StartupSource::PolicyMachineRun,
            Hive::LocalMachine,
            MACHINE_POLICY_RUN_KEY,
        ),
    ],
    approved_root: APPROVED_ROOT,
    folders: true,
    packaged_root: Some(packaged::TASKS_ROOT),
    packages: packaged::installed_packages,
};

/// Name, command line and resolved executable of one entry before its state is read.
struct RawEntry {
    name: String,
    command: String,
    path: String,
}

impl Layout<'_> {
    fn approved_key(&self, source: StartupSource) -> Option<String> {
        source
            .approved_subkey()
            .map(|subkey| format!("{}\\{subkey}", self.approved_root))
    }

    /// Registry value that turns entry `id` on or off in this layout; see
    /// [`registry_target`].
    fn registry_target(&self, id: &str) -> Option<RegistryTarget> {
        let (source, key) = parse_id(id)?;
        if source.is_policy() {
            return None;
        }
        if source == StartupSource::PackagedTask {
            let (family, task_id) = key.split_once('\\')?;
            if family.is_empty() || task_id.is_empty() {
                return None;
            }
            return Some(RegistryTarget {
                hive: Hive::CurrentUser,
                key_path: packaged::task_key_path(self.packaged_root?, family, task_id),
                value_name: packaged::STATE_VALUE.to_string(),
            });
        }
        Some(RegistryTarget {
            hive: source.approved_hive(),
            key_path: self.approved_key(source)?,
            value_name: key.to_string(),
        })
    }

    /// Entries of every source in this layout, sorted by name (case-insensitive). Per-user
    /// entries are marked as not toggleable unless `account` is the signed-in user.
    fn list(&self, account: &Account) -> Result<Vec<StartupEntry>> {
        let mut entries = Vec::new();
        for source in StartupSource::ALL {
            entries.extend(self.entries(source, account)?);
        }
        entries.sort_by_cached_key(|e| (e.name.to_lowercase(), e.id.clone()));
        Ok(entries)
    }

    /// Entries of one source; empty when the layout does not include it.
    fn entries(&self, source: StartupSource, account: &Account) -> Result<Vec<StartupEntry>> {
        let mut entries = if source == StartupSource::PackagedTask {
            self.packaged_entries()?
        } else {
            self.approved_entries(source)?
        };
        if let Some(note) = account.per_user_note().filter(|_| source.is_per_user()) {
            for entry in entries.iter_mut().filter(|e| e.can_toggle) {
                entry.can_toggle = false;
                entry.note = note.to_string();
            }
        }
        Ok(entries)
    }

    /// Entries of a Run key or Startup folder source, with their StartupApproved state.
    fn approved_entries(&self, source: StartupSource) -> Result<Vec<StartupEntry>> {
        let raw: Vec<RawEntry> = if source.is_folder() {
            if !self.folders {
                return Ok(Vec::new());
            }
            folder::items(source)?
                .into_iter()
                .map(|item| RawEntry {
                    name: item.name,
                    command: item.command,
                    path: item.path,
                })
                .collect()
        } else {
            let Some(&(_, hive, key)) = self.run_keys.iter().find(|(s, _, _)| *s == source) else {
                return Ok(Vec::new());
            };
            run_key::string_values(hive, key)?
                .into_iter()
                .map(|(name, command)| {
                    let path = command::executable_path(&command);
                    RawEntry {
                        name,
                        command,
                        path,
                    }
                })
                .collect()
        };

        let approved = match self.approved_key(source) {
            Some(key) => Key::open(source.approved_hive(), &key, false)?,
            None => None,
        };
        let mut entries = Vec::with_capacity(raw.len());
        for RawEntry {
            name,
            command,
            path,
        } in raw
        {
            let state = match &approved {
                Some(key) => key.query_raw(&name)?,
                None => None,
            };
            let policy = source.is_policy();
            entries.push(StartupEntry {
                id: entry_id(source, &name),
                location: source.location().to_string(),
                publisher: publisher::company_name(&path),
                enabled: policy
                    || approved_is_enabled(state.as_ref().map(|raw| raw.data.as_slice())),
                requires_admin: source.requires_admin(),
                can_toggle: !policy,
                note: if policy {
                    POLICY_NOTE.to_string()
                } else {
                    String::new()
                },
                exists: path_exists(&path),
                name,
                source,
                command,
                path,
            });
        }
        Ok(entries)
    }

    /// Startup tasks of packaged apps installed for the current user.
    fn packaged_entries(&self) -> Result<Vec<StartupEntry>> {
        let Some(root) = self.packaged_root else {
            return Ok(Vec::new());
        };
        let source = StartupSource::PackagedTask;
        let mut entries = Vec::new();
        for task in packaged::task_keys(root)? {
            let info = match (self.packages)(&task.family) {
                Some(packages) if packages.is_empty() => continue,
                Some(packages) => {
                    match packaged::describe(&task.family, &task.task_id, &packages) {
                        Some(info) => info,
                        None => continue,
                    }
                }
                None => packaged::TaskInfo {
                    name: packaged::fallback_name(&task.family, &task.task_id),
                    path: String::new(),
                    publisher: String::new(),
                },
            };
            let state = TaskState::from_raw(task.state);
            let note = match state {
                _ if state.set_by_policy() => POLICY_NOTE.to_string(),
                TaskState::Unknown(raw) => {
                    format!("Unrecognized startup task state {raw}; it is left unchanged.")
                }
                _ => String::new(),
            };
            let publisher = match publisher::company_name(&info.path) {
                company if company.is_empty() => info.publisher,
                company => company,
            };
            entries.push(StartupEntry {
                id: entry_id(source, &packaged_key(&task.family, &task.task_id)),
                name: info.name,
                source,
                location: source.location().to_string(),
                command: if info.path.is_empty() {
                    String::new()
                } else {
                    command::quote(&info.path)
                },
                exists: path_exists(&info.path),
                path: info.path,
                publisher,
                enabled: state.is_enabled(),
                requires_admin: source.requires_admin(),
                can_toggle: note.is_empty(),
                note,
            });
        }
        Ok(entries)
    }

    /// `other_user` reports whether this process runs as a different account than the
    /// signed-in user; it is consulted only for per-user entries, which are refused when it
    /// is true or fails.
    fn set_enabled(
        &self,
        safety: &Safety,
        id: &str,
        enabled: bool,
        other_user: &dyn Fn() -> Result<bool>,
    ) -> Result<MutationOutcome> {
        let unknown = || Error::Other(format!("unknown startup entry {id:?}"));
        let (source, _) = parse_id(id).ok_or_else(unknown)?;
        if source.is_policy() {
            return Err(Error::Other(format!(
                "startup entry {id:?} is set by Group Policy and cannot be turned on or off here"
            )));
        }
        if source.is_per_user() {
            Account::from_check(other_user()).ensure_signed_in()?;
        }
        let entry = self
            .entries(source, &Account::SignedIn)?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(unknown)?;
        if !entry.can_toggle {
            return Err(Error::Other(format!(
                "startup entry {:?} cannot be turned on or off here: {}",
                entry.name, entry.note
            )));
        }
        if entry.enabled == enabled {
            return Ok(MutationOutcome::AlreadyInDesiredState);
        }
        if entry.requires_admin {
            safety.ensure_elevated()?;
        }

        let target = self.registry_target(&entry.id).ok_or_else(unknown)?;
        let value = if source == StartupSource::PackagedTask {
            RegValue::Dword(if enabled {
                TaskState::ENABLE_RAW
            } else {
                TaskState::DISABLE_RAW
            })
        } else if enabled {
            RegValue::Binary(approved_enabled_value())
        } else {
            RegValue::Binary(approved_disabled_value(filetime_now()))
        };
        safety.set_registry_value(target.hive, &target.key_path, &target.value_name, &value)
    }
}

fn path_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).exists()
}
