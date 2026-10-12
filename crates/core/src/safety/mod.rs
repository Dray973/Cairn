//! Reversibility layer. Every mutation the engine performs goes through [`Safety`],
//! which records the original state in the [`state_log::Journal`] *before* changing
//! anything, and can return the machine to that baseline with
//! [`Safety::rollback_to_baseline`] or the free function [`rollback::rollback_to_baseline`].
//!
//! Journaled kinds: registry values, services, scheduled task enabled flags, task
//! definitions (Task Scheduler tasks the engine registered), the power scheme, DNS servers
//! and Appx packages. A rollback restores them in that order, pushing restored live settings
//! (the mouse) to the running session right after the registry values, newest first within
//! each group.

pub mod restore_point;
pub mod rollback;
pub mod state_log;
pub mod verify;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

pub use restore_point::forbidden as restore_points_forbidden;
pub use restore_point::{create_restore_point, is_system_restore_enabled, RestorePoint};
pub use rollback::{rollback_filtered, rollback_to_baseline, RollbackFilter, RollbackReport};
pub use state_log::{Journal, JournalSummary, Written};

use crate::network::IpFamily;
use crate::win::registry::{
    delete_key_if_empty, path_within, write_requires_elevation, Hive, Key, RawValue, RegValue,
};
use crate::win::scm::{Scm, Service, ServiceConfig, ServiceStatus, StartType, MUTATE_ACCESS};
use crate::win::session;
use crate::{is_elevated, Error, Result, VERSION};

const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePointPolicy {
    /// Do not create a restore point.
    Skip,
    /// Attempt one; on failure continue and surface a warning.
    Try,
    /// Attempt one; on failure abort the session.
    Require,
}

#[derive(Debug, Clone)]
pub struct SafetyOptions {
    pub label: String,
    pub restore_point: RestorePointPolicy,
    pub restore_description: String,
    pub require_elevation: bool,
}

impl Default for SafetyOptions {
    fn default() -> Self {
        Self {
            label: "optimization".to_string(),
            restore_point: RestorePointPolicy::Try,
            restore_description: restore_point::DEFAULT_DESCRIPTION.to_string(),
            require_elevation: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOutcome {
    Applied,
    AlreadyInDesiredState,
    Skipped(String),
}

/// Whether this process runs as the account signed in at the desktop, which owns
/// HKEY_CURRENT_USER and the per-user Store packages that a session changes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UserCheck {
    Interactive,
    OtherUser,
    Unknown(String),
}

impl UserCheck {
    fn probe() -> UserCheck {
        UserCheck::from_result(session::elevated_as_other_user())
    }

    /// A failed comparison is kept as [`UserCheck::Unknown`], which refuses like
    /// [`UserCheck::OtherUser`].
    fn from_result(other_user: Result<bool>) -> UserCheck {
        match other_user {
            Ok(false) => UserCheck::Interactive,
            Ok(true) => UserCheck::OtherUser,
            Err(e) => {
                warn!(error = %e, "cannot compare this process's account with the signed-in user");
                UserCheck::Unknown(e.to_string())
            }
        }
    }

    fn to_result(&self) -> Result<()> {
        match self {
            UserCheck::Interactive => Ok(()),
            UserCheck::OtherUser => Err(session::other_user_error()),
            UserCheck::Unknown(reason) => Err(Error::Other(format!(
                "cannot confirm that per-user settings would change the signed-in user's \
                 profile ({reason}); nothing was changed"
            ))),
        }
    }
}

/// The check behind [`Safety::ensure_interactive_user`], for deciding before a session is
/// opened whether per-user changes can be made at all. Fails when this process runs as
/// another account than the signed-in user, and when the two cannot be compared.
pub fn check_interactive_user() -> Result<()> {
    UserCheck::probe().to_result()
}

/// A journaled mutation session.
#[derive(Debug)]
pub struct Safety {
    journal: Arc<Journal>,
    session_id: i64,
    restore_point: Option<RestorePoint>,
    warnings: Vec<String>,
    require_elevation: bool,
    /// Resolved on the first per-user change and reused for the rest of the session.
    user_check: OnceLock<UserCheck>,
}

impl Safety {
    pub fn begin(journal: Arc<Journal>, opts: SafetyOptions) -> Result<Safety> {
        if opts.require_elevation && !is_elevated() {
            return Err(Error::NotElevated);
        }
        let session_id = journal.begin_session(&opts.label, VERSION)?;
        let mut warnings = Vec::new();
        let mut restore_point = None;

        if opts.restore_point != RestorePointPolicy::Skip {
            match create_restore_point(&opts.restore_description, true) {
                Ok(rp) => {
                    journal.set_session_restore_point(session_id, rp.sequence)?;
                    journal.log_op(
                        Some(session_id),
                        "restore_point",
                        &rp.description,
                        "created",
                        Some(&rp.sequence.to_string()),
                    )?;
                    info!(sequence = rp.sequence, "restore point created");
                    restore_point = Some(rp);
                }
                Err(e) => {
                    journal.log_op(
                        Some(session_id),
                        "restore_point",
                        &opts.restore_description,
                        "failed",
                        Some(&e.to_string()),
                    )?;
                    if opts.restore_point == RestorePointPolicy::Require {
                        journal.end_session(session_id)?;
                        return Err(e);
                    }
                    warn!(error = %e, "restore point unavailable; continuing with journal only");
                    warnings.push(format!("restore point unavailable: {e}"));
                }
            }
        }

        Ok(Safety {
            journal,
            session_id,
            restore_point,
            warnings,
            require_elevation: opts.require_elevation,
            user_check: OnceLock::new(),
        })
    }

    pub fn begin_default(label: &str) -> Result<Safety> {
        let journal = Arc::new(Journal::open_default()?);
        Safety::begin(
            journal,
            SafetyOptions {
                label: label.to_string(),
                ..Default::default()
            },
        )
    }

    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    pub fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    pub fn restore_point(&self) -> Option<&RestorePoint> {
        self.restore_point.as_ref()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn check_elevation(&self, needed: bool) -> Result<()> {
        if self.require_elevation && needed && !is_elevated() {
            Err(Error::NotElevated)
        } else {
            Ok(())
        }
    }

    /// Fails with [`Error::NotElevated`] when this session requires elevation and the
    /// process is not elevated. Mutators outside this module call it before changing
    /// anything.
    pub fn ensure_elevated(&self) -> Result<()> {
        self.check_elevation(true)
    }

    /// Fails when this process runs as a different account than the one signed in at the
    /// desktop (UAC approved with another administrator's credentials), because per-user
    /// changes such as HKCU values and per-user Store packages would then land in that
    /// account's profile. Also fails when the two accounts cannot be compared. Registry
    /// writes under HKCU call it; other per-user mutators call it before changing
    /// anything. The answer is determined once per session.
    pub fn ensure_interactive_user(&self) -> Result<()> {
        self.user_check.get_or_init(UserCheck::probe).to_result()
    }

    /// Treats this session as running under another account than the signed-in user.
    #[cfg(test)]
    pub(crate) fn assume_other_user(&self) {
        let _ = self.user_check.set(UserCheck::from_result(Ok(true)));
    }

    /// Treats this session as unable to compare its account with the signed-in user.
    #[cfg(test)]
    pub(crate) fn assume_user_check_failed(&self, reason: &str) {
        let _ = self.user_check.set(UserCheck::from_result(Err(Error::Other(
            reason.to_string(),
        ))));
    }

    /// Checks shared by every registry mutation, run before anything is journaled.
    fn check_registry_write(&self, hive: Hive, key_path: &str) -> Result<()> {
        self.check_elevation(write_requires_elevation(hive, key_path))?;
        if hive == Hive::CurrentUser {
            self.ensure_interactive_user()?;
        }
        Ok(())
    }

    /// Records an Appx package before it is removed. Returns `true` when this call
    /// captured the baseline.
    pub fn record_appx(&self, rec: &state_log::NewAppxRecord) -> Result<bool> {
        self.journal.record_appx(self.session_id, rec)
    }

    /// Records the active power scheme before it is changed. Returns `true` when this
    /// call captured the baseline.
    pub fn record_power(&self, rec: &state_log::NewPowerRecord) -> Result<bool> {
        self.journal.record_power(self.session_id, rec)
    }

    /// Records a scheduled task's enabled flag before it is changed. Returns `true` when
    /// this call captured the baseline.
    pub fn record_scheduled_task(&self, rec: &state_log::NewScheduledTaskRecord) -> Result<bool> {
        self.journal.record_scheduled_task(self.session_id, rec)
    }

    /// Records one interface's static DNS servers of one address family before they are
    /// changed. Returns `true` when this call captured the baseline.
    pub fn record_dns(&self, rec: &state_log::NewDnsRecord) -> Result<bool> {
        self.journal.record_dns(self.session_id, rec)
    }

    /// Records a Task Scheduler task before it is registered; its baseline is "no task at
    /// this path". Returns `true` when this call captured the baseline.
    pub fn record_task_definition(&self, rec: &state_log::NewTaskDefinitionRecord) -> Result<bool> {
        self.journal.record_task_definition(self.session_id, rec)
    }

    /// Updates the target of the active DNS record of `interface_guid` and `family` after a
    /// write over an older baseline; the baseline is kept. Returns whether a record was
    /// updated.
    pub fn update_dns_target(
        &self,
        interface_guid: &str,
        family: IpFamily,
        target_servers: &str,
    ) -> Result<bool> {
        self.journal
            .update_dns_target(interface_guid, family, target_servers)
    }

    /// Appends an entry to the audit log under this session.
    pub fn log_op(
        &self,
        op: &str,
        target: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.journal
            .log_op(Some(self.session_id), op, target, outcome, detail)
    }

    // ───────────────────────────── registry ─────────────────────────────

    /// Records the current value, then writes `value`. Creates the key and any missing
    /// parents if needed; the record names the shallowest key created, so rollback can
    /// remove all of them. The record then keeps what was written (also when the value was
    /// already set), so later reads can tell whether the target still holds it.
    /// HKCU writes fail before anything is recorded when [`Self::ensure_interactive_user`]
    /// does. A write that fails (the key cannot be created or the value cannot be set)
    /// leaves the target as it was: the keys the write created are removed again, the
    /// record this call captured is withdrawn (see [`Self::withdraw_registry`]) and the
    /// failure is logged.
    pub fn set_registry_value(
        &self,
        hive: Hive,
        key_path: &str,
        value_name: &str,
        value: &RegValue,
    ) -> Result<MutationOutcome> {
        self.check_registry_write(hive, key_path)?;
        let target = registry_target(hive, key_path, value_name);
        let snapshot = self.snapshot_registry(hive, key_path, value_name)?;

        let desired = value.to_raw();
        if snapshot.current.as_ref() == Some(&desired) {
            self.store_written(
                hive,
                key_path,
                value_name,
                Written::Value { value: desired },
            );
            self.journal.log_op(
                Some(self.session_id),
                "set_registry_value",
                &target,
                "already_in_desired_state",
                None,
            )?;
            return Ok(MutationOutcome::AlreadyInDesiredState);
        }

        let written = Key::create(hive, key_path)
            .and_then(|(key, _)| key.set_raw(value_name, desired.kind, &desired.data));
        if let Err(e) = written {
            self.withdraw_registry(hive, key_path, value_name, &snapshot);
            self.journal.log_op(
                Some(self.session_id),
                "set_registry_value",
                &target,
                "failed",
                Some(&e.to_string()),
            )?;
            return Err(e);
        }
        self.store_written(
            hive,
            key_path,
            value_name,
            Written::Value { value: desired },
        );
        self.journal.log_op(
            Some(self.session_id),
            "set_registry_value",
            &target,
            "applied",
            Some(&value.display()),
        )?;
        info!(target = %target, value = %value.display(), "registry value set");
        Ok(MutationOutcome::Applied)
    }

    /// Records the current value, then deletes it. The record then keeps that the value was
    /// deleted, also when it was already absent. HKCU deletes fail before anything is recorded
    /// when [`Self::ensure_interactive_user`] does. A delete that fails withdraws the record
    /// this call captured, like a failed write.
    pub fn delete_registry_value(
        &self,
        hive: Hive,
        key_path: &str,
        value_name: &str,
    ) -> Result<MutationOutcome> {
        self.check_registry_write(hive, key_path)?;
        let target = registry_target(hive, key_path, value_name);
        let snapshot = self.snapshot_registry(hive, key_path, value_name)?;
        if snapshot.current.is_none() {
            self.store_written(hive, key_path, value_name, Written::Deleted);
            return Ok(MutationOutcome::AlreadyInDesiredState);
        }
        let deleted = Key::open(hive, key_path, true).and_then(|key| match key {
            Some(key) => key.delete_value(value_name).map(|_| ()),
            None => Ok(()),
        });
        if let Err(e) = deleted {
            self.withdraw_registry(hive, key_path, value_name, &snapshot);
            self.journal.log_op(
                Some(self.session_id),
                "delete_registry_value",
                &target,
                "failed",
                Some(&e.to_string()),
            )?;
            return Err(e);
        }
        self.store_written(hive, key_path, value_name, Written::Deleted);
        self.journal.log_op(
            Some(self.session_id),
            "delete_registry_value",
            &target,
            "applied",
            None,
        )?;
        info!(target = %target, "registry value deleted");
        Ok(MutationOutcome::Applied)
    }

    /// Undoes the bookkeeping of a registry write that failed: the keys the write created
    /// (from the target's key up to `snapshot.created_root`) are deleted while they are
    /// empty, and the record this call captured is ended, so no active record claims a
    /// change that did not happen and no rollback touches the untouched target. Best
    /// effort: what cannot be undone is logged, and the write's own error is what the
    /// caller reports.
    fn withdraw_registry(
        &self,
        hive: Hive,
        key_path: &str,
        value_name: &str,
        snapshot: &RegistrySnapshot,
    ) {
        let target = registry_target(hive, key_path, value_name);
        if let Some(root) = snapshot.created_root.as_deref() {
            let mut path = key_path;
            while path_within(path, root) {
                match delete_key_if_empty(hive, path) {
                    Ok(_) => {}
                    Err(e) => {
                        warn!(key = %path, error = %e, "cannot remove a key created by a failed write");
                        break;
                    }
                }
                match path.rsplit_once('\\') {
                    Some((parent, _)) if !parent.is_empty() => path = parent,
                    _ => break,
                }
            }
        }
        if !snapshot.captured {
            return;
        }
        match self
            .journal
            .withdraw_registry(self.session_id, hive, key_path, value_name)
        {
            Ok(true) => debug!(target = %target, "baseline withdrawn after a failed write"),
            Ok(false) => warn!(target = %target, "no record of this session to withdraw"),
            Err(e) => warn!(target = %target, error = %e, "cannot withdraw the journal record"),
        }
    }

    /// Stores `written` on the active record of the target. A failure only costs the target
    /// its drift check, so it is logged and never fails the write.
    fn store_written(&self, hive: Hive, key_path: &str, value_name: &str, written: Written) {
        match self
            .journal
            .set_registry_written(hive, key_path, value_name, &written)
        {
            Ok(true) => {}
            Ok(false) => warn!(
                target = %registry_target(hive, key_path, value_name),
                "no active journal record to keep the written value on"
            ),
            Err(e) => warn!(
                target = %registry_target(hive, key_path, value_name),
                error = %e,
                "cannot keep the written value in the journal"
            ),
        }
    }

    /// Captures the baseline for a registry value and returns its current raw form. When
    /// the key is missing, the record also names the shallowest missing key on its path,
    /// because creating the key creates every missing ancestor as well.
    fn snapshot_registry(
        &self,
        hive: Hive,
        key_path: &str,
        value_name: &str,
    ) -> Result<RegistrySnapshot> {
        let key = Key::open(hive, key_path, false)?;
        let key_existed = key.is_some();
        let current = match &key {
            Some(k) => k.query_raw(value_name)?,
            None => None,
        };
        let created_root = (!key_existed).then(|| first_missing_key(hive, key_path));
        let captured = self.journal.record_registry(
            self.session_id,
            &state_log::NewRegistryRecord {
                hive,
                key_path: key_path.to_string(),
                value_name: value_name.to_string(),
                key_existed,
                value_existed: current.is_some(),
                original: current.clone(),
                created_root: created_root.clone(),
            },
        )?;
        if captured {
            debug!(target = %registry_target(hive, key_path, value_name), "baseline captured");
        }
        Ok(RegistrySnapshot {
            current,
            captured,
            created_root,
        })
    }

    // ───────────────────────────── services ─────────────────────────────

    /// Records the service's start type and running state, then sets the start type.
    /// `delayed` applies only when the new start type is `Automatic`. When the start type
    /// cannot be set, the service is as it was: the record this call captured is withdrawn
    /// and the failure logged. A delayed-start flag that cannot be set after the start type
    /// changed keeps the record, because the start type did change.
    pub fn set_service_start_type(
        &self,
        name: &str,
        start_type: StartType,
        delayed: Option<bool>,
    ) -> Result<MutationOutcome> {
        self.check_elevation(true)?;
        let scm = Scm::connect()?;
        let (svc, cfg, _, captured) = self.snapshot_service(&scm, name)?;
        let target = format!("service {name}");

        let delayed = delayed.unwrap_or(cfg.delayed_auto_start);
        if cfg.start_type == start_type
            && (start_type != StartType::Automatic || cfg.delayed_auto_start == delayed)
        {
            self.journal.log_op(
                Some(self.session_id),
                "set_service_start_type",
                &target,
                "already_in_desired_state",
                None,
            )?;
            return Ok(MutationOutcome::AlreadyInDesiredState);
        }

        if let Err(e) = svc.set_start_type(start_type) {
            self.withdraw_service(name, captured);
            self.journal.log_op(
                Some(self.session_id),
                "set_service_start_type",
                &target,
                "failed",
                Some(&e.to_string()),
            )?;
            return Err(e);
        }
        if start_type == StartType::Automatic {
            svc.set_delayed_auto_start(delayed)?;
        }
        self.journal.log_op(
            Some(self.session_id),
            "set_service_start_type",
            &target,
            "applied",
            Some(start_type.label()),
        )?;
        info!(
            service = name,
            start_type = start_type.label(),
            "service start type set"
        );
        Ok(MutationOutcome::Applied)
    }

    /// Records the service's state, then stops it and waits for it to report stopped. A stop
    /// request that fails withdraws the record this call captured and logs the failure.
    pub fn stop_service(&self, name: &str) -> Result<MutationOutcome> {
        self.check_elevation(true)?;
        let scm = Scm::connect()?;
        let (svc, _, status, captured) = self.snapshot_service(&scm, name)?;
        let target = format!("service {name}");

        if !status.state.is_active() {
            return Ok(MutationOutcome::AlreadyInDesiredState);
        }
        if !status.accepts_stop {
            let reason = "service does not accept stop requests".to_string();
            self.journal.log_op(
                Some(self.session_id),
                "stop_service",
                &target,
                "skipped",
                Some(&reason),
            )?;
            return Ok(MutationOutcome::Skipped(reason));
        }

        let final_state = match svc.stop(SERVICE_STOP_TIMEOUT) {
            Ok(state) => state,
            Err(e) => {
                self.withdraw_service(name, captured);
                self.journal.log_op(
                    Some(self.session_id),
                    "stop_service",
                    &target,
                    "failed",
                    Some(&e.to_string()),
                )?;
                return Err(e);
            }
        };
        let outcome = if final_state.is_active() {
            "timeout"
        } else {
            "applied"
        };
        self.journal.log_op(
            Some(self.session_id),
            "stop_service",
            &target,
            outcome,
            None,
        )?;
        info!(service = name, state = ?final_state, "service stop requested");
        Ok(MutationOutcome::Applied)
    }

    /// Opens the service and records its baseline. The last element says whether this call
    /// captured the record (an older active record is kept).
    fn snapshot_service(
        &self,
        scm: &Scm,
        name: &str,
    ) -> Result<(Service, ServiceConfig, ServiceStatus, bool)> {
        let svc = scm.open_required(name, MUTATE_ACCESS)?;
        let cfg = svc.config()?;
        let status = svc.status()?;
        let captured = self.journal.record_service(
            self.session_id,
            &state_log::NewServiceRecord {
                name: cfg.name.clone(),
                display_name: cfg.display_name.clone(),
                start_type: cfg.start_type,
                delayed_auto_start: cfg.delayed_auto_start,
                was_running: status.state.is_active(),
            },
        )?;
        if captured {
            debug!(service = name, "baseline captured");
        }
        Ok((svc, cfg, status, captured))
    }

    /// Ends the service record this session captured for `name` after the change that
    /// followed failed, so nothing claims a change that did not happen. Best effort: a
    /// journal error is only logged.
    fn withdraw_service(&self, name: &str, captured: bool) {
        if !captured {
            return;
        }
        match self.journal.withdraw_service(self.session_id, name) {
            Ok(true) => debug!(service = name, "baseline withdrawn after a failed change"),
            Ok(false) => warn!(service = name, "no record of this session to withdraw"),
            Err(e) => warn!(service = name, error = %e, "cannot withdraw the journal record"),
        }
    }

    // ───────────────────────────── rollback ─────────────────────────────

    pub fn rollback_to_baseline(&self) -> Result<RollbackReport> {
        rollback::rollback_journal(&self.journal, false)
    }

    pub fn rollback_plan(&self) -> Result<RollbackReport> {
        rollback::rollback_journal(&self.journal, true)
    }
}

impl Drop for Safety {
    fn drop(&mut self) {
        let _ = self.journal.end_session(self.session_id);
    }
}

/// Session for unit tests: never asks for a restore point.
///
/// Unlike [`Safety::begin`], it opens even when `require_elevation` is set in an unelevated
/// process; the session's mutators then refuse with [`Error::NotElevated`], so tests can
/// check that refusal.
#[cfg(test)]
pub(crate) fn test_safety(journal: Arc<Journal>, label: &str, require_elevation: bool) -> Safety {
    let session_id = journal
        .begin_session(label, VERSION)
        .expect("begin a unit-test session");
    Safety {
        journal,
        session_id,
        restore_point: None,
        warnings: Vec::new(),
        require_elevation,
        user_check: OnceLock::new(),
    }
}

/// Session for unit tests that carries a restore point outcome as [`Safety::begin`] would
/// leave it: `restore_point` as if one was created, `warnings` as if creating one failed.
/// Never asks for a restore point; elevation is not required.
#[cfg(test)]
pub(crate) fn test_safety_with_outcome(
    journal: Arc<Journal>,
    label: &str,
    restore_point: Option<RestorePoint>,
    warnings: Vec<String>,
) -> Safety {
    let mut safety = test_safety(journal, label, false);
    safety.restore_point = restore_point;
    safety.warnings = warnings;
    safety
}

/// What [`Safety::snapshot_registry`] found and recorded before a write.
#[derive(Debug)]
struct RegistrySnapshot {
    /// The value as it is now; `None` when it is absent.
    current: Option<RawValue>,
    /// This snapshot captured the record (no older active record of the target existed).
    captured: bool,
    /// The shallowest key the write will create, when the target's key does not exist.
    created_root: Option<String>,
}

/// The shallowest missing key on the path to `key_path`, which is itself missing: walks up
/// through the parents until one exists. A parent that cannot be read counts as existing,
/// so rollback never deletes above it.
fn first_missing_key(hive: Hive, key_path: &str) -> String {
    let mut root = key_path;
    while let Some((parent, _)) = root.rsplit_once('\\') {
        if parent.is_empty() || !matches!(Key::open(hive, parent, false), Ok(None)) {
            break;
        }
        root = parent;
    }
    root.to_string()
}

/// `HKLM\<key>\<value>`, the target text of a registry value in logs and reports, exactly as
/// [`state_log::RegistryRecord::target`] words it.
pub(crate) fn registry_target(hive: Hive, key_path: &str, value_name: &str) -> String {
    let name = if value_name.is_empty() {
        "(Default)"
    } else {
        value_name
    };
    format!("{}\\{}\\{}", hive.short(), key_path, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::registry::{delete_key_if_empty, exists, read_value};

    /// Parent of the per-test sandbox keys; each test uses its own subkey.
    const SANDBOX: &str = r"Software\PCOptimizer\SelfTest\SafetyUnit";

    /// Removes the key tree the test created, deepest first, even when the test panics.
    struct KeyCleanup(Vec<String>);

    impl Drop for KeyCleanup {
        fn drop(&mut self) {
            for path in &self.0 {
                if let Ok(Some(key)) = Key::open(Hive::CurrentUser, path, true) {
                    let _ = key.delete_value("Probe");
                }
                let _ = delete_key_if_empty(Hive::CurrentUser, path);
            }
            let _ = delete_key_if_empty(Hive::CurrentUser, SANDBOX);
        }
    }

    fn unit_session(dir: &std::path::Path) -> Safety {
        let journal = Journal::open(dir.join("journal.db")).unwrap();
        test_safety(Arc::new(journal), "safety unit", false)
    }

    /// Both registry mutators fail with `expected` for `key`, and nothing is written or
    /// journaled.
    fn assert_per_user_writes_refused(safety: &Safety, key: &str, expected: &str) {
        let _cleanup = KeyCleanup(vec![key.to_string()]);
        let err = safety
            .set_registry_value(Hive::CurrentUser, key, "Probe", &RegValue::Dword(1))
            .unwrap_err();
        assert_eq!(err.to_string(), expected);
        let err = safety
            .delete_registry_value(Hive::CurrentUser, key, "Probe")
            .unwrap_err();
        assert_eq!(err.to_string(), expected);
        assert_eq!(
            safety.ensure_interactive_user().unwrap_err().to_string(),
            expected
        );

        assert!(!exists(Hive::CurrentUser, key).unwrap(), "nothing written");
        assert_eq!(
            safety.journal().summary().unwrap().registry_total,
            0,
            "nothing journaled"
        );
    }

    #[test]
    fn unit_test_sessions_never_take_a_restore_point() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let safety = test_safety(journal.clone(), "unit", true);
        assert!(safety.restore_point().is_none());
        assert!(safety.warnings().is_empty());
        assert_eq!(safety.ensure_elevated().is_ok(), is_elevated());
        assert_eq!(journal.sessions().unwrap()[0].label, "unit");
        assert!(
            journal.ops(10).unwrap().is_empty(),
            "no restore point was attempted"
        );
        assert!(restore_points_forbidden());
    }

    #[test]
    fn per_user_writes_are_refused_for_another_account() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        safety.assume_other_user();
        let key = format!(r"{SANDBOX}\OtherAccount");
        let expected = session::other_user_error().to_string();
        assert_per_user_writes_refused(&safety, &key, &expected);
    }

    #[test]
    fn per_user_writes_are_refused_when_the_accounts_cannot_be_compared() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        safety.assume_user_check_failed("Access is denied.");
        let key = format!(r"{SANDBOX}\UnknownAccount");
        let expected = "cannot confirm that per-user settings would change the signed-in \
                        user's profile (Access is denied.); nothing was changed";
        assert_per_user_writes_refused(&safety, &key, expected);
    }

    #[test]
    fn a_failed_account_comparison_refuses_like_another_account() {
        assert_eq!(UserCheck::from_result(Ok(false)), UserCheck::Interactive);
        assert_eq!(UserCheck::from_result(Ok(true)), UserCheck::OtherUser);
        let failed = UserCheck::from_result(Err(Error::Other("no session".into())));
        assert_eq!(failed, UserCheck::Unknown("no session".into()));
        assert!(failed.to_result().is_err());
        assert!(UserCheck::OtherUser.to_result().is_err());
        assert!(UserCheck::Interactive.to_result().is_ok());
    }

    #[test]
    fn signed_in_user_passes_the_check_once_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        safety.ensure_interactive_user().unwrap();
        assert_eq!(safety.user_check.get(), Some(&UserCheck::Interactive));
        safety.ensure_interactive_user().unwrap();
        check_interactive_user().unwrap();
    }

    /// A value name longer than the registry's 16,383-character limit: the key is created,
    /// then the value write fails with `ERROR_INVALID_PARAMETER`.
    fn too_long_value_name() -> String {
        "n".repeat(16_384)
    }

    #[test]
    fn a_failed_registry_write_removes_the_created_keys_and_withdraws_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        let root = format!(r"{SANDBOX}\FailedWrite");
        let key_path = format!(r"{root}\Leaf");
        let _cleanup = KeyCleanup(vec![key_path.clone(), root.clone()]);
        // The parent of the key the write creates exists, so only Leaf and FailedWrite are new.
        let (sandbox, _) = Key::create(Hive::CurrentUser, SANDBOX).unwrap();
        drop(sandbox);
        assert!(!exists(Hive::CurrentUser, &root).unwrap(), "clean start");

        let err = safety
            .set_registry_value(
                Hive::CurrentUser,
                &key_path,
                &too_long_value_name(),
                &RegValue::Dword(1),
            )
            .unwrap_err();
        assert!(!err.to_string().is_empty(), "the write reports the failure");

        // The keys the write created on the way to the value are gone again.
        assert!(
            !exists(Hive::CurrentUser, &key_path).unwrap(),
            "the leaf key the write created was removed"
        );
        assert!(
            !exists(Hive::CurrentUser, &root).unwrap(),
            "the parent key the write created was removed"
        );
        // No active record claims a change, so the drift check lists nothing and Undo has
        // nothing to restore: the registry is untouched.
        assert!(
            safety.journal().active_registry().unwrap().is_empty(),
            "the record captured for the failed write was withdrawn"
        );
        let report = safety.rollback_to_baseline().unwrap();
        assert_eq!(report.registry_restored, 0);
        assert_eq!(report.registry_deleted, 0);
        assert!(report.is_clean());
    }

    #[test]
    fn a_failed_registry_write_keeps_a_record_an_earlier_session_made() {
        // When an earlier session already recorded the target, a later session's failed write
        // of that same target does not withdraw the older record: its change did happen.
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let key_path = format!(r"{SANDBOX}\SharedTarget");
        let _cleanup = KeyCleanup(vec![key_path.clone()]);
        let (key, _) = Key::create(Hive::CurrentUser, &key_path).unwrap();
        drop(key);
        let name = too_long_value_name();
        let first = journal.begin_session("first", "test").unwrap();
        assert!(journal
            .record_registry(
                first,
                &state_log::NewRegistryRecord {
                    hive: Hive::CurrentUser,
                    key_path: key_path.clone(),
                    value_name: name.clone(),
                    key_existed: true,
                    value_existed: false,
                    original: None,
                    created_root: None,
                },
            )
            .unwrap());
        journal.end_session(first).unwrap();

        let second = test_safety(journal.clone(), "second", false);
        second
            .set_registry_value(Hive::CurrentUser, &key_path, &name, &RegValue::Dword(2))
            .unwrap_err();
        // The write itself was made and failed (not the read before it).
        let ops = journal.ops(10).unwrap();
        assert!(
            ops.iter()
                .any(|op| op.session_id == Some(second.session_id())
                    && op.op == "set_registry_value"
                    && op.outcome == "failed"),
            "{ops:#?}"
        );
        // The older session's record of the same target stays active, and the key it found
        // existing is kept.
        let active = journal.active_registry().unwrap();
        assert_eq!(active.len(), 1, "only the older session's record remains");
        assert_eq!(active[0].session_id, first);
        assert_eq!(active[0].value_name, name);
        assert!(exists(Hive::CurrentUser, &key_path).unwrap());
    }

    #[test]
    fn first_missing_key_is_the_child_of_the_deepest_existing_key() {
        let parent = format!(r"{SANDBOX}\FirstMissing");
        let _cleanup = KeyCleanup(vec![parent.clone()]);
        let (key, _) = Key::create(Hive::CurrentUser, &parent).unwrap();
        drop(key);
        assert_eq!(
            first_missing_key(Hive::CurrentUser, &format!(r"{parent}\A\B\C")),
            format!(r"{parent}\A")
        );
        assert_eq!(
            first_missing_key(Hive::CurrentUser, &format!(r"{parent}\A")),
            format!(r"{parent}\A")
        );
        assert_eq!(
            first_missing_key(Hive::CurrentUser, "PCOptimizerNoSuchRootKey"),
            "PCOptimizerNoSuchRootKey"
        );
        assert_eq!(read_value(Hive::CurrentUser, &parent, "x").unwrap(), None);
    }

    /// The active record of `key`\Probe.
    fn probe_record(safety: &Safety, key: &str) -> state_log::RegistryRecord {
        safety
            .journal()
            .active_registry()
            .unwrap()
            .into_iter()
            .find(|r| r.key_path.eq_ignore_ascii_case(key) && r.value_name == "Probe")
            .expect("an active record of Probe")
    }

    fn written_dword(value: u32) -> Option<Written> {
        Some(Written::Value {
            value: RegValue::Dword(value).to_raw(),
        })
    }

    #[test]
    fn set_registry_value_records_what_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        let key = format!(r"{SANDBOX}\Written");
        let _cleanup = KeyCleanup(vec![key.clone()]);
        let (handle, _) = Key::create(Hive::CurrentUser, &key).unwrap();
        handle.set("Probe", &RegValue::Dword(1)).unwrap();
        drop(handle);

        // Already set: the record keeps the value all the same.
        let outcome = safety
            .set_registry_value(Hive::CurrentUser, &key, "Probe", &RegValue::Dword(1))
            .unwrap();
        assert_eq!(outcome, MutationOutcome::AlreadyInDesiredState);
        assert_eq!(probe_record(&safety, &key).written, written_dword(1));

        let outcome = safety
            .set_registry_value(Hive::CurrentUser, &key, "Probe", &RegValue::Dword(0))
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
        let record = probe_record(&safety, &key);
        assert_eq!(record.written, written_dword(0));
        assert_eq!(
            record.original,
            Some(RegValue::Dword(1).to_raw()),
            "the baseline is kept"
        );
        assert_eq!(
            safety.journal().summary().unwrap().registry_total,
            1,
            "one record of the target"
        );
    }

    #[test]
    fn delete_registry_value_records_a_delete() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        let key = format!(r"{SANDBOX}\WrittenDelete");
        let _cleanup = KeyCleanup(vec![key.clone()]);
        let (handle, _) = Key::create(Hive::CurrentUser, &key).unwrap();
        handle.set("Probe", &RegValue::Dword(5)).unwrap();
        drop(handle);

        let outcome = safety
            .delete_registry_value(Hive::CurrentUser, &key, "Probe")
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied);
        assert_eq!(read_value(Hive::CurrentUser, &key, "Probe").unwrap(), None);
        let record = probe_record(&safety, &key);
        assert_eq!(record.written, Some(Written::Deleted));
        assert_eq!(record.original, Some(RegValue::Dword(5).to_raw()));
    }

    #[test]
    fn already_absent_delete_records_written_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let safety = unit_session(dir.path());
        let key = format!(r"{SANDBOX}\WrittenAbsent");
        let _cleanup = KeyCleanup(vec![key.clone()]);
        assert!(!exists(Hive::CurrentUser, &key).unwrap());

        let outcome = safety
            .delete_registry_value(Hive::CurrentUser, &key, "Probe")
            .unwrap();
        assert_eq!(outcome, MutationOutcome::AlreadyInDesiredState);
        let record = probe_record(&safety, &key);
        assert_eq!(record.written, Some(Written::Deleted));
        assert_eq!(record.original, None);
        assert!(
            !exists(Hive::CurrentUser, &key).unwrap(),
            "nothing was created"
        );
    }
}
