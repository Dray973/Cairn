//! Replays the journal to return the system to its recorded baseline, either completely
//! or for a selected subset of records.
//!
//! Records are restored by group in a fixed order: registry values, services, scheduled
//! tasks, task definitions, the power scheme, DNS servers, then Appx packages. Within each
//! group the newest record comes first. Restored values that Windows reads only at sign-in
//! (the mouse settings) are pushed to the running session after the registry group, once
//! per setting.
//!
//! Scheduled tasks, task definitions (the Task Scheduler tasks Cairn registered, which a
//! rollback deletes), live settings, DNS servers and the elevation check reach the system
//! through `RollbackSystem`; registry values, services, the power scheme and Appx packages
//! are restored directly.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::state_log::{
    AppxRecord, DnsRecord, Journal, JournalTable, PowerRecord, RegistryRecord, ScheduledTaskRecord,
    ServiceRecord, TaskDefinitionRecord,
};
use crate::debloat::appx::{self, RestoreOutcome};
use crate::debloat::catalog::{self, RestartNeed};
use crate::debloat::live::{self, LiveSetting};
use crate::debloat::power;
use crate::debloat::scheduled_tasks::{self, TaskRestore, TaskStore};
use crate::network::{self, canonical_guid, DnsRestore};
use crate::win::registry::{delete_key_if_empty, path_within, write_requires_elevation, Hive, Key};
use crate::win::scm::{Scm, StartType, MUTATE_ACCESS};
use crate::win::task_scheduler::TaskScheduler;
use crate::{Error, Result, VERSION};

const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(20);

const SESSION_LABEL_ALL: &str = "rollback_to_baseline";
const SESSION_LABEL_FILTERED: &str = "rollback_filtered";

const OP_ROLLBACK_DNS: &str = "rollback_dns";
const OP_FLUSH_DNS: &str = "flush_dns_cache";
const FLUSH_TARGET: &str = "DNS resolver cache";
const OP_ROLLBACK_TASK_DEFINITION: &str = "rollback_task_definition";

/// A selected record that could not be restored. Its journal record stays active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackFailure {
    pub target: String,
    pub error: String,
}

/// A removed package whose files are gone, so it can only come back from the Microsoft
/// Store. `store_link` opens its Store page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreReinstall {
    pub package_family: String,
    pub store_link: String,
}

/// Outcome of a rollback. The counters cover records that were restored and marked
/// reverted in the journal; a dry run leaves them at zero and only fills `actions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RollbackReport {
    pub dry_run: bool,
    /// Registry values written back to their recorded data.
    pub registry_restored: usize,
    /// Registry values deleted because they did not exist before the first change.
    pub registry_deleted: usize,
    /// Services whose start type was restored.
    pub services_restored: usize,
    /// Restored services that were started because they were running before.
    pub services_started: usize,
    /// Scheduled tasks whose recorded enabled flag was written back or was already in
    /// place, plus recorded tasks that no longer exist.
    #[serde(default)]
    pub scheduled_tasks_restored: usize,
    /// Appx packages re-registered from their recorded install location, plus removed
    /// packages whose family is installed again (for example reinstalled by Windows or
    /// from the Store), which need nothing restored.
    pub appx_restored: usize,
    /// Packages that could not be re-registered locally and whose family was not found
    /// installed again. Their journal records stay active so a later rollback retries them.
    pub appx_store_required: Vec<StoreReinstall>,
    /// Power scheme baselines reactivated.
    pub power_restored: usize,
    /// DNS server baselines (one per adapter and address family) written back or already
    /// in place, plus records of adapters that no longer exist.
    #[serde(default)]
    pub dns_restored: usize,
    /// Scheduled tasks Cairn had registered that were deleted, plus recorded tasks that no
    /// longer existed.
    #[serde(default)]
    pub task_definitions_deleted: usize,
    /// One line per selected record, in restore order.
    pub actions: Vec<String>,
    /// Records that could not be restored. Their journal records stay active.
    pub failures: Vec<RollbackFailure>,
    /// Strongest restart need among the catalog tweaks whose records were restored (what
    /// the user has to do before the restored settings are in effect). In a dry run, the
    /// need of the selected records. Records no tweak owns, such as startup entries, Appx
    /// packages, DNS servers and task definitions, add nothing. A live setting that could not
    /// be pushed to the running session raises it to at least a sign-out.
    #[serde(default)]
    pub restart: RestartNeed,
}

impl RollbackReport {
    /// True when nothing failed and no package is waiting for a Store reinstall.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.appx_store_required.is_empty()
    }

    pub fn total_reverted(&self) -> usize {
        self.registry_restored
            + self.registry_deleted
            + self.services_restored
            + self.scheduled_tasks_restored
            + self.appx_restored
            + self.power_restored
            + self.dns_restored
            + self.task_definitions_deleted
    }
}

/// A registry value addressed by hive, key path and value name. Comparison with journal
/// records ignores ASCII case in the path and name, as the registry does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryTarget {
    pub hive: Hive,
    pub key_path: String,
    pub value_name: String,
}

/// Selects the journal records a partial rollback reverts. Service names, package families,
/// scheduled task paths and task definition paths compare case-insensitively; interface GUIDs
/// compare in canonical form. An empty filter selects nothing. Missing fields deserialize as
/// empty, so a filter may name only the kinds of records it selects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RollbackFilter {
    pub registry: Vec<RegistryTarget>,
    pub services: Vec<String>,
    pub appx_families: Vec<String>,
    pub power: bool,
    /// Full task paths, such as `\Microsoft\Windows\Autochk\Proxy`.
    pub scheduled_tasks: Vec<String>,
    /// Adapter interface GUIDs, with or without braces. A GUID selects the records of both
    /// address families.
    pub dns: Vec<String>,
    /// Full paths of tasks Cairn registered, such as `\Cairn\Maintenance-S-1-5-21-…`.
    pub task_definitions: Vec<String>,
}

impl RollbackFilter {
    pub fn is_empty(&self) -> bool {
        self.registry.is_empty()
            && self.services.is_empty()
            && self.appx_families.is_empty()
            && !self.power
            && self.scheduled_tasks.is_empty()
            && self.dns.is_empty()
            && self.task_definitions.is_empty()
    }

    fn selects_registry(&self, rec: &RegistryRecord) -> bool {
        self.registry.iter().any(|t| {
            t.hive == rec.hive
                && t.key_path.eq_ignore_ascii_case(&rec.key_path)
                && t.value_name.eq_ignore_ascii_case(&rec.value_name)
        })
    }

    fn selects_service(&self, rec: &ServiceRecord) -> bool {
        self.services
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&rec.name))
    }

    fn selects_appx(&self, rec: &AppxRecord) -> bool {
        self.appx_families
            .iter()
            .any(|family| family.eq_ignore_ascii_case(&rec.package_family))
    }

    fn selects_scheduled_task(&self, rec: &ScheduledTaskRecord) -> bool {
        self.scheduled_tasks
            .iter()
            .any(|path| path.eq_ignore_ascii_case(&rec.path))
    }

    fn selects_dns(&self, rec: &DnsRecord) -> bool {
        let guid = canonical_guid(&rec.interface_guid);
        self.dns.iter().any(|g| canonical_guid(g) == guid)
    }

    fn selects_task_definition(&self, path: &str) -> bool {
        self.task_definitions
            .iter()
            .any(|p| p.eq_ignore_ascii_case(path))
    }
}

/// What [`TaskDefinitionRemover::delete_folder_if_empty`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderRemoval {
    Removed,
    /// The folder still holds a task or a subfolder and was kept.
    NotEmpty,
    /// The folder did not exist.
    Missing,
}

/// Deletes Task Scheduler tasks (and their folder) that Cairn registered.
pub(crate) trait TaskDefinitionRemover {
    /// Ok(true) deleted; Ok(false) the task did not exist.
    fn delete(&self, path: &str) -> Result<bool>;
    /// Deletes a single-level folder such as "\Cairn" when it holds no task and no subfolder.
    fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval>;
}

/// The parts of the system a rollback reaches through a seam: the elevation check,
/// Task Scheduler, the running session's live settings and the DNS client. Production uses
/// [`LiveSystem`]; tests use fakes.
pub(crate) trait RollbackSystem {
    fn is_elevated(&self) -> bool;
    /// Task Scheduler, requested at most once per rollback and only when a scheduled task
    /// record is restored (never in a dry run).
    fn task_store(&self) -> Result<Box<dyn TaskStore + '_>>;
    /// Task Scheduler for deleting task definitions. Requested at most once per rollback and
    /// never in a dry run.
    fn task_definitions(&self) -> Result<Box<dyn TaskDefinitionRemover + '_>>;
    /// Pushes a live setting into the running session after its registry values were
    /// restored.
    fn refresh_live(&self, setting: LiveSetting) -> Result<()>;
    /// The live setting a restored registry value feeds, when Windows reads it only at
    /// sign-in.
    fn live_setting(&self, hive: Hive, key_path: &str, value_name: &str) -> Option<LiveSetting> {
        live::setting_for(hive, key_path, value_name)
    }
    /// Read-only: the servers now set for the record's adapter and family when they differ
    /// from both the recorded baseline and the recorded target.
    fn dns_drift(&self, rec: &DnsRecord) -> Result<Option<String>>;
    fn restore_dns(&self, rec: &DnsRecord) -> Result<DnsRestore>;
    fn flush_dns(&self) -> Result<()>;
}

/// This PC.
#[derive(Debug)]
struct LiveSystem;

impl RollbackSystem for LiveSystem {
    fn is_elevated(&self) -> bool {
        crate::is_elevated()
    }

    fn task_store(&self) -> Result<Box<dyn TaskStore + '_>> {
        let scheduler = TaskScheduler::connect()
            .map_err(|e| Error::Other(format!("cannot connect to Task Scheduler: {e}")))?;
        Ok(Box::new(scheduler))
    }

    fn task_definitions(&self) -> Result<Box<dyn TaskDefinitionRemover + '_>> {
        crate::maintenance::task::live_remover()
    }

    fn refresh_live(&self, setting: LiveSetting) -> Result<()> {
        live::refresh(setting)
    }

    fn dns_drift(&self, rec: &DnsRecord) -> Result<Option<String>> {
        network::dns_drift(rec)
    }

    fn restore_dns(&self, rec: &DnsRecord) -> Result<DnsRestore> {
        network::restore_dns(rec)
    }

    fn flush_dns(&self) -> Result<()> {
        network::flush_resolver_cache()
    }
}

/// Rolls back every active journal record in the default journal.
pub fn rollback_to_baseline() -> Result<RollbackReport> {
    let journal = Journal::open_default()?;
    rollback_journal(&journal, false)
}

/// Rolls back every active record in `journal`. With `dry_run`, nothing is changed and
/// the report's `actions` list describes what would happen.
///
/// Without `dry_run`, fails with [`Error::NotElevated`] before changing anything when a
/// record needs elevation (a registry value outside HKCU or under HKCU\Software\Policies,
/// a service, a scheduled task, a task definition, the power scheme or DNS servers) and the
/// process is not elevated. Appx re-registration is per user and needs none.
pub fn rollback_journal(journal: &Journal, dry_run: bool) -> Result<RollbackReport> {
    rollback_with(journal, None, dry_run, &LiveSystem)
}

/// Reverts only the active records selected by `filter`, with the same ordering, failure
/// handling, elevation check and dry-run semantics as [`rollback_journal`]. When the
/// filter is empty or selects no active record, the report is empty and no journal
/// session is opened.
pub fn rollback_filtered(
    journal: &Journal,
    filter: &RollbackFilter,
    dry_run: bool,
) -> Result<RollbackReport> {
    rollback_with(journal, Some(filter), dry_run, &LiveSystem)
}

/// Active records chosen for one rollback, each group newest first.
#[derive(Default)]
struct Selection {
    registry: Vec<RegistryRecord>,
    services: Vec<ServiceRecord>,
    scheduled_tasks: Vec<ScheduledTaskRecord>,
    task_definitions: Vec<TaskDefinitionRecord>,
    power: Vec<PowerRecord>,
    dns: Vec<DnsRecord>,
    appx: Vec<AppxRecord>,
}

impl Selection {
    /// Every active record when `filter` is `None`, else the records it selects.
    fn load(journal: &Journal, filter: Option<&RollbackFilter>) -> Result<Selection> {
        let mut sel = Selection {
            registry: journal.active_registry()?,
            services: journal.active_services()?,
            scheduled_tasks: journal.active_scheduled_tasks()?,
            task_definitions: journal.active_task_definitions()?,
            power: journal.active_power()?,
            dns: journal.active_dns()?,
            appx: journal.active_appx()?,
        };
        if let Some(f) = filter {
            sel.registry.retain(|r| f.selects_registry(r));
            sel.services.retain(|r| f.selects_service(r));
            sel.scheduled_tasks.retain(|r| f.selects_scheduled_task(r));
            sel.task_definitions
                .retain(|r| f.selects_task_definition(&r.path));
            if !f.power {
                sel.power.clear();
            }
            sel.dns.retain(|r| f.selects_dns(r));
            sel.appx.retain(|r| f.selects_appx(r));
        }
        Ok(sel)
    }

    fn is_empty(&self) -> bool {
        self.registry.is_empty()
            && self.services.is_empty()
            && self.scheduled_tasks.is_empty()
            && self.task_definitions.is_empty()
            && self.power.is_empty()
            && self.dns.is_empty()
            && self.appx.is_empty()
    }

    /// Services, scheduled tasks under `\Microsoft\Windows\`, task definitions, the power
    /// scheme and DNS servers are machine-wide, as are registry values outside the writable
    /// part of HKCU.
    fn needs_elevation(&self) -> bool {
        self.registry
            .iter()
            .any(|r| write_requires_elevation(r.hive, &r.key_path))
            || !self.services.is_empty()
            || !self.scheduled_tasks.is_empty()
            || !self.task_definitions.is_empty()
            || !self.power.is_empty()
            || !self.dns.is_empty()
    }
}

/// Shared implementation of [`rollback_journal`] (`filter` is `None`) and
/// [`rollback_filtered`], with the system behind `sys`.
fn rollback_with(
    journal: &Journal,
    filter: Option<&RollbackFilter>,
    dry_run: bool,
    sys: &dyn RollbackSystem,
) -> Result<RollbackReport> {
    let mut report = RollbackReport {
        dry_run,
        ..Default::default()
    };
    if filter.is_some_and(RollbackFilter::is_empty) {
        return Ok(report);
    }

    let selection = Selection::load(journal, filter)?;
    if filter.is_some() && selection.is_empty() {
        return Ok(report);
    }
    if selection.needs_elevation() && !dry_run && !sys.is_elevated() {
        return Err(Error::NotElevated);
    }

    let session = if dry_run {
        None
    } else {
        let label = if filter.is_some() {
            SESSION_LABEL_FILTERED
        } else {
            SESSION_LABEL_ALL
        };
        Some(journal.begin_session(label, VERSION)?)
    };

    let live_settings = revert_registry(journal, session, &selection.registry, &mut report, sys)?;
    refresh_live(journal, session, &live_settings, &mut report, sys)?;
    revert_services(journal, session, &selection.services, &mut report)?;
    revert_scheduled_tasks(
        journal,
        session,
        &selection.scheduled_tasks,
        &mut report,
        sys,
    )?;
    revert_task_definitions(
        journal,
        session,
        &selection.task_definitions,
        &mut report,
        sys,
    )?;
    revert_power(journal, session, &selection.power, &mut report)?;
    revert_dns(journal, session, &selection.dns, &mut report, sys)?;
    revert_appx(journal, session, &selection.appx, &mut report)?;

    if let Some(id) = session {
        journal.end_session(id)?;
        info!(
            restored = report.total_reverted(),
            store_required = report.appx_store_required.len(),
            failed = report.failures.len(),
            "rollback complete"
        );
    }
    Ok(report)
}

/// Logs a failed restore and adds it to the report. The journal record stays active.
fn record_failure(
    journal: &Journal,
    session: Option<i64>,
    op: &str,
    target: String,
    error: &Error,
    report: &mut RollbackReport,
) -> Result<()> {
    let error = error.to_string();
    warn!(op, target = %target, error = %error, "rollback failed");
    journal.log_op(session, op, &target, "failed", Some(&error))?;
    report.failures.push(RollbackFailure { target, error });
    Ok(())
}

// ───────────────────────────── registry ─────────────────────────────

enum RegistryOutcome {
    Restored,
    Deleted,
}

/// Restores the registry records and returns the distinct live settings of the records that
/// were restored or deleted (none in a dry run), for [`refresh_live`].
fn revert_registry(
    journal: &Journal,
    session: Option<i64>,
    records: &[RegistryRecord],
    report: &mut RollbackReport,
    sys: &dyn RollbackSystem,
) -> Result<Vec<LiveSetting>> {
    let mut live_settings = BTreeSet::new();
    for rec in records {
        let target = rec.target();
        let restart = catalog::restart_for_registry(rec.hive, &rec.key_path, &rec.value_name);
        report
            .actions
            .push(format!("{}: {target}", describe_registry(rec)));
        if report.dry_run {
            report.restart = report.restart.max(restart);
            continue;
        }
        let outcome = match restore_registry(journal, rec) {
            Ok(RegistryOutcome::Restored) => {
                report.registry_restored += 1;
                "restored"
            }
            Ok(RegistryOutcome::Deleted) => {
                report.registry_deleted += 1;
                "deleted"
            }
            Err(e) => {
                record_failure(journal, session, "rollback_registry", target, &e, report)?;
                continue;
            }
        };
        report.restart = report.restart.max(restart);
        journal.mark_reverted(JournalTable::Registry, rec.id)?;
        journal.log_op(session, "rollback_registry", &target, outcome, None)?;
        if let Some(setting) = sys.live_setting(rec.hive, &rec.key_path, &rec.value_name) {
            live_settings.insert(setting);
        }
    }
    Ok(live_settings.into_iter().collect())
}

/// Pushes each restored live setting to the running session once. A failure is not a
/// rollback failure (the values are restored and their records reverted): it is logged and
/// raises the report's restart need to at least a sign-out, when Windows reads the values.
fn refresh_live(
    journal: &Journal,
    session: Option<i64>,
    settings: &[LiveSetting],
    report: &mut RollbackReport,
    sys: &dyn RollbackSystem,
) -> Result<()> {
    for &setting in settings {
        match sys.refresh_live(setting) {
            Ok(()) => {
                journal.log_op(session, live::OP_REFRESH, setting.target(), "applied", None)?
            }
            Err(e) => {
                warn!(setting = setting.label(), error = %e, "cannot update a restored setting in this session");
                report.restart = report.restart.max(RestartNeed::SignOut);
                journal.log_op(
                    session,
                    live::OP_REFRESH,
                    setting.target(),
                    "failed",
                    Some(&e.to_string()),
                )?;
            }
        }
    }
    Ok(())
}

fn describe_registry(rec: &RegistryRecord) -> String {
    match rec.original_decoded() {
        Some(v) => format!("restore {}", v.display()),
        None if rec.key_existed => "delete value".to_string(),
        None => match own_created_root(rec) {
            Some(root) if !root.eq_ignore_ascii_case(&rec.key_path) => format!(
                "delete value and created keys up to {}\\{root}",
                rec.hive.short()
            ),
            _ => "delete value and created key".to_string(),
        },
    }
}

/// The shallowest key this record's write created, when its key did not exist. Rows
/// without a usable `created_root` (older journals) fall back to the key itself.
fn own_created_root(rec: &RegistryRecord) -> Option<&str> {
    if rec.key_existed {
        return None;
    }
    Some(match rec.created_root.as_deref() {
        Some(root) if path_within(&rec.key_path, root) => root,
        _ => rec.key_path.as_str(),
    })
}

/// Restores or deletes the value. After a delete, the keys the engine created on the way
/// to it are removed while they are empty (see [`remove_created_keys`]).
fn restore_registry(journal: &Journal, rec: &RegistryRecord) -> Result<RegistryOutcome> {
    match &rec.original {
        Some(raw) => {
            let (key, _) = Key::create(rec.hive, &rec.key_path)?;
            key.set_raw(&rec.value_name, raw.kind, &raw.data)?;
            Ok(RegistryOutcome::Restored)
        }
        None => {
            if let Some(key) = Key::open(rec.hive, &rec.key_path, true)? {
                key.delete_value(&rec.value_name)?;
            }
            remove_created_keys(journal, rec)?;
            Ok(RegistryOutcome::Deleted)
        }
    }
}

/// Deletes empty keys from the record's key upward, as long as the engine created them.
/// A key counts as created when this record's write created it (from the leaf up to its
/// `created_root`) or when an older record's write did and the key has stood since then
/// (see [`Journal::created_keys_before`]). That covers partial rollbacks that revert the
/// creating record while later values under the key remain. The walk stops at the first
/// key that still holds values or subkeys, or that existed before the engine touched it.
/// Only a failure on the record's own key fails the record; the value is gone by then, so
/// failures further up are logged and end the walk.
fn remove_created_keys(journal: &Journal, rec: &RegistryRecord) -> Result<()> {
    let own_root = own_created_root(rec);
    let older = journal.created_keys_before(rec)?;
    let created = |key: &str| {
        own_root.is_some_and(|root| path_within(key, root))
            || older
                .iter()
                .any(|(path, root)| path_within(key, root) && path_within(path, key))
    };

    let mut path = rec.key_path.as_str();
    let mut leaf = true;
    while created(path) {
        match delete_key_if_empty(rec.hive, path) {
            Ok(true) => {}
            // Not deleted: either it still holds something (stop) or it is already gone.
            Ok(false) => match Key::open(rec.hive, path, false) {
                Ok(None) => {}
                Ok(Some(_)) => break,
                Err(e) if leaf => return Err(e),
                Err(e) => {
                    warn!(key = %path, error = %e, "cannot check a created registry key");
                    break;
                }
            },
            Err(e) if leaf => return Err(e),
            Err(e) => {
                warn!(key = %path, error = %e, "cannot remove a created registry key");
                break;
            }
        }
        match path.rsplit_once('\\') {
            Some((parent, _)) if !parent.is_empty() => path = parent,
            _ => break,
        }
        leaf = false;
    }
    Ok(())
}

// ───────────────────────────── services ─────────────────────────────

fn revert_services(
    journal: &Journal,
    session: Option<i64>,
    records: &[ServiceRecord],
    report: &mut RollbackReport,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let scm = if report.dry_run {
        None
    } else {
        Some(Scm::connect()?)
    };
    for rec in records {
        let target = format!("service {}", rec.name);
        let restart = catalog::restart_for_service(&rec.name);
        report
            .actions
            .push(format!("{}: {target}", describe_service(rec)));
        let Some(scm) = scm.as_ref() else {
            report.restart = report.restart.max(restart);
            continue;
        };
        match restore_service(scm, rec) {
            Ok(started) => {
                report.services_restored += 1;
                report.restart = report.restart.max(restart);
                if started {
                    report.services_started += 1;
                }
                journal.mark_reverted(JournalTable::Service, rec.id)?;
                journal.log_op(session, "rollback_service", &target, "restored", None)?;
            }
            Err(e) => record_failure(journal, session, "rollback_service", target, &e, report)?,
        }
    }
    Ok(())
}

fn describe_service(rec: &ServiceRecord) -> String {
    let mut s = format!("set start type {}", rec.start_type.label());
    if rec.start_type == StartType::Automatic && rec.delayed_auto_start {
        s.push_str(" (delayed)");
    }
    if rec.was_running {
        s.push_str(" and start");
    }
    s
}

/// Returns `true` when the service was started as part of the restore.
fn restore_service(scm: &Scm, rec: &ServiceRecord) -> Result<bool> {
    let svc = scm.open_required(&rec.name, MUTATE_ACCESS)?;
    svc.set_start_type(rec.start_type)?;
    if rec.start_type == StartType::Automatic {
        svc.set_delayed_auto_start(rec.delayed_auto_start)?;
    }
    if rec.was_running && rec.start_type != StartType::Disabled && !svc.status()?.state.is_active()
    {
        svc.start()?;
        let deadline = std::time::Instant::now() + SERVICE_START_TIMEOUT;
        while std::time::Instant::now() < deadline && !svc.status()?.state.is_active() {
            std::thread::sleep(Duration::from_millis(100));
        }
        return Ok(true);
    }
    Ok(false)
}

// ───────────────────────────── scheduled tasks ─────────────────────────────

/// Writes each record's enabled flag back. Task Scheduler is requested once, and only
/// outside a dry run; when it cannot be reached, every record fails and the rollback
/// continues with the next group.
fn revert_scheduled_tasks(
    journal: &Journal,
    session: Option<i64>,
    records: &[ScheduledTaskRecord],
    report: &mut RollbackReport,
    sys: &dyn RollbackSystem,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let store = if report.dry_run {
        None
    } else {
        Some(sys.task_store())
    };
    for rec in records {
        let target = rec.target();
        let restart = catalog::restart_for_scheduled_task(&rec.path);
        let verb = if rec.was_enabled { "enable" } else { "disable" };
        report.actions.push(format!("{verb} {target}"));
        let store = match store.as_ref() {
            None => {
                report.restart = report.restart.max(restart);
                continue;
            }
            Some(Err(e)) => {
                record_failure(
                    journal,
                    session,
                    scheduled_tasks::OP_ROLLBACK,
                    target,
                    e,
                    report,
                )?;
                continue;
            }
            Some(Ok(store)) => store,
        };
        let state = if rec.was_enabled {
            "enabled"
        } else {
            "disabled"
        };
        match scheduled_tasks::restore_with(&**store, rec) {
            Ok(TaskRestore::Restored) => {
                report.scheduled_tasks_restored += 1;
                report.restart = report.restart.max(restart);
                journal.mark_reverted(JournalTable::ScheduledTask, rec.id)?;
                journal.log_op(
                    session,
                    scheduled_tasks::OP_ROLLBACK,
                    &target,
                    "restored",
                    Some(state),
                )?;
            }
            Ok(TaskRestore::AlreadyInState) => {
                report.scheduled_tasks_restored += 1;
                report.restart = report.restart.max(restart);
                journal.mark_reverted(JournalTable::ScheduledTask, rec.id)?;
                journal.log_op(
                    session,
                    scheduled_tasks::OP_ROLLBACK,
                    &target,
                    "restored",
                    Some(&format!("already {state}")),
                )?;
            }
            Ok(TaskRestore::Missing) => {
                report.scheduled_tasks_restored += 1;
                if let Some(action) = report.actions.last_mut() {
                    *action = format!("{target} no longer exists; nothing to restore");
                }
                journal.mark_reverted(JournalTable::ScheduledTask, rec.id)?;
                journal.log_op(
                    session,
                    scheduled_tasks::OP_ROLLBACK,
                    &target,
                    "not_found",
                    Some("the task no longer exists"),
                )?;
            }
            Err(e) => record_failure(
                journal,
                session,
                scheduled_tasks::OP_ROLLBACK,
                target,
                &e,
                report,
            )?,
        }
    }
    Ok(())
}

// ───────────────────────────── task definitions ─────────────────────────────

/// Deletes each scheduled task Cairn registered, and its folder once empty when the record
/// created it. Task Scheduler is requested once, and only outside a dry run; when it cannot
/// be reached, every record fails and the rollback continues with the next group. A task
/// that no longer exists counts as deleted.
fn revert_task_definitions(
    journal: &Journal,
    session: Option<i64>,
    records: &[TaskDefinitionRecord],
    report: &mut RollbackReport,
    sys: &dyn RollbackSystem,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let remover = if report.dry_run {
        None
    } else {
        Some(sys.task_definitions())
    };
    for rec in records {
        let target = rec.target();
        report.actions.push(format!(
            "delete the scheduled task {} that Cairn created",
            rec.path
        ));
        let remover = match remover.as_ref() {
            None => continue,
            Some(Err(e)) => {
                record_failure(
                    journal,
                    session,
                    OP_ROLLBACK_TASK_DEFINITION,
                    target,
                    e,
                    report,
                )?;
                continue;
            }
            Some(Ok(remover)) => remover,
        };
        let outcome = match remover.delete(&rec.path) {
            Ok(true) => "deleted",
            Ok(false) => "not_found",
            Err(e) => {
                record_failure(
                    journal,
                    session,
                    OP_ROLLBACK_TASK_DEFINITION,
                    target,
                    &e,
                    report,
                )?;
                continue;
            }
        };
        let detail = folder_detail(&**remover, rec);
        report.task_definitions_deleted += 1;
        journal.mark_reverted(JournalTable::TaskDefinition, rec.id)?;
        journal.log_op(
            session,
            OP_ROLLBACK_TASK_DEFINITION,
            &target,
            outcome,
            Some(&detail),
        )?;
    }
    Ok(())
}

/// Removes the task's folder when this record created it and it is empty now, and describes
/// the result. A folder error does not fail the record: the task itself is gone.
fn folder_detail(remover: &dyn TaskDefinitionRemover, rec: &TaskDefinitionRecord) -> String {
    let folder = match rec.path.rsplit_once('\\') {
        Some((folder, _)) if rec.folder_created && !folder.is_empty() => folder,
        _ => return "folder kept".to_string(),
    };
    match remover.delete_folder_if_empty(folder) {
        Ok(FolderRemoval::Removed) => "folder removed".to_string(),
        Ok(FolderRemoval::NotEmpty) => "folder kept: not empty".to_string(),
        Ok(FolderRemoval::Missing) => "folder already removed".to_string(),
        Err(e) => {
            warn!(folder, error = %e, "cannot remove the task folder Cairn created");
            format!("folder kept: {e}")
        }
    }
}

// ───────────────────────────── power ─────────────────────────────

fn revert_power(
    journal: &Journal,
    session: Option<i64>,
    records: &[PowerRecord],
    report: &mut RollbackReport,
) -> Result<()> {
    for rec in records {
        let target = format!("power scheme {}", rec.previous_scheme);
        report.actions.push(format!("restore {target}"));
        if report.dry_run {
            report.restart = report.restart.max(catalog::restart_for_power());
            continue;
        }
        match power::restore(rec) {
            Ok(()) => {
                report.power_restored += 1;
                report.restart = report.restart.max(catalog::restart_for_power());
                journal.mark_reverted(JournalTable::Power, rec.id)?;
                journal.log_op(session, "rollback_power", &target, "restored", None)?;
            }
            Err(e) => record_failure(journal, session, "rollback_power", target, &e, report)?,
        }
    }
    Ok(())
}

// ───────────────────────────── DNS servers ─────────────────────────────

/// Writes each record's DNS servers back. A dry run only reads whether the servers were
/// changed outside Cairn since. The resolver cache is flushed once, best effort,
/// after at least one record was written.
fn revert_dns(
    journal: &Journal,
    session: Option<i64>,
    records: &[DnsRecord],
    report: &mut RollbackReport,
    sys: &dyn RollbackSystem,
) -> Result<()> {
    let mut written = false;
    for rec in records {
        let target = rec.target();
        let mut action = format!("restore {target} to {}", rec.previous_text());
        if report.dry_run {
            match sys.dns_drift(rec) {
                Ok(Some(current)) => {
                    action.push_str(&format!(" (currently {current}, changed outside Cairn)"))
                }
                Ok(None) => {}
                Err(e) => {
                    debug!(target = %target, error = %e, "cannot read the current DNS servers")
                }
            }
            report.actions.push(action);
            continue;
        }
        report.actions.push(action);
        match sys.restore_dns(rec) {
            Ok(DnsRestore::Written) => {
                written = true;
                report.dns_restored += 1;
                journal.mark_reverted(JournalTable::Dns, rec.id)?;
                journal.log_op(
                    session,
                    OP_ROLLBACK_DNS,
                    &target,
                    "restored",
                    Some("written"),
                )?;
            }
            Ok(DnsRestore::AlreadyInState) => {
                report.dns_restored += 1;
                journal.mark_reverted(JournalTable::Dns, rec.id)?;
                journal.log_op(
                    session,
                    OP_ROLLBACK_DNS,
                    &target,
                    "restored",
                    Some("already set"),
                )?;
            }
            Ok(DnsRestore::NotFound) => {
                report.dns_restored += 1;
                if let Some(action) = report.actions.last_mut() {
                    *action = format!("{target}: the adapter no longer exists; nothing to restore");
                }
                journal.mark_reverted(JournalTable::Dns, rec.id)?;
                journal.log_op(
                    session,
                    OP_ROLLBACK_DNS,
                    &target,
                    "not_found",
                    Some("the adapter no longer exists"),
                )?;
            }
            Err(e) => record_failure(journal, session, OP_ROLLBACK_DNS, target, &e, report)?,
        }
    }
    if written {
        flush_dns_cache(journal, session, sys);
    }
    Ok(())
}

/// Flushes the resolver cache so restored servers are used at once. Best effort: neither a
/// failed flush nor a failed log write fails the rollback.
fn flush_dns_cache(journal: &Journal, session: Option<i64>, sys: &dyn RollbackSystem) {
    let (outcome, detail) = match sys.flush_dns() {
        Ok(()) => ("flushed", None),
        Err(e) => {
            warn!(error = %e, "cannot flush the DNS resolver cache after restoring DNS servers");
            ("failed", Some(e.to_string()))
        }
    };
    if let Err(e) = journal.log_op(
        session,
        OP_FLUSH_DNS,
        FLUSH_TARGET,
        outcome,
        detail.as_deref(),
    ) {
        warn!(error = %e, "cannot log the DNS resolver cache flush");
    }
}

// ───────────────────────────── Appx ─────────────────────────────

/// Restores the selected packages with [`appx::reregister_all`], which looks up the
/// packages whose files are gone in one PowerShell call.
fn revert_appx(
    journal: &Journal,
    session: Option<i64>,
    records: &[AppxRecord],
    report: &mut RollbackReport,
) -> Result<()> {
    let package = |rec: &AppxRecord| format!("Appx package {}", rec.package_full_name);
    let first_action = report.actions.len();
    report.actions.extend(
        records
            .iter()
            .map(|rec| format!("re-register {}", package(rec))),
    );
    if report.dry_run || records.is_empty() {
        return Ok(());
    }
    let outcomes = appx::reregister_all(records);
    for (index, (rec, outcome)) in records.iter().zip(outcomes).enumerate() {
        let target = package(rec);
        match outcome {
            Ok(RestoreOutcome::Reregistered) => {
                report.appx_restored += 1;
                journal.mark_reverted(JournalTable::Appx, rec.id)?;
                journal.log_op(session, "rollback_appx", &target, "restored", None)?;
            }
            Ok(RestoreOutcome::AlreadyInstalled { package_full_name }) => {
                info!(target = %target, installed = %package_full_name, "package is installed again; nothing to restore");
                if let Some(action) = report.actions.get_mut(first_action + index) {
                    *action = format!(
                        "already reinstalled as {package_full_name}, nothing to re-register: {target}"
                    );
                }
                report.appx_restored += 1;
                journal.mark_reverted(JournalTable::Appx, rec.id)?;
                journal.log_op(
                    session,
                    "rollback_appx",
                    &target,
                    "already_installed",
                    Some(&package_full_name),
                )?;
            }
            Ok(RestoreOutcome::StoreRequired {
                package_family,
                store_link,
            }) => {
                warn!(target = %target, store_link = %store_link, "package files are gone; Store reinstall required");
                journal.log_op(
                    session,
                    "rollback_appx",
                    &target,
                    "store_required",
                    Some(&store_link),
                )?;
                report.appx_store_required.push(StoreReinstall {
                    package_family,
                    store_link,
                });
            }
            Err(e) => record_failure(journal, session, "rollback_appx", target, &e, report)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::*;
    use crate::debloat::scheduled_tasks::fake::FakeTasks;
    use crate::network::IpFamily;
    use crate::safety::state_log::{
        NewDnsRecord, NewRegistryRecord, NewScheduledTaskRecord, NewTaskDefinitionRecord,
    };
    use crate::win::registry::{delete_sandbox_tree, read_value, RegValue};

    const TASK: &str = r"\PCOptimizerSelfTest\Rollback";
    const OTHER_TASK: &str = r"\PCOptimizerSelfTest\RollbackOther";
    const GUID: &str = "{00000000-0000-0000-0000-00000000c0de}";
    const OTHER_GUID: &str = "{00000000-0000-0000-0000-00000000beef}";
    const ADAPTER: &str = "Cairn self-test";

    /// Name prefix of the registry keys the live-setting tests restore: one key per test,
    /// each a direct child of the SelfTest key, so removing a test's key leaves nothing
    /// behind. [`FakeSystem`] maps the mouse value names of those keys to
    /// [`LiveSetting::Mouse`].
    const LIVE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\RollbackLive";

    /// Whether `key_path` is a live-setting test key: [`LIVE_SANDBOX`] followed by a name
    /// suffix without a further path component, ignoring ASCII case.
    fn is_live_sandbox(key_path: &str) -> bool {
        key_path
            .get(..LIVE_SANDBOX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LIVE_SANDBOX))
            && !key_path[LIVE_SANDBOX.len()..].contains('\\')
    }

    /// Scripted system: tasks in memory, DNS outcomes and drift readings popped in order,
    /// task definitions deleted in memory, live refreshes recorded.
    struct FakeSystem {
        elevated: bool,
        tasks: FakeTasks,
        /// `task_store` fails with this message.
        connect_error: Option<String>,
        /// Answers of `dns_drift`; `Ok(None)` once empty.
        drift: RefCell<VecDeque<Result<Option<String>>>>,
        /// Answers of `restore_dns`; a call with none left panics.
        dns: RefCell<VecDeque<Result<DnsRestore>>>,
        /// `flush_dns` fails with this message.
        flush_error: Option<String>,
        flushes: Cell<usize>,
        store_requests: Cell<usize>,
        /// `task_definitions` fails with this message.
        definitions_error: Option<String>,
        definition_requests: Cell<usize>,
        /// `delete` fails with this message.
        delete_error: Option<String>,
        /// Task paths `delete` reports as not existing.
        missing_tasks: Vec<String>,
        /// Answer of `delete_folder_if_empty`.
        folder_result: std::result::Result<FolderRemoval, String>,
        deleted: RefCell<Vec<String>>,
        folders: RefCell<Vec<String>>,
        /// `refresh_live` fails with this message.
        live_error: Option<String>,
        live_calls: RefCell<Vec<LiveSetting>>,
    }

    impl FakeSystem {
        fn new() -> FakeSystem {
            FakeSystem {
                elevated: true,
                tasks: FakeTasks::default(),
                connect_error: None,
                drift: RefCell::new(VecDeque::new()),
                dns: RefCell::new(VecDeque::new()),
                flush_error: None,
                flushes: Cell::new(0),
                store_requests: Cell::new(0),
                definitions_error: None,
                definition_requests: Cell::new(0),
                delete_error: None,
                missing_tasks: Vec::new(),
                folder_result: Ok(FolderRemoval::Removed),
                deleted: RefCell::new(Vec::new()),
                folders: RefCell::new(Vec::new()),
                live_error: None,
                live_calls: RefCell::new(Vec::new()),
            }
        }

        fn with_dns(outcomes: Vec<Result<DnsRestore>>) -> FakeSystem {
            let sys = FakeSystem::new();
            sys.dns.borrow_mut().extend(outcomes);
            sys
        }
    }

    impl RollbackSystem for FakeSystem {
        fn is_elevated(&self) -> bool {
            self.elevated
        }

        fn task_store(&self) -> Result<Box<dyn TaskStore + '_>> {
            self.store_requests.set(self.store_requests.get() + 1);
            match &self.connect_error {
                Some(message) => Err(Error::Other(message.clone())),
                None => Ok(Box::new(&self.tasks)),
            }
        }

        fn task_definitions(&self) -> Result<Box<dyn TaskDefinitionRemover + '_>> {
            self.definition_requests
                .set(self.definition_requests.get() + 1);
            match &self.definitions_error {
                Some(message) => Err(Error::Other(message.clone())),
                None => Ok(Box::new(self)),
            }
        }

        fn refresh_live(&self, setting: LiveSetting) -> Result<()> {
            self.live_calls.borrow_mut().push(setting);
            match &self.live_error {
                Some(message) => Err(Error::Other(message.clone())),
                None => Ok(()),
            }
        }

        fn live_setting(
            &self,
            hive: Hive,
            key_path: &str,
            value_name: &str,
        ) -> Option<LiveSetting> {
            if hive == Hive::CurrentUser && is_live_sandbox(key_path) {
                return live::setting_for(hive, catalog::MOUSE_KEY, value_name);
            }
            live::setting_for(hive, key_path, value_name)
        }

        fn dns_drift(&self, _rec: &DnsRecord) -> Result<Option<String>> {
            self.drift.borrow_mut().pop_front().unwrap_or(Ok(None))
        }

        fn restore_dns(&self, rec: &DnsRecord) -> Result<DnsRestore> {
            self.dns
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected DNS restore of {}", rec.target()))
        }

        fn flush_dns(&self) -> Result<()> {
            self.flushes.set(self.flushes.get() + 1);
            match &self.flush_error {
                Some(message) => Err(Error::Other(message.clone())),
                None => Ok(()),
            }
        }
    }

    impl TaskDefinitionRemover for &FakeSystem {
        fn delete(&self, path: &str) -> Result<bool> {
            if let Some(message) = &self.delete_error {
                return Err(Error::Other(message.clone()));
            }
            if self
                .missing_tasks
                .iter()
                .any(|p| p.eq_ignore_ascii_case(path))
            {
                return Ok(false);
            }
            self.deleted.borrow_mut().push(path.to_string());
            Ok(true)
        }

        fn delete_folder_if_empty(&self, folder: &str) -> Result<FolderRemoval> {
            self.folders.borrow_mut().push(folder.to_string());
            self.folder_result.clone().map_err(Error::Other)
        }
    }

    fn journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        (dir, journal)
    }

    fn record_task(journal: &Journal, path: &str, was_enabled: bool) {
        let session = journal.begin_session("task-unit", "test").unwrap();
        assert!(journal
            .record_scheduled_task(
                session,
                &NewScheduledTaskRecord {
                    path: path.to_string(),
                    was_enabled,
                },
            )
            .unwrap());
        journal.end_session(session).unwrap();
    }

    fn record_dns(journal: &Journal, guid: &str, family: IpFamily, previous: &str) {
        let session = journal.begin_session("dns-unit", "test").unwrap();
        assert!(journal
            .record_dns(
                session,
                &NewDnsRecord {
                    interface_guid: guid.to_string(),
                    family,
                    adapter_name: ADAPTER.to_string(),
                    previous_servers: previous.to_string(),
                    target_servers: "1.1.1.1,1.0.0.1".to_string(),
                },
            )
            .unwrap());
        journal.end_session(session).unwrap();
    }

    fn sessions(journal: &Journal) -> i64 {
        journal.summary().unwrap().sessions
    }

    /// `(op, target, outcome, detail)` of every audit row, oldest first.
    fn ops(journal: &Journal) -> Vec<(String, String, String, Option<String>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .map(|o| (o.op, o.target, o.outcome, o.detail))
            .collect();
        rows.reverse();
        rows
    }

    fn row(
        op: &str,
        target: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> (String, String, String, Option<String>) {
        (
            op.to_string(),
            target.to_string(),
            outcome.to_string(),
            detail.map(str::to_string),
        )
    }

    fn task_target(path: &str) -> String {
        format!("scheduled task {path}")
    }

    fn dns_target(family: IpFamily) -> String {
        format!("{} DNS servers of {ADAPTER}", family.label())
    }

    fn task_record(path: &str) -> ScheduledTaskRecord {
        ScheduledTaskRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            path: path.to_string(),
            was_enabled: true,
            active: true,
            reverted_at: None,
        }
    }

    fn dns_record(guid: &str, family: IpFamily) -> DnsRecord {
        DnsRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            interface_guid: guid.to_string(),
            family,
            adapter_name: ADAPTER.to_string(),
            previous_servers: String::new(),
            target_servers: "1.1.1.1".to_string(),
            active: true,
            reverted_at: None,
        }
    }

    // ── scheduled tasks ──

    #[test]
    fn restores_the_recorded_flag_and_marks_the_record_reverted() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        record_task(&journal, OTHER_TASK, false);
        let mut sys = FakeSystem::new();
        sys.tasks = FakeTasks::with(&[(TASK, false, false), (OTHER_TASK, true, true)]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.scheduled_tasks_restored, 2);
        assert_eq!(report.total_reverted(), 2);
        assert_eq!(
            report.actions,
            vec![
                format!("disable {}", task_target(OTHER_TASK)),
                format!("enable {}", task_target(TASK)),
            ],
            "newest record first"
        );
        assert_eq!(report.restart, RestartNeed::None);
        assert_eq!(sys.tasks.get(TASK).map(|t| t.enabled), Some(true));
        assert_eq!(sys.tasks.get(OTHER_TASK).map(|t| t.enabled), Some(false));
        assert_eq!(sys.tasks.writes.get(), 2);
        assert_eq!(
            sys.store_requests.get(),
            1,
            "one connection for both records"
        );
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
        let rows = ops(&journal);
        assert_eq!(
            rows,
            vec![
                row(
                    "rollback_scheduled_task",
                    &task_target(OTHER_TASK),
                    "restored",
                    Some("disabled")
                ),
                row(
                    "rollback_scheduled_task",
                    &task_target(TASK),
                    "restored",
                    Some("enabled")
                ),
            ]
        );
    }

    #[test]
    fn already_restored_task_needs_no_write() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        let mut sys = FakeSystem::new();
        sys.tasks = FakeTasks::with(&[(TASK, true, false)]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.scheduled_tasks_restored, 1);
        assert_eq!(sys.tasks.writes.get(), 0);
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_scheduled_task",
                &task_target(TASK),
                "restored",
                Some("already enabled")
            )]
        );
    }

    #[test]
    fn missing_task_leaves_the_journal() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        let sys = FakeSystem::new();

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.scheduled_tasks_restored, 1);
        assert_eq!(
            report.actions,
            vec![format!(
                "{} no longer exists; nothing to restore",
                task_target(TASK)
            )]
        );
        assert_eq!(sys.tasks.writes.get(), 0);
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_scheduled_task",
                &task_target(TASK),
                "not_found",
                Some("the task no longer exists")
            )]
        );
    }

    #[test]
    fn failed_restore_keeps_the_record_active() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        let mut sys = FakeSystem::new();
        sys.tasks = FakeTasks::with(&[(TASK, false, false)]);
        sys.tasks.fail_writes = Some("Access is denied.".to_string());

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.scheduled_tasks_restored, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].target, task_target(TASK));
        assert_eq!(report.failures[0].error, "Access is denied.");
        assert_eq!(
            report.actions,
            vec![format!("enable {}", task_target(TASK))]
        );
        assert_eq!(journal.active_scheduled_tasks().unwrap().len(), 1);
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_scheduled_task",
                &task_target(TASK),
                "failed",
                Some("Access is denied.")
            )]
        );
    }

    #[test]
    fn connection_failure_fails_each_record_and_continues() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        record_task(&journal, OTHER_TASK, true);
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        let mut sys = FakeSystem::with_dns(vec![Ok(DnsRestore::Written)]);
        sys.connect_error = Some("cannot connect to Task Scheduler: unavailable".to_string());

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert_eq!(sys.store_requests.get(), 1);
        assert_eq!(report.scheduled_tasks_restored, 0);
        let failed: Vec<(&str, &str)> = report
            .failures
            .iter()
            .map(|f| (f.target.as_str(), f.error.as_str()))
            .collect();
        let error = "cannot connect to Task Scheduler: unavailable";
        assert_eq!(
            failed,
            vec![
                (task_target(OTHER_TASK).as_str(), error),
                (task_target(TASK).as_str(), error),
            ]
        );
        assert_eq!(report.actions.len(), 3, "{:?}", report.actions);
        assert_eq!(report.dns_restored, 1, "the DNS group still ran");
        assert_eq!(sys.flushes.get(), 1);
        assert_eq!(journal.active_scheduled_tasks().unwrap().len(), 2);
        assert!(journal.active_dns().unwrap().is_empty());
    }

    #[test]
    fn dry_run_never_requests_the_task_store() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        record_task(&journal, OTHER_TASK, false);
        let before = sessions(&journal);
        let mut sys = FakeSystem::new();
        sys.tasks = FakeTasks::with(&[(TASK, false, false), (OTHER_TASK, true, false)]);

        for filter in [
            None,
            Some(RollbackFilter {
                scheduled_tasks: vec![TASK.to_string(), OTHER_TASK.to_string()],
                ..Default::default()
            }),
        ] {
            let plan = rollback_with(&journal, filter.as_ref(), true, &sys).unwrap();
            assert!(plan.dry_run);
            assert_eq!(
                plan.actions,
                vec![
                    format!("disable {}", task_target(OTHER_TASK)),
                    format!("enable {}", task_target(TASK)),
                ]
            );
            assert_eq!(plan.total_reverted(), 0);
            assert!(plan.is_clean());
        }
        assert_eq!(sys.store_requests.get(), 0);
        assert_eq!(sys.tasks.writes.get(), 0);
        assert_eq!(sessions(&journal), before);
        assert_eq!(journal.active_scheduled_tasks().unwrap().len(), 2);
        assert!(journal.ops(10).unwrap().is_empty());
    }

    // ── filters and elevation ──

    #[test]
    fn filter_selects_tasks_and_dns_ignoring_case_and_braces() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        record_task(&journal, OTHER_TASK, true);
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        record_dns(&journal, GUID, IpFamily::Ipv6, "2001:db8::1");
        record_dns(&journal, OTHER_GUID, IpFamily::Ipv4, "9.9.9.9");

        let filter = RollbackFilter {
            scheduled_tasks: vec![TASK.to_uppercase()],
            dns: vec!["00000000-0000-0000-0000-00000000C0DE".to_string()],
            ..Default::default()
        };
        assert!(!filter.is_empty());
        assert!(filter.selects_scheduled_task(&task_record(&TASK.to_lowercase())));
        assert!(!filter.selects_scheduled_task(&task_record(OTHER_TASK)));
        assert!(filter.selects_dns(&dns_record(GUID, IpFamily::Ipv4)));
        assert!(filter.selects_dns(&dns_record(&GUID.to_uppercase(), IpFamily::Ipv6)));
        assert!(!filter.selects_dns(&dns_record(OTHER_GUID, IpFamily::Ipv4)));

        let sys = FakeSystem::new();
        let plan = rollback_with(&journal, Some(&filter), true, &sys).unwrap();
        assert_eq!(
            plan.actions,
            vec![
                format!("enable {}", task_target(TASK)),
                format!("restore {} to 2001:db8::1", dns_target(IpFamily::Ipv6)),
                format!("restore {} to automatic", dns_target(IpFamily::Ipv4)),
            ]
        );

        for only in [
            RollbackFilter {
                scheduled_tasks: vec![TASK.to_string()],
                ..Default::default()
            },
            RollbackFilter {
                dns: vec![GUID.to_string()],
                ..Default::default()
            },
        ] {
            assert!(!only.is_empty());
        }
        let parsed: RollbackFilter =
            serde_json::from_str(r#"{"scheduled_tasks": ["\\A\\B"], "dns": ["{C0DE}"]}"#).unwrap();
        assert_eq!(parsed.scheduled_tasks, vec![r"\A\B".to_string()]);
        assert_eq!(parsed.dns, vec!["{C0DE}".to_string()]);
        let old: RollbackFilter = serde_json::from_str(r#"{"power": true}"#).unwrap();
        assert!(old.scheduled_tasks.is_empty() && old.dns.is_empty() && !old.is_empty());
    }

    #[test]
    fn selection_needs_elevation_for_tasks_and_dns() {
        assert!(!Selection::default().needs_elevation());
        let tasks = Selection {
            scheduled_tasks: vec![task_record(TASK)],
            ..Selection::default()
        };
        assert!(tasks.needs_elevation());
        let dns = Selection {
            dns: vec![dns_record(GUID, IpFamily::Ipv6)],
            ..Selection::default()
        };
        assert!(dns.needs_elevation());

        // A plain per-user value needs none, so the new kinds are what decides.
        let (_dir, journal) = journal();
        let session = journal.begin_session("registry-unit", "test").unwrap();
        assert!(journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: Hive::CurrentUser,
                    key_path: r"Software\PCOptimizer\SelfTest\RollbackUnit".to_string(),
                    value_name: "Probe".to_string(),
                    key_existed: true,
                    value_existed: false,
                    original: None,
                    created_root: None,
                },
            )
            .unwrap());
        let registry = Selection::load(&journal, None).unwrap();
        assert!(!registry.needs_elevation());
        let with_task = Selection {
            scheduled_tasks: vec![task_record(TASK)],
            ..registry
        };
        assert!(with_task.needs_elevation());
    }

    #[test]
    fn unelevated_system_refuses_before_a_session() {
        let (_dir, journal) = journal();
        record_task(&journal, TASK, true);
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        let before = sessions(&journal);
        let mut sys = FakeSystem::new();
        sys.elevated = false;
        sys.tasks = FakeTasks::with(&[(TASK, false, false)]);

        let filters = [
            None,
            Some(RollbackFilter {
                scheduled_tasks: vec![TASK.to_string()],
                ..Default::default()
            }),
            Some(RollbackFilter {
                dns: vec![GUID.to_string()],
                ..Default::default()
            }),
        ];
        for filter in &filters {
            let err = rollback_with(&journal, filter.as_ref(), false, &sys).unwrap_err();
            assert!(matches!(err, Error::NotElevated), "{err}");
        }
        assert_eq!(sessions(&journal), before, "no session before the check");
        assert_eq!(sys.store_requests.get(), 0);
        assert_eq!(sys.tasks.writes.get(), 0);
        assert_eq!(journal.summary().unwrap().scheduled_tasks_active, 1);
        assert_eq!(journal.summary().unwrap().dns_active, 1);

        // Planning needs no elevation.
        let plan = rollback_with(&journal, None, true, &sys).unwrap();
        assert_eq!(plan.actions.len(), 2, "{:?}", plan.actions);
    }

    // ── DNS servers ──

    #[test]
    fn revert_dns_written_marks_reverted_logs_and_flushes_once() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        record_dns(&journal, GUID, IpFamily::Ipv6, "2001:db8::1");
        let sys = FakeSystem::with_dns(vec![Ok(DnsRestore::Written), Ok(DnsRestore::Written)]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.dns_restored, 2);
        assert_eq!(report.total_reverted(), 2);
        assert_eq!(report.restart, RestartNeed::None);
        assert_eq!(
            report.actions,
            vec![
                format!("restore {} to 2001:db8::1", dns_target(IpFamily::Ipv6)),
                format!("restore {} to automatic", dns_target(IpFamily::Ipv4)),
            ]
        );
        assert_eq!(sys.flushes.get(), 1);
        assert!(journal.active_dns().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![
                row(
                    "rollback_dns",
                    &dns_target(IpFamily::Ipv6),
                    "restored",
                    Some("written")
                ),
                row(
                    "rollback_dns",
                    &dns_target(IpFamily::Ipv4),
                    "restored",
                    Some("written")
                ),
                row("flush_dns_cache", "DNS resolver cache", "flushed", None),
            ]
        );
        let session = journal.summary().unwrap().last_session.unwrap();
        assert_eq!(session.label, "rollback_to_baseline");
        assert!(journal
            .ops(10)
            .unwrap()
            .iter()
            .all(|o| o.session_id == Some(session.id)));
    }

    #[test]
    fn already_set_dns_does_not_flush() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "9.9.9.9");
        let sys = FakeSystem::with_dns(vec![Ok(DnsRestore::AlreadyInState)]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.dns_restored, 1);
        assert_eq!(
            report.actions,
            vec![format!("restore {} to 9.9.9.9", dns_target(IpFamily::Ipv4))]
        );
        assert_eq!(sys.flushes.get(), 0);
        assert!(journal.active_dns().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_dns",
                &dns_target(IpFamily::Ipv4),
                "restored",
                Some("already set")
            )]
        );
    }

    #[test]
    fn not_found_dns_is_marked_reverted_without_flush() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        let sys = FakeSystem::with_dns(vec![Ok(DnsRestore::NotFound)]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.dns_restored, 1);
        assert_eq!(
            report.actions,
            vec![format!(
                "{}: the adapter no longer exists; nothing to restore",
                dns_target(IpFamily::Ipv4)
            )]
        );
        assert_eq!(sys.flushes.get(), 0);
        assert!(journal.active_dns().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_dns",
                &dns_target(IpFamily::Ipv4),
                "not_found",
                Some("the adapter no longer exists")
            )]
        );
    }

    #[test]
    fn revert_dns_failure_keeps_record_active() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        record_dns(&journal, OTHER_GUID, IpFamily::Ipv4, "");
        let error = "network adapter is disabled or disconnected";
        // The newer record (OTHER_GUID) comes first and fails; the older one is written.
        let sys = FakeSystem::with_dns(vec![
            Err(Error::Other(error.to_string())),
            Ok(DnsRestore::Written),
        ]);

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.dns_restored, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].target, dns_target(IpFamily::Ipv4));
        assert_eq!(report.failures[0].error, error);
        let active = journal.active_dns().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].interface_guid, OTHER_GUID);
        assert_eq!(sys.flushes.get(), 1, "the written record still flushes");
        let rows = ops(&journal);
        assert_eq!(
            rows[0],
            row(
                "rollback_dns",
                &dns_target(IpFamily::Ipv4),
                "failed",
                Some(error)
            )
        );

        // Nothing written: no flush.
        let (_dir, journal) = self::journal();
        record_dns(&journal, GUID, IpFamily::Ipv6, "");
        let sys = FakeSystem::with_dns(vec![Err(Error::Other(error.to_string()))]);
        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(sys.flushes.get(), 0);
        assert_eq!(journal.active_dns().unwrap().len(), 1);
    }

    #[test]
    fn dns_dry_run_reads_drift_only() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        record_dns(&journal, GUID, IpFamily::Ipv6, "2001:db8::1");
        let before = sessions(&journal);
        let sys = FakeSystem::new();
        // IPv6 (newest) was changed elsewhere; IPv4's drift cannot be read.
        sys.drift.borrow_mut().extend([
            Ok(Some("2001:db8::99".to_string())),
            Err(Error::Other("cannot read".to_string())),
        ]);

        let plan = rollback_with(&journal, None, true, &sys).unwrap();
        assert_eq!(
            plan.actions,
            vec![
                format!(
                    "restore {} to 2001:db8::1 (currently 2001:db8::99, changed outside Cairn)",
                    dns_target(IpFamily::Ipv6)
                ),
                format!("restore {} to automatic", dns_target(IpFamily::Ipv4)),
            ]
        );
        assert_eq!(plan.dns_restored, 0);
        assert!(plan.is_clean());
        assert_eq!(sys.flushes.get(), 0);
        assert!(
            sys.drift.borrow().is_empty(),
            "each record read its drift once"
        );
        assert_eq!(sessions(&journal), before);
        assert_eq!(journal.active_dns().unwrap().len(), 2);
        assert!(journal.ops(10).unwrap().is_empty());
    }

    /// The DNS part of the system is an in-memory network stack.
    struct StackSystem<'a>(&'a crate::network::tests::FakeStack);

    impl RollbackSystem for StackSystem<'_> {
        fn is_elevated(&self) -> bool {
            true
        }

        fn task_store(&self) -> Result<Box<dyn TaskStore + '_>> {
            Err(Error::Other("no scheduled tasks here".to_string()))
        }

        fn task_definitions(&self) -> Result<Box<dyn TaskDefinitionRemover + '_>> {
            Err(Error::Other("no scheduled tasks here".to_string()))
        }

        fn refresh_live(&self, _setting: LiveSetting) -> Result<()> {
            Ok(())
        }

        fn dns_drift(&self, rec: &DnsRecord) -> Result<Option<String>> {
            crate::network::dns::drift_with(self.0, rec)
        }

        fn restore_dns(&self, rec: &DnsRecord) -> Result<DnsRestore> {
            crate::network::dns::restore_with(self.0, rec)
        }

        fn flush_dns(&self) -> Result<()> {
            crate::network::NetStack::flush_resolver_cache(self.0)
        }
    }

    #[test]
    fn a_second_dns_change_by_cairn_is_not_reported_as_outside() {
        use crate::network::adapters::AdapterKind;
        use crate::network::dns::{run, DnsRequest, Mode};
        use crate::network::tests::{adapter, guid, journal as shared_journal, FakeStack};
        use crate::safety::test_safety;

        let (_dir, journal) = shared_journal();
        let stack = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        for preset in ["cloudflare", "google"] {
            let safety = test_safety(std::sync::Arc::clone(&journal), "dns", false);
            run(
                &stack,
                Mode::Apply(&safety),
                &guid(1),
                &DnsRequest::preset(preset).unwrap(),
            )
            .unwrap();
        }
        let sys = StackSystem(&stack);
        let filter = RollbackFilter {
            dns: vec![guid(1)],
            ..Default::default()
        };
        for plan in [
            rollback_with(&journal, Some(&filter), true, &sys).unwrap(),
            rollback_with(&journal, None, true, &sys).unwrap(),
        ] {
            assert_eq!(
                plan.actions,
                vec![
                    "restore IPv6 DNS servers of Wi-Fi to automatic".to_string(),
                    "restore IPv4 DNS servers of Wi-Fi to automatic".to_string(),
                ]
            );
        }

        // A change made elsewhere afterwards is still named.
        stack.set_static(&guid(1), IpFamily::Ipv4, "9.9.9.9");
        let plan = rollback_with(&journal, Some(&filter), true, &sys).unwrap();
        assert_eq!(
            plan.actions[1],
            "restore IPv4 DNS servers of Wi-Fi to automatic (currently 9.9.9.9, changed outside \
             Cairn)"
        );

        let report = rollback_with(&journal, Some(&filter), false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.dns_restored, 2);
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv4), "");
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv6), "");
    }

    #[test]
    fn flush_failure_is_logged_and_does_not_fail_the_rollback() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        let mut sys = FakeSystem::with_dns(vec![Ok(DnsRestore::Written)]);
        sys.flush_error = Some("the DNS Client service did not flush its cache".to_string());

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.dns_restored, 1);
        assert_eq!(sys.flushes.get(), 1);
        assert_eq!(
            ops(&journal).last().cloned(),
            Some(row(
                "flush_dns_cache",
                "DNS resolver cache",
                "failed",
                Some("the DNS Client service did not flush its cache")
            ))
        );
    }

    #[test]
    fn report_without_the_new_counters_still_deserializes() {
        let old: RollbackReport = serde_json::from_str(
            r#"{"dry_run":false,"registry_restored":1,"registry_deleted":0,"services_restored":0,
                "services_started":0,"appx_restored":0,"appx_store_required":[],"power_restored":0,
                "actions":[],"failures":[]}"#,
        )
        .unwrap();
        assert_eq!(old.scheduled_tasks_restored, 0);
        assert_eq!(old.dns_restored, 0);
        assert_eq!(old.task_definitions_deleted, 0);
        assert_eq!(old.total_reverted(), 1);
        let json = serde_json::to_value(RollbackReport::default()).unwrap();
        assert_eq!(json["scheduled_tasks_restored"], 0);
        assert_eq!(json["dns_restored"], 0);
        assert_eq!(json["task_definitions_deleted"], 0);
        let counted = RollbackReport {
            task_definitions_deleted: 2,
            ..Default::default()
        };
        assert_eq!(counted.total_reverted(), 2);
    }

    // ── task definitions ──

    const DEFINITION: &str = r"\PCOptimizerSelfTest\RollbackDefinition";
    const OTHER_DEFINITION: &str = r"\PCOptimizerSelfTest\RollbackDefinitionOther";

    fn record_definition(journal: &Journal, path: &str, folder_created: bool) {
        let session = journal.begin_session("definition-unit", "test").unwrap();
        assert!(journal
            .record_task_definition(
                session,
                &NewTaskDefinitionRecord {
                    path: path.to_string(),
                    purpose: "maintenance".to_string(),
                    folder_created,
                },
            )
            .unwrap());
        journal.end_session(session).unwrap();
    }

    fn definition_target(path: &str) -> String {
        format!("task {path}")
    }

    fn delete_action(path: &str) -> String {
        format!("delete the scheduled task {path} that Cairn created")
    }

    fn definition_record(path: &str) -> TaskDefinitionRecord {
        TaskDefinitionRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            path: path.to_string(),
            purpose: "maintenance".to_string(),
            folder_created: false,
            active: true,
            reverted_at: None,
        }
    }

    #[test]
    fn task_definitions_are_deleted_in_restore_order() {
        let (_dir, journal) = journal();
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        record_definition(&journal, DEFINITION, false);
        record_task(&journal, TASK, true);
        let mut sys = FakeSystem::with_dns(vec![Ok(DnsRestore::AlreadyInState)]);
        sys.tasks = FakeTasks::with(&[(TASK, false, false)]);

        let expected = vec![
            format!("enable {}", task_target(TASK)),
            delete_action(DEFINITION),
            format!("restore {} to automatic", dns_target(IpFamily::Ipv4)),
        ];
        let plan = rollback_with(&journal, None, true, &sys).unwrap();
        assert_eq!(plan.actions, expected, "tasks, then definitions, then DNS");

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.actions, expected);
        assert_eq!(report.task_definitions_deleted, 1);
        assert_eq!(report.total_reverted(), 3);
        assert_eq!(report.restart, RestartNeed::None);
        assert_eq!(*sys.deleted.borrow(), vec![DEFINITION.to_string()]);
        assert!(
            sys.folders.borrow().is_empty(),
            "the record did not create the folder"
        );
        assert_eq!(sys.definition_requests.get(), 1);
        assert!(journal.active_task_definitions().unwrap().is_empty());
        let rows = ops(&journal);
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            vec![
                "rollback_scheduled_task",
                "rollback_task_definition",
                "rollback_dns"
            ]
        );
        assert_eq!(
            rows[1],
            row(
                "rollback_task_definition",
                &definition_target(DEFINITION),
                "deleted",
                Some("folder kept")
            )
        );
        let summary = journal.summary().unwrap();
        assert_eq!(summary.task_definitions_active, 0);
        assert_eq!(summary.task_definitions_total, 1);
    }

    #[test]
    fn missing_task_definition_is_not_found() {
        let (_dir, journal) = journal();
        record_definition(&journal, DEFINITION, false);
        let mut sys = FakeSystem::new();
        sys.missing_tasks = vec![DEFINITION.to_uppercase()];

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.task_definitions_deleted, 1);
        assert_eq!(report.actions, vec![delete_action(DEFINITION)]);
        assert!(sys.deleted.borrow().is_empty());
        assert!(journal.active_task_definitions().unwrap().is_empty());
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_task_definition",
                &definition_target(DEFINITION),
                "not_found",
                Some("folder kept")
            )]
        );
    }

    #[test]
    fn folder_removed_only_when_created_and_empty() {
        let cases: [(std::result::Result<FolderRemoval, String>, &str); 4] = [
            (Ok(FolderRemoval::Removed), "folder removed"),
            (Ok(FolderRemoval::NotEmpty), "folder kept: not empty"),
            (Ok(FolderRemoval::Missing), "folder already removed"),
            (
                Err("Access is denied.".to_string()),
                "folder kept: Access is denied.",
            ),
        ];
        for (answer, detail) in cases {
            let (_dir, journal) = journal();
            record_definition(&journal, OTHER_DEFINITION, false);
            record_definition(&journal, DEFINITION, true);
            let mut sys = FakeSystem::new();
            sys.folder_result = answer;

            let report = rollback_with(&journal, None, false, &sys).unwrap();
            assert!(report.is_clean(), "{:?}", report.failures);
            assert_eq!(report.task_definitions_deleted, 2);
            assert_eq!(
                *sys.folders.borrow(),
                vec![r"\PCOptimizerSelfTest".to_string()],
                "only the record that created the folder removes it"
            );
            assert_eq!(
                ops(&journal),
                vec![
                    row(
                        "rollback_task_definition",
                        &definition_target(DEFINITION),
                        "deleted",
                        Some(detail)
                    ),
                    row(
                        "rollback_task_definition",
                        &definition_target(OTHER_DEFINITION),
                        "deleted",
                        Some("folder kept")
                    ),
                ]
            );
            assert!(journal.active_task_definitions().unwrap().is_empty());
        }
    }

    #[test]
    fn task_definition_delete_failure_keeps_the_record() {
        let (_dir, journal) = journal();
        record_definition(&journal, DEFINITION, true);
        let mut sys = FakeSystem::new();
        sys.delete_error = Some("Access is denied.".to_string());

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.task_definitions_deleted, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].target, definition_target(DEFINITION));
        assert_eq!(report.failures[0].error, "Access is denied.");
        assert!(
            sys.folders.borrow().is_empty(),
            "no folder removal after a failure"
        );
        assert_eq!(journal.active_task_definitions().unwrap().len(), 1);
        assert_eq!(
            ops(&journal),
            vec![row(
                "rollback_task_definition",
                &definition_target(DEFINITION),
                "failed",
                Some("Access is denied.")
            )]
        );

        // Task Scheduler cannot be reached: every record fails, one request.
        let (_dir, journal) = self::journal();
        record_definition(&journal, DEFINITION, false);
        record_definition(&journal, OTHER_DEFINITION, false);
        record_dns(&journal, GUID, IpFamily::Ipv4, "");
        let mut sys = FakeSystem::with_dns(vec![Ok(DnsRestore::AlreadyInState)]);
        sys.definitions_error = Some("cannot connect to Task Scheduler: unavailable".to_string());
        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert_eq!(sys.definition_requests.get(), 1);
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.dns_restored, 1, "the DNS group still ran");
        assert_eq!(journal.active_task_definitions().unwrap().len(), 2);
    }

    #[test]
    fn dry_run_never_connects_for_task_definitions() {
        let (_dir, journal) = journal();
        record_definition(&journal, DEFINITION, true);
        record_definition(&journal, OTHER_DEFINITION, false);
        let before = sessions(&journal);
        let sys = FakeSystem::new();

        for filter in [
            None,
            Some(RollbackFilter {
                task_definitions: vec![DEFINITION.to_lowercase(), OTHER_DEFINITION.to_string()],
                ..Default::default()
            }),
        ] {
            let plan = rollback_with(&journal, filter.as_ref(), true, &sys).unwrap();
            assert!(plan.dry_run);
            assert_eq!(
                plan.actions,
                vec![delete_action(OTHER_DEFINITION), delete_action(DEFINITION)]
            );
            assert_eq!(plan.total_reverted(), 0);
            assert_eq!(plan.restart, RestartNeed::None);
        }
        assert_eq!(sys.definition_requests.get(), 0);
        assert!(sys.deleted.borrow().is_empty());
        assert!(sys.folders.borrow().is_empty());
        assert_eq!(sessions(&journal), before);
        assert_eq!(journal.active_task_definitions().unwrap().len(), 2);
        assert!(journal.ops(10).unwrap().is_empty());
    }

    #[test]
    fn selection_needs_elevation_for_task_definitions() {
        let definitions = Selection {
            task_definitions: vec![definition_record(DEFINITION)],
            ..Selection::default()
        };
        assert!(!definitions.is_empty());
        assert!(definitions.needs_elevation());

        let filter = RollbackFilter {
            task_definitions: vec![DEFINITION.to_uppercase()],
            ..Default::default()
        };
        assert!(!filter.is_empty());
        assert!(filter.selects_task_definition(DEFINITION));
        assert!(!filter.selects_task_definition(OTHER_DEFINITION));
        let parsed: RollbackFilter =
            serde_json::from_str(r#"{"task_definitions": ["\\Cairn\\Maintenance-S-1-5-18"]}"#)
                .unwrap();
        assert_eq!(
            parsed.task_definitions,
            vec![r"\Cairn\Maintenance-S-1-5-18"]
        );
        assert!(!parsed.is_empty());

        let (_dir, journal) = journal();
        record_definition(&journal, DEFINITION, false);
        let before = sessions(&journal);
        let mut sys = FakeSystem::new();
        sys.elevated = false;
        for filter in [None, Some(filter)] {
            let err = rollback_with(&journal, filter.as_ref(), false, &sys).unwrap_err();
            assert!(matches!(err, Error::NotElevated), "{err}");
        }
        assert_eq!(sessions(&journal), before, "no session before the check");
        assert_eq!(sys.definition_requests.get(), 0);
        assert_eq!(journal.active_task_definitions().unwrap().len(), 1);
    }

    // ── live settings ──

    /// Removes a live-setting test's sandbox key, even when the test panics.
    struct SandboxKey(String);

    impl SandboxKey {
        /// `<LIVE_SANDBOX><name>`, for example `...\SelfTest\RollbackLiveOnce`.
        fn name_for(name: &str) -> String {
            format!("{LIVE_SANDBOX}{name}")
        }

        fn new(name: &str) -> SandboxKey {
            let key = SandboxKey(SandboxKey::name_for(name));
            delete_sandbox_tree(&key.0).unwrap();
            key
        }
    }

    impl Drop for SandboxKey {
        fn drop(&mut self) {
            let _ = delete_sandbox_tree(&self.0);
        }
    }

    /// Writes `current` to `name` under `key` and journals `original` as its baseline.
    fn record_live_value(journal: &Journal, key: &str, name: &str, original: &str, current: &str) {
        let (handle, _) = Key::create(Hive::CurrentUser, key).unwrap();
        handle
            .set(name, &RegValue::Sz(current.to_string()))
            .unwrap();
        let session = journal.begin_session("live-unit", "test").unwrap();
        assert!(journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: Hive::CurrentUser,
                    key_path: key.to_string(),
                    value_name: name.to_string(),
                    key_existed: true,
                    value_existed: true,
                    original: Some(RegValue::Sz(original.to_string()).to_raw()),
                    created_root: None,
                },
            )
            .unwrap());
        journal.end_session(session).unwrap();
    }

    #[test]
    fn live_sandbox_keys_are_direct_children_of_the_self_test_key() {
        assert!(is_live_sandbox(LIVE_SANDBOX));
        assert!(is_live_sandbox(&SandboxKey::name_for("Once")));
        assert!(is_live_sandbox(
            r"software\pcoptimizer\selftest\ROLLBACKLIVEOnce"
        ));
        assert!(!is_live_sandbox(
            r"Software\PCOptimizer\SelfTest\RollbackLive\Once"
        ));
        assert!(!is_live_sandbox(r"Software\PCOptimizer\SelfTest\Rollback"));
        assert!(!is_live_sandbox(r"Control Panel\Mouse"));
        let sys = FakeSystem::new();
        assert_eq!(
            sys.live_setting(
                Hive::CurrentUser,
                &SandboxKey::name_for("Once"),
                "MouseSpeed"
            ),
            Some(LiveSetting::Mouse)
        );
        assert_eq!(
            sys.live_setting(
                Hive::LocalMachine,
                &SandboxKey::name_for("Once"),
                "MouseSpeed"
            ),
            None
        );
    }

    fn live_rows(journal: &Journal) -> Vec<(String, String, String, Option<String>)> {
        ops(journal)
            .into_iter()
            .filter(|r| r.0 == live::OP_REFRESH)
            .collect()
    }

    #[test]
    fn restored_mouse_values_refresh_the_session_once() {
        let key = SandboxKey::new("Once");
        let (_dir, journal) = journal();
        record_live_value(&journal, &key.0, "MouseSpeed", "1", "0");
        record_live_value(&journal, &key.0, "MouseThreshold1", "6", "0");
        let sys = FakeSystem::new();

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.is_clean(), "{:?}", report.failures);
        assert_eq!(report.registry_restored, 2);
        assert_eq!(*sys.live_calls.borrow(), vec![LiveSetting::Mouse]);
        assert_eq!(report.restart, RestartNeed::None);
        assert_eq!(
            read_value(Hive::CurrentUser, &key.0, "MouseSpeed").unwrap(),
            Some(RegValue::Sz("1".to_string()))
        );
        assert_eq!(
            read_value(Hive::CurrentUser, &key.0, "MouseThreshold1").unwrap(),
            Some(RegValue::Sz("6".to_string()))
        );
        let rows = ops(&journal);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(
            rows[2],
            row(
                "refresh_live_setting",
                LiveSetting::Mouse.target(),
                "applied",
                None
            ),
            "the refresh follows the registry group"
        );
        assert!(journal.active_registry().unwrap().is_empty());
    }

    #[test]
    fn failed_live_refresh_asks_for_sign_out() {
        let key = SandboxKey::new("Failed");
        let (_dir, journal) = journal();
        record_live_value(&journal, &key.0, "MouseSpeed", "1", "0");
        let mut sys = FakeSystem::new();
        sys.live_error = Some("this process does not run in a signed-in desktop session".into());

        let report = rollback_with(&journal, None, false, &sys).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.registry_restored, 1);
        assert_eq!(report.restart, RestartNeed::SignOut);
        assert!(
            journal.active_registry().unwrap().is_empty(),
            "the record is reverted"
        );
        assert_eq!(
            live_rows(&journal),
            vec![row(
                "refresh_live_setting",
                LiveSetting::Mouse.target(),
                "failed",
                Some("this process does not run in a signed-in desktop session")
            )]
        );
    }

    #[test]
    fn dry_run_never_refreshes_live_settings() {
        let key = SandboxKey::new("DryRun");
        let (_dir, journal) = journal();
        record_live_value(&journal, &key.0, "MouseSpeed", "1", "0");
        let sys = FakeSystem::new();

        let plan = rollback_with(&journal, None, true, &sys).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert!(sys.live_calls.borrow().is_empty());
        assert_eq!(
            read_value(Hive::CurrentUser, &key.0, "MouseSpeed").unwrap(),
            Some(RegValue::Sz("0".to_string())),
            "nothing restored"
        );
        assert!(journal.ops(10).unwrap().is_empty());
    }
}
