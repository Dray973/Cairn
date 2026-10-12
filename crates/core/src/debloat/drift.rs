//! Targets that no longer hold what Cairn set (drift), and Keep as is: Cairn stops tracking
//! a changed target without writing anything on the PC.
//!
//! A scan reads every recorded catalog target once and lists the ones that no longer hold on
//! the tweak's row ([`item_drift`], [`drift_note`]). [`Engine::drift`] checks every active
//! journal record: catalog targets as the scan reads them, other registry values against what
//! Cairn last wrote, and DNS servers against the servers Cairn set. Cairn never sets a changed
//! target again on its own: Apply sets it again, Undo restores its original and Keep as is
//! ([`Engine::forget`]) stops tracking it.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::catalog::{self, Action, Former, RegistryAction, Tweak};
use super::engine::{action_label, registry_key, ActiveRecords, Engine};
use super::power;
use super::scheduled_tasks::TaskConnection;
use super::verify::{self, ReadBack};
use super::{ActionState, ActionStatus};
use crate::network::{same_servers, servers_text};
use crate::safety::rollback::{RegistryTarget, RollbackFailure, RollbackFilter};
use crate::safety::state_log::{DnsRecord, Journal, JournalTable, RegistryRecord, Written};
use crate::safety::verify::{
    dns_label, enabled_text, fill, holds_written, raw_text, read_raw, registry_label, same_setting,
    scheme_text, unverified, written_text, Mismatch,
};
use crate::win::registry::{Hive, RawValue};
use crate::Result;

/// The kind of journal record a drift entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    Registry,
    Service,
    ScheduledTask,
    Power,
    Dns,
}

/// One active journal record whose target no longer holds what Cairn set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftEntry {
    /// The catalog tweak that owns the target. `None` for startup entries, Windows Update
    /// settings, DNS servers and records of earlier builds.
    pub item: Option<String>,
    pub kind: DriftKind,
    pub mismatch: Mismatch,
    /// Selects exactly this record, for Undo (`revert_targets`) and Keep as is
    /// (`forget_targets`).
    pub filter: RollbackFilter,
    pub recorded_at: String,
}

/// Outcome of a drift check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriftReport {
    pub entries: Vec<DriftEntry>,
    /// Active records read.
    pub checked: usize,
    /// Active records not read. These are values outside the catalog whose written value
    /// is unknown (recorded before 0.3), and values Windows changes on its own (`unverified`).
    pub unchecked: usize,
    /// Of `unchecked`, the registry records that carry no written value because an earlier
    /// version of Cairn recorded them without storing what it set. The UI names this reason.
    #[serde(default)]
    pub unchecked_no_value: usize,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

/// Outcome of Keep as is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForgetReport {
    /// Targets Cairn no longer tracks, with the value they keep.
    pub forgotten: Vec<Mismatch>,
    /// Targets refused: they still hold what Cairn set, Cairn does not know what it wrote,
    /// or they could not be read.
    pub refused: Vec<RollbackFailure>,
}

/// Note of a tweak with one recorded target back at its value from before Cairn's change:
/// `{label}`, `{found}`.
pub(crate) const NOTE_CHANGED_BACK: &str =
    "Changed back since Cairn applied it: {label} is {found} again, its value before Cairn's \
     change.";

/// Note of a tweak with one recorded target that holds another value: `{label}`, `{found}`,
/// `{expected}`.
pub(crate) const NOTE_CHANGED: &str =
    "Changed since Cairn applied it: {label} is {found}, not {expected}.";

/// Note of a tweak with two recorded targets that no longer hold: `{first}`, `{second}`.
pub(crate) const NOTE_CHANGED_TWO: &str =
    "Changed since Cairn applied it: 2 settings no longer hold ({first} and {second}).";

/// Note of a tweak with three or more recorded targets that no longer hold, naming the first
/// two: `{count}`, `{first}`, `{second}`, `{more}`.
pub(crate) const NOTE_CHANGED_MORE: &str = "Changed since Cairn applied it: {count} settings no \
     longer hold ({first}, {second} and {more} more).";

/// Second sentence of the note of one target.
pub(crate) const NOTE_APPLY_ONE: &str = "Apply sets it again.";

/// Second sentence of the note of several targets.
pub(crate) const NOTE_APPLY_SEVERAL: &str = "Apply sets them again.";

/// Keep as is refused for a target that still holds Cairn's value: `{target}`.
pub(crate) const FORGET_STILL_HOLDS: &str =
    "{target} still holds the value Cairn set; Undo restores the original.";

/// Keep as is refused for a target whose written value is unknown: `{target}`.
pub(crate) const FORGET_WRITTEN_UNKNOWN: &str =
    "Cairn does not know what it wrote to {target}; use Undo.";

/// Keep as is refused for a Store package or a scheduled task Cairn registered.
pub(crate) const FORGET_SETTINGS_ONLY: &str = "Keep as is applies to settings only; use Undo.";

/// Keep as is refused for a target that could not be read: `{target}`, `{error}`.
pub(crate) const FORGET_UNREADABLE: &str =
    "{target} could not be read ({error}), so Cairn still tracks it.";

/// Keep as is refused for DNS servers of an adapter that is not listed: `{target}`. A disabled
/// adapter is not listed either, and Undo still restores its servers.
pub(crate) const FORGET_ADAPTER_NOT_LISTED: &str = "{target} cannot be read while the adapter is \
     not listed (removed or disabled), so Cairn still tracks it; use Undo.";

/// Keep as is refused for a record that ended meanwhile: `{target}`.
pub(crate) const FORGET_NOT_TRACKED: &str = "Cairn no longer tracks {target}.";

/// Drift check warning of a target that could not be read: `{target}`, `{error}`.
pub(crate) const DRIFT_UNREADABLE: &str = "{target} could not be read ({error}).";

/// Audit log operation of Keep as is.
pub(crate) const OP_FORGET: &str = "forget_record";

/// Mismatches of a scanned tweak's recorded targets, from the scan's single read of each
/// action (`reads` pairs an action index with its read-back). Targets without an active
/// record are not drift, and neither is a target at its original whose recorded write never
/// happened ([`ActiveRecords::never_changed`]). A retired tweak's row shows only why it is
/// retired, so it lists none; History still does.
pub(crate) fn item_drift(
    t: &Tweak,
    records: &ActiveRecords,
    reads: &[(usize, ReadBack)],
) -> Vec<Mismatch> {
    if t.retired.is_some() {
        return Vec::new();
    }
    reads
        .iter()
        .filter(|(_, read)| !read.held)
        .filter_map(|(index, read)| {
            let action = t.actions.get(*index)?;
            if records.never_changed(action, read) {
                return None;
            }
            let original = verify::recorded_original(action, records)?;
            Some(verify::action_mismatch(action, read, Some(original)))
        })
        .collect()
}

/// The scan note for a tweak whose recorded targets no longer hold; `None` without drift.
/// One target is named with its value; for several, the first two labels are named.
pub(crate) fn drift_note(drift: &[Mismatch]) -> Option<String> {
    let sentence = match drift {
        [] => return None,
        [one] if one.back_to_original => fill(
            NOTE_CHANGED_BACK,
            &[("label", &one.label), ("found", &one.found)],
        ),
        [one] => fill(
            NOTE_CHANGED,
            &[
                ("label", &one.label),
                ("found", &one.found),
                ("expected", &one.expected),
            ],
        ),
        [first, second] => fill(
            NOTE_CHANGED_TWO,
            &[("first", &first.label), ("second", &second.label)],
        ),
        [first, second, rest @ ..] => {
            let (count, more) = (drift.len().to_string(), rest.len().to_string());
            fill(
                NOTE_CHANGED_MORE,
                &[
                    ("count", &count),
                    ("first", &first.label),
                    ("second", &second.label),
                    ("more", &more),
                ],
            )
        }
    };
    let apply = if drift.len() == 1 {
        NOTE_APPLY_ONE
    } else {
        NOTE_APPLY_SEVERAL
    };
    Some(format!("{sentence} {apply}"))
}

/// The note a scan shows for a tweak whose recorded targets changed since Cairn set them:
/// the drift note of `drift`, the mismatches [`item_drift`] found in the scan's read of the
/// tweak's actions.
pub(crate) fn scan_note(
    _t: &Tweak,
    _actions: &[ActionStatus],
    drift: &[Mismatch],
) -> Option<String> {
    drift_note(drift)
}

/// The catalog actions and former values of a set of tweaks, by the journal target they
/// write: lowercase registry keys, service names and task paths.
struct Owners {
    registry: HashMap<(Hive, String, String), (&'static Action, &'static RegistryAction)>,
    formers: HashMap<(Hive, String, String), &'static Former>,
    services: HashMap<String, &'static Action>,
    tasks: HashMap<String, &'static Action>,
    power: Option<&'static Action>,
}

impl Owners {
    fn of(tweaks: &[&'static Tweak]) -> Owners {
        let mut owners = Owners {
            registry: HashMap::new(),
            formers: HashMap::new(),
            services: HashMap::new(),
            tasks: HashMap::new(),
            power: None,
        };
        for t in tweaks {
            for action in t.actions {
                match action {
                    Action::Registry(r) => {
                        owners
                            .registry
                            .insert(registry_key(r.hive, r.path, r.name), (action, r));
                    }
                    Action::Service(s) => {
                        owners.services.insert(s.name.to_ascii_lowercase(), action);
                    }
                    Action::ScheduledTask(s) => {
                        owners.tasks.insert(s.path.to_ascii_lowercase(), action);
                    }
                    Action::Power(_) => owners.power = Some(action),
                }
            }
            for former in t.former {
                let r = &former.action;
                owners
                    .formers
                    .insert(registry_key(r.hive, r.path, r.name), former);
            }
        }
        owners
    }
}

/// Where a catalog action's active record is: its kind, the filter that selects exactly
/// it, when it was recorded and the target's value before Cairn's first change.
struct Recorded {
    kind: DriftKind,
    filter: RollbackFilter,
    recorded_at: String,
    original: String,
}

/// The active record of `action`'s target, `None` when there is none.
fn recorded(action: &Action, records: &ActiveRecords) -> Option<Recorded> {
    let original = verify::recorded_original(action, records)?;
    let (kind, filter, recorded_at) = match action {
        Action::Registry(r) => {
            let rec = records
                .registry
                .get(&registry_key(r.hive, r.path, r.name))?;
            (
                DriftKind::Registry,
                registry_filter(rec),
                rec.recorded_at.clone(),
            )
        }
        Action::Service(s) => {
            let rec = records.services.get(&s.name.to_ascii_lowercase())?;
            let filter = RollbackFilter {
                services: vec![rec.name.clone()],
                ..Default::default()
            };
            (DriftKind::Service, filter, rec.recorded_at.clone())
        }
        Action::ScheduledTask(s) => {
            let rec = records.scheduled_tasks.get(&s.path.to_ascii_lowercase())?;
            let filter = RollbackFilter {
                scheduled_tasks: vec![rec.path.clone()],
                ..Default::default()
            };
            (DriftKind::ScheduledTask, filter, rec.recorded_at.clone())
        }
        Action::Power(_) => {
            let rec = records.power.first()?;
            let filter = RollbackFilter {
                power: true,
                ..Default::default()
            };
            (DriftKind::Power, filter, rec.recorded_at.clone())
        }
    };
    Some(Recorded {
        kind,
        filter,
        recorded_at,
        original,
    })
}

/// The filter that selects exactly `rec`.
fn registry_filter(rec: &RegistryRecord) -> RollbackFilter {
    RollbackFilter {
        registry: vec![RegistryTarget {
            hive: rec.hive,
            key_path: rec.key_path.clone(),
            value_name: rec.value_name.clone(),
        }],
        ..Default::default()
    }
}

/// The mismatch of a registry record outside the catalog whose target (`current`) no longer
/// holds what Cairn last wrote.
fn written_mismatch(
    rec: &RegistryRecord,
    written: &Written,
    current: Option<&RawValue>,
) -> Mismatch {
    let (hive, key_path, value_name) = (rec.hive, rec.key_path.as_str(), rec.value_name.as_str());
    Mismatch {
        target: rec.target(),
        label: registry_label(value_name),
        expected: written_text(hive, key_path, value_name, written),
        found: raw_text(hive, key_path, value_name, current),
        original: Some(raw_text(hive, key_path, value_name, rec.original.as_ref())),
        back_to_original: same_setting(hive, key_path, value_name, rec.original.as_ref(), current),
    }
}

/// The mismatch of a DNS record whose adapter now has `current` (a server list in the
/// journal's form).
fn dns_mismatch(rec: &DnsRecord, current: &str) -> Mismatch {
    Mismatch {
        target: rec.target(),
        label: dns_label(rec),
        expected: servers_text(&rec.target_servers),
        found: servers_text(current),
        original: Some(rec.previous_text()),
        back_to_original: same_servers(current, &rec.previous_servers),
    }
}

/// What Keep as is does with one selected record.
enum Decision {
    /// Stop tracking the record; the target keeps the value in the mismatch.
    Forget(Mismatch),
    /// Keep tracking it, for this reason.
    Refuse(String),
}

impl Decision {
    fn still_holds(target: &str) -> Decision {
        Decision::Refuse(fill(FORGET_STILL_HOLDS, &[("target", target)]))
    }
}

/// Ends the record of `table` and `id` without restoring it (`mark_forgotten`) and logs
/// `forget_record` with no session, or adds the refusal to the report.
fn conclude(
    journal: &Journal,
    table: JournalTable,
    id: i64,
    target: String,
    decision: Decision,
    report: &mut ForgetReport,
) -> Result<()> {
    match decision {
        Decision::Refuse(error) => report.refused.push(RollbackFailure { target, error }),
        Decision::Forget(mismatch) => {
            if journal.mark_forgotten(table, id)? {
                let detail = format!(
                    "kept {}; original {}",
                    mismatch.found,
                    mismatch.original.as_deref().unwrap_or("unknown")
                );
                journal.log_op(None, OP_FORGET, &target, "forgotten", Some(&detail))?;
                report.forgotten.push(mismatch);
            } else {
                let error = fill(FORGET_NOT_TRACKED, &[("target", &target)]);
                report.refused.push(RollbackFailure { target, error });
            }
        }
    }
    Ok(())
}

impl Engine {
    /// Read-only; no session, no elevation. Lists every active journal record whose target no
    /// longer holds what Cairn set: catalog targets read as the scan reads them (retired
    /// tweaks included), other registry values compared with what Cairn last wrote (startup
    /// switches by their on or off state), and DNS servers. A catalog target whose recorded
    /// write never happened and that holds its original is not listed: it never changed.
    /// Store packages and scheduled tasks Cairn registered are not checked.
    pub fn drift(&self) -> Result<DriftReport> {
        let tweaks: Vec<&'static Tweak> = catalog::TWEAKS.iter().collect();
        self.check(&tweaks, true)
    }

    /// Read-only. The same check for the catalog tweaks `ids` only; other ids select
    /// nothing.
    pub fn verify(&self, ids: &[String]) -> Result<DriftReport> {
        let tweaks: Vec<&'static Tweak> = catalog::TWEAKS.iter().collect();
        self.verify_in(&tweaks, ids)
    }

    /// Keep as is: stops tracking the selected records whose targets changed. Changes
    /// nothing on the PC, opens no session and needs no elevation. Each selected record is
    /// read as [`Engine::drift`] reads it, and refused while its target still holds what
    /// Cairn set, when Cairn does not know what it wrote, or when it cannot be read (DNS
    /// servers of an adapter that is not listed included). A value an earlier version of
    /// Cairn set and this version no longer sets (`former`) may be kept whatever it holds.
    /// Store packages and scheduled tasks Cairn registered are refused.
    pub fn forget(&self, filter: &RollbackFilter) -> Result<ForgetReport> {
        let tweaks: Vec<&'static Tweak> = catalog::TWEAKS.iter().collect();
        self.forget_in(&tweaks, filter)
    }

    /// [`Engine::verify`] over `tweaks` instead of the catalog.
    fn verify_in(&self, tweaks: &[&'static Tweak], ids: &[String]) -> Result<DriftReport> {
        let selected: Vec<&'static Tweak> = tweaks
            .iter()
            .copied()
            .filter(|t| ids.iter().any(|id| id == t.id))
            .collect();
        self.check(&selected, false)
    }

    /// The drift check of `tweaks`' recorded targets and, with `everything`, of every other
    /// active registry and DNS record; other services, scheduled tasks and power schemes are
    /// counted as unchecked, since only the catalog says what was set on them.
    fn check(&self, tweaks: &[&'static Tweak], everything: bool) -> Result<DriftReport> {
        let started = Instant::now();
        let records = ActiveRecords::load(self.journal())?;
        let tasks = TaskConnection::new();
        let mut report = DriftReport::default();

        for t in tweaks {
            for action in t.actions {
                let Some(recorded) = recorded(action, &records) else {
                    continue;
                };
                match verify::action_read(action, &tasks) {
                    Ok((_, read)) => {
                        report.checked += 1;
                        // A record whose write never happened, with the target at its
                        // original, claims no change: Undo ends it without writing.
                        if !read.held && !records.never_changed(action, &read) {
                            report.entries.push(DriftEntry {
                                item: Some(t.id.to_string()),
                                kind: recorded.kind,
                                mismatch: verify::action_mismatch(
                                    action,
                                    &read,
                                    Some(recorded.original),
                                ),
                                filter: recorded.filter,
                                recorded_at: recorded.recorded_at,
                            });
                        }
                    }
                    Err(e) => {
                        report.unchecked += 1;
                        let (target, error) = (action_label(action), e.to_string());
                        report.warnings.push(fill(
                            DRIFT_UNREADABLE,
                            &[("target", &target), ("error", &error)],
                        ));
                    }
                }
            }
        }

        if everything {
            check_other_records(&records, &Owners::of(tweaks), &mut report);
            self.check_dns(&mut report)?;
        }
        report.duration_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// DNS records, compared with the servers Cairn set; a record whose adapter is no
    /// longer listed is not read.
    fn check_dns(&self, report: &mut DriftReport) -> Result<()> {
        for rec in self.journal().active_dns()? {
            match (self.system().dns_now)(&rec) {
                Ok(None) => report.unchecked += 1,
                Ok(Some(current)) => {
                    report.checked += 1;
                    if !same_servers(&current, &rec.target_servers) {
                        report.entries.push(DriftEntry {
                            item: None,
                            kind: DriftKind::Dns,
                            mismatch: dns_mismatch(&rec, &current),
                            filter: RollbackFilter {
                                dns: vec![rec.interface_guid.clone()],
                                ..Default::default()
                            },
                            recorded_at: rec.recorded_at.clone(),
                        });
                    }
                }
                Err(e) => {
                    report.unchecked += 1;
                    let (target, error) = (rec.target(), e.to_string());
                    report.warnings.push(fill(
                        DRIFT_UNREADABLE,
                        &[("target", &target), ("error", &error)],
                    ));
                }
            }
        }
        Ok(())
    }

    /// [`Engine::forget`] with `tweaks` standing for the catalog.
    fn forget_in(
        &self,
        tweaks: &[&'static Tweak],
        filter: &RollbackFilter,
    ) -> Result<ForgetReport> {
        let journal = self.journal();
        let owners = Owners::of(tweaks);
        let tasks = TaskConnection::new();
        let mut report = ForgetReport::default();
        let unreadable = |target: &str, e: crate::Error| {
            let error = e.to_string();
            Decision::Refuse(fill(
                FORGET_UNREADABLE,
                &[("target", target), ("error", &error)],
            ))
        };

        for rec in journal
            .active_registry()?
            .iter()
            .filter(|rec| filter.selects_registry(rec))
        {
            let target = rec.target();
            let decision = registry_decision(&owners, &tasks, rec, &target)
                .unwrap_or_else(|e| unreadable(&target, e));
            conclude(
                journal,
                JournalTable::Registry,
                rec.id,
                target,
                decision,
                &mut report,
            )?;
        }
        for rec in journal
            .active_services()?
            .iter()
            .filter(|rec| filter.selects_service(rec))
        {
            let target = format!("service {}", rec.name);
            let decision = match owners.services.get(&rec.name.to_ascii_lowercase()) {
                Some(action) => {
                    let original = rec.start_type.label().to_string();
                    action_decision(action, &tasks, &target, original)
                        .unwrap_or_else(|e| unreadable(&target, e))
                }
                None => written_unknown(&target),
            };
            conclude(
                journal,
                JournalTable::Service,
                rec.id,
                target,
                decision,
                &mut report,
            )?;
        }
        for rec in journal
            .active_scheduled_tasks()?
            .iter()
            .filter(|rec| filter.selects_scheduled_task(rec))
        {
            let target = rec.target();
            let decision = match owners.tasks.get(&rec.path.to_ascii_lowercase()) {
                Some(action) => {
                    let original = enabled_text(rec.was_enabled).to_string();
                    action_decision(action, &tasks, &target, original)
                        .unwrap_or_else(|e| unreadable(&target, e))
                }
                None => written_unknown(&target),
            };
            let table = JournalTable::ScheduledTask;
            conclude(journal, table, rec.id, target, decision, &mut report)?;
        }
        if filter.power {
            for rec in journal.active_power()? {
                let target = format!("power scheme {}", rec.previous_scheme);
                let decision = match owners.power {
                    Some(action) => {
                        let name = power::scheme_name(&rec.previous_scheme);
                        let original = scheme_text(&rec.previous_scheme, name.as_deref());
                        action_decision(action, &tasks, &target, original)
                            .unwrap_or_else(|e| unreadable(&target, e))
                    }
                    None => written_unknown(&target),
                };
                conclude(
                    journal,
                    JournalTable::Power,
                    rec.id,
                    target,
                    decision,
                    &mut report,
                )?;
            }
        }
        for rec in journal
            .active_dns()?
            .iter()
            .filter(|rec| filter.selects_dns(rec))
        {
            let target = rec.target();
            let decision = match (self.system().dns_now)(rec) {
                Ok(None) => {
                    Decision::Refuse(fill(FORGET_ADAPTER_NOT_LISTED, &[("target", &target)]))
                }
                Ok(Some(current)) if same_servers(&current, &rec.target_servers) => {
                    Decision::still_holds(&target)
                }
                Ok(Some(current)) => Decision::Forget(dns_mismatch(rec, &current)),
                Err(e) => unreadable(&target, e),
            };
            conclude(
                journal,
                JournalTable::Dns,
                rec.id,
                target,
                decision,
                &mut report,
            )?;
        }

        for rec in journal
            .active_appx()?
            .iter()
            .filter(|rec| filter.selects_appx(rec))
        {
            report.refused.push(RollbackFailure {
                target: format!("Appx package {}", rec.package_full_name),
                error: FORGET_SETTINGS_ONLY.to_string(),
            });
        }
        for rec in journal
            .active_task_definitions()?
            .iter()
            .filter(|rec| filter.selects_task_definition(&rec.path))
        {
            report.refused.push(RollbackFailure {
                target: rec.target(),
                error: FORGET_SETTINGS_ONLY.to_string(),
            });
        }
        Ok(report)
    }
}

/// Registry records outside the catalog (`owners`), compared with what Cairn last wrote,
/// newest first; services, scheduled tasks and power schemes outside the catalog are counted
/// as unchecked, since only the catalog says what was set on them.
fn check_other_records(records: &ActiveRecords, owners: &Owners, report: &mut DriftReport) {
    let mut others: Vec<&RegistryRecord> = records
        .registry
        .iter()
        .filter(|(key, _)| !owners.registry.contains_key(*key))
        .map(|(_, rec)| rec)
        .collect();
    others.sort_by_key(|rec| Reverse(rec.id));
    for rec in others {
        let (hive, key_path, value_name) = (rec.hive, &rec.key_path, &rec.value_name);
        let written = rec
            .written
            .as_ref()
            .filter(|_| !unverified(hive, key_path, value_name));
        let Some(written) = written else {
            report.unchecked += 1;
            // A record with no written value at all was made by an earlier version of Cairn,
            // which did not store what it set; a value Windows changes on its own had a
            // written value but is skipped (`unverified`), so it is not counted here.
            if rec.written.is_none() {
                report.unchecked_no_value += 1;
            }
            continue;
        };
        match read_raw(hive, key_path, value_name) {
            Ok(current) => {
                report.checked += 1;
                if !holds_written(hive, key_path, value_name, written, current.as_ref()) {
                    report.entries.push(DriftEntry {
                        item: None,
                        kind: DriftKind::Registry,
                        mismatch: written_mismatch(rec, written, current.as_ref()),
                        filter: registry_filter(rec),
                        recorded_at: rec.recorded_at.clone(),
                    });
                }
            }
            Err(e) => {
                report.unchecked += 1;
                let (target, error) = (rec.target(), e.to_string());
                report.warnings.push(fill(
                    DRIFT_UNREADABLE,
                    &[("target", &target), ("error", &error)],
                ));
            }
        }
    }
    report.unchecked += records
        .services
        .keys()
        .filter(|name| !owners.services.contains_key(*name))
        .count();
    report.unchecked += records
        .scheduled_tasks
        .keys()
        .filter(|path| !owners.tasks.contains_key(*path))
        .count();
    if owners.power.is_none() {
        report.unchecked += records.power.len();
    }
}

/// Keep as is refused because Cairn does not know what it wrote to `target`.
fn written_unknown(target: &str) -> Decision {
    Decision::Refuse(fill(FORGET_WRITTEN_UNKNOWN, &[("target", target)]))
}

/// Keep as is of a catalog action's record: refused while the target holds what the action
/// sets; a target that no longer exists may be kept.
fn action_decision(
    action: &Action,
    tasks: &TaskConnection,
    target: &str,
    original: String,
) -> Result<Decision> {
    let (status, read) = verify::action_read(action, tasks)?;
    let gone = status.state == ActionState::Unavailable;
    if read.held && !gone {
        return Ok(Decision::still_holds(target));
    }
    Ok(Decision::Forget(verify::action_mismatch(
        action,
        &read,
        Some(original),
    )))
}

/// Keep as is of a registry record: a catalog target by its action, a value an earlier
/// version set (`former`) whatever it holds, any other value by what Cairn last wrote.
fn registry_decision(
    owners: &Owners,
    tasks: &TaskConnection,
    rec: &RegistryRecord,
    target: &str,
) -> Result<Decision> {
    let (hive, key_path, value_name) = (rec.hive, rec.key_path.as_str(), rec.value_name.as_str());
    let key = registry_key(hive, key_path, value_name);
    if let Some((action, r)) = owners.registry.get(&key) {
        let original = verify::registry_value_text(r, rec.original_decoded().as_ref());
        return action_decision(action, tasks, target, original);
    }
    if let Some(former) = owners.formers.get(&key) {
        let r = &former.action;
        let current = read_raw(hive, key_path, value_name)?;
        let found = current.as_ref().map(RawValue::decode);
        return Ok(Decision::Forget(Mismatch {
            target: target.to_string(),
            label: registry_label(value_name),
            expected: verify::expected_text(r),
            found: verify::registry_value_text(r, found.as_ref()),
            original: Some(verify::registry_value_text(
                r,
                rec.original_decoded().as_ref(),
            )),
            back_to_original: same_setting(
                hive,
                key_path,
                value_name,
                rec.original.as_ref(),
                current.as_ref(),
            ),
        }));
    }
    let Some(written) = &rec.written else {
        return Ok(written_unknown(target));
    };
    let current = read_raw(hive, key_path, value_name)?;
    if holds_written(hive, key_path, value_name, written, current.as_ref()) {
        return Ok(Decision::still_holds(target));
    }
    Ok(Decision::Forget(written_mismatch(
        rec,
        written,
        current.as_ref(),
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::debloat::catalog::{
        reg_dword, Build, Category, Editions, Requirement, RestartNeed, Risk, ScheduledTaskAction,
        Topic, WindowsRange, ANY_WINDOWS, TWEAKS,
    };
    use crate::debloat::engine::{EngineSystem, RECORDED_UNMET_NOTE, REINSTALLED_NOTE};
    use crate::debloat::live::LiveSetting;
    use crate::debloat::requirements::{
        gpu_note, range_availability, Availability, EditionClass, GpuScheduling, Probes,
        Requirements, WindowsFacts, SYSTEM_PROBES,
    };
    use crate::network::IpFamily;
    use crate::safety::state_log::{
        NewAppxRecord, NewDnsRecord, NewPowerRecord, NewRegistryRecord, NewScheduledTaskRecord,
        NewServiceRecord, NewTaskDefinitionRecord,
    };
    use crate::safety::test_safety;
    use crate::win::registry::{delete_sandbox_tree, read_value, Key, RegValue};
    use crate::win::scm::StartType;
    use crate::Error;

    type ItemDrift = fn(&Tweak, &ActiveRecords, &[(usize, ReadBack)]) -> Vec<Mismatch>;
    type DriftNote = fn(&[Mismatch]) -> Option<String>;
    type ScanNote = fn(&Tweak, &[ActionStatus], &[Mismatch]) -> Option<String>;

    #[test]
    fn drift_functions_keep_their_signatures() {
        let _: ItemDrift = item_drift;
        let _: DriftNote = drift_note;
        let _: ScanNote = scan_note;
    }

    // ── test fixtures ──

    const HKCU: Hive = Hive::CurrentUser;
    const USER_APPROVED_RUN: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// A startup entry name no PC has; its StartupApproved value is only ever read.
    const ABSENT_ENTRY: &str = "PCOptimizerSelfTestDriftEntry";
    /// A packaged startup task key no PC has; it is only ever read.
    const ABSENT_TASK_KEY: &str = concat!(
        r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion",
        r"\AppModel\SystemAppData\PCOptimizer.SelfTest_0000000000000\DriftTask"
    );
    const ADAPTER: &str = "Cairn self-test";
    /// An adapter whose DNS servers changed since Cairn set them.
    const CHANGED_GUID: &str = "{00000000-0000-0000-0000-0000000d0001}";
    /// An adapter that still has the servers Cairn set.
    const HELD_GUID: &str = "{00000000-0000-0000-0000-0000000d0002}";
    /// An adapter that is no longer listed.
    const GONE_GUID: &str = "{00000000-0000-0000-0000-0000000d0003}";
    /// An adapter whose DNS settings cannot be read.
    const BROKEN_GUID: &str = "{00000000-0000-0000-0000-0000000d0004}";
    /// The servers Cairn set on every test adapter.
    const CAIRN_SERVERS: &str = "1.1.1.1,1.0.0.1";

    /// A tweak of sandbox values: the drift code reads tweaks only through their targets.
    const fn sandbox_tweak(
        id: &'static str,
        actions: &'static [Action],
        former: &'static [Former],
        retired: Option<&'static str>,
    ) -> Tweak {
        Tweak {
            id,
            category: Category::Privacy,
            topic: Topic::Activity,
            title: "Sandbox values",
            description: "Values under the self-test key.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: None,
            windows: ANY_WINDOWS,
            retired,
            undo_limit: None,
            actions,
            former,
        }
    }

    const fn value(path: &'static str, name: &'static str) -> Action {
        Action::Registry(reg_dword(Hive::CurrentUser, path, name, 1))
    }

    fn no_settle() {}

    fn no_refresh(setting: LiveSetting) -> Result<()> {
        panic!("{setting:?} was pushed to the session")
    }

    /// The DNS servers of the test adapters.
    fn fake_dns(rec: &DnsRecord) -> Result<Option<String>> {
        match rec.interface_guid.as_str() {
            CHANGED_GUID => Ok(Some("9.9.9.9".to_string())),
            HELD_GUID => Ok(Some("1.1.1.1 1.0.0.1".to_string())),
            GONE_GUID => Ok(None),
            _ => Err(Error::Other("Access is denied.".into())),
        }
    }

    fn engine(journal: &Arc<Journal>) -> Engine {
        Engine::with_system(
            journal.clone(),
            EngineSystem {
                refresh: no_refresh,
                settle: no_settle,
                dns_now: fake_dns,
                ..EngineSystem::LIVE
            },
        )
    }

    fn temp_journal(dir: &tempfile::TempDir) -> Arc<Journal> {
        Arc::new(Journal::open(dir.path().join("journal.db")).unwrap())
    }

    /// Removes a sandbox key when dropped, so a failing test leaves nothing behind.
    struct SandboxKey(&'static str);

    impl SandboxKey {
        fn new(path: &'static str) -> SandboxKey {
            delete_sandbox_tree(path).unwrap();
            SandboxKey(path)
        }
    }

    impl Drop for SandboxKey {
        fn drop(&mut self) {
            let _ = delete_sandbox_tree(self.0);
        }
    }

    /// Sets a sandbox value outside Cairn, as Windows or another program would.
    fn set(path: &str, name: &str, value: u32) {
        let (key, _) = Key::create(HKCU, path).unwrap();
        key.set(name, &RegValue::Dword(value)).unwrap();
    }

    fn current(path: &str, name: &str) -> Option<RegValue> {
        read_value(HKCU, path, name).unwrap()
    }

    /// Sets a sandbox value through Cairn's safety layer, which journals it first and keeps
    /// what it wrote.
    fn apply(journal: &Arc<Journal>, path: &str, name: &str, value: u32) {
        test_safety(journal.clone(), "drift unit", false)
            .set_registry_value(HKCU, path, name, &RegValue::Dword(value))
            .unwrap();
    }

    /// Journals a registry record and what Cairn wrote to it, without touching the PC.
    fn record_written(journal: &Journal, hive: Hive, path: &str, name: &str, written: &Written) {
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive,
                    key_path: path.to_string(),
                    value_name: name.to_string(),
                    key_existed: true,
                    value_existed: false,
                    original: None,
                    created_root: None,
                },
            )
            .unwrap();
        assert!(journal
            .set_registry_written(hive, path, name, written)
            .unwrap());
        journal.end_session(session).unwrap();
    }

    /// Journals a registry record of an earlier build, which kept nothing of what it wrote.
    fn record_earlier(journal: &Journal, path: &str, name: &str) {
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: HKCU,
                    key_path: path.to_string(),
                    value_name: name.to_string(),
                    key_existed: true,
                    value_existed: false,
                    original: None,
                    created_root: None,
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
    }

    fn record_dns(journal: &Journal, guid: &str) {
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_dns(
                session,
                &NewDnsRecord {
                    interface_guid: guid.to_string(),
                    family: IpFamily::Ipv4,
                    adapter_name: ADAPTER.to_string(),
                    previous_servers: String::new(),
                    target_servers: CAIRN_SERVERS.to_string(),
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
    }

    fn registry_filter_of(path: &str, name: &str) -> RollbackFilter {
        RollbackFilter {
            registry: vec![RegistryTarget {
                hive: HKCU,
                key_path: path.to_string(),
                value_name: name.to_string(),
            }],
            ..Default::default()
        }
    }

    fn target_of(path: &str, name: &str) -> String {
        format!(r"HKCU\{path}\{name}")
    }

    fn dword_text(value: u32) -> String {
        format!("0x{value:08x} ({value})")
    }

    fn mismatch(label: &str, found: &str, expected: &str, back: bool) -> Mismatch {
        Mismatch {
            target: format!(r"HKLM\SOFTWARE\Policies\Example\{label}"),
            label: label.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
            original: Some("not set".to_string()),
            back_to_original: back,
        }
    }

    /// `(target, outcome, detail, session)` of every row of `op`, oldest first.
    fn rows(journal: &Journal, op: &str) -> Vec<(String, String, Option<String>, Option<i64>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|row| row.op == op)
            .map(|row| (row.target, row.outcome, row.detail, row.session_id))
            .collect();
        rows.reverse();
        rows
    }

    // ── the scan's row ──

    #[test]
    fn note_texts_are_whole_sentences() {
        // Each note is one sentence template filled in one pass; no template is a fragment
        // that another sentence completes.
        for text in [
            NOTE_CHANGED_BACK,
            NOTE_CHANGED,
            NOTE_CHANGED_TWO,
            NOTE_CHANGED_MORE,
            NOTE_APPLY_ONE,
            NOTE_APPLY_SEVERAL,
        ] {
            assert!(
                text.starts_with(|c: char| c.is_ascii_uppercase()) && text.ends_with('.'),
                "{text:?} is not a whole sentence"
            );
        }
    }

    #[test]
    fn drift_note_texts() {
        assert_eq!(drift_note(&[]), None);
        let back = mismatch("AllowTelemetry", "not set", "0x00000000 (0)", true);
        assert_eq!(
            drift_note(&[back]).unwrap(),
            "Changed back since Cairn applied it: AllowTelemetry is not set again, its value \
             before Cairn's change. Apply sets it again."
        );
        let other = mismatch("AllowTelemetry", "0x00000003 (3)", "0x00000000 (0)", false);
        assert_eq!(
            drift_note(std::slice::from_ref(&other)).unwrap(),
            "Changed since Cairn applied it: AllowTelemetry is 0x00000003 (3), not \
             0x00000000 (0). Apply sets it again."
        );
        let several = |labels: &[&str]| {
            let drift: Vec<Mismatch> = labels
                .iter()
                .map(|label| mismatch(label, "enabled", "disabled", true))
                .collect();
            drift_note(&drift).unwrap()
        };
        assert_eq!(
            several(&["task Proxy", "task Consolidator"]),
            "Changed since Cairn applied it: 2 settings no longer hold (task Proxy and task \
             Consolidator). Apply sets them again."
        );
        assert_eq!(
            several(&["A", "B", "C"]),
            "Changed since Cairn applied it: 3 settings no longer hold (A, B and 1 more). Apply \
             sets them again."
        );
        assert_eq!(
            several(&["A", "B", "C", "D", "E"]),
            "Changed since Cairn applied it: 5 settings no longer hold (A, B and 3 more). Apply \
             sets them again."
        );

        // A found value that looks like a field of the note is shown as it is.
        let braces = mismatch("Value", "\"{expected}\"", "0x00000000 (0)", false);
        assert_eq!(
            drift_note(&[braces]).unwrap(),
            "Changed since Cairn applied it: Value is \"{expected}\", not 0x00000000 (0). Apply \
             sets it again."
        );

        // The scan's note is the drift note.
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();
        assert_eq!(scan_note(ceip, &[], &[]), None);
        assert_eq!(
            scan_note(ceip, &[], std::slice::from_ref(&other)),
            drift_note(&[other])
        );
    }

    const SCAN_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitScan";
    static SCAN_TWEAK: Tweak = sandbox_tweak(
        "selftest.drift_scan",
        &[value(SCAN_SANDBOX, "First"), value(SCAN_SANDBOX, "Second")],
        &[],
        None,
    );
    static RETIRED_SCAN_TWEAK: Tweak = sandbox_tweak(
        "selftest.drift_scan_retired",
        &[value(SCAN_SANDBOX, "First"), value(SCAN_SANDBOX, "Second")],
        &[],
        Some("Cairn found no effect of this policy on Windows 11."),
    );

    /// The read-back of every action of `t`, as the scan reads them.
    fn scan_reads(t: &Tweak) -> Vec<(usize, ReadBack)> {
        let tasks = TaskConnection::new();
        t.actions
            .iter()
            .enumerate()
            .map(|(index, action)| (index, verify::action_read(action, &tasks).unwrap().1))
            .collect()
    }

    #[test]
    fn scan_reports_drift_for_recorded_targets_only() {
        let _cleanup = SandboxKey::new(SCAN_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        // First was 0 before Cairn set it; Second is not recorded at all.
        set(SCAN_SANDBOX, "First", 0);
        apply(&journal, SCAN_SANDBOX, "First", 1);
        set(SCAN_SANDBOX, "First", 7);
        set(SCAN_SANDBOX, "Second", 7);
        let records = ActiveRecords::load(&journal).unwrap();

        let reads = scan_reads(&SCAN_TWEAK);
        assert!(reads.iter().all(|(_, read)| !read.held));
        let drift = item_drift(&SCAN_TWEAK, &records, &reads);
        assert_eq!(
            drift,
            vec![Mismatch {
                target: target_of(SCAN_SANDBOX, "First"),
                label: "First".to_string(),
                expected: dword_text(1),
                found: dword_text(7),
                original: Some(dword_text(0)),
                back_to_original: false,
            }],
            "the unrecorded target is not drift"
        );

        set(SCAN_SANDBOX, "First", 0);
        let drift = item_drift(&SCAN_TWEAK, &records, &scan_reads(&SCAN_TWEAK));
        assert_eq!(drift.len(), 1);
        assert!(drift[0].back_to_original, "{drift:?}");
        assert_eq!(drift[0].found, dword_text(0));

        set(SCAN_SANDBOX, "First", 1);
        assert!(item_drift(&SCAN_TWEAK, &records, &scan_reads(&SCAN_TWEAK)).is_empty());

        // A retired tweak's row shows why it is retired, not its drift.
        set(SCAN_SANDBOX, "First", 7);
        let reads = scan_reads(&RETIRED_SCAN_TWEAK);
        assert!(item_drift(&RETIRED_SCAN_TWEAK, &records, &reads).is_empty());
        assert_eq!(item_drift(&SCAN_TWEAK, &records, &reads).len(), 1);
    }

    // ── History's check ──

    const LIST_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitList";
    const LIST_OTHER: &str = r"Software\PCOptimizer\SelfTest\DriftUnitListOther";
    static LIST_TWEAK: Tweak = sandbox_tweak(
        "selftest.drift_list",
        &[value(LIST_SANDBOX, "First"), value(LIST_SANDBOX, "Second")],
        &[],
        None,
    );

    fn turned_off() -> Written {
        Written::Value {
            value: RegValue::Binary(vec![0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]).to_raw(),
        }
    }

    #[test]
    fn drift_lists_catalog_registry_startup_and_dns_records() {
        let _list = SandboxKey::new(LIST_SANDBOX);
        let _other = SandboxKey::new(LIST_OTHER);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        apply(&journal, LIST_SANDBOX, "First", 1);
        apply(&journal, LIST_SANDBOX, "Second", 1);
        apply(&journal, LIST_OTHER, "Setting", 1);
        set(LIST_SANDBOX, "First", 2);
        set(LIST_OTHER, "Setting", 3);
        // A startup entry Cairn turned off whose StartupApproved value is gone.
        record_written(
            &journal,
            HKCU,
            USER_APPROVED_RUN,
            ABSENT_ENTRY,
            &turned_off(),
        );
        for guid in [CHANGED_GUID, HELD_GUID, GONE_GUID] {
            record_dns(&journal, guid);
        }
        let sessions = journal.summary().unwrap().sessions;

        let report = engine(&journal).check(&[&LIST_TWEAK], true).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let entries: Vec<(Option<&str>, DriftKind, &str, &str, &str)> = report
            .entries
            .iter()
            .map(|e| {
                (
                    e.item.as_deref(),
                    e.kind,
                    e.mismatch.label.as_str(),
                    e.mismatch.found.as_str(),
                    e.mismatch.expected.as_str(),
                )
            })
            .collect();
        let (one, two, three) = (dword_text(1), dword_text(2), dword_text(3));
        assert_eq!(
            entries,
            vec![
                (
                    Some("selftest.drift_list"),
                    DriftKind::Registry,
                    "First",
                    two.as_str(),
                    one.as_str()
                ),
                (None, DriftKind::Registry, ABSENT_ENTRY, "not set", "off"),
                (
                    None,
                    DriftKind::Registry,
                    "Setting",
                    three.as_str(),
                    one.as_str()
                ),
                (
                    None,
                    DriftKind::Dns,
                    "DNS servers of Cairn self-test (IPv4)",
                    "9.9.9.9",
                    "1.1.1.1, 1.0.0.1"
                ),
            ],
            "catalog targets first, then other values newest first, then DNS servers"
        );
        assert_eq!(
            (report.checked, report.unchecked),
            (6, 1),
            "the adapter that is gone is not read"
        );

        let first = &report.entries[0];
        assert_eq!(first.mismatch.target, target_of(LIST_SANDBOX, "First"));
        assert_eq!(first.mismatch.original.as_deref(), Some("not set"));
        assert!(!first.mismatch.back_to_original);
        assert_eq!(
            first.filter.registry,
            registry_filter_of(LIST_SANDBOX, "First").registry
        );
        assert!(first.filter.services.is_empty() && first.filter.dns.is_empty());
        assert!(!first.recorded_at.is_empty());
        let setting = &report.entries[2];
        assert_eq!(setting.mismatch.target, target_of(LIST_OTHER, "Setting"));
        assert_eq!(setting.filter.registry[0].value_name, "Setting");
        let dns = &report.entries[3];
        assert_eq!(dns.mismatch.target, "IPv4 DNS servers of Cairn self-test");
        assert_eq!(dns.mismatch.original.as_deref(), Some("automatic"));
        assert_eq!(dns.filter.dns, vec![CHANGED_GUID.to_string()]);

        // Reading changes nothing.
        assert_eq!(journal.summary().unwrap().sessions, sessions);
        assert_eq!(current(LIST_SANDBOX, "First"), Some(RegValue::Dword(2)));
        assert_eq!(journal.active_registry().unwrap().len(), 4);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["entries"][3]["kind"], "dns");
        assert_eq!(json["entries"][0]["item"], "selftest.drift_list");
    }

    const SKIP_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitSkip";

    #[test]
    fn drift_skips_records_without_written_and_unverified_targets() {
        let _cleanup = SandboxKey::new(SKIP_SANDBOX);
        set(SKIP_SANDBOX, "Earlier", 5);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier(&journal, SKIP_SANDBOX, "Earlier");
        // Values Windows changes on its own; their records are never read.
        record_written(
            &journal,
            Hive::LocalMachine,
            r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings",
            "PauseUpdatesExpiryTime",
            &Written::Deleted,
        );
        record_written(
            &journal,
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam",
            "LastSetTime",
            &Written::Deleted,
        );
        // A service, a scheduled task and a power scheme no tweak of this check sets.
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_service(
                session,
                &NewServiceRecord {
                    name: "PCOptimizerNoSuchService".to_string(),
                    display_name: "Cairn self-test".to_string(),
                    start_type: StartType::Manual,
                    delayed_auto_start: false,
                    was_running: false,
                },
            )
            .unwrap();
        journal
            .record_scheduled_task(
                session,
                &NewScheduledTaskRecord {
                    path: r"\PCOptimizerSelfTest\DriftUnit".to_string(),
                    was_enabled: true,
                },
            )
            .unwrap();
        journal
            .record_power(
                session,
                &NewPowerRecord {
                    previous_scheme: "381b4222-f694-41f0-9685-ff5bb260df2e".to_string(),
                    target_scheme: "00000000-0000-0000-0000-00000000c0de".to_string(),
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();

        let report = engine(&journal).check(&[], true).unwrap();
        assert!(report.entries.is_empty(), "{:?}", report.entries);
        assert_eq!((report.checked, report.unchecked), (0, 6));
        // Of the six, only the one registry record with no written value was recorded by an
        // earlier version without storing what it set; the Windows-changed value, the
        // consent-store time, the service, the task and the power scheme are not counted.
        assert_eq!(report.unchecked_no_value, 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    const NEVER_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitNeverWritten";
    static NEVER_TWEAK: Tweak = sandbox_tweak(
        "selftest.drift_never_written",
        &[
            value(NEVER_SANDBOX, "Failed"),
            value(NEVER_SANDBOX, "Undone"),
        ],
        &[],
        None,
    );

    #[test]
    fn a_record_whose_write_never_happened_is_not_drift() {
        let _cleanup = SandboxKey::new(NEVER_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        // An earlier version recorded Failed and then failed to write it, logging nothing, so
        // it is still absent, its original. Undone was set by Cairn and has been removed
        // outside Cairn since, back to its original.
        record_earlier(&journal, NEVER_SANDBOX, "Failed");
        apply(&journal, NEVER_SANDBOX, "Undone", 1);
        Key::open(HKCU, NEVER_SANDBOX, true)
            .unwrap()
            .unwrap()
            .delete_value("Undone")
            .unwrap();
        let [failed, undone] = NEVER_TWEAK.actions else {
            unreachable!("two actions")
        };

        let report = engine(&journal).check(&[&NEVER_TWEAK], false).unwrap();
        let labels: Vec<&str> = report
            .entries
            .iter()
            .map(|e| e.mismatch.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec!["Undone"],
            "only the change Cairn made is listed"
        );
        assert!(report.entries[0].mismatch.back_to_original);
        assert_eq!(report.checked, 2);

        let records = ActiveRecords::load(&journal).unwrap();
        let reads = scan_reads(&NEVER_TWEAK);
        assert!(records.never_changed(failed, &reads[0].1));
        assert!(!records.never_changed(undone, &reads[1].1));
        let drift = item_drift(&NEVER_TWEAK, &records, &reads);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].label, "Undone");

        // Once something else sets the value, the target has changed and is listed.
        set(NEVER_SANDBOX, "Failed", 5);
        let reads = scan_reads(&NEVER_TWEAK);
        assert!(!records.never_changed(failed, &reads[0].1));
        assert_eq!(item_drift(&NEVER_TWEAK, &records, &reads).len(), 2);
        let report = engine(&journal).check(&[&NEVER_TWEAK], false).unwrap();
        assert_eq!(report.entries.len(), 2);
    }

    const VERIFY_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitVerify";
    static VERIFY_A: Tweak = sandbox_tweak(
        "selftest.verify_a",
        &[value(VERIFY_SANDBOX, "A")],
        &[],
        None,
    );
    static VERIFY_B: Tweak = sandbox_tweak(
        "selftest.verify_b",
        &[value(VERIFY_SANDBOX, "B")],
        &[],
        None,
    );

    #[test]
    fn verify_checks_only_the_given_ids() {
        let _cleanup = SandboxKey::new(VERIFY_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        for name in ["A", "B", "Other"] {
            apply(&journal, VERIFY_SANDBOX, name, 1);
            set(VERIFY_SANDBOX, name, 2);
        }
        record_dns(&journal, CHANGED_GUID);
        let engine = engine(&journal);
        let tweaks = [&VERIFY_A, &VERIFY_B];
        let items = |report: &DriftReport| -> Vec<Option<String>> {
            report.entries.iter().map(|e| e.item.clone()).collect()
        };

        let ids = ["selftest.verify_a", "selftest.unknown"].map(String::from);
        let report = engine.verify_in(&tweaks, &ids).unwrap();
        assert_eq!(items(&report), [Some("selftest.verify_a".to_string())]);
        assert_eq!((report.checked, report.unchecked), (1, 0));

        let both = ["selftest.verify_b", "selftest.verify_a"].map(String::from);
        assert_eq!(engine.verify_in(&tweaks, &both).unwrap().entries.len(), 2);
        let none = engine.verify_in(&tweaks, &[]).unwrap();
        assert!(none.entries.is_empty());
        assert_eq!((none.checked, none.unchecked), (0, 0));

        // The full check also lists the value outside these tweaks and the DNS servers.
        let all = engine.check(&tweaks, true).unwrap();
        assert_eq!(all.entries.len(), 4, "{:?}", all.entries);
        assert!(
            engine.verify(&ids).unwrap().entries.is_empty(),
            "the sandbox tweaks are not catalog tweaks"
        );
    }

    #[test]
    fn startup_drift_compares_state_not_bytes() {
        let off =
            |stamp: u8| RegValue::Binary(vec![0x03, 0, 0, 0, stamp, 0, 0, 0, 0, 0, 0, 0]).to_raw();
        let on = RegValue::Binary(vec![0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).to_raw();
        let written = Written::Value { value: off(1) };
        let holds = |current: Option<&RawValue>| {
            holds_written(HKCU, USER_APPROVED_RUN, "Contoso", &written, current)
        };
        assert!(
            holds(Some(&off(9))),
            "turned off again with another time stamp: still off"
        );
        assert!(!holds(Some(&on)));
        assert!(!holds(None), "a missing StartupApproved value is on");

        let record = RegistryRecord {
            id: 1,
            session_id: 1,
            recorded_at: "2026-10-05T00:00:00+00:00".to_string(),
            hive: HKCU,
            key_path: USER_APPROVED_RUN.to_string(),
            value_name: "Contoso".to_string(),
            key_existed: true,
            value_existed: true,
            original: Some(on.clone()),
            active: true,
            reverted_at: None,
            created_root: None,
            written: Some(written.clone()),
        };
        let turned_on = written_mismatch(&record, &written, Some(&on));
        assert_eq!(
            (
                turned_on.found.as_str(),
                turned_on.expected.as_str(),
                turned_on.original.as_deref(),
                turned_on.back_to_original
            ),
            ("on", "off", Some("on"), true)
        );
        assert_eq!(turned_on.label, "Contoso");

        let task = RegistryRecord {
            key_path: ABSENT_TASK_KEY.to_string(),
            value_name: "State".to_string(),
            original: Some(RegValue::Dword(2).to_raw()),
            ..record
        };
        let disabled = Written::Value {
            value: RegValue::Dword(1).to_raw(),
        };
        let enabled = RegValue::Dword(2).to_raw();
        let by_policy = RegValue::Dword(4).to_raw();
        assert!(!holds_written(
            HKCU,
            ABSENT_TASK_KEY,
            "State",
            &disabled,
            Some(&enabled)
        ));
        let mismatch = written_mismatch(&task, &disabled, Some(&by_policy));
        assert_eq!(
            (mismatch.found.as_str(), mismatch.back_to_original),
            ("on", true),
            "enabled by policy is on, like the original"
        );
    }

    #[test]
    fn startup_record_removed_outside_cairn_has_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        // Cairn turned these off; the StartupApproved value and the packaged task's state are
        // gone now. The real keys are only read.
        record_written(
            &journal,
            HKCU,
            USER_APPROVED_RUN,
            ABSENT_ENTRY,
            &turned_off(),
        );
        let disabled = Written::Value {
            value: RegValue::Dword(1).to_raw(),
        };
        record_written(&journal, HKCU, ABSENT_TASK_KEY, "State", &disabled);

        let report = engine(&journal).check(&[], true).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // History words the line from what is found: "not set" is "Removed outside Cairn.",
        // where a value found "on" is "Turned on again outside Cairn".
        let lines: Vec<(Option<&str>, &str, &str, &str)> = report
            .entries
            .iter()
            .map(|e| {
                (
                    e.item.as_deref(),
                    e.mismatch.label.as_str(),
                    e.mismatch.found.as_str(),
                    e.mismatch.expected.as_str(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                (None, "State", "not set", "off"),
                (None, ABSENT_ENTRY, "not set", "off"),
            ]
        );
        assert_eq!(report.checked, 2);
    }

    // ── Keep as is ──

    const MARK_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitForgetMark";
    static MARK_TWEAK: Tweak = sandbox_tweak(
        "selftest.forget_mark",
        &[value(MARK_SANDBOX, "Changed")],
        &[],
        None,
    );

    #[test]
    fn forget_marks_drifted_records_reverted_and_logs_the_kept_value() {
        let _cleanup = SandboxKey::new(MARK_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        set(MARK_SANDBOX, "Changed", 0);
        apply(&journal, MARK_SANDBOX, "Changed", 1);
        set(MARK_SANDBOX, "Changed", 3);

        let report = engine(&journal)
            .forget_in(&[&MARK_TWEAK], &registry_filter_of(MARK_SANDBOX, "changed"))
            .unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let target = target_of(MARK_SANDBOX, "Changed");
        assert_eq!(
            report.forgotten,
            vec![Mismatch {
                target: target.clone(),
                label: "Changed".to_string(),
                expected: dword_text(1),
                found: dword_text(3),
                original: Some(dword_text(0)),
                back_to_original: false,
            }]
        );
        assert_eq!(
            current(MARK_SANDBOX, "Changed"),
            Some(RegValue::Dword(3)),
            "the value stays as it is"
        );
        assert!(journal.active_registry().unwrap().is_empty());
        assert_eq!(
            rows(&journal, OP_FORGET),
            vec![(
                target,
                "forgotten".to_string(),
                Some(format!(
                    "kept {}; original {}",
                    dword_text(3),
                    dword_text(0)
                )),
                None
            )]
        );
        // Nothing is left to keep a second time.
        let again = engine(&journal)
            .forget_in(&[&MARK_TWEAK], &registry_filter_of(MARK_SANDBOX, "Changed"))
            .unwrap();
        assert!(again.forgotten.is_empty() && again.refused.is_empty());
    }

    const STAMP_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitForgetStamp";

    #[test]
    fn forget_stamps_forgotten_at_and_keeps_reverted_at() {
        let _cleanup = SandboxKey::new(STAMP_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        apply(&journal, STAMP_SANDBOX, "Setting", 1);
        set(STAMP_SANDBOX, "Setting", 4);
        record_dns(&journal, CHANGED_GUID);

        let filter = RollbackFilter {
            dns: vec![CHANGED_GUID.trim_matches(['{', '}']).to_ascii_uppercase()],
            ..registry_filter_of(STAMP_SANDBOX, "Setting")
        };
        let report = engine(&journal).forget_in(&[], &filter).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let kept: Vec<(&str, &str)> = report
            .forgotten
            .iter()
            .map(|m| (m.label.as_str(), m.found.as_str()))
            .collect();
        let four = dword_text(4);
        assert_eq!(
            kept,
            [
                ("Setting", four.as_str()),
                ("DNS servers of Cairn self-test (IPv4)", "9.9.9.9")
            ]
        );

        let export = journal.export().unwrap();
        let registry = &export.registry[0];
        assert!(!registry.active && registry.forgotten);
        assert!(registry.reverted_at.is_some(), "Cairn's control ended");
        let dns = &export.dns[0];
        assert!(!dns.active && dns.forgotten && dns.reverted_at.is_some());
        let summary = journal.summary().unwrap();
        assert_eq!((summary.registry_active, summary.dns_active), (0, 0));
    }

    const HOLD_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitForgetHold";
    static HOLD_TWEAK: Tweak = sandbox_tweak(
        "selftest.forget_hold",
        &[value(HOLD_SANDBOX, "Held")],
        &[],
        None,
    );

    #[test]
    fn forget_refuses_targets_that_still_hold_cairns_value() {
        let _cleanup = SandboxKey::new(HOLD_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        apply(&journal, HOLD_SANDBOX, "Held", 1);
        apply(&journal, HOLD_SANDBOX, "Written", 1);
        set(HOLD_SANDBOX, "Unknown", 1);
        record_earlier(&journal, HOLD_SANDBOX, "Unknown");
        for guid in [GONE_GUID, HELD_GUID, BROKEN_GUID] {
            record_dns(&journal, guid);
        }
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_service(
                session,
                &NewServiceRecord {
                    name: "PCOptimizerNoSuchService".to_string(),
                    display_name: "Cairn self-test".to_string(),
                    start_type: StartType::Manual,
                    delayed_auto_start: false,
                    was_running: false,
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();

        let filter = RollbackFilter {
            registry: ["Held", "Written", "Unknown"]
                .iter()
                .flat_map(|name| registry_filter_of(HOLD_SANDBOX, name).registry)
                .collect(),
            services: vec!["pcoptimizernosuchservice".to_string()],
            dns: vec![
                HELD_GUID.to_string(),
                BROKEN_GUID.to_string(),
                GONE_GUID.to_string(),
            ],
            ..Default::default()
        };
        let report = engine(&journal).forget_in(&[&HOLD_TWEAK], &filter).unwrap();
        assert!(report.forgotten.is_empty(), "{:?}", report.forgotten);
        let refused: Vec<(&str, &str)> = report
            .refused
            .iter()
            .map(|f| (f.target.as_str(), f.error.as_str()))
            .collect();
        let held = target_of(HOLD_SANDBOX, "Held");
        let written = target_of(HOLD_SANDBOX, "Written");
        let unknown = target_of(HOLD_SANDBOX, "Unknown");
        let still = |target: &str| {
            format!("{target} still holds the value Cairn set; Undo restores the original.")
        };
        let dns = "IPv4 DNS servers of Cairn self-test";
        let (held_text, written_text, dns_text) = (still(&held), still(&written), still(dns));
        let unknown_text = format!("Cairn does not know what it wrote to {unknown}; use Undo.");
        let service_text =
            "Cairn does not know what it wrote to service PCOptimizerNoSuchService; use Undo.";
        let broken_text = "IPv4 DNS servers of Cairn self-test could not be read (Access is \
                           denied.), so Cairn still tracks it.";
        // A disabled adapter is not listed either, and Undo still restores its servers.
        let gone_text = "IPv4 DNS servers of Cairn self-test cannot be read while the adapter \
                         is not listed (removed or disabled), so Cairn still tracks it; use Undo.";
        assert_eq!(
            refused,
            vec![
                (unknown.as_str(), unknown_text.as_str()),
                (written.as_str(), written_text.as_str()),
                (held.as_str(), held_text.as_str()),
                ("service PCOptimizerNoSuchService", service_text),
                (dns, broken_text),
                (dns, dns_text.as_str()),
                (dns, gone_text),
            ]
        );
        let summary = journal.summary().unwrap();
        assert_eq!(
            (
                summary.registry_active,
                summary.services_active,
                summary.dns_active
            ),
            (3, 1, 3),
            "every record is still tracked"
        );
        assert!(rows(&journal, OP_FORGET).is_empty());
    }

    #[test]
    fn forget_refuses_appx_and_task_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_appx(
                session,
                &NewAppxRecord {
                    package_full_name: "PCOptimizer.SelfTest_1.0.0.0_x64__0000000000000".into(),
                    package_family: "PCOptimizer.SelfTest_0000000000000".into(),
                    install_location: r"C:\PCOptimizerSelfTest".into(),
                    all_users: false,
                },
            )
            .unwrap();
        journal
            .record_task_definition(
                session,
                &NewTaskDefinitionRecord {
                    path: r"\PCOptimizerSelfTest\DriftDefinition".into(),
                    purpose: "maintenance".into(),
                    folder_created: false,
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();

        let filter = RollbackFilter {
            appx_families: vec!["pcoptimizer.selftest_0000000000000".into()],
            task_definitions: vec![r"\PCOptimizerSelfTest\DriftDefinition".into()],
            ..Default::default()
        };
        let report = engine(&journal).forget(&filter).unwrap();
        assert!(report.forgotten.is_empty());
        let refused: Vec<(&str, &str)> = report
            .refused
            .iter()
            .map(|f| (f.target.as_str(), f.error.as_str()))
            .collect();
        assert_eq!(
            refused,
            [
                (
                    "Appx package PCOptimizer.SelfTest_1.0.0.0_x64__0000000000000",
                    "Keep as is applies to settings only; use Undo."
                ),
                (
                    r"task \PCOptimizerSelfTest\DriftDefinition",
                    "Keep as is applies to settings only; use Undo."
                ),
            ]
        );
        let summary = journal.summary().unwrap();
        assert_eq!(
            (summary.appx_active, summary.task_definitions_active),
            (1, 1)
        );
    }

    const SESSION_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitForgetSession";

    #[test]
    fn forget_opens_no_session_and_writes_nothing() {
        let _cleanup = SandboxKey::new(SESSION_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        apply(&journal, SESSION_SANDBOX, "Setting", 1);
        set(SESSION_SANDBOX, "Setting", 6);
        let sessions = journal.summary().unwrap().sessions;
        let ops = journal.ops(100).unwrap().len();

        let report = engine(&journal)
            .forget(&registry_filter_of(SESSION_SANDBOX, "Setting"))
            .unwrap();
        assert_eq!(report.forgotten.len(), 1, "{:?}", report.refused);
        assert_eq!(journal.summary().unwrap().sessions, sessions, "no session");
        assert_eq!(
            current(SESSION_SANDBOX, "Setting"),
            Some(RegValue::Dword(6))
        );
        assert_eq!(
            journal.ops(100).unwrap().len(),
            ops + 1,
            "only the forget_record row"
        );
        assert!(rows(&journal, OP_FORGET).iter().all(|row| row.3.is_none()));
    }

    const FORMER_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\DriftUnitForgetFormer";
    static FORMER_TWEAK: Tweak = sandbox_tweak(
        "selftest.forget_former",
        &[value(FORMER_SANDBOX, "Current")],
        &[Former {
            action: reg_dword(Hive::CurrentUser, FORMER_SANDBOX, "Former", 0),
            reason: "Cairn 0.2 also set this value, which this tweak no longer needs.",
        }],
        None,
    );

    #[test]
    fn forget_allows_former_targets() {
        let _cleanup = SandboxKey::new(FORMER_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        // An earlier version set Former to 0 over a 5; it still holds the 0.
        set(FORMER_SANDBOX, "Former", 5);
        apply(&journal, FORMER_SANDBOX, "Former", 0);

        let report = engine(&journal)
            .forget_in(
                &[&FORMER_TWEAK],
                &registry_filter_of(FORMER_SANDBOX, "Former"),
            )
            .unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(
            report.forgotten,
            vec![Mismatch {
                target: target_of(FORMER_SANDBOX, "Former"),
                label: "Former".to_string(),
                expected: dword_text(0),
                found: dword_text(0),
                original: Some(dword_text(5)),
                back_to_original: false,
            }]
        );
        assert_eq!(current(FORMER_SANDBOX, "Former"), Some(RegValue::Dword(0)));
        assert!(journal.active_registry().unwrap().is_empty());
    }

    const MISSING_TASK: &str = r"\PCOptimizerSelfTest\DriftUnitMissing";
    static MISSING_TASK_TWEAK: Tweak = sandbox_tweak(
        "selftest.forget_task",
        &[Action::ScheduledTask(ScheduledTaskAction {
            path: MISSING_TASK,
            enabled: false,
        })],
        &[],
        None,
    );

    #[test]
    fn forget_lets_go_of_a_task_that_no_longer_exists() {
        // Task Scheduler is only asked about a task that does not exist.
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let session = journal.begin_session("drift unit", "test").unwrap();
        journal
            .record_scheduled_task(
                session,
                &NewScheduledTaskRecord {
                    path: MISSING_TASK.to_string(),
                    was_enabled: true,
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
        let filter = RollbackFilter {
            scheduled_tasks: vec![MISSING_TASK.to_string()],
            ..Default::default()
        };

        // Not drift: a task that no longer exists is not checked.
        let report = engine(&journal)
            .check(&[&MISSING_TASK_TWEAK], true)
            .unwrap();
        assert!(report.entries.is_empty(), "{:?}", report.entries);
        let report = engine(&journal)
            .forget_in(&[&MISSING_TASK_TWEAK], &filter)
            .unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let forgotten: Vec<(&str, &str, Option<&str>)> = report
            .forgotten
            .iter()
            .map(|m| (m.label.as_str(), m.found.as_str(), m.original.as_deref()))
            .collect();
        assert_eq!(
            forgotten,
            [("task DriftUnitMissing", "not on this PC", Some("enabled"))]
        );
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
    }

    #[test]
    fn the_catalog_drift_of_an_empty_journal_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::new(journal.clone());
        let report = engine.drift().unwrap();
        assert!(report.entries.is_empty() && report.warnings.is_empty());
        assert_eq!((report.checked, report.unchecked), (0, 0));
        let ids: Vec<String> = TWEAKS.iter().map(|t| t.id.to_string()).collect();
        assert!(engine.verify(&ids).unwrap().entries.is_empty());
        let everything = RollbackFilter {
            power: true,
            ..registry_filter_of(MARK_SANDBOX, "Changed")
        };
        let forget = engine.forget(&everything).unwrap();
        assert!(forget.forgotten.is_empty() && forget.refused.is_empty());
        assert_eq!(journal.summary().unwrap().sessions, 0);
    }

    #[test]
    fn drift_entries_serialize_their_kind_snake_case() {
        let entry = DriftEntry {
            item: None,
            kind: DriftKind::ScheduledTask,
            mismatch: Mismatch {
                target: r"scheduled task \Example\Task".into(),
                label: "task Task".into(),
                expected: "disabled".into(),
                found: "enabled".into(),
                original: Some("enabled".into()),
                back_to_original: true,
            },
            filter: RollbackFilter::default(),
            recorded_at: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["kind"], "scheduled_task");
        assert_eq!(json["mismatch"]["found"], "enabled");
        let report: DriftReport = serde_json::from_value(serde_json::json!({
            "entries": [json], "checked": 1, "unchecked": 0, "warnings": [], "duration_ms": 3
        }))
        .unwrap();
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].kind, DriftKind::ScheduledTask);
        let forget: ForgetReport = serde_json::from_value(serde_json::json!({
            "forgotten": [], "refused": [{"target": "service Example", "error": "refused"}]
        }))
        .unwrap();
        assert_eq!(forget.refused[0].target, "service Example");
    }

    // ── wording ──

    /// Phrases no engine text may use: a read-back shows a value, never an effect, and Cairn
    /// cannot tell who changed a value.
    const FORBIDDEN: [&str; 6] = [
        "enforced",
        "in effect",
        "works",
        "blocked by Windows",
        "Windows did not keep",
        "by Windows",
    ];

    /// Whether `text` holds `phrase` as whole words, ignoring case.
    fn contains_phrase(text: &str, phrase: &str) -> bool {
        let (text, phrase) = (text.to_lowercase(), phrase.to_lowercase());
        text.match_indices(&phrase).any(|(start, _)| {
            let before = text[..start].chars().next_back();
            let after = text[start + phrase.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    }

    /// A floor no 22H2 build reaches.
    const FLOOR_24H2: &[Build] = &[Build::new(26100, 3915)];

    /// The Windows build and edition notes a scan shows, for every range of the catalog and
    /// one range of each kind, on Home 22H2, Pro 26H2 and an edition Cairn does not know, and
    /// the scan's warning when the Windows facts cannot be read.
    fn windows_range_notes() -> Vec<String> {
        let home_22h2 = WindowsFacts {
            build: Build::new(22621, 0),
            family: 22621,
            version: Some("22H2".to_string()),
            edition_id: "Core".to_string(),
            edition: EditionClass::Home,
            name: "Windows 11 Home".to_string(),
        };
        let pro_26h2 = WindowsFacts {
            build: Build::new(26300, 9457),
            family: 26100,
            version: Some("26H2".to_string()),
            edition_id: "Professional".to_string(),
            edition: EditionClass::Pro,
            name: "Windows 11 Pro".to_string(),
        };
        let unknown_edition = WindowsFacts {
            edition_id: "ContosoEdition".to_string(),
            edition: EditionClass::Unknown,
            name: "Windows 11 Contoso".to_string(),
            ..pro_26h2.clone()
        };
        let mut ranges = vec![
            WindowsRange {
                from: FLOOR_24H2,
                through: None,
                editions: Editions::All,
            },
            WindowsRange {
                from: &[],
                through: Some(22621),
                editions: Editions::All,
            },
            WindowsRange {
                from: &[],
                through: None,
                editions: Editions::ProOrHigher,
            },
            WindowsRange {
                from: &[],
                through: None,
                editions: Editions::EnterpriseOrEducation,
            },
        ];
        for t in TWEAKS {
            if !ranges.contains(&t.windows) {
                ranges.push(t.windows);
            }
        }
        let mut notes = Vec::new();
        for range in &ranges {
            for pc in [&home_22h2, &pro_26h2, &unknown_edition] {
                match range_availability(range, pc) {
                    Availability::Met => {}
                    Availability::Missing(unmet) => notes.push(unmet.text),
                    Availability::Unknown(text) => notes.push(text),
                }
            }
        }
        for sentence in [
            "Needs Windows 11 24H2 (build 26100.3915) or later; this PC runs Windows 11 Home",
            "and later no longer use this setting; this PC runs Windows 11 Pro",
            "Needs Windows 11 Pro, Enterprise or Education; this PC runs Windows 11 Home.",
            "Needs Windows 11 Enterprise or Education; this PC runs Windows 11 Pro.",
            "Cairn does not know the edition \"ContosoEdition\"",
        ] {
            assert!(
                notes.iter().any(|note| note.contains(sentence)),
                "{sentence:?} is not among {notes:?}"
            );
        }

        let unreadable = Requirements::new(Probes {
            windows: || Err(Error::Other("Access is denied.".into())),
            ..SYSTEM_PROBES
        });
        assert_eq!(unreadable.check_windows(&ranges[0]), Availability::Met);
        let warning = unreadable.windows_warning();
        assert!(
            warning
                .as_deref()
                .is_some_and(|w| w.starts_with("Cannot read the Windows version")),
            "{warning:?}"
        );
        notes.extend(warning);
        notes
    }

    #[test]
    fn engine_texts_never_claim_an_effect() {
        assert!(contains_phrase("It works.", "works"));
        assert!(!contains_phrase("Wi-Fi networks", "works"));
        assert!(contains_phrase("Changed by Windows.", "by Windows"));
        for allowed in [
            "kept",
            "still set",
            "changed back",
            "no longer holds",
            "Windows or another program changed it",
        ] {
            assert!(
                FORBIDDEN
                    .iter()
                    .all(|phrase| !contains_phrase(allowed, phrase)),
                "{allowed}"
            );
        }

        let mut texts: Vec<String> = [
            NOTE_CHANGED_BACK,
            NOTE_CHANGED,
            NOTE_CHANGED_TWO,
            NOTE_CHANGED_MORE,
            NOTE_APPLY_ONE,
            NOTE_APPLY_SEVERAL,
            FORGET_STILL_HOLDS,
            FORGET_WRITTEN_UNKNOWN,
            FORGET_SETTINGS_ONLY,
            FORGET_UNREADABLE,
            FORGET_ADAPTER_NOT_LISTED,
            FORGET_NOT_TRACKED,
            DRIFT_UNREADABLE,
            verify::READ_BACK_DETAIL,
            verify::READ_BACK_WARNING,
            verify::AUDIT_FAILED,
            crate::safety::rollback::READ_BACK_WARNING,
            RECORDED_UNMET_NOTE,
            REINSTALLED_NOTE,
        ]
        .iter()
        .map(|text| text.to_string())
        .collect();
        texts.extend(
            [
                Requirement::Office,
                Requirement::OfficeBusiness,
                Requirement::Edge,
                Requirement::GpuScheduling,
                Requirement::RecallComponent,
                Requirement::NotepadAi,
                Requirement::Paint,
                Requirement::SystemAiModels,
            ]
            .map(|req| req.missing_text().to_string()),
        );
        for (stored, enabled) in [(Some(2), false), (Some(1), true), (None, true)] {
            let gpu = GpuScheduling {
                supported: true,
                enabled,
            };
            texts.extend(gpu_note(stored, gpu).map(str::to_string));
        }
        for t in TWEAKS {
            texts.extend(t.retired.map(str::to_string));
            texts.extend(t.undo_limit.map(str::to_string));
            texts.extend(t.former.iter().map(|f| f.reason.to_string()));
        }
        texts.extend(windows_range_notes());
        // Notes as a scan shows them.
        let drift = [
            mismatch("AllowTelemetry", "0x00000003 (3)", "0x00000000 (0)", false),
            mismatch("Start", "on", "off", true),
            mismatch("task Proxy", "enabled", "disabled", true),
        ];
        texts.extend(drift_note(&drift[..1]));
        texts.extend(drift_note(&drift[1..2]));
        texts.extend(drift_note(&drift));

        for text in &texts {
            for phrase in FORBIDDEN {
                assert!(!contains_phrase(text, phrase), "{text:?} says {phrase:?}");
            }
        }
    }
}
