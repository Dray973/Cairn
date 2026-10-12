//! Scan, apply and revert across the catalog.
//!
//! Items are addressed by id: a tweak id from [`catalog::TWEAKS`] such as
//! `privacy.activity_history`, or `appx.<PackageName>` for an installed bloatware package.
//! Scanning is read-only and works without elevation. Applying opens one
//! [`Safety`] session, so every change in a call shares one restore point and one journal
//! session; [`Engine::apply_in`] applies under a session the caller already opened (a
//! profile applies tweaks next to other settings in one session). Reverting replays only
//! the journal records that belong to the given items, and the rollback report says which
//! restart the restored tweaks need.
//!
//! A tweak that is retired, whose Windows range excludes this PC or whose requirement
//! (Office, Edge, GPU scheduling support, Recall, Notepad, Paint, text and image generation)
//! is known to be missing scans as unavailable with a note and is skipped on apply before a
//! session is opened or anything is recorded; while it has an active journal record it is
//! read normally, so it stays undoable. Values an earlier version of Cairn set for a tweak and
//! this version no longer sets (its former values) belong to the tweak: their records undo
//! with it, and applying the tweak restores them first, under the apply's own session.
//! Settings Windows reads only at sign-in (the mouse) are pushed to the running session after
//! they are applied. Once an apply has written something, it waits [`SETTLE`] and reads the
//! written targets back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::appx::{self, AppxPackage};
use super::catalog::{
    self, Action, Category, Former, Requirement, RestartNeed, Risk, Topic, Tweak,
    FORMERLY_RECOMMENDED, GRAPHICS_DRIVERS,
};
use super::drift;
use super::live::{self, LiveSetting};
use super::requirements::{
    gpu_note, Availability, Probes, Requirements, Unavailable, WindowsFacts, SYSTEM_PROBES,
};
use super::scheduled_tasks::{self, TaskConnection};
use super::verify::{self, ReadBack, WrittenTarget};
use super::{power, registry, services, ActionState, ActionStatus};
use crate::safety::rollback::{self, RegistryTarget, RollbackFilter, RollbackReport};
use crate::safety::state_log::{
    AppxRecord, DnsRecord, Journal, PowerRecord, RegistryRecord, ScheduledTaskRecord, ServiceRecord,
};
use crate::safety::verify::{Mismatch, SETTLE};
use crate::safety::{
    self, MutationOutcome, RestorePoint, RestorePointPolicy, Safety, SafetyOptions,
};
use crate::win::registry::{read_value, Hive, RegValue};
use crate::{is_elevated, Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Tweak,
    Appx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Applied,
    NotApplied,
    /// Some actions are applied and others are not.
    Partial,
    /// Nothing in the item exists on this machine.
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanItem {
    pub id: String,
    pub kind: ItemKind,
    pub category: Category,
    pub title: String,
    pub description: String,
    pub risk: Risk,
    /// Selected when the whole category is enabled.
    pub recommended: bool,
    pub state: ItemState,
    pub restart: RestartNeed,
    /// One entry per action (tweaks) or per package (Appx).
    pub actions: Vec<ActionStatus>,
    /// The journal holds an active record for at least one of the item's targets, so
    /// reverting it restores something. An item can be applied without being revertible
    /// when its settings were already in place before this tool ran.
    #[serde(default)]
    pub revertible: bool,
    /// Explanation of a state the other fields cannot express, such as a removed package
    /// that Windows installed again or scheduled tasks this tool turned off that were
    /// turned back on.
    #[serde(default)]
    pub note: Option<String>,
    /// Groups the tweak with related tweaks of every category; `None` for Store packages.
    #[serde(default)]
    pub topic: Option<Topic>,
    /// Why the tweak is not offered on this PC. Set even when the item has active journal
    /// records, which then still undo.
    #[serde(default)]
    pub unavailable: Option<Unavailable>,
    /// What Undo cannot bring back, shown before the tweak is applied.
    #[serde(default)]
    pub undo_limit: Option<String>,
    /// Active records of values an earlier version of Cairn set for the tweak.
    #[serde(default)]
    pub former: Vec<FormerRecord>,
    /// Turning the category's mode off restores the item.
    #[serde(default)]
    pub mode_undo: bool,
    /// Recorded targets that no longer hold what Cairn set.
    #[serde(default)]
    pub drift: Vec<Mismatch>,
}

/// An active journal record of a value an earlier version of Cairn set for a tweak and this
/// version no longer sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormerRecord {
    /// The value, as `registry::describe` words it (`HKLM\…\EnableActivityFeed`).
    pub target: String,
    /// The value it is restored to, as reports show it, or "not set".
    pub original: String,
    /// Why this version no longer sets the value.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryStatus {
    pub category: Category,
    pub items: usize,
    pub applied: usize,
    pub recommended: usize,
    pub recommended_applied: usize,
    /// Every available recommended item is applied (and there is at least one).
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub elevated: bool,
    pub has_battery: bool,
    pub items: Vec<ScanItem>,
    pub categories: Vec<CategoryStatus>,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
    /// Why the Store package inventory could not be read, when it could not; the report
    /// then lists no installed bloatware.
    #[serde(default)]
    pub appx_unavailable: Option<String>,
    /// The running Windows, when its build and edition could be read.
    #[serde(default)]
    pub windows: Option<WindowsFacts>,
}

impl ScanReport {
    pub fn item(&self, id: &str) -> Option<&ScanItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOptions {
    pub restore_point: RestorePointPolicy,
    /// Report what would change without changing anything or opening a session.
    pub dry_run: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            restore_point: RestorePointPolicy::Try,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcome {
    Applied,
    AlreadyApplied,
    Skipped,
    Failed,
    /// Dry run: the item would be applied.
    Planned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: String,
    pub outcome: ItemOutcome,
    pub details: Vec<String>,
    /// Targets this apply wrote that did not hold when read back after [`SETTLE`]. Their
    /// records stay active, so Undo restores the originals. An already-applied item has
    /// entries only through a restored former value.
    #[serde(default)]
    pub not_kept: Vec<Mismatch>,
}

impl ItemResult {
    /// A result with no read-back entries.
    fn new(id: impl Into<String>, outcome: ItemOutcome, details: Vec<String>) -> Self {
        Self {
            id: id.into(),
            outcome,
            details,
            not_kept: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub dry_run: bool,
    pub session_id: Option<i64>,
    pub restore_point: Option<RestorePoint>,
    pub results: Vec<ItemResult>,
    pub warnings: Vec<String>,
    /// Strongest restart requirement among applied items.
    pub restart: RestartNeed,
}

impl ApplyReport {
    pub fn failed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == ItemOutcome::Failed)
            .count()
    }
}

/// Serializable description of one catalog entry, for listing without scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Tweak id, or the package Name pattern for bloatware (`appx.<pattern>`).
    pub id: String,
    pub kind: ItemKind,
    pub category: Category,
    pub title: String,
    pub description: String,
    pub risk: Risk,
    pub default_on: bool,
    pub restart: RestartNeed,
    /// What this PC must have for the tweak to take effect; None for bloatware packages.
    #[serde(default)]
    pub requires: Option<Requirement>,
    /// What each action changes, e.g. `HKLM\...\AllowTelemetry` or `service SysMain`.
    pub targets: Vec<String>,
    /// Topic of a tweak; `None` for bloatware packages.
    #[serde(default)]
    pub topic: Option<Topic>,
    /// The Windows builds and editions that use the tweak's settings, in words; `None` for
    /// every Windows 11 build and edition.
    #[serde(default)]
    pub windows: Option<String>,
    /// Why the tweak is no longer offered.
    #[serde(default)]
    pub retired: Option<String>,
    /// What Undo cannot bring back.
    #[serde(default)]
    pub undo_limit: Option<String>,
    /// Values earlier versions of Cairn set for the tweak, worded as `targets`; their journal
    /// records belong to the tweak.
    #[serde(default)]
    pub former_targets: Vec<String>,
}

/// Every tweak followed by every bloatware package pattern, in catalog order.
pub fn catalog_view() -> Vec<CatalogEntry> {
    let tweaks = catalog::TWEAKS.iter().map(tweak_entry);
    let packages = catalog::BLOAT_PACKAGES.iter().map(|b| CatalogEntry {
        id: catalog::appx_item_id(b.name),
        kind: ItemKind::Appx,
        category: Category::Bloatware,
        title: b.title.to_string(),
        description: b.description.to_string(),
        risk: b.risk,
        default_on: b.default_on,
        restart: RestartNeed::None,
        requires: None,
        targets: vec![format!("Store package {}", b.name)],
        topic: None,
        windows: None,
        retired: None,
        undo_limit: None,
        former_targets: Vec::new(),
    });
    tweaks.chain(packages).collect()
}

/// The catalog entry of one tweak.
fn tweak_entry(t: &Tweak) -> CatalogEntry {
    CatalogEntry {
        id: t.id.to_string(),
        kind: ItemKind::Tweak,
        category: t.category,
        title: t.title.to_string(),
        description: t.description.to_string(),
        risk: t.risk,
        default_on: t.default_on,
        restart: t.restart,
        requires: t.requires,
        targets: t.actions.iter().map(action_label).collect(),
        topic: Some(t.topic),
        windows: t.windows.describe(),
        retired: t.retired.map(str::to_string),
        undo_limit: t.undo_limit.map(str::to_string),
        former_targets: t
            .former
            .iter()
            .map(|f| registry::describe(&f.action))
            .collect(),
    }
}

/// The parts of this PC the engine reaches through a seam (requirement probes, the live
/// settings a tweak feeds and their push to the session, the wait before a read-back and the
/// DNS servers of an adapter); tests substitute fakes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EngineSystem {
    pub(crate) probes: Probes,
    pub(crate) refresh: fn(LiveSetting) -> Result<()>,
    /// The live settings the values of a tweak feed.
    pub(crate) live_settings: fn(&Tweak) -> Vec<LiveSetting>,
    /// Waits before an apply's writes are read back. LIVE sleeps [`SETTLE`].
    pub(crate) settle: fn(),
    /// The DNS servers now set for a record's adapter and family, `None` when the adapter is
    /// gone. LIVE: [`crate::network::dns_now`].
    pub(crate) dns_now: fn(&DnsRecord) -> Result<Option<String>>,
}

impl EngineSystem {
    pub(crate) const LIVE: EngineSystem = EngineSystem {
        probes: SYSTEM_PROBES,
        refresh: live::refresh,
        live_settings: live::settings_of,
        settle: settle_live,
        dns_now: crate::network::dns_now,
    };
}

/// Lets the targets an apply wrote settle before they are read back.
fn settle_live() {
    std::thread::sleep(SETTLE);
}

/// What applying one tweak did, and the targets it wrote, to be read back once the whole
/// apply has settled.
pub(crate) struct TweakApply {
    pub(crate) result: ItemResult,
    pub(crate) written: Vec<WrittenTarget>,
}

/// Entry point for scan, apply and revert against one journal.
#[derive(Debug, Clone)]
pub struct Engine {
    journal: Arc<Journal>,
    system: EngineSystem,
}

impl Engine {
    pub fn new(journal: Arc<Journal>) -> Self {
        Self {
            journal,
            system: EngineSystem::LIVE,
        }
    }

    /// An engine whose requirement probes and live-settings push are `system`.
    #[cfg(test)]
    pub(crate) fn with_system(journal: Arc<Journal>, system: EngineSystem) -> Self {
        Self { journal, system }
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Arc::new(Journal::open_default()?)))
    }

    pub fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    /// The seams this engine reaches the PC through.
    pub(crate) fn system(&self) -> &EngineSystem {
        &self.system
    }

    /// Requirement answers for one scan or apply through this engine's probes.
    fn requirements(&self) -> Requirements {
        Requirements::new(self.system.probes)
    }

    // ───────────────────────────── scan ─────────────────────────────

    pub fn scan(&self) -> Result<ScanReport> {
        self.scan_with(catalog::TWEAKS, true)
    }

    /// Scans `tweaks` and, with `with_appx`, the Store packages.
    fn scan_with(&self, tweaks: &'static [Tweak], with_appx: bool) -> Result<ScanReport> {
        let started = Instant::now();
        let mut warnings = Vec::new();
        let has_battery = power::has_system_battery();
        let active = ActiveRecords::load(&self.journal)?;
        let tasks = TaskConnection::new();
        let reqs = self.requirements();
        let mut items: Vec<ScanItem> = tweaks
            .iter()
            .map(|t| {
                let revertible = active.covers(t);
                // A tweak with an active record is read normally, so it stays undoable.
                let (mut item, reads) =
                    read_tweak(t, has_battery, &tasks, &reqs, !revertible, &mut warnings);
                item.revertible = revertible;
                item.former = active.former_records(t);
                active.mark_held(t, &mut item.actions, &reads);
                item.drift = drift::item_drift(t, &active, &reads);
                let availability = reqs.check_tweak(t);
                if let Availability::Missing(unmet) = &availability {
                    item.unavailable = Some(unmet.kind);
                }
                item.note = item
                    .note
                    .take()
                    .or_else(|| recorded_unmet_note(&availability, revertible))
                    .or_else(|| drift::scan_note(t, &item.actions, &item.drift))
                    .or_else(|| gpu_scan_note(t, &reqs));
                item
            })
            .collect();
        let mut appx_unavailable = None;
        if with_appx {
            items.extend(self.scan_appx(&active.appx, &mut warnings, &mut appx_unavailable)?);
        }
        let categories = Category::ALL
            .iter()
            .map(|&c| category_status(c, &items))
            .collect();
        warnings.extend(reqs.windows_warning());
        Ok(ScanReport {
            elevated: is_elevated(),
            has_battery,
            items,
            categories,
            warnings,
            duration_ms: started.elapsed().as_millis() as u64,
            appx_unavailable,
            windows: reqs.windows().cloned(),
        })
    }

    /// Installed bloatware packages (not applied) plus packages this tool removed that are
    /// not installed again (applied), so removed items stay visible and revertible. An
    /// installed package that still has an active removal record was installed again after
    /// the removal; it is reported as installed, revertible, with a note. When the
    /// inventory cannot be read, `unavailable` gets the reason and only removed packages
    /// are listed.
    fn scan_appx(
        &self,
        removed: &[AppxRecord],
        warnings: &mut Vec<String>,
        unavailable: &mut Option<String>,
    ) -> Result<Vec<ScanItem>> {
        let installed = match appx::inventory() {
            Ok(list) => list,
            Err(e) => {
                warnings.push(format!("Store package inventory unavailable: {e}"));
                *unavailable = Some(e.to_string());
                Vec::new()
            }
        };

        let mut by_name: BTreeMap<String, (String, Vec<AppxPackage>)> = BTreeMap::new();
        for pkg in installed {
            if catalog::bloat_entry_for(&pkg.name).is_none() || !appx::is_removable(&pkg) {
                continue;
            }
            by_name
                .entry(pkg.name.to_ascii_lowercase())
                .or_insert_with(|| (pkg.name.clone(), Vec::new()))
                .1
                .push(pkg);
        }

        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let has_record = |name: &str| {
            removed
                .iter()
                .any(|r| package_name_from_family(&r.package_family).eq_ignore_ascii_case(name))
        };
        for (key, (name, pkgs)) in &by_name {
            let entry = catalog::bloat_entry_for(name).expect("filtered above");
            seen.insert(key.clone());
            let mut item = appx_item(
                name,
                entry,
                ItemState::NotApplied,
                pkgs.iter()
                    .map(|p| {
                        ActionStatus::new(
                            ActionState::NotApplied,
                            format!("installed: {}", p.full_name),
                        )
                    })
                    .collect(),
            );
            if has_record(name) {
                item.revertible = true;
                item.note = Some(REINSTALLED_NOTE.to_string());
            }
            items.push(item);
        }

        for rec in removed {
            let name = package_name_from_family(&rec.package_family).to_string();
            if !seen.insert(name.to_ascii_lowercase()) {
                continue;
            }
            let Some(entry) = catalog::bloat_entry_for(&name) else {
                continue;
            };
            let mut item = appx_item(
                &name,
                entry,
                ItemState::Applied,
                vec![ActionStatus::new(
                    ActionState::Applied,
                    format!("removed: {}", rec.package_full_name),
                )],
            );
            item.revertible = true;
            items.push(item);
        }
        Ok(items)
    }

    // ───────────────────────────── apply ─────────────────────────────

    /// Applies the given item ids in one [`Safety`] session. Items that change per-user
    /// state (tweaks with an HKCU value, Store packages) are skipped whole, before anything
    /// is changed, when this process runs as another account than the signed-in user or
    /// the two cannot be compared; when nothing else is selected, no session or restore
    /// point is created. A dry run reports those items as skipped too. Tweaks whose
    /// requirement is missing on this PC are skipped the same way: before anything is
    /// recorded, and without a session or restore point when nothing else is selected.
    pub fn apply(&self, ids: &[String], opts: &ApplyOptions) -> Result<ApplyReport> {
        self.apply_selected(select(ids), format!("apply: {}", ids.join(", ")), opts)
    }

    /// [`Engine::apply`] of a resolved selection; `label` names the session it opens. Each
    /// requirement probe runs at most once for the whole apply.
    fn apply_selected(
        &self,
        mut sel: Selection,
        label: String,
        opts: &ApplyOptions,
    ) -> Result<ApplyReport> {
        withhold_per_user(
            &mut sel.tweaks,
            &mut sel.appx_names,
            &mut sel.results,
            safety::check_interactive_user,
        );
        let Selection {
            mut tweaks,
            appx_names,
            mut results,
        } = sel;
        let reqs = self.requirements();

        if opts.dry_run {
            let tasks = TaskConnection::new();
            let has_battery = power::has_system_battery();
            // An apply restores a tweak's former values first, so the plan lists them.
            let active = if tweaks.iter().any(|t| !t.former.is_empty()) {
                Some(ActiveRecords::load(&self.journal)?)
            } else {
                None
            };
            let mut warnings = Vec::new();
            for t in &tweaks {
                let mut item = scan_tweak(t, has_battery, &tasks, &reqs, true, &mut warnings);
                if let Some(active) = &active {
                    item.former = active.former_records(t);
                }
                results.push(plan_result(&item));
            }
            if !appx_names.is_empty() {
                let installed = appx::inventory()?;
                for name in &appx_names {
                    let matches: Vec<&AppxPackage> = installed
                        .iter()
                        .filter(|p| p.name.eq_ignore_ascii_case(name))
                        .collect();
                    let id = catalog::appx_item_id(name);
                    results.push(if matches.is_empty() {
                        ItemResult::new(
                            id,
                            ItemOutcome::AlreadyApplied,
                            vec!["not installed".into()],
                        )
                    } else {
                        ItemResult::new(
                            id,
                            ItemOutcome::Planned,
                            matches
                                .iter()
                                .map(|p| format!("remove {}", p.full_name))
                                .collect(),
                        )
                    });
                }
            }
            return Ok(ApplyReport {
                dry_run: true,
                session_id: None,
                restore_point: None,
                results,
                warnings,
                restart: RestartNeed::None,
            });
        }

        withhold_unavailable(&mut tweaks, &mut results, &reqs);
        if tweaks.is_empty() && appx_names.is_empty() {
            return Ok(ApplyReport {
                dry_run: false,
                session_id: None,
                restore_point: None,
                results,
                warnings: Vec::new(),
                restart: RestartNeed::None,
            });
        }
        if !is_elevated() {
            return Err(Error::NotElevated);
        }

        let safety = Safety::begin(
            self.journal.clone(),
            SafetyOptions {
                label,
                restore_point: opts.restore_point,
                ..Default::default()
            },
        )?;
        Ok(self.apply_selection(
            &safety,
            Selection {
                tweaks,
                appx_names,
                results,
            },
            &reqs,
        ))
    }

    /// Applies `ids` under the caller's open session (opens no session, creates no restore
    /// point): same id resolution, per-user withholding (through
    /// [`Safety::ensure_interactive_user`]) and per-item apply as [`Engine::apply`]. The
    /// session must belong to this engine's journal.
    pub fn apply_in(&self, safety: &Safety, ids: &[String]) -> Result<ApplyReport> {
        if !Arc::ptr_eq(safety.journal(), &self.journal)
            && safety.journal().path() != self.journal.path()
        {
            return Err(Error::Other(
                "the session belongs to another journal; nothing was changed".into(),
            ));
        }
        let mut sel = select(ids);
        withhold_per_user(
            &mut sel.tweaks,
            &mut sel.appx_names,
            &mut sel.results,
            || safety.ensure_interactive_user(),
        );
        safety.ensure_elevated()?;
        Ok(self.apply_selection(safety, sel, &self.requirements()))
    }

    /// Applies a resolved selection under `safety`; every path that applies ends here, so
    /// [`Engine::apply`] and [`Engine::apply_in`] behave the same once a session exists.
    /// Tweaks that are not offered on this PC (`reqs`) are skipped; the live settings of
    /// applied tweaks are pushed to the running session after the tweaks. When anything was
    /// written, the apply then waits once (`settle`) and reads every written target back,
    /// before the Store packages.
    fn apply_selection(&self, safety: &Safety, sel: Selection, reqs: &Requirements) -> ApplyReport {
        let Selection {
            tweaks,
            appx_names,
            mut results,
        } = sel;
        let tasks = TaskConnection::new();
        let mut restart = RestartNeed::None;
        let mut applied_live: Vec<LiveSetting> = Vec::new();
        let mut written: Vec<(usize, WrittenTarget)> = Vec::new();

        for t in &tweaks {
            let TweakApply {
                result,
                written: targets,
            } = apply_tweak(safety, t, &tasks, reqs);
            if result.outcome == ItemOutcome::Applied {
                restart = restart.max(t.restart);
                for setting in (self.system.live_settings)(t) {
                    if !applied_live.contains(&setting) {
                        applied_live.push(setting);
                    }
                }
            }
            let index = results.len();
            written.extend(targets.into_iter().map(|target| (index, target)));
            results.push(result);
        }

        let mut warnings = safety.warnings().to_vec();
        refresh_live(
            safety,
            &applied_live,
            self.system.refresh,
            &mut restart,
            &mut warnings,
        );

        if !written.is_empty() {
            (self.system.settle)();
            if let Err(e) =
                verify::after_apply(&self.system, safety, &written, &mut results, &mut warnings)
            {
                warn!(error = %e, "read-back after apply failed");
                warnings.push(format!(
                    "The changes could not be read back after they were applied ({e})."
                ));
            }
        }

        if !appx_names.is_empty() {
            results.extend(apply_appx(safety, &appx_names));
        }

        ApplyReport {
            dry_run: false,
            session_id: Some(safety.session_id()),
            restore_point: safety.restore_point().cloned(),
            results,
            warnings,
            restart,
        }
    }

    /// Applies every recommended item of `category` that is offered on this PC and not done
    /// yet: not applied, partly applied, or holding values an earlier version of Cairn set.
    pub fn apply_category(&self, category: Category, opts: &ApplyOptions) -> Result<ApplyReport> {
        let scan = self.scan()?;
        let ids: Vec<String> = scan
            .items
            .iter()
            .filter(|i| {
                i.category == category
                    && i.recommended
                    && i.unavailable.is_none()
                    && (matches!(i.state, ItemState::NotApplied | ItemState::Partial)
                        || !i.former.is_empty())
            })
            .map(|i| i.id.clone())
            .collect();
        self.apply(&ids, opts)
    }

    // ───────────────────────────── revert ─────────────────────────────

    /// Reverts the journal records that belong to `ids`. Unknown ids select nothing. The
    /// report's `restart` is the strongest restart need among the tweaks whose records
    /// were restored.
    pub fn revert(&self, ids: &[String], dry_run: bool) -> Result<RollbackReport> {
        let filter = self.revert_filter(ids)?;
        if filter.is_empty() {
            return Ok(RollbackReport {
                dry_run,
                ..Default::default()
            });
        }
        rollback::rollback_filtered(&self.journal, &filter, dry_run)
    }

    /// Reverts every item of `category`, including Appx packages this tool removed.
    pub fn revert_category(&self, category: Category, dry_run: bool) -> Result<RollbackReport> {
        let mut ids: Vec<String> = catalog::tweaks_in(category)
            .map(|t| t.id.to_string())
            .collect();
        if category == Category::Bloatware {
            for rec in self.journal.active_appx()? {
                ids.push(catalog::appx_item_id(package_name_from_family(
                    &rec.package_family,
                )));
            }
        }
        self.revert(&ids, dry_run)
    }

    /// Reverts every active journal record.
    pub fn revert_all(&self, dry_run: bool) -> Result<RollbackReport> {
        rollback::rollback_journal(&self.journal, dry_run)
    }

    /// Journal targets of `ids`: every target of each tweak, its former values included, and
    /// the recorded families of each removed bloatware package. Unknown ids add nothing.
    pub fn revert_filter(&self, ids: &[String]) -> Result<RollbackFilter> {
        let mut filter = RollbackFilter::default();
        let mut appx_journal = None;
        for id in ids {
            if let Some(t) = catalog::tweak(id) {
                add_tweak_targets(t, &mut filter);
            } else if let Some(name) = catalog::appx_name_from_item_id(id) {
                if appx_journal.is_none() {
                    appx_journal = Some(self.journal.active_appx()?);
                }
                for rec in appx_journal.as_deref().unwrap_or_default() {
                    if package_name_from_family(&rec.package_family).eq_ignore_ascii_case(name)
                        && !filter
                            .appx_families
                            .iter()
                            .any(|f| f.eq_ignore_ascii_case(&rec.package_family))
                    {
                        filter.appx_families.push(rec.package_family.clone());
                    }
                }
            }
        }
        Ok(filter)
    }
}

// ───────────────────────────── helpers ─────────────────────────────

/// The items one apply resolves from its ids: tweaks and bloatware package names to apply,
/// and the results of ids that resolve to nothing (or were withheld).
struct Selection {
    tweaks: Vec<&'static Tweak>,
    appx_names: Vec<String>,
    results: Vec<ItemResult>,
}

/// Resolves `ids` (compared ignoring ASCII case; repeats ignored) into tweaks and package
/// names; unknown ids and packages this tool does not remove fail.
fn select(ids: &[String]) -> Selection {
    let mut sel = Selection {
        tweaks: Vec::new(),
        appx_names: Vec::new(),
        results: Vec::new(),
    };
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.to_ascii_lowercase()) {
            continue;
        }
        if let Some(t) = catalog::tweak(id) {
            sel.tweaks.push(t);
        } else if let Some(name) = catalog::appx_name_from_item_id(id) {
            if catalog::bloat_entry_for(name).is_some() {
                sel.appx_names.push(name.to_string());
            } else {
                sel.results
                    .push(failed(id, "not a bloatware package this tool removes"));
            }
        } else {
            sel.results.push(failed(id, "unknown item"));
        }
    }
    sel
}

/// Adds every journal target of `t` to `filter`: its actions' targets and its former values.
fn add_tweak_targets(t: &Tweak, filter: &mut RollbackFilter) {
    for action in t.actions {
        match action {
            Action::Registry(r) => filter.registry.push(registry::target(r)),
            Action::Service(s) => filter.services.push(s.name.to_string()),
            Action::ScheduledTask(s) => filter.scheduled_tasks.push(s.path.to_string()),
            Action::Power(_) => filter.power = true,
        }
    }
    for f in t.former {
        filter.registry.push(registry::target(&f.action));
    }
}

/// Note on an installed bloatware package that still has an active removal record. The scan
/// cannot tell who installed it again, so the note names no one.
pub const REINSTALLED_NOTE: &str = "This app is installed again after Cairn removed it.";

/// The active journal records of a scan or a read-back, loaded once, keyed by lowercase
/// target. Where the journal holds several active records of one target, the oldest is
/// kept: it holds the baseline.
pub(crate) struct ActiveRecords {
    /// Keyed by hive, lowercase key path and lowercase value name.
    pub(crate) registry: HashMap<(Hive, String, String), RegistryRecord>,
    /// Keyed by lowercase service name.
    pub(crate) services: HashMap<String, ServiceRecord>,
    /// Keyed by lowercase task path.
    pub(crate) scheduled_tasks: HashMap<String, ScheduledTaskRecord>,
    pub(crate) power: Vec<PowerRecord>,
    pub(crate) appx: Vec<AppxRecord>,
    /// Ids of the registry records whose write never happened
    /// ([`Journal::unwritten_registry_ids`]).
    pub(crate) unwritten: HashSet<i64>,
}

impl ActiveRecords {
    pub(crate) fn load(journal: &Journal) -> Result<ActiveRecords> {
        let mut registry = HashMap::new();
        for r in journal.active_registry()? {
            let key = registry_key(r.hive, &r.key_path, &r.value_name);
            keep_oldest(&mut registry, key, r, |r| r.id);
        }
        let mut services = HashMap::new();
        for r in journal.active_services()? {
            keep_oldest(&mut services, r.name.to_ascii_lowercase(), r, |r| r.id);
        }
        let mut scheduled_tasks = HashMap::new();
        for r in journal.active_scheduled_tasks()? {
            keep_oldest(&mut scheduled_tasks, r.path.to_ascii_lowercase(), r, |r| {
                r.id
            });
        }
        Ok(ActiveRecords {
            registry,
            services,
            scheduled_tasks,
            power: journal.active_power()?,
            appx: journal.active_appx()?,
            unwritten: journal.unwritten_registry_ids()?,
        })
    }

    /// True when `read` shows `action`'s target at its value from before Cairn's change and
    /// the record's write never happened ([`Journal::unwritten_registry_ids`]): Cairn never
    /// changed the target, so it has not changed since either. Undo ends such a record without
    /// writing anything.
    pub(crate) fn never_changed(&self, action: &Action, read: &ReadBack) -> bool {
        let Action::Registry(r) = action else {
            return false;
        };
        self.registry
            .get(&registry_key(r.hive, r.path, r.name))
            .is_some_and(|rec| self.unwritten.contains(&rec.id))
            && verify::recorded_original(action, self).as_deref() == Some(read.found.as_str())
    }

    /// True when any action or former value of `t` has an active record.
    pub(crate) fn covers(&self, t: &Tweak) -> bool {
        t.actions.iter().any(|a| self.records(a))
            || t.former.iter().any(|f| self.former_record(f).is_some())
    }

    /// The active record of a former value, the oldest when there are several.
    fn former_record(&self, f: &Former) -> Option<&RegistryRecord> {
        self.registry
            .get(&registry_key(f.action.hive, f.action.path, f.action.name))
    }

    /// The former values of `t` that have an active record, with the value Undo or an apply
    /// restores and why this version no longer sets them.
    pub(crate) fn former_records(&self, t: &Tweak) -> Vec<FormerRecord> {
        t.former
            .iter()
            .filter_map(|f| {
                let rec = self.former_record(f)?;
                Some(FormerRecord {
                    target: registry::describe(&f.action),
                    original: original_text(rec),
                    reason: f.reason.to_string(),
                })
            })
            .collect()
    }

    /// True when the target of `action` has an active record.
    pub(crate) fn records(&self, action: &Action) -> bool {
        match action {
            Action::Registry(r) => self
                .registry
                .contains_key(&registry_key(r.hive, r.path, r.name)),
            Action::Service(s) => self.services.contains_key(&s.name.to_ascii_lowercase()),
            Action::ScheduledTask(s) => self
                .scheduled_tasks
                .contains_key(&s.path.to_ascii_lowercase()),
            Action::Power(_) => !self.power.is_empty(),
        }
    }

    /// Sets `held` on the scanned statuses of `t`'s recorded targets from their read-backs
    /// (`reads` pairs an action index with its read-back); targets without a record, and
    /// targets whose recorded write never happened ([`Self::never_changed`]), keep `None`.
    fn mark_held(&self, t: &Tweak, actions: &mut [ActionStatus], reads: &[(usize, ReadBack)]) {
        for (index, read) in reads {
            let recorded = t
                .actions
                .get(*index)
                .is_some_and(|a| self.records(a) && !self.never_changed(a, read));
            if let (true, Some(status)) = (recorded, actions.get_mut(*index)) {
                status.held = Some(read.held);
            }
        }
    }
}

/// Key of a registry target in [`ActiveRecords::registry`].
pub(crate) fn registry_key(hive: Hive, key_path: &str, value_name: &str) -> (Hive, String, String) {
    (
        hive,
        key_path.to_ascii_lowercase(),
        value_name.to_ascii_lowercase(),
    )
}

/// Stores `record` under `key` unless an older record (lower `id`) is stored there already.
fn keep_oldest<K: std::hash::Hash + Eq, R>(
    map: &mut HashMap<K, R>,
    key: K,
    record: R,
    id: fn(&R) -> i64,
) {
    match map.get(&key) {
        Some(kept) if id(kept) <= id(&record) => {}
        _ => {
            map.insert(key, record);
        }
    }
}

/// The value restoring `rec` puts back, as reports show it, or "not set" when the value did
/// not exist.
fn original_text(rec: &RegistryRecord) -> String {
    rec.original_decoded()
        .map_or_else(|| "not set".to_string(), |value| value.display())
}

/// Package Name part of a family name (`Name_PublisherId`). Package names cannot contain
/// underscores, so the last underscore separates the publisher id.
pub fn package_name_from_family(family: &str) -> &str {
    family.rsplit_once('_').map_or(family, |(name, _)| name)
}

pub(crate) fn action_label(action: &Action) -> String {
    match action {
        Action::Registry(r) => registry::describe(r),
        Action::Service(s) => services::describe(s),
        Action::ScheduledTask(s) => scheduled_tasks::describe(s),
        Action::Power(p) => power::describe(*p),
    }
}

/// Reads the state of a tweak; see [`read_tweak`].
fn scan_tweak(
    t: &'static Tweak,
    has_battery: bool,
    tasks: &TaskConnection,
    reqs: &Requirements,
    check_requirement: bool,
    warnings: &mut Vec<String>,
) -> ScanItem {
    read_tweak(t, has_battery, tasks, reqs, check_requirement, warnings).0
}

/// Reads the state of a tweak, each target once, and returns the item with the read-back of
/// every action that could be read (paired with its action index). With
/// `check_requirement`, a tweak that is retired, outside its Windows range or whose
/// requirement is known to be missing is reported unavailable with the reason as its note,
/// without reading anything; a check that fails adds a warning routed to the item
/// (`<id>: …`) and the state is read normally.
fn read_tweak(
    t: &'static Tweak,
    has_battery: bool,
    tasks: &TaskConnection,
    reqs: &Requirements,
    check_requirement: bool,
    warnings: &mut Vec<String>,
) -> (ScanItem, Vec<(usize, ReadBack)>) {
    let power_on_battery = has_battery && t.actions.iter().any(|a| matches!(a, Action::Power(_)));
    let item = |actions: Vec<ActionStatus>, state: ItemState, note: Option<String>| ScanItem {
        id: t.id.to_string(),
        kind: ItemKind::Tweak,
        category: t.category,
        title: t.title.to_string(),
        description: t.description.to_string(),
        risk: t.risk,
        recommended: t.default_on && !power_on_battery && t.retired.is_none(),
        state,
        restart: t.restart,
        actions,
        revertible: false,
        note,
        topic: Some(t.topic),
        unavailable: None,
        undo_limit: t.undo_limit.map(str::to_string),
        former: Vec::new(),
        mode_undo: t.default_on || FORMERLY_RECOMMENDED.contains(&t.id),
        drift: Vec::new(),
    };

    if check_requirement {
        match reqs.check_tweak(t) {
            Availability::Met => {}
            Availability::Missing(unmet) => {
                let actions = t
                    .actions
                    .iter()
                    .map(|a| {
                        ActionStatus::new(
                            ActionState::Unavailable,
                            format!("{}: {}", action_label(a), unmet.text),
                        )
                    })
                    .collect();
                let mut unavailable = item(actions, ItemState::Unavailable, Some(unmet.text));
                unavailable.unavailable = Some(unmet.kind);
                return (unavailable, Vec::new());
            }
            Availability::Unknown(e) => {
                warn!(item = t.id, error = %e, "cannot check the requirement");
                warnings.push(format!("{}: {e}", t.id));
            }
        }
    }

    let mut reads = Vec::new();
    let actions: Vec<ActionStatus> = t
        .actions
        .iter()
        .enumerate()
        .map(|(index, a)| match verify::action_read(a, tasks) {
            Ok((status, read)) => {
                reads.push((index, read));
                status
            }
            Err(e) => {
                warn!(item = t.id, error = %e, "cannot read state");
                warnings.push(format!("{}: cannot read {}: {e}", t.id, action_label(a)));
                ActionStatus::new(
                    ActionState::Unavailable,
                    format!("{}: {e}", action_label(a)),
                )
            }
        })
        .collect();
    let state = combine_states(&actions);
    (item(actions, state, None), reads)
}

/// Second sentence of the note of an item that is not offered on this PC but still has
/// active journal records.
pub(crate) const RECORDED_UNMET_NOTE: &str = "Undo removes the settings Cairn stored.";

/// Note of an item that is not offered on this PC (`availability`) but still has active
/// journal records: why it is not offered, and that Undo still restores what Cairn stored.
fn recorded_unmet_note(availability: &Availability, revertible: bool) -> Option<String> {
    match availability {
        Availability::Missing(unmet) if revertible => {
            Some(format!("{} {RECORDED_UNMET_NOTE}", unmet.text))
        }
        _ => None,
    }
}

/// Note of the GPU scheduling tweak: what the GPU runs with now when that differs from the
/// stored setting. `None` for every other tweak and while the requirement is not met.
fn gpu_scan_note(t: &Tweak, reqs: &Requirements) -> Option<String> {
    let req = t.requires.filter(|r| *r == Requirement::GpuScheduling)?;
    if reqs.check(req) != Availability::Met {
        return None;
    }
    let stored = match read_value(Hive::LocalMachine, GRAPHICS_DRIVERS, "HwSchMode") {
        Ok(Some(RegValue::Dword(v))) => Some(v),
        _ => None,
    };
    gpu_note(stored, reqs.gpu()?).map(str::to_string)
}

/// Pushes the stored values behind `settings` to the running session. A failed or refused
/// push keeps the stored values, which Windows reads at the next sign-in, so the report asks
/// for a sign-out. Audit rows that cannot be written become warnings: the values themselves
/// are already journaled.
fn refresh_live(
    safety: &Safety,
    settings: &[LiveSetting],
    refresh: fn(LiveSetting) -> Result<()>,
    restart: &mut RestartNeed,
    warnings: &mut Vec<String>,
) {
    for &setting in settings {
        let logged = match refresh(setting) {
            Ok(()) => safety.log_op(live::OP_REFRESH, setting.target(), "applied", None),
            Err(e) => {
                warn!(setting = setting.label(), error = %e, "live refresh failed");
                *restart = (*restart).max(RestartNeed::SignOut);
                warnings.push(format!(
                    "{} could not be updated in this session ({e}); they take effect after \
                     you sign out.",
                    setting.label()
                ));
                safety.log_op(
                    live::OP_REFRESH,
                    setting.target(),
                    "failed",
                    Some(&e.to_string()),
                )
            }
        };
        if let Err(e) = logged {
            warnings.push(format!(
                "the update of {} in this session could not be written to the audit log: {e}",
                setting.label()
            ));
        }
    }
}

fn combine_states(actions: &[ActionStatus]) -> ItemState {
    let applied = actions
        .iter()
        .filter(|a| a.state == ActionState::Applied)
        .count();
    let not_applied = actions
        .iter()
        .filter(|a| a.state == ActionState::NotApplied)
        .count();
    match (applied, not_applied) {
        (0, 0) => ItemState::Unavailable,
        (_, 0) => ItemState::Applied,
        (0, _) => ItemState::NotApplied,
        _ => ItemState::Partial,
    }
}

fn appx_item(
    name: &str,
    entry: &'static catalog::BloatPackage,
    state: ItemState,
    actions: Vec<ActionStatus>,
) -> ScanItem {
    let title = if entry.name.ends_with('*') {
        format!("{} ({name})", entry.title)
    } else {
        entry.title.to_string()
    };
    ScanItem {
        id: catalog::appx_item_id(name),
        kind: ItemKind::Appx,
        category: Category::Bloatware,
        title,
        description: entry.description.to_string(),
        risk: entry.risk,
        recommended: entry.default_on,
        state,
        restart: RestartNeed::None,
        actions,
        revertible: false,
        note: None,
        topic: None,
        unavailable: None,
        undo_limit: None,
        former: Vec::new(),
        mode_undo: entry.default_on,
        drift: Vec::new(),
    }
}

/// Mode status of `category`. Only recommended items offered on this PC count, so an item
/// that is not offered but still has records never makes a mode look on; an applied item
/// that still holds values an earlier version of Cairn set counts as not done, since applying
/// it restores them.
fn category_status(category: Category, items: &[ScanItem]) -> CategoryStatus {
    let in_cat: Vec<&ScanItem> = items.iter().filter(|i| i.category == category).collect();
    let recommended: Vec<&&ScanItem> = in_cat
        .iter()
        .filter(|i| i.recommended && i.unavailable.is_none() && i.state != ItemState::Unavailable)
        .collect();
    let recommended_applied = recommended
        .iter()
        .filter(|i| i.state == ItemState::Applied && i.former.is_empty())
        .count();
    CategoryStatus {
        category,
        items: in_cat.len(),
        applied: in_cat
            .iter()
            .filter(|i| i.state == ItemState::Applied)
            .count(),
        recommended: recommended.len(),
        recommended_applied,
        active: !recommended.is_empty() && recommended_applied == recommended.len(),
    }
}

fn failed(id: &str, reason: impl Into<String>) -> ItemResult {
    ItemResult::new(id, ItemOutcome::Failed, vec![reason.into()])
}

/// What a dry run reports for a scanned tweak: skipped with its note when it is not offered
/// on this PC; planned when anything would change, the restores of its former values first;
/// already applied otherwise.
fn plan_result(item: &ScanItem) -> ItemResult {
    if item.unavailable.is_some() {
        let note = item.note.clone().unwrap_or_default();
        return ItemResult::new(item.id.clone(), ItemOutcome::Skipped, vec![note]);
    }
    let mut details: Vec<String> = item
        .former
        .iter()
        .map(|f| {
            format!(
                "restore {} to {} (no longer part of this tweak)",
                f.target, f.original
            )
        })
        .collect();
    let outcome = match item.state {
        _ if !details.is_empty() => ItemOutcome::Planned,
        ItemState::Applied => ItemOutcome::AlreadyApplied,
        ItemState::Unavailable => ItemOutcome::Skipped,
        ItemState::NotApplied | ItemState::Partial => ItemOutcome::Planned,
    };
    details.extend(item.actions.iter().map(|a| a.detail.clone()));
    ItemResult::new(item.id.clone(), outcome, details)
}

/// True when the tweak writes a value under HKEY_CURRENT_USER, which has to land in the
/// signed-in user's profile: one of its actions, or a former value it restores.
pub(crate) fn touches_current_user(t: &Tweak) -> bool {
    t.actions
        .iter()
        .any(|a| matches!(a, Action::Registry(r) if r.hive == Hive::CurrentUser))
        || t.former.iter().any(|f| f.action.hive == Hive::CurrentUser)
}

fn skipped(id: &str, reason: &str) -> ItemResult {
    ItemResult::new(id, ItemOutcome::Skipped, vec![reason.to_string()])
}

/// When a per-user item is selected and `check` fails, moves every per-user item (tweaks
/// with an HKCU value and Store packages) from the selection into `results` as skipped
/// with the check's error. `check` runs at most once.
fn withhold_per_user(
    tweaks: &mut Vec<&'static Tweak>,
    appx_names: &mut Vec<String>,
    results: &mut Vec<ItemResult>,
    check: impl FnOnce() -> Result<()>,
) {
    if appx_names.is_empty() && !tweaks.iter().any(|t| touches_current_user(t)) {
        return;
    }
    let Err(e) = check() else {
        return;
    };
    let reason = e.to_string();
    warn!(error = %reason, "per-user items withheld");
    tweaks.retain(|t| {
        let per_user = touches_current_user(t);
        if per_user {
            results.push(skipped(t.id, &reason));
        }
        !per_user
    });
    results.extend(
        appx_names
            .drain(..)
            .map(|name| skipped(&catalog::appx_item_id(&name), &reason)),
    );
}

/// Moves every tweak that is not offered on this PC (retired, outside its Windows range or
/// with a requirement known to be missing) from the selection into `results` as skipped with
/// the reason. A check that fails keeps its tweak selected.
fn withhold_unavailable(
    tweaks: &mut Vec<&'static Tweak>,
    results: &mut Vec<ItemResult>,
    reqs: &Requirements,
) {
    tweaks.retain(|t| match reqs.check_tweak(t) {
        Availability::Missing(unmet) => {
            results.push(skipped(t.id, &unmet.text));
            false
        }
        _ => true,
    });
}

/// What restoring a tweak's former values did.
#[derive(Default)]
struct FormerRestore {
    /// One line per former value with an active record.
    details: Vec<String>,
    /// The restored values with their recorded originals, for the read-back.
    written: Vec<WrittenTarget>,
    restored: bool,
    failed: bool,
}

/// Restores the former values of `t` that have an active record, each to the original its
/// oldest record holds, like the registry group of a rollback but under `safety`'s own
/// session, and marks their records reverted. A value that cannot be restored keeps its record
/// active and fails the tweak.
fn restore_former(safety: &Safety, t: &Tweak) -> FormerRestore {
    let mut out = FormerRestore::default();
    if t.former.is_empty() {
        return out;
    }
    let records = match safety.journal().active_registry() {
        Ok(records) => records,
        Err(e) => {
            out.failed = true;
            out.details.extend(t.former.iter().map(|f| {
                format!(
                    "{}: could not be restored ({e})",
                    registry::describe(&f.action)
                )
            }));
            return out;
        }
    };
    for f in t.former {
        let target = registry::target(&f.action);
        let Some(oldest) = records
            .iter()
            .filter(|rec| is_record_of(rec, &target))
            .min_by_key(|rec| rec.id)
        else {
            continue;
        };
        let filter = RollbackFilter {
            registry: vec![target.clone()],
            ..Default::default()
        };
        let failure = match rollback::restore_registry_in(safety, &filter) {
            Ok(lines) if lines.is_empty() => continue,
            Ok(lines) => lines
                .into_iter()
                .find_map(|line| line.err())
                .map(|failure| failure.error),
            Err(e) => Some(e.to_string()),
        };
        let label = registry::describe(&f.action);
        match failure {
            None => {
                out.restored = true;
                out.details.push(format!(
                    "{label}: restored to {} (no longer part of this tweak)",
                    original_text(oldest)
                ));
                out.written.push(WrittenTarget::Former {
                    target,
                    original: oldest.original.clone(),
                });
            }
            Some(error) => {
                out.failed = true;
                out.details
                    .push(format!("{label}: could not be restored ({error})"));
            }
        }
    }
    out
}

/// Whether `rec` records the registry value `target` (hive exact, path and name ignoring
/// ASCII case).
fn is_record_of(rec: &RegistryRecord, target: &RegistryTarget) -> bool {
    rec.hive == target.hive
        && rec.key_path.eq_ignore_ascii_case(&target.key_path)
        && rec.value_name.eq_ignore_ascii_case(&target.value_name)
}

/// Applies every action of a tweak, continuing past failures so one broken value does not
/// block the rest. Applied wins over already-applied; any failure marks the item failed.
/// A tweak that is not offered on this PC is skipped first, before anything is checked or
/// recorded. A tweak with an HKCU value is skipped whole, with nothing changed, when
/// [`Safety::ensure_interactive_user`] fails, so it is never left half applied. The former
/// values with an active record are restored before the actions; a restore makes the tweak
/// applied even when every action was already in place. Scheduled tasks are changed through
/// `tasks`, which connects to Task Scheduler on first use. The result lists every target this
/// apply wrote, restored former values included, for the read-back.
fn apply_tweak(
    safety: &Safety,
    t: &'static Tweak,
    tasks: &TaskConnection,
    reqs: &Requirements,
) -> TweakApply {
    let alone = |result: ItemResult| TweakApply {
        result,
        written: Vec::new(),
    };
    if let Availability::Missing(unmet) = reqs.check_tweak(t) {
        return alone(skipped(t.id, &unmet.text));
    }
    if touches_current_user(t) {
        if let Err(e) = safety.ensure_interactive_user() {
            return alone(skipped(t.id, &e.to_string()));
        }
    }
    let former = restore_former(safety, t);
    let mut details = former.details;
    let mut written = former.written;
    let mut any_applied = former.restored;
    let mut any_failed = former.failed;
    let mut all_skipped = !(former.restored || former.failed);

    for action in t.actions {
        let label = action_label(action);
        let outcome = match action {
            Action::Registry(r) => registry::apply(safety, r),
            Action::Service(s) => services::apply(safety, s).map(|o| {
                details.extend(o.notes);
                o.outcome
            }),
            Action::ScheduledTask(s) => scheduled_tasks::apply_in(safety, tasks, s),
            Action::Power(p) => power::apply(safety, *p),
        };
        match outcome {
            Ok(MutationOutcome::Applied) => {
                any_applied = true;
                all_skipped = false;
                details.push(format!("{label}: applied"));
                written.push(WrittenTarget::Action(action));
            }
            Ok(MutationOutcome::AlreadyInDesiredState) => {
                all_skipped = false;
                details.push(format!("{label}: already set"));
            }
            Ok(MutationOutcome::Skipped(reason)) => {
                details.push(format!("{label}: skipped ({reason})"));
            }
            Err(e) => {
                any_failed = true;
                all_skipped = false;
                details.push(format!("{label}: failed ({e})"));
            }
        }
    }

    let outcome = if any_failed {
        ItemOutcome::Failed
    } else if any_applied {
        ItemOutcome::Applied
    } else if all_skipped {
        ItemOutcome::Skipped
    } else {
        ItemOutcome::AlreadyApplied
    };
    TweakApply {
        result: ItemResult::new(t.id, outcome, details),
        written,
    }
}

fn apply_appx(safety: &Safety, names: &[String]) -> Vec<ItemResult> {
    let installed = match appx::inventory() {
        Ok(list) => list,
        Err(e) => {
            return names
                .iter()
                .map(|n| failed(&catalog::appx_item_id(n), format!("inventory failed: {e}")))
                .collect();
        }
    };

    let mut packages = Vec::new();
    let mut owners = Vec::new();
    let mut results: Vec<ItemResult> = Vec::new();
    for name in names {
        let id = catalog::appx_item_id(name);
        let matches: Vec<&AppxPackage> = installed
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
            .collect();
        if matches.is_empty() {
            results.push(ItemResult::new(
                id,
                ItemOutcome::AlreadyApplied,
                vec!["not installed".into()],
            ));
            continue;
        }
        let slot = results.len();
        results.push(ItemResult::new(id, ItemOutcome::AlreadyApplied, Vec::new()));
        for p in matches {
            packages.push(p.clone());
            owners.push(slot);
        }
    }

    if packages.is_empty() {
        return results;
    }
    match appx::remove(safety, &packages) {
        Ok(removals) => {
            for (removal, &slot) in removals.iter().zip(&owners) {
                let item = &mut results[slot];
                match (&removal.outcome, &removal.error) {
                    (_, Some(err)) => {
                        item.outcome = ItemOutcome::Failed;
                        item.details
                            .push(format!("{}: failed ({err})", removal.full_name));
                    }
                    (MutationOutcome::Applied, None) => {
                        if item.outcome != ItemOutcome::Failed {
                            item.outcome = ItemOutcome::Applied;
                        }
                        item.details.push(format!("{}: removed", removal.full_name));
                    }
                    (MutationOutcome::Skipped(reason), None) => {
                        if item.outcome == ItemOutcome::AlreadyApplied {
                            item.outcome = ItemOutcome::Skipped;
                        }
                        item.details
                            .push(format!("{}: skipped ({reason})", removal.full_name));
                    }
                    (MutationOutcome::AlreadyInDesiredState, None) => {
                        item.details
                            .push(format!("{}: not installed", removal.full_name));
                    }
                }
            }
        }
        Err(e) => {
            for &slot in &owners {
                let item = &mut results[slot];
                item.outcome = ItemOutcome::Failed;
                if item.details.is_empty() {
                    item.details.push(format!("removal failed: {e}"));
                }
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debloat::catalog::{
        reg_dword, Build, Editions, RegData, RegistryAction, Topic, WindowsRange, ANY_WINDOWS,
    };
    use crate::debloat::requirements::{
        EditionClass, GpuScheduling, OfficeProduct, RecallState, OFFICE_NOT_INSTALLED,
    };
    use crate::safety::state_log::{JournalTable, NewRegistryRecord, NewScheduledTaskRecord};
    use crate::safety::test_safety;
    use crate::win::registry::{delete_sandbox_tree, exists, Key};

    fn status(state: ActionState) -> ActionStatus {
        ActionStatus::new(state, "")
    }

    /// Probes that report every requirement as met.
    const ALL_MET: Probes = Probes {
        office: || Ok(true),
        office_product: || Ok(OfficeProduct::Business),
        edge: || Ok(true),
        gpu: || {
            Ok(GpuScheduling {
                supported: true,
                enabled: true,
            })
        },
        windows: || Err(Error::Other("not read in this test".into())),
        recall: || Ok(RecallState::Installed),
        packages: |_| Ok(vec![[11, 2507, 2, 0]]),
        system_ai_models: || Ok(true),
    };

    /// Probes of a PC without Office: `office` and the Office product both report none, so the
    /// Office-policy tweaks are not offered.
    const NO_OFFICE: Probes = Probes {
        office: || Ok(false),
        office_product: || Ok(OfficeProduct::None),
        ..ALL_MET
    };

    /// A live-settings push that must not run: the tweaks it is given feed no live setting.
    fn no_refresh(setting: LiveSetting) -> Result<()> {
        panic!("{setting:?} was pushed to the session")
    }

    /// Windows 11 Home 26H2 as the test PC reports it.
    fn home_26h2() -> WindowsFacts {
        WindowsFacts {
            build: catalog::Build::new(26300, 9457),
            family: 26100,
            version: Some("26H2".into()),
            edition_id: "Core".into(),
            edition: EditionClass::Home,
            name: "Windows 11 Home".into(),
        }
    }

    /// Reads back at once instead of waiting.
    fn no_settle() {}

    /// No adapter of a DNS record exists.
    fn no_dns(_rec: &DnsRecord) -> Result<Option<String>> {
        Ok(None)
    }

    /// A test system with `probes` and `refresh`, the catalog's live settings, no wait before
    /// a read-back and no network adapters.
    fn test_system(probes: Probes, refresh: fn(LiveSetting) -> Result<()>) -> EngineSystem {
        EngineSystem {
            probes,
            refresh,
            live_settings: live::settings_of,
            settle: no_settle,
            dns_now: no_dns,
        }
    }

    /// Applies `sel` under `safety` with the requirement probes of `engine`.
    fn apply_own(engine: &Engine, safety: &Safety, sel: Selection) -> ApplyReport {
        engine.apply_selection(safety, sel, &engine.requirements())
    }

    /// Windows 11 Home 22H2.
    fn home_22h2() -> WindowsFacts {
        WindowsFacts {
            build: catalog::Build::new(22621, 4890),
            family: 22621,
            version: Some("22H2".into()),
            edition_id: "Core".into(),
            edition: EditionClass::Home,
            name: "Windows 11 Home".into(),
        }
    }

    /// Windows 11 Pro 26H2.
    fn pro_26h2() -> WindowsFacts {
        WindowsFacts {
            edition_id: "Professional".into(),
            edition: EditionClass::Pro,
            name: "Windows 11 Pro".into(),
            ..home_26h2()
        }
    }

    /// Every requirement met, on Windows 11 Home 22H2, Home 26H2 and Pro 26H2.
    const HOME_22H2: Probes = Probes {
        windows: || Ok(home_22h2()),
        ..ALL_MET
    };
    const HOME_26H2: Probes = Probes {
        windows: || Ok(home_26h2()),
        ..ALL_MET
    };
    const PRO_26H2: Probes = Probes {
        windows: || Ok(pro_26h2()),
        ..ALL_MET
    };

    /// Removes a sandbox key when dropped, so a failing test leaves nothing behind.
    struct SandboxKey(&'static str);

    impl Drop for SandboxKey {
        fn drop(&mut self) {
            let _ = delete_sandbox_tree(self.0);
        }
    }

    /// Never written: the tests that use it only read or skip it.
    const OFFICE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitOffice";
    /// Written by the one test that applies an Office-like tweak.
    const OFFICE_APPLY_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitOfficeApply";

    /// A tweak that needs Office, with two values in the self-test sandbox.
    static OFFICE_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.office",
        category: Category::Privacy,
        topic: Topic::Office,
        title: "Sandbox Office policies",
        description: "Two values under the self-test key that stand in for Office policies.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: OFFICE_SANDBOX,
                name: "First",
                data: RegData::Dword(1),
            }),
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: OFFICE_SANDBOX,
                name: "Second",
                data: RegData::Dword(2),
            }),
        ],
        former: &[],
    };

    static OFFICE_APPLY_TWEAK: Tweak = Tweak {
        id: "selftest.office_apply",
        category: Category::Privacy,
        topic: Topic::Office,
        title: "Sandbox Office policies to apply",
        description: "Two values under the self-test key that stand in for Office policies.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: OFFICE_APPLY_SANDBOX,
                name: "First",
                data: RegData::Dword(1),
            }),
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: OFFICE_APPLY_SANDBOX,
                name: "Second",
                data: RegData::Dword(2),
            }),
        ],
        former: &[],
    };

    /// Written by the test of the live-settings push.
    const LIVE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitLive";

    /// Two tweaks that need Office, with one value each in the self-test sandbox.
    static LIVE_SANDBOX_TWEAKS: [Tweak; 2] = [
        Tweak {
            id: "selftest.live_first",
            category: Category::Gaming,
            topic: Topic::Input,
            title: "Sandbox live value, first",
            description: "A value under the self-test key that stands in for a mouse value.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: Some(Requirement::Office),
            windows: ANY_WINDOWS,
            retired: None,
            undo_limit: None,
            actions: &[Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: LIVE_SANDBOX,
                name: "First",
                data: RegData::Dword(1),
            })],
            former: &[],
        },
        Tweak {
            id: "selftest.live_second",
            category: Category::Gaming,
            topic: Topic::Input,
            title: "Sandbox live value, second",
            description: "A value under the self-test key that stands in for a mouse value.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: Some(Requirement::Office),
            windows: ANY_WINDOWS,
            retired: None,
            undo_limit: None,
            actions: &[Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: LIVE_SANDBOX,
                name: "Second",
                data: RegData::Dword(2),
            })],
            former: &[],
        },
    ];

    /// Every audit row as (operation, outcome), oldest first.
    fn op_rows(journal: &Journal) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .map(|op| (op.op, op.outcome))
            .collect();
        rows.reverse();
        rows
    }

    fn outcomes(report: &ApplyReport) -> Vec<ItemOutcome> {
        report.results.iter().map(|r| r.outcome).collect()
    }

    fn refresh_rows(journal: &Journal) -> Vec<(String, String, Option<String>)> {
        journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|op| op.op == live::OP_REFRESH)
            .map(|op| (op.target, op.outcome, op.detail))
            .collect()
    }

    #[test]
    fn unmet_requirement_makes_the_tweak_unavailable_with_a_note() {
        let text = Requirement::Office.missing_text();
        let reqs = Requirements::new(NO_OFFICE);
        let mut warnings = Vec::new();
        let item = scan_tweak(
            &OFFICE_SANDBOX_TWEAK,
            false,
            &TaskConnection::new(),
            &reqs,
            true,
            &mut warnings,
        );
        assert_eq!(item.state, ItemState::Unavailable);
        assert_eq!(item.note.as_deref(), Some(text));
        assert_eq!(item.unavailable, Some(Unavailable::NotOnThisPc));
        assert_eq!(item.actions.len(), 2);
        for action in &item.actions {
            assert_eq!(action.state, ActionState::Unavailable);
            assert_eq!(action.held, None);
            assert!(
                action.detail.ends_with(&format!(": {text}")),
                "{}",
                action.detail
            );
        }
        assert_eq!(item.recommended, OFFICE_SANDBOX_TWEAK.default_on);
        assert_eq!(item.topic, Some(Topic::Office));
        assert!(
            item.mode_undo,
            "a recommended tweak is restored with its mode"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!exists(Hive::CurrentUser, OFFICE_SANDBOX).unwrap());
    }

    #[test]
    fn unmet_requirement_skips_apply_before_anything_is_recorded() {
        let text = Requirement::Office.missing_text();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        // The requirement is checked before the account: the reason is the missing product.
        safety.assume_other_user();
        let TweakApply { result, written } = apply_tweak(
            &safety,
            &OFFICE_SANDBOX_TWEAK,
            &TaskConnection::new(),
            &Requirements::new(NO_OFFICE),
        );
        assert!(written.is_empty());
        assert_eq!(result.outcome, ItemOutcome::Skipped);
        assert_eq!(result.details, vec![text.to_string()]);
        let summary = journal.summary().unwrap();
        assert_eq!(summary.registry_total, 0, "nothing journaled");
        assert!(journal.ops(10).unwrap().is_empty(), "nothing logged");
        assert!(!exists(Hive::CurrentUser, OFFICE_SANDBOX).unwrap());
    }

    #[test]
    fn failed_requirement_check_warns_and_reads_the_state() {
        let reqs = Requirements::new(Probes {
            office: || Err(Error::Other("probe".into())),
            ..ALL_MET
        });
        let mut warnings = Vec::new();
        let item = scan_tweak(
            &OFFICE_SANDBOX_TWEAK,
            false,
            &TaskConnection::new(),
            &reqs,
            true,
            &mut warnings,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(
                "selftest.office: cannot check whether Microsoft Office is installed: "
            ),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].ends_with("probe"), "{}", warnings[0]);
        assert_eq!(item.state, ItemState::NotApplied, "{:?}", item.actions);
        assert_eq!(item.note, None);
        assert_eq!(item.unavailable, None);
        assert_eq!(
            recorded_unmet_note(&reqs.check_tweak(&OFFICE_SANDBOX_TWEAK), true),
            None
        );
    }

    /// The note `scan` gives an item from the two requirement notes, as for a tweak without
    /// drift.
    fn requirement_note(t: &Tweak, reqs: &Requirements, revertible: bool) -> Option<String> {
        recorded_unmet_note(&reqs.check_tweak(t), revertible).or_else(|| gpu_scan_note(t, reqs))
    }

    #[test]
    fn revertible_items_skip_the_requirement_check() {
        let text = Requirement::Office.missing_text();
        let reqs = Requirements::new(NO_OFFICE);
        let mut warnings = Vec::new();
        let item = scan_tweak(
            &OFFICE_SANDBOX_TWEAK,
            false,
            &TaskConnection::new(),
            &reqs,
            false,
            &mut warnings,
        );
        assert_eq!(item.state, ItemState::NotApplied, "read normally");
        assert_eq!(item.note, None);
        assert!(warnings.is_empty());
        assert_eq!(
            requirement_note(&OFFICE_SANDBOX_TWEAK, &reqs, true),
            Some(format!("{text} Undo removes the settings Cairn stored."))
        );
        assert_eq!(requirement_note(&OFFICE_SANDBOX_TWEAK, &reqs, false), None);
        assert_eq!(
            requirement_note(&OFFICE_SANDBOX_TWEAK, &Requirements::new(ALL_MET), true),
            None
        );
        assert_eq!(requirement_note(&SANDBOX_TWEAK, &reqs, true), None);
    }

    #[test]
    fn gpu_note_comes_from_the_driver_answer() {
        let gpu = catalog::tweak("gaming.gpu_scheduling").unwrap();
        let unsupported = Requirements::new(Probes {
            gpu: || Ok(GpuScheduling::default()),
            ..ALL_MET
        });
        assert_eq!(
            requirement_note(gpu, &unsupported, true),
            Some(format!(
                "{} Undo removes the settings Cairn stored.",
                Requirement::GpuScheduling.missing_text()
            ))
        );
        assert_eq!(requirement_note(gpu, &unsupported, false), None);
        assert_eq!(gpu_scan_note(gpu, &unsupported), None);
        let failed = Requirements::new(Probes {
            gpu: || Err(Error::Other("no adapters".into())),
            ..ALL_MET
        });
        assert_eq!(requirement_note(gpu, &failed, false), None);
        // With support, the note depends on the stored HwSchMode, which is read-only here.
        let stored = match read_value(Hive::LocalMachine, GRAPHICS_DRIVERS, "HwSchMode") {
            Ok(Some(RegValue::Dword(v))) => Some(v),
            _ => None,
        };
        let running = Requirements::new(ALL_MET);
        assert_eq!(
            requirement_note(gpu, &running, false),
            gpu_note(stored, running.gpu().unwrap()).map(str::to_string)
        );
        assert_eq!(gpu_scan_note(&SANDBOX_TWEAK, &running), None);
    }

    #[test]
    fn refresh_live_logs_applied_and_keeps_the_restart() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let mut restart = RestartNeed::None;
        let mut warnings = Vec::new();
        refresh_live(
            &safety,
            &[LiveSetting::Mouse],
            |_| Ok(()),
            &mut restart,
            &mut warnings,
        );
        assert_eq!(restart, RestartNeed::None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            refresh_rows(&journal),
            vec![(
                LiveSetting::Mouse.target().to_string(),
                "applied".to_string(),
                None
            )]
        );

        // Nothing to push writes nothing.
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        refresh_live(&safety, &[], no_refresh, &mut restart, &mut warnings);
        assert!(refresh_rows(&journal).is_empty());
    }

    #[test]
    fn failed_live_refresh_asks_for_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let mut restart = RestartNeed::Explorer;
        let mut warnings = vec!["earlier warning".to_string()];
        refresh_live(
            &safety,
            &[LiveSetting::Mouse],
            |_| Err(Error::Other("refused".into())),
            &mut restart,
            &mut warnings,
        );
        assert_eq!(restart, RestartNeed::SignOut);
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings[1].starts_with("Mouse settings could not be updated in this session"),
            "{}",
            warnings[1]
        );
        assert!(warnings[1].contains("refused"), "{}", warnings[1]);
        assert_eq!(
            refresh_rows(&journal),
            vec![(
                LiveSetting::Mouse.target().to_string(),
                "failed".to_string(),
                Some("refused".to_string())
            )]
        );

        // A stronger restart need is kept.
        let mut restart = RestartNeed::Restart;
        refresh_live(
            &safety,
            &[LiveSetting::Mouse],
            |_| Err(Error::Other("refused".into())),
            &mut restart,
            &mut Vec::new(),
        );
        assert_eq!(restart, RestartNeed::Restart);
    }

    #[test]
    fn apply_passes_the_requirements_to_the_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::with_system(journal.clone(), test_system(NO_OFFICE, no_refresh));
        let report = engine
            .apply(
                &["privacy.office_telemetry".to_string()],
                &ApplyOptions {
                    restore_point: RestorePointPolicy::Skip,
                    dry_run: true,
                },
            )
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.session_id, None);
        assert_eq!(report.results.len(), 1);
        let result = &report.results[0];
        assert_eq!(result.outcome, ItemOutcome::Skipped, "{:?}", result.details);
        // NO_OFFICE reports no Office product at all, which has its own sentence.
        let text = OFFICE_NOT_INSTALLED;
        assert!(!result.details.is_empty());
        assert!(
            result.details.iter().all(|d| d.ends_with(text)),
            "{:?}",
            result.details
        );
        assert_eq!(journal.summary().unwrap().sessions, 0);
    }

    #[test]
    fn apply_selection_skips_unmet_requirements_and_applies_met_ones() {
        let _cleanup = SandboxKey(OFFICE_APPLY_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let selection = || Selection {
            tweaks: vec![&OFFICE_APPLY_TWEAK],
            appx_names: Vec::new(),
            results: Vec::new(),
        };

        let without = Engine::with_system(journal.clone(), test_system(NO_OFFICE, no_refresh));
        let safety = test_safety(journal.clone(), "engine unit", false);
        let report = apply_own(&without, &safety, selection());
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].outcome, ItemOutcome::Skipped);
        assert_eq!(
            report.results[0].details,
            vec![Requirement::Office.missing_text().to_string()]
        );
        assert_eq!(journal.summary().unwrap().registry_total, 0);
        assert!(!exists(Hive::CurrentUser, OFFICE_APPLY_SANDBOX).unwrap());

        let with = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let report = apply_own(&with, &safety, selection());
        assert_eq!(
            report.results[0].outcome,
            ItemOutcome::Applied,
            "{:?}",
            report.results[0].details
        );
        assert_eq!(report.restart, RestartNeed::None);
        assert_eq!(journal.summary().unwrap().registry_total, 2);
        assert!(refresh_rows(&journal).is_empty(), "no live setting was fed");
    }

    #[test]
    fn apply_opens_no_session_when_only_unavailable_tweaks_are_selected() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::with_system(
            journal.clone(),
            test_system(
                Probes {
                    edge: || Ok(false),
                    ..ALL_MET
                },
                no_refresh,
            ),
        );
        let options = ApplyOptions {
            restore_point: RestorePointPolicy::Skip,
            dry_run: false,
        };
        let text = Requirement::Edge.missing_text().to_string();
        let view = |report: &ApplyReport| -> Vec<(String, ItemOutcome, Vec<String>)> {
            report
                .results
                .iter()
                .map(|r| (r.id.clone(), r.outcome, r.details.clone()))
                .collect()
        };

        // The tweak writes machine-wide values, so the accounts are not compared, and the
        // refusal comes before the elevation check: the result is the same elevated or not.
        // Should the refusal ever be missing, an unelevated run ends with NotElevated and an
        // elevated one is still skipped by apply_tweak, so nothing is written either way.
        let report = engine
            .apply(&["privacy.edge_telemetry".to_string()], &options)
            .unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.session_id, None);
        assert!(report.restore_point.is_none());
        assert_eq!(
            view(&report),
            vec![(
                "privacy.edge_telemetry".to_string(),
                ItemOutcome::Skipped,
                vec![text.clone()]
            )]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.restart, RestartNeed::None);

        // Ids that resolve to nothing next to it change nothing about that.
        let ids = ["selftest.nothing", "privacy.edge_shopping"].map(String::from);
        let report = engine.apply(&ids, &options).unwrap();
        assert_eq!(report.session_id, None);
        assert_eq!(
            view(&report),
            vec![
                (
                    "selftest.nothing".to_string(),
                    ItemOutcome::Failed,
                    vec!["unknown item".to_string()]
                ),
                (
                    "privacy.edge_shopping".to_string(),
                    ItemOutcome::Skipped,
                    vec![text]
                ),
            ]
        );

        assert_eq!(journal.summary().unwrap().sessions, 0);
        assert!(journal.ops(10).unwrap().is_empty(), "nothing logged");
    }

    #[test]
    fn only_tweaks_with_a_missing_requirement_are_withheld() {
        let gpu = catalog::tweak("gaming.gpu_scheduling").unwrap();
        let edge = catalog::tweak("privacy.edge_telemetry").unwrap();
        let reqs = Requirements::new(Probes {
            office: || Ok(false),
            gpu: || Err(Error::Other("probe".into())),
            ..ALL_MET
        });
        let mut tweaks: Vec<&'static Tweak> = vec![
            &OFFICE_SANDBOX_TWEAK,
            &SANDBOX_TWEAK,
            gpu,
            &OFFICE_APPLY_TWEAK,
            edge,
        ];
        let mut results = vec![failed("selftest.nothing", "unknown item")];
        withhold_unavailable(&mut tweaks, &mut results, &reqs);

        let kept: Vec<&str> = tweaks.iter().map(|t| t.id).collect();
        assert_eq!(
            kept,
            vec![SANDBOX_TWEAK.id, gpu.id, edge.id],
            "no requirement, a failed check and a met requirement stay selected"
        );
        let text = Requirement::Office.missing_text().to_string();
        let withheld: Vec<(&str, ItemOutcome, &[String])> = results
            .iter()
            .map(|r| (r.id.as_str(), r.outcome, r.details.as_slice()))
            .collect();
        let missing = [text];
        let unknown = ["unknown item".to_string()];
        assert_eq!(
            withheld,
            vec![
                ("selftest.nothing", ItemOutcome::Failed, &unknown[..]),
                (OFFICE_SANDBOX_TWEAK.id, ItemOutcome::Skipped, &missing[..]),
                (OFFICE_APPLY_TWEAK.id, ItemOutcome::Skipped, &missing[..]),
            ]
        );
    }

    #[test]
    fn applied_tweaks_push_their_live_settings_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static PUSHES: AtomicUsize = AtomicUsize::new(0);
        fn count_push(setting: LiveSetting) -> Result<()> {
            assert_eq!(setting, LiveSetting::Mouse);
            PUSHES.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let pushes = || PUSHES.load(Ordering::SeqCst);

        let _cleanup = SandboxKey(LIVE_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        // Both sandbox tweaks stand in for tweaks that feed the mouse setting.
        let engine = |probes: Probes, refresh: fn(LiveSetting) -> Result<()>| {
            Engine::with_system(
                journal.clone(),
                EngineSystem {
                    live_settings: |_| vec![LiveSetting::Mouse],
                    ..test_system(probes, refresh)
                },
            )
        };
        let selection = || Selection {
            tweaks: LIVE_SANDBOX_TWEAKS.iter().collect(),
            appx_names: Vec::new(),
            results: Vec::new(),
        };
        let row = |op: &str, outcome: &str| (op.to_string(), outcome.to_string());
        let pushed = (
            LiveSetting::Mouse.target().to_string(),
            "applied".to_string(),
            None,
        );

        // Skipped tweaks push nothing.
        let report = apply_own(&engine(NO_OFFICE, count_push), &safety, selection());
        assert_eq!(outcomes(&report), vec![ItemOutcome::Skipped; 2]);
        assert_eq!(pushes(), 0);
        assert!(op_rows(&journal).is_empty());

        // Two applied tweaks that feed one setting push it once, after their values are
        // written.
        let report = apply_own(&engine(ALL_MET, count_push), &safety, selection());
        assert_eq!(
            outcomes(&report),
            vec![ItemOutcome::Applied; 2],
            "{:?}",
            report.results
        );
        assert_eq!(pushes(), 1);
        assert_eq!(report.restart, RestartNeed::None);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(
            op_rows(&journal),
            vec![
                row("set_registry_value", "applied"),
                row("set_registry_value", "applied"),
                row(live::OP_REFRESH, "applied"),
            ]
        );
        assert_eq!(refresh_rows(&journal), vec![pushed.clone()]);

        // Values that are already in place are not pushed again.
        let report = apply_own(&engine(ALL_MET, count_push), &safety, selection());
        assert_eq!(outcomes(&report), vec![ItemOutcome::AlreadyApplied; 2]);
        assert_eq!(pushes(), 1);
        assert_eq!(refresh_rows(&journal), vec![pushed.clone()]);

        // A push that fails reaches the report: the values are stored, so signing out
        // applies them.
        delete_sandbox_tree(LIVE_SANDBOX).unwrap();
        let failing = engine(ALL_MET, |_| Err(Error::Other("refused".into())));
        let report = apply_own(&failing, &safety, selection());
        assert_eq!(
            outcomes(&report),
            vec![ItemOutcome::Applied; 2],
            "{:?}",
            report.results
        );
        assert_eq!(report.restart, RestartNeed::SignOut);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].starts_with("Mouse settings could not be updated in this session"),
            "{}",
            report.warnings[0]
        );
        assert_eq!(pushes(), 1);
        // Newest first.
        assert_eq!(
            refresh_rows(&journal),
            vec![
                (
                    LiveSetting::Mouse.target().to_string(),
                    "failed".to_string(),
                    Some("refused".to_string())
                ),
                pushed,
            ]
        );
    }

    /// Written by the test of the wait before the read-back.
    const SETTLE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitSettle";

    static SETTLE_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.settle",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Sandbox value read back",
        description: "A value under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Registry(RegistryAction {
            hive: Hive::CurrentUser,
            path: SETTLE_SANDBOX,
            name: "Value",
            data: RegData::Dword(1),
        })],
        former: &[],
    };

    #[test]
    fn apply_settles_once_when_it_wrote_something() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SETTLES: AtomicUsize = AtomicUsize::new(0);
        fn count_settle() {
            SETTLES.fetch_add(1, Ordering::SeqCst);
        }
        let settles = || SETTLES.load(Ordering::SeqCst);

        let _cleanup = SandboxKey(SETTLE_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let engine = Engine::with_system(
            journal.clone(),
            EngineSystem {
                settle: count_settle,
                ..test_system(ALL_MET, no_refresh)
            },
        );
        let selection = || Selection {
            tweaks: vec![&SETTLE_SANDBOX_TWEAK],
            appx_names: Vec::new(),
            results: vec![failed("selftest.nothing", "unknown item")],
        };

        let report = apply_own(&engine, &safety, selection());
        assert_eq!(
            outcomes(&report),
            vec![ItemOutcome::Failed, ItemOutcome::Applied],
            "{:?}",
            report.results
        );
        assert_eq!(settles(), 1, "one wait for the apply that wrote a value");
        assert!(report.results.iter().all(|r| r.not_kept.is_empty()));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // Nothing written: no wait.
        let report = apply_own(&engine, &safety, selection());
        assert_eq!(outcomes(&report)[1], ItemOutcome::AlreadyApplied);
        assert_eq!(settles(), 1);

        // A dry run never waits, even when it plans a write.
        let report = engine
            .apply(
                &["privacy.error_reporting".to_string()],
                &ApplyOptions {
                    restore_point: RestorePointPolicy::Skip,
                    dry_run: true,
                },
            )
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(settles(), 1);
    }

    /// Written by the test of a value another program resets while the apply waits.
    const RESET_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitReset";

    static RESET_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.reset",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Sandbox value another program resets",
        description: "A value under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Registry(RegistryAction {
            hive: Hive::CurrentUser,
            path: RESET_SANDBOX,
            name: "Value",
            data: RegData::Dword(1),
        })],
        former: &[],
    };

    /// Puts the value from before the apply back, as Windows or another program could while
    /// the apply waits.
    fn reset_sandbox() {
        let (key, _) = Key::create(Hive::CurrentUser, RESET_SANDBOX).unwrap();
        key.set("Value", &RegValue::Dword(0)).unwrap();
    }

    #[test]
    fn a_value_windows_resets_is_not_kept_and_stays_recorded() {
        let _cleanup = SandboxKey(RESET_SANDBOX);
        reset_sandbox();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let engine = Engine::with_system(
            journal.clone(),
            EngineSystem {
                settle: reset_sandbox,
                ..test_system(ALL_MET, no_refresh)
            },
        );
        let report = apply_own(
            &engine,
            &safety,
            Selection {
                tweaks: vec![&RESET_SANDBOX_TWEAK],
                appx_names: Vec::new(),
                results: Vec::new(),
            },
        );
        let result = &report.results[0];
        assert_eq!(result.outcome, ItemOutcome::Applied, "{:?}", result.details);
        let not_kept: Vec<(&str, &str, Option<&str>, bool)> = result
            .not_kept
            .iter()
            .map(|m| {
                (
                    m.expected.as_str(),
                    m.found.as_str(),
                    m.original.as_deref(),
                    m.back_to_original,
                )
            })
            .collect();
        assert_eq!(
            not_kept,
            [(
                "0x00000001 (1)",
                "0x00000000 (0)",
                Some("0x00000000 (0)"),
                true
            )]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(
            op_rows(&journal).contains(&("verify_after_apply".to_string(), "not_kept".to_string()))
        );
        assert_eq!(
            journal.active_registry().unwrap().len(),
            1,
            "the record stays, so Undo restores the original"
        );
    }

    #[test]
    fn apply_tweak_lists_only_the_targets_it_applied() {
        let _lock = SANDBOX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _cleanup = SandboxKey(SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let reqs = Requirements::new(ALL_MET);
        let apply = || apply_tweak(&safety, &SANDBOX_TWEAK, &TaskConnection::new(), &reqs);

        let first = apply();
        assert_eq!(first.result.outcome, ItemOutcome::Applied);
        let targets: Vec<&Action> = first
            .written
            .iter()
            .map(|w| match w {
                WrittenTarget::Action(a) => *a,
                WrittenTarget::Former { .. } => panic!("the sandbox tweak has no former value"),
            })
            .collect();
        assert_eq!(targets, SANDBOX_TWEAK.actions.iter().collect::<Vec<_>>());

        let again = apply();
        assert_eq!(again.result.outcome, ItemOutcome::AlreadyApplied);
        assert!(
            again.written.is_empty(),
            "already-set values are not read back"
        );
    }

    #[test]
    fn the_engine_reaches_dns_servers_through_its_system() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_system(temp_journal(&dir), test_system(ALL_MET, no_refresh));
        let record = DnsRecord {
            id: 1,
            session_id: 1,
            recorded_at: "2026-01-01T00:00:00+00:00".into(),
            interface_guid: "{00000000-0000-0000-0000-000000000001}".into(),
            family: crate::network::IpFamily::Ipv4,
            adapter_name: "Ethernet".into(),
            previous_servers: String::new(),
            target_servers: "1.1.1.1,1.0.0.1".into(),
            active: true,
            reverted_at: None,
        };
        assert_eq!((engine.system().dns_now)(&record).unwrap(), None);
        (engine.system().settle)();
    }

    #[test]
    fn the_live_system_maps_the_mouse_tweak_to_its_setting() {
        let settings = EngineSystem::LIVE.live_settings;
        let mouse = catalog::tweak("gaming.mouse_acceleration").unwrap();
        assert_eq!(settings(mouse), vec![LiveSetting::Mouse]);
        let recall = catalog::tweak("privacy.recall").unwrap();
        assert_eq!(settings(recall), Vec::new());
    }

    #[test]
    fn scan_marks_unmet_requirements_and_keeps_recorded_items_readable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        // A journal record and its logged write (nothing is written to the registry) make the
        // tweak revertible.
        let telemetry = catalog::tweak("privacy.office_telemetry").unwrap();
        let Action::Registry(target) = &telemetry.actions[0] else {
            panic!("the Office telemetry tweak writes registry values");
        };
        let session = journal.begin_session("engine unit", "test").unwrap();
        journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: target.hive,
                    key_path: target.path.to_string(),
                    value_name: target.name.to_string(),
                    key_existed: false,
                    value_existed: false,
                    original: None,
                    created_root: None,
                },
            )
            .unwrap();
        let logged = format!(r"{}\{}\{}", target.hive.short(), target.path, target.name);
        journal
            .log_op(
                Some(session),
                "set_registry_value",
                &logged,
                "applied",
                None,
            )
            .unwrap();
        journal.end_session(session).unwrap();

        let engine = Engine::with_system(
            journal.clone(),
            test_system(
                Probes {
                    windows: || Ok(home_26h2()),
                    ..NO_OFFICE
                },
                no_refresh,
            ),
        );
        let report = engine.scan().unwrap();
        assert_eq!(report.windows, Some(home_26h2()));
        // The Office-policy tweaks gate on a policy-honouring Office; NO_OFFICE reports none.
        let text = crate::debloat::requirements::OFFICE_NOT_INSTALLED;
        let recorded = report.item("privacy.office_telemetry").unwrap();
        assert!(recorded.revertible);
        assert_ne!(recorded.state, ItemState::Unavailable, "read normally");
        assert_eq!(recorded.unavailable, Some(Unavailable::NotOnThisPc));
        assert_eq!(
            recorded.note,
            Some(format!("{text} Undo removes the settings Cairn stored."))
        );
        assert!(
            recorded.actions[0].held.is_some(),
            "the recorded target was read back"
        );
        assert!(recorded.actions[1..].iter().all(|a| a.held.is_none()));
        assert!(recorded.former.is_empty(), "the tweak has no former values");
        for id in [
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
        ] {
            let item = report.item(id).unwrap();
            assert_eq!(item.state, ItemState::Unavailable, "{id}");
            assert_eq!(item.note.as_deref(), Some(text), "{id}");
            assert_eq!(item.unavailable, Some(Unavailable::NotOnThisPc), "{id}");
            assert!(!item.revertible, "{id}");
        }
        for item in &report.items {
            assert_eq!(
                item.topic.is_some(),
                item.kind == ItemKind::Tweak,
                "{}",
                item.id
            );
        }
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("cannot check whether")),
            "{:?}",
            report.warnings
        );
    }

    const TASK_TWEAKS: [&str; 4] = [
        "privacy.feedback_tasks",
        "privacy.error_report_task",
        "privacy.ceip_tasks",
        "privacy.compatibility_telemetry",
    ];

    fn task_paths(t: &Tweak) -> Vec<&'static str> {
        t.actions
            .iter()
            .filter_map(|a| match a {
                Action::ScheduledTask(s) => Some(s.path),
                Action::Registry(_) | Action::Service(_) | Action::Power(_) => None,
            })
            .collect()
    }

    fn temp_journal(dir: &tempfile::TempDir) -> Arc<Journal> {
        Arc::new(Journal::open(dir.path().join("journal.db")).unwrap())
    }

    fn record_tasks(journal: &Journal, paths: &[&str]) {
        let session = journal.begin_session("engine unit", "test").unwrap();
        for path in paths {
            assert!(journal
                .record_scheduled_task(
                    session,
                    &NewScheduledTaskRecord {
                        path: path.to_string(),
                        was_enabled: true,
                    },
                )
                .unwrap());
        }
        journal.end_session(session).unwrap();
    }

    #[test]
    fn catalog_json_carries_each_tweak_requirement() {
        let view = serde_json::to_value(catalog_view()).unwrap();
        let entries = view.as_array().unwrap();
        let requires = |id: &str| {
            entries
                .iter()
                .find(|e| e["id"] == id)
                .unwrap_or_else(|| panic!("{id} is in the catalog"))["requires"]
                .clone()
        };
        assert_eq!(requires("privacy.edge_telemetry"), "edge");
        assert_eq!(requires("privacy.office_telemetry"), "office_business");
        assert_eq!(requires("gaming.gpu_scheduling"), "gpu_scheduling");
        assert_eq!(requires("privacy.ceip"), serde_json::Value::Null);
        let appx = catalog::appx_item_id(catalog::BLOAT_PACKAGES[0].name);
        assert_eq!(requires(&appx), serde_json::Value::Null);
        for (entry, t) in catalog_view().iter().zip(catalog::TWEAKS) {
            assert_eq!(entry.requires, t.requires, "{}", t.id);
        }

        let mut old = entries[0].clone();
        old.as_object_mut().unwrap().remove("requires");
        let parsed: CatalogEntry = serde_json::from_value(old).unwrap();
        assert_eq!(
            parsed.requires, None,
            "older catalog JSON still deserializes"
        );
    }

    #[test]
    fn catalog_json_carries_the_new_fields() {
        let view = catalog_view();
        for (entry, t) in view.iter().zip(catalog::TWEAKS) {
            assert_eq!(entry.topic, Some(t.topic), "{}", t.id);
            assert_eq!(entry.windows, t.windows.describe(), "{}", t.id);
            assert_eq!(entry.retired.as_deref(), t.retired, "{}", t.id);
            assert_eq!(entry.undo_limit.as_deref(), t.undo_limit, "{}", t.id);
            let former: Vec<String> = t
                .former
                .iter()
                .map(|f| registry::describe(&f.action))
                .collect();
            assert_eq!(entry.former_targets, former, "{}", t.id);
        }
        for entry in view.iter().filter(|e| e.kind == ItemKind::Appx) {
            assert_eq!(entry.topic, None, "{}", entry.id);
            assert_eq!(entry.windows, None, "{}", entry.id);
            assert!(entry.retired.is_none() && entry.undo_limit.is_none());
            assert!(entry.former_targets.is_empty());
        }

        let json = serde_json::to_value(&view[0]).unwrap();
        assert_eq!(
            json["topic"],
            serde_json::to_value(catalog::TWEAKS[0].topic).unwrap()
        );
        assert_eq!(json["former_targets"], serde_json::json!([]));
        let mut old = json;
        for key in [
            "topic",
            "windows",
            "retired",
            "undo_limit",
            "former_targets",
        ] {
            old.as_object_mut().unwrap().remove(key);
        }
        let parsed: CatalogEntry = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.topic, None);
        assert_eq!(parsed.windows, None);
        assert!(parsed.retired.is_none() && parsed.undo_limit.is_none());
        assert!(parsed.former_targets.is_empty());
    }

    #[test]
    fn scan_and_apply_json_without_the_new_fields_deserializes() {
        let item: ScanItem = serde_json::from_value(serde_json::json!({
            "id": "privacy.ceip", "kind": "tweak", "category": "privacy", "title": "Title",
            "description": "Description.", "risk": "low", "recommended": true,
            "state": "applied", "restart": "none",
            "actions": [{"state": "applied", "detail": "value: 0"}]
        }))
        .unwrap();
        assert_eq!(item.actions[0].held, None);
        assert_eq!((item.topic, item.unavailable), (None, None));
        assert!(item.undo_limit.is_none() && item.former.is_empty() && item.drift.is_empty());
        assert!(!item.mode_undo);

        let report: ScanReport = serde_json::from_value(serde_json::json!({
            "elevated": false, "has_battery": false, "items": [], "categories": [],
            "warnings": [], "duration_ms": 1
        }))
        .unwrap();
        assert_eq!(report.windows, None);

        let result: ItemResult = serde_json::from_value(serde_json::json!({
            "id": "privacy.ceip", "outcome": "applied", "details": []
        }))
        .unwrap();
        assert!(result.not_kept.is_empty());

        let former = FormerRecord {
            target: r"HKLM\SOFTWARE\Policies\Example\Value".into(),
            original: "not set".into(),
            reason: "Cairn 0.2 also set this value.".into(),
        };
        let json = serde_json::to_value(&former).unwrap();
        assert_eq!(json["original"], "not set");
        assert_eq!(
            serde_json::from_value::<FormerRecord>(json).unwrap(),
            former
        );
    }

    /// Never written: the tweak is retired. Read only while a test records it.
    const RETIRED_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitRetired";

    static RETIRED_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.retired",
        category: Category::Privacy,
        topic: Topic::Search,
        title: "Retired sandbox value",
        description: "A value under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some("Cairn found no effect of this policy on Windows 11."),
        undo_limit: None,
        actions: &[Action::Registry(RegistryAction {
            hive: Hive::CurrentUser,
            path: RETIRED_SANDBOX,
            name: "Value",
            data: RegData::Dword(1),
        })],
        former: &[],
    };

    #[test]
    fn retired_sandbox_tweak_scans_unavailable_and_is_never_applied() {
        let reqs = Requirements::new(ALL_MET);
        let mut warnings = Vec::new();
        let item = scan_tweak(
            &RETIRED_SANDBOX_TWEAK,
            false,
            &TaskConnection::failed("Task Scheduler must not be reached"),
            &reqs,
            true,
            &mut warnings,
        );
        let reason = RETIRED_SANDBOX_TWEAK.retired.unwrap();
        assert_eq!(item.state, ItemState::Unavailable);
        assert_eq!(item.unavailable, Some(Unavailable::Retired));
        assert_eq!(item.note.as_deref(), Some(reason));
        assert!(!item.recommended && !item.mode_undo);
        assert!(warnings.is_empty(), "{warnings:?}");

        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let TweakApply { result, written } = apply_tweak(
            &safety,
            &RETIRED_SANDBOX_TWEAK,
            &TaskConnection::new(),
            &reqs,
        );
        assert_eq!(result.outcome, ItemOutcome::Skipped);
        assert_eq!(result.details, vec![reason.to_string()]);
        assert!(written.is_empty());
        let mut tweaks = vec![&RETIRED_SANDBOX_TWEAK];
        let mut results = Vec::new();
        withhold_unavailable(&mut tweaks, &mut results, &reqs);
        assert!(tweaks.is_empty());
        assert_eq!(results[0].details, vec![reason.to_string()]);
        assert_eq!(journal.summary().unwrap().registry_total, 0);
        assert!(!exists(Hive::CurrentUser, RETIRED_SANDBOX).unwrap());
    }

    #[test]
    fn mode_undo_covers_recommended_retired_and_formerly_recommended() {
        let reqs = Requirements::new(ALL_MET);
        let mut warnings = Vec::new();
        let mut scan = |t: &'static Tweak| {
            scan_tweak(t, false, &TaskConnection::new(), &reqs, true, &mut warnings)
        };
        // Earlier versions recommended these, so a mode still restores them, retired or not.
        for id in FORMERLY_RECOMMENDED {
            let item = scan(catalog::tweak(id).unwrap());
            assert!(item.mode_undo, "{id}");
        }
        // Recommended now.
        let item = scan(&OFFICE_SANDBOX_TWEAK);
        assert!(item.recommended && item.mode_undo);
        // Retired alone does not put a tweak in a mode.
        let item = scan(&RETIRED_SANDBOX_TWEAK);
        assert!(!item.recommended && !item.mode_undo);
        // Not recommended, then or now.
        let item = scan(&SANDBOX_TWEAK);
        assert!(!item.recommended && !item.mode_undo);
        let t = catalog::tweak("interface.dark_mode").unwrap();
        let item = scan(t);
        assert_eq!(item.mode_undo, t.default_on);
        assert_eq!(item.recommended, t.default_on);
        assert_eq!(item.undo_limit.as_deref(), t.undo_limit);
    }

    #[test]
    fn revert_filter_maps_task_tweaks_to_scheduled_task_filters() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(temp_journal(&dir));
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();

        let filter = engine.revert_filter(&[ceip.id.to_string()]).unwrap();
        assert_eq!(filter.scheduled_tasks, task_paths(ceip));
        assert_eq!(filter.scheduled_tasks.len(), 5);
        assert!(filter.registry.is_empty() && filter.services.is_empty());
        assert!(!filter.power && filter.dns.is_empty() && filter.appx_families.is_empty());

        let ids = [
            "privacy.ceip",
            "privacy.ceip_tasks",
            "privacy.feedback_tasks",
        ]
        .map(String::from);
        let filter = engine.revert_filter(&ids).unwrap();
        assert_eq!(filter.registry.len(), 1, "privacy.ceip is a registry value");
        assert_eq!(filter.scheduled_tasks.len(), 7);

        let all: Vec<String> = TASK_TWEAKS.iter().map(|id| id.to_string()).collect();
        assert_eq!(
            engine.revert_filter(&all).unwrap().scheduled_tasks.len(),
            12
        );
    }

    #[test]
    fn active_task_records_make_the_tweak_revertible() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let feedback = catalog::tweak("privacy.feedback_tasks").unwrap();
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();
        record_tasks(&journal, &[&task_paths(feedback)[1].to_ascii_uppercase()]);

        let active = ActiveRecords::load(&journal).unwrap();
        assert!(
            active.covers(feedback),
            "a record for one of its tasks is enough"
        );
        assert!(!active.covers(ceip));
        assert!(!active.covers(catalog::tweak("privacy.feedback_prompts").unwrap()));

        journal
            .mark_reverted(
                JournalTable::ScheduledTask,
                journal.active_scheduled_tasks().unwrap()[0].id,
            )
            .unwrap();
        assert!(!ActiveRecords::load(&journal).unwrap().covers(feedback));
    }

    #[test]
    fn held_is_set_only_for_recorded_targets_that_were_read() {
        use ActionState::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();
        let paths = task_paths(ceip);
        // The first three tasks were turned off by this tool; the last two were not.
        record_tasks(&journal, &paths[..3]);
        let active = ActiveRecords::load(&journal).unwrap();
        assert!((0..5).all(|i| active.records(&ceip.actions[i]) == (i < 3)));

        let read = |held: bool| ReadBack {
            held,
            expected: String::new(),
            found: String::new(),
        };
        let mut actions = [NotApplied, Applied, Unavailable, NotApplied, Applied].map(status);
        // The third task could not be read, so it has no read-back.
        let reads = [
            (0, read(false)),
            (1, read(true)),
            (3, read(false)),
            (4, read(true)),
        ];
        active.mark_held(ceip, &mut actions, &reads);
        let held: Vec<Option<bool>> = actions.iter().map(|a| a.held).collect();
        assert_eq!(held, vec![Some(false), Some(true), None, None, None]);

        // An index past the tweak's actions changes nothing.
        let mut actions = [status(NotApplied)];
        let registry_only = catalog::tweak("privacy.ceip").unwrap();
        active.mark_held(
            registry_only,
            &mut actions,
            &[(0, read(false)), (7, read(false))],
        );
        assert_eq!(actions[0].held, None, "privacy.ceip has no record");
    }

    #[test]
    fn held_is_not_set_for_a_target_whose_write_never_happened() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let widgets = catalog::tweak("interface.widgets").unwrap();
        let [action @ Action::Registry(r)] = widgets.actions else {
            unreachable!("one registry action")
        };
        // An earlier version of Cairn recorded the policy value, created its key and then
        // failed to write the value, logging nothing.
        let session = journal.begin_session("engine unit", "0.2.0").unwrap();
        journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: r.hive,
                    key_path: r.path.to_string(),
                    value_name: r.name.to_string(),
                    key_existed: false,
                    value_existed: false,
                    original: None,
                    created_root: Some(r.path.to_string()),
                },
            )
            .unwrap();
        journal.end_session(session).unwrap();
        let active = ActiveRecords::load(&journal).unwrap();
        assert!(active.records(action));
        let original = verify::recorded_original(action, &active).unwrap();
        let at_original = ReadBack {
            held: false,
            expected: verify::expected_text(r),
            found: original,
        };
        assert!(active.never_changed(action, &at_original));
        let mut actions = [status(ActionState::NotApplied)];
        active.mark_held(widgets, &mut actions, &[(0, at_original.clone())]);
        assert_eq!(actions[0].held, None, "Cairn never changed the value");

        // A value set since is a change, read as usual.
        let changed = ReadBack {
            found: "0x00000005 (5)".to_string(),
            ..at_original.clone()
        };
        assert!(!active.never_changed(action, &changed));
        active.mark_held(widgets, &mut actions, &[(0, changed)]);
        assert_eq!(actions[0].held, Some(false));

        // So is a record whose session logged the write.
        journal
            .log_op(
                Some(session),
                "set_registry_value",
                &format!(r"HKLM\{}\{}", r.path, r.name),
                "applied",
                None,
            )
            .unwrap();
        let logged = ActiveRecords::load(&journal).unwrap();
        assert!(!logged.never_changed(action, &at_original));
    }

    #[test]
    fn active_records_keep_the_oldest_record_of_a_target() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let first = journal.begin_session("engine unit", "test").unwrap();
        let record = |original: u32| NewRegistryRecord {
            hive: Hive::CurrentUser,
            key_path: SANDBOX.to_string(),
            value_name: "First".to_string(),
            key_existed: true,
            value_existed: true,
            original: Some(RegValue::Dword(original).to_raw()),
            created_root: None,
        };
        journal.record_registry(first, &record(7)).unwrap();
        journal.end_session(first).unwrap();
        let active = ActiveRecords::load(&journal).unwrap();
        let kept = &active.registry[&registry_key(Hive::CurrentUser, SANDBOX, "FIRST")];
        assert_eq!(kept.original_decoded(), Some(RegValue::Dword(7)));
        assert!(active.covers(&SANDBOX_TWEAK));
        assert!(active.services.is_empty() && active.scheduled_tasks.is_empty());
        assert!(active.power.is_empty() && active.appx.is_empty());

        let mut map = HashMap::new();
        keep_oldest(&mut map, "target", (2_i64, "newer"), |r| r.0);
        keep_oldest(&mut map, "target", (1_i64, "older"), |r| r.0);
        keep_oldest(&mut map, "target", (3_i64, "newest"), |r| r.0);
        assert_eq!(map["target"], (1, "older"));
    }

    #[test]
    fn task_tweaks_are_not_per_user() {
        for id in TASK_TWEAKS {
            let t = catalog::tweak(id).unwrap();
            assert!(!touches_current_user(t), "{id}");
            assert_eq!(task_paths(t).len(), t.actions.len(), "{id}");
        }
        let mut tweaks: Vec<&'static Tweak> = TASK_TWEAKS
            .iter()
            .map(|id| catalog::tweak(id).unwrap())
            .collect();
        let mut results = Vec::new();
        withhold_per_user(&mut tweaks, &mut Vec::new(), &mut results, || {
            panic!("account check ran for machine-wide scheduled tasks")
        });
        assert_eq!(tweaks.len(), 4);
        assert!(results.is_empty());
    }

    #[test]
    fn failed_task_connection_marks_every_task_action() {
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();
        let error = "cannot connect to Task Scheduler: unavailable";

        let reqs = Requirements::new(ALL_MET);
        let mut warnings = Vec::new();
        let item = scan_tweak(
            ceip,
            false,
            &TaskConnection::failed(error),
            &reqs,
            true,
            &mut warnings,
        );
        assert_eq!(item.state, ItemState::Unavailable);
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        for (a, path) in item.actions.iter().zip(task_paths(ceip)) {
            assert_eq!(a.state, ActionState::Unavailable);
            assert_eq!(a.detail, format!("scheduled task {path}: {error}"));
        }

        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let TweakApply { result, written } =
            apply_tweak(&safety, ceip, &TaskConnection::failed(error), &reqs);
        assert!(written.is_empty(), "nothing was written");
        assert_eq!(result.outcome, ItemOutcome::Failed);
        assert_eq!(result.details.len(), 5);
        assert!(result
            .details
            .iter()
            .all(|d| d.ends_with(&format!(": failed ({error})"))));
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 0);
    }

    const SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnit";
    /// Serializes the test that writes [`SANDBOX`] with the one that checks nothing did.
    static SANDBOX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A per-user tweak whose values live in the self-test sandbox.
    static SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.per_user",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Sandbox values",
        description: "Two values under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: SANDBOX,
                name: "First",
                data: RegData::Dword(1),
            }),
            Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: SANDBOX,
                name: "Second",
                data: RegData::Dword(1),
            }),
        ],
        former: &[],
    };

    #[test]
    fn per_user_tweaks_are_skipped_whole_when_the_user_check_fails() {
        let _lock = SANDBOX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        for other_user in [true, false] {
            let safety = test_safety(journal.clone(), "engine unit", false);
            if other_user {
                safety.assume_other_user();
            } else {
                safety.assume_user_check_failed("Access is denied.");
            }
            let reason = safety.ensure_interactive_user().unwrap_err().to_string();

            let result = apply_tweak(
                &safety,
                &SANDBOX_TWEAK,
                &TaskConnection::new(),
                &Requirements::new(ALL_MET),
            )
            .result;
            assert_eq!(result.outcome, ItemOutcome::Skipped, "{:?}", result.details);
            assert_eq!(result.details, vec![reason]);
        }
        assert!(
            !exists(Hive::CurrentUser, SANDBOX).unwrap(),
            "nothing written"
        );
        assert_eq!(
            journal.summary().unwrap().registry_total,
            0,
            "nothing journaled"
        );
    }

    #[test]
    fn apply_in_withholds_per_user_items_for_another_account() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::new(journal.clone());
        let safety = test_safety(journal.clone(), "profile: unit", false);
        safety.assume_other_user();
        let reason = safety.ensure_interactive_user().unwrap_err().to_string();
        let sessions = journal.summary().unwrap().sessions;

        let ids = ["privacy.recall", "appx.Microsoft.BingNews"].map(String::from);
        let report = engine.apply_in(&safety, &ids).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.session_id, Some(safety.session_id()));
        assert!(report.restore_point.is_none());
        let outcomes: Vec<(&str, ItemOutcome, &[String])> = report
            .results
            .iter()
            .map(|r| (r.id.as_str(), r.outcome, r.details.as_slice()))
            .collect();
        let refused = [reason];
        assert_eq!(
            outcomes,
            vec![
                ("privacy.recall", ItemOutcome::Skipped, &refused[..]),
                (
                    "appx.Microsoft.BingNews",
                    ItemOutcome::Skipped,
                    &refused[..]
                ),
            ]
        );
        let summary = journal.summary().unwrap();
        assert_eq!(summary.sessions, sessions, "apply_in opens no session");
        assert_eq!(summary.registry_total, 0, "nothing journaled");
        assert_eq!(report.restart, RestartNeed::None);
    }

    #[test]
    fn apply_in_resolves_unknown_and_non_bloat_ids_like_apply() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::new(journal.clone());
        let ids = [
            "selftest.nothing",
            "appx.Contoso.NotBloatware",
            "SELFTEST.NOTHING",
        ]
        .map(String::from);

        let safety = test_safety(journal.clone(), "profile: unit", false);
        let sessions = journal.summary().unwrap().sessions;
        let within = engine.apply_in(&safety, &ids).unwrap();
        let alone = engine
            .apply(
                &ids,
                &ApplyOptions {
                    restore_point: RestorePointPolicy::Skip,
                    dry_run: false,
                },
            )
            .unwrap();
        let planned = engine
            .apply(
                &ids,
                &ApplyOptions {
                    restore_point: RestorePointPolicy::Skip,
                    dry_run: true,
                },
            )
            .unwrap();
        let view = |report: &ApplyReport| -> Vec<(String, ItemOutcome, Vec<String>)> {
            report
                .results
                .iter()
                .map(|r| (r.id.clone(), r.outcome, r.details.clone()))
                .collect()
        };
        assert_eq!(
            view(&within),
            vec![
                (
                    "selftest.nothing".to_string(),
                    ItemOutcome::Failed,
                    vec!["unknown item".to_string()]
                ),
                (
                    "appx.Contoso.NotBloatware".to_string(),
                    ItemOutcome::Failed,
                    vec!["not a bloatware package this tool removes".to_string()]
                ),
            ],
            "repeats differing only in case are ignored"
        );
        assert_eq!(view(&within), view(&alone));
        assert_eq!(view(&within), view(&planned));
        assert_eq!(alone.session_id, None, "nothing to apply opens no session");
        assert_eq!(within.session_id, Some(safety.session_id()));
        assert_eq!(journal.summary().unwrap().sessions, sessions);

        // A session of another journal is refused before anything is resolved.
        let other_dir = tempfile::tempdir().unwrap();
        let other = test_safety(temp_journal(&other_dir), "profile: unit", false);
        let err = engine.apply_in(&other, &ids).unwrap_err();
        assert!(err.to_string().contains("another journal"), "{err}");
    }

    #[test]
    fn tweaks_with_a_user_value_count_as_per_user() {
        assert!(touches_current_user(&SANDBOX_TWEAK));
        let recall = catalog::tweak("privacy.recall").unwrap();
        assert!(touches_current_user(recall), "Recall has a per-user value");
        assert!(recall.actions.iter().any(|a| !matches!(
            a,
            Action::Registry(r) if r.hive == Hive::CurrentUser
        )));
        assert!(catalog::TWEAKS.iter().any(|t| !touches_current_user(t)));
    }

    #[test]
    fn per_user_items_are_withheld_when_the_user_check_fails() {
        let recall = catalog::tweak("privacy.recall").unwrap();
        let machine = catalog::TWEAKS
            .iter()
            .find(|t| !touches_current_user(t))
            .unwrap();
        let package = "Microsoft.BingNews".to_string();

        let mut tweaks = vec![recall, machine];
        let mut appx_names = vec![package.clone()];
        let mut results = Vec::new();
        withhold_per_user(&mut tweaks, &mut appx_names, &mut results, || {
            Err(Error::Other("refused".into()))
        });
        assert_eq!(tweaks.len(), 1);
        assert_eq!(tweaks[0].id, machine.id);
        assert!(appx_names.is_empty());
        let skipped: Vec<(&str, ItemOutcome, &[String])> = results
            .iter()
            .map(|r| (r.id.as_str(), r.outcome, r.details.as_slice()))
            .collect();
        let refused = ["refused".to_string()];
        assert_eq!(
            skipped,
            vec![
                (recall.id, ItemOutcome::Skipped, &refused[..]),
                (
                    catalog::appx_item_id(&package).as_str(),
                    ItemOutcome::Skipped,
                    &refused[..]
                ),
            ]
        );

        // A passing check keeps the selection.
        let mut tweaks = vec![recall, machine];
        let mut appx_names = vec![package];
        let mut results = Vec::new();
        withhold_per_user(&mut tweaks, &mut appx_names, &mut results, || Ok(()));
        assert_eq!((tweaks.len(), appx_names.len(), results.len()), (2, 1, 0));

        // Without a per-user item the accounts are not compared at all.
        let mut tweaks = vec![machine];
        withhold_per_user(&mut tweaks, &mut Vec::new(), &mut results, || {
            panic!("account check ran without a per-user item")
        });
        assert_eq!(tweaks.len(), 1);
    }

    #[test]
    fn states_combine() {
        use ActionState::*;
        assert_eq!(
            combine_states(&[status(Applied), status(Applied)]),
            ItemState::Applied
        );
        assert_eq!(
            combine_states(&[status(Applied), status(Unavailable)]),
            ItemState::Applied
        );
        assert_eq!(
            combine_states(&[status(NotApplied), status(Unavailable)]),
            ItemState::NotApplied
        );
        assert_eq!(
            combine_states(&[status(Applied), status(NotApplied)]),
            ItemState::Partial
        );
        assert_eq!(
            combine_states(&[status(Unavailable)]),
            ItemState::Unavailable
        );
        assert_eq!(combine_states(&[]), ItemState::Unavailable);
    }

    #[test]
    fn family_names_split() {
        assert_eq!(
            package_name_from_family("Microsoft.BingNews_8wekyb3d8bbwe"),
            "Microsoft.BingNews"
        );
        assert_eq!(
            package_name_from_family("king.com.CandyCrushSaga_kgqvnymyfvs32"),
            "king.com.CandyCrushSaga"
        );
        assert_eq!(package_name_from_family("NoPublisher"), "NoPublisher");
    }

    #[test]
    fn reinstalled_note_names_no_one() {
        assert_eq!(
            REINSTALLED_NOTE,
            "This app is installed again after Cairn removed it."
        );
        assert!(!REINSTALLED_NOTE.contains("Windows"));
    }

    // ─────────────── Windows ranges, retired tweaks and former values ───────────────

    /// Never written: the tweaks for some Windows builds or editions are only scanned or
    /// skipped here.
    const RANGE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitRange";

    /// Tweaks for 24H2 (build 26100.3915) or later, for 23H2 and earlier, and for Pro,
    /// Enterprise or Education, with one value each in the sandbox.
    static RANGE_SANDBOX_TWEAKS: [Tweak; 3] = [
        Tweak {
            id: "selftest.new_builds",
            category: Category::Privacy,
            topic: Topic::Ai,
            title: "Sandbox value for new builds",
            description: "A value under the self-test key.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: None,
            windows: WindowsRange {
                from: &[Build::new(26100, 3915)],
                through: None,
                editions: Editions::All,
            },
            retired: None,
            undo_limit: None,
            actions: &[Action::Registry(reg_dword(
                Hive::CurrentUser,
                RANGE_SANDBOX,
                "NewBuilds",
                1,
            ))],
            former: &[],
        },
        Tweak {
            id: "selftest.old_builds",
            category: Category::Privacy,
            topic: Topic::Ai,
            title: "Sandbox value for old builds",
            description: "A value under the self-test key.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: None,
            windows: WindowsRange {
                from: &[],
                through: Some(22621),
                editions: Editions::All,
            },
            retired: None,
            undo_limit: None,
            actions: &[Action::Registry(reg_dword(
                Hive::CurrentUser,
                RANGE_SANDBOX,
                "OldBuilds",
                1,
            ))],
            former: &[],
        },
        Tweak {
            id: "selftest.pro_only",
            category: Category::Privacy,
            topic: Topic::Ai,
            title: "Sandbox value for Pro and higher",
            description: "A value under the self-test key.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: None,
            windows: WindowsRange {
                from: &[],
                through: None,
                editions: Editions::ProOrHigher,
            },
            retired: None,
            undo_limit: None,
            actions: &[Action::Registry(reg_dword(
                Hive::CurrentUser,
                RANGE_SANDBOX,
                "ProOnly",
                1,
            ))],
            former: &[],
        },
    ];

    const TOO_OLD: &str = "Needs Windows 11 24H2 (build 26100.3915) or later; this PC runs \
                           Windows 11 Home 22H2 (build 22621.4890).";
    const TOO_NEW: &str = "Windows 11 24H2 and later no longer use this setting; this PC runs \
                           Windows 11 Home 26H2 (build 26300.9457).";
    const NEEDS_PRO: &str =
        "Needs Windows 11 Pro, Enterprise or Education; this PC runs Windows 11 Home.";

    /// A scan of `t` that must not read anything: its read-backs and warnings must be empty.
    fn unread_scan(t: &'static Tweak, probes: Probes) -> ScanItem {
        let mut warnings = Vec::new();
        let (item, reads) = read_tweak(
            t,
            false,
            &TaskConnection::failed("Task Scheduler must not be reached"),
            &Requirements::new(probes),
            true,
            &mut warnings,
        );
        assert!(reads.is_empty(), "{} was read", t.id);
        assert!(warnings.is_empty(), "{warnings:?}");
        item
    }

    #[test]
    fn windows_range_makes_the_tweak_unavailable_with_its_kind_and_text() {
        let [new_builds, old_builds, _] = &RANGE_SANDBOX_TWEAKS;
        let item = unread_scan(new_builds, HOME_22H2);
        assert_eq!(item.state, ItemState::Unavailable);
        assert_eq!(item.unavailable, Some(Unavailable::WindowsVersion));
        assert_eq!(item.note.as_deref(), Some(TOO_OLD));
        assert_eq!(item.actions.len(), 1);
        assert_eq!(item.actions[0].state, ActionState::Unavailable);
        assert_eq!(
            item.actions[0].detail,
            format!(r"HKCU\{RANGE_SANDBOX}\NewBuilds: {TOO_OLD}")
        );
        assert!(!item.revertible);

        let item = unread_scan(old_builds, HOME_26H2);
        assert_eq!(item.unavailable, Some(Unavailable::WindowsVersion));
        assert_eq!(item.note.as_deref(), Some(TOO_NEW));

        // Inside their ranges both are read.
        let mut warnings = Vec::new();
        for (t, probes) in [(new_builds, HOME_26H2), (old_builds, HOME_22H2)] {
            let reqs = Requirements::new(probes);
            let item = scan_tweak(t, false, &TaskConnection::new(), &reqs, true, &mut warnings);
            assert_eq!(item.state, ItemState::NotApplied, "{}", t.id);
            assert_eq!(item.unavailable, None, "{}", t.id);
            assert_eq!(item.note, None, "{}", t.id);
        }
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!exists(Hive::CurrentUser, RANGE_SANDBOX).unwrap());

        // Recorded but outside the range: the note adds that Undo still restores it.
        let reqs = Requirements::new(HOME_22H2);
        assert_eq!(
            recorded_unmet_note(&reqs.check_tweak(new_builds), true),
            Some(
                "Needs Windows 11 24H2 (build 26100.3915) or later; this PC runs Windows 11 \
                 Home 22H2 (build 22621.4890). Undo removes the settings Cairn stored."
                    .to_string()
            )
        );
        assert_eq!(
            recorded_unmet_note(&reqs.check_tweak(new_builds), false),
            None
        );
    }

    #[test]
    fn edition_range_on_home_is_unavailable_as_edition() {
        let pro_only = &RANGE_SANDBOX_TWEAKS[2];
        let home = unread_scan(pro_only, HOME_26H2);
        assert_eq!(home.state, ItemState::Unavailable);
        assert_eq!(home.unavailable, Some(Unavailable::Edition));
        assert_eq!(home.note.as_deref(), Some(NEEDS_PRO));

        let mut warnings = Vec::new();
        let reqs = Requirements::new(PRO_26H2);
        let pro = scan_tweak(
            pro_only,
            false,
            &TaskConnection::new(),
            &reqs,
            true,
            &mut warnings,
        );
        assert_eq!((pro.state, pro.unavailable), (ItemState::NotApplied, None));
        assert!(warnings.is_empty(), "{warnings:?}");

        // Skipped on apply with the same sentence.
        let mut tweaks: Vec<&'static Tweak> = vec![pro_only];
        let mut results = Vec::new();
        withhold_unavailable(&mut tweaks, &mut results, &Requirements::new(HOME_26H2));
        assert!(tweaks.is_empty());
        assert_eq!(results[0].outcome, ItemOutcome::Skipped);
        assert_eq!(results[0].details, vec![NEEDS_PRO.to_string()]);
    }

    #[test]
    fn retired_tweak_is_unavailable_and_reads_nothing() {
        let reason = RETIRED_SANDBOX_TWEAK.retired.unwrap();
        let item = unread_scan(
            &RETIRED_SANDBOX_TWEAK,
            Probes {
                windows: || panic!("the Windows facts were read for a retired tweak"),
                ..ALL_MET
            },
        );
        assert_eq!(item.state, ItemState::Unavailable);
        assert_eq!(item.unavailable, Some(Unavailable::Retired));
        assert_eq!(item.note.as_deref(), Some(reason));
        assert_eq!(
            item.actions[0].detail,
            format!(r"HKCU\{RETIRED_SANDBOX}\Value: {reason}")
        );
        assert!(!item.recommended && !item.mode_undo);
    }

    /// Records `name` under `path` as an earlier version of Cairn changed it (`original` before
    /// its change, `None` when the value did not exist), in a session of its own that logs the
    /// write, as that version did. Writes nothing.
    fn record_earlier_change(journal: &Journal, path: &str, name: &str, original: Option<u32>) {
        let session = journal.begin_session("earlier version", "0.2.0").unwrap();
        assert!(journal
            .record_registry(
                session,
                &NewRegistryRecord {
                    hive: Hive::CurrentUser,
                    key_path: path.to_string(),
                    value_name: name.to_string(),
                    key_existed: true,
                    value_existed: original.is_some(),
                    original: original.map(|v| RegValue::Dword(v).to_raw()),
                    created_root: None,
                },
            )
            .unwrap());
        let target = format!(r"HKCU\{path}\{name}");
        journal
            .log_op(
                Some(session),
                "set_registry_value",
                &target,
                "applied",
                None,
            )
            .unwrap();
        journal.end_session(session).unwrap();
    }

    /// Sets a DWORD value under a sandbox key.
    fn set_sandbox_dword(path: &str, name: &str, value: u32) {
        assert!(
            path.starts_with(r"Software\PCOptimizer\SelfTest\"),
            "{path}"
        );
        let (key, _) = Key::create(Hive::CurrentUser, path).unwrap();
        key.set(name, &RegValue::Dword(value)).unwrap();
    }

    #[test]
    fn retired_tweak_with_a_record_reads_normally_and_offers_undo() {
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier_change(&journal, RETIRED_SANDBOX, "Value", None);
        let sessions = journal.summary().unwrap().sessions;
        let engine = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let report = engine
            .scan_with(std::slice::from_ref(&RETIRED_SANDBOX_TWEAK), false)
            .unwrap();
        let item = report.item(RETIRED_SANDBOX_TWEAK.id).unwrap();
        let reason = RETIRED_SANDBOX_TWEAK.retired.unwrap();
        assert!(item.revertible, "Undo is offered");
        assert_eq!(item.state, ItemState::NotApplied, "read normally");
        assert_eq!(
            item.actions[0].held,
            Some(false),
            "the recorded target was read"
        );
        assert_eq!(item.unavailable, Some(Unavailable::Retired));
        assert_eq!(
            item.note,
            Some(format!("{reason} Undo removes the settings Cairn stored."))
        );
        assert!(!item.recommended && !item.mode_undo);
        let privacy = report
            .categories
            .iter()
            .find(|c| c.category == Category::Privacy)
            .unwrap();
        assert_eq!((privacy.recommended, privacy.active), (0, false));

        let mut filter = RollbackFilter::default();
        add_tweak_targets(&RETIRED_SANDBOX_TWEAK, &mut filter);
        assert_eq!(
            filter.registry,
            vec![RegistryTarget {
                hive: Hive::CurrentUser,
                key_path: RETIRED_SANDBOX.to_string(),
                value_name: "Value".to_string(),
            }]
        );
        assert_eq!(journal.summary().unwrap().sessions, sessions, "read-only");
        assert!(!exists(Hive::CurrentUser, RETIRED_SANDBOX).unwrap());
    }

    #[test]
    fn retired_tweak_is_never_applied_even_with_a_record() {
        let _cleanup = SandboxKey(RETIRED_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier_change(&journal, RETIRED_SANDBOX, "Value", None);
        let reason = vec![RETIRED_SANDBOX_TWEAK.retired.unwrap().to_string()];
        let safety = test_safety(journal.clone(), "engine unit", false);

        let TweakApply { result, written } = apply_tweak(
            &safety,
            &RETIRED_SANDBOX_TWEAK,
            &TaskConnection::new(),
            &Requirements::new(ALL_MET),
        );
        assert_eq!(
            (result.outcome, &result.details),
            (ItemOutcome::Skipped, &reason)
        );
        assert!(written.is_empty());

        let engine = Engine::with_system(
            journal.clone(),
            EngineSystem {
                settle: || panic!("nothing was written, so nothing is read back"),
                ..test_system(ALL_MET, no_refresh)
            },
        );
        let selection = || Selection {
            tweaks: vec![&RETIRED_SANDBOX_TWEAK],
            appx_names: Vec::new(),
            results: Vec::new(),
        };
        let report = apply_own(&engine, &safety, selection());
        assert_eq!(report.results[0].outcome, ItemOutcome::Skipped);
        assert_eq!(report.results[0].details, reason);
        assert_eq!(report.restart, RestartNeed::None);
        for dry_run in [true, false] {
            let report = engine
                .apply_selected(
                    selection(),
                    "apply: selftest.retired".into(),
                    &ApplyOptions {
                        restore_point: RestorePointPolicy::Skip,
                        dry_run,
                    },
                )
                .unwrap();
            assert_eq!(report.session_id, None, "dry run {dry_run}");
            assert_eq!(report.results[0].outcome, ItemOutcome::Skipped);
            assert_eq!(report.results[0].details, reason, "dry run {dry_run}");
        }

        assert!(!exists(Hive::CurrentUser, RETIRED_SANDBOX).unwrap());
        let summary = journal.summary().unwrap();
        assert_eq!((summary.registry_total, summary.registry_active), (1, 1));
        let rows = op_rows(&journal);
        assert_eq!(
            rows.iter()
                .filter(|(op, _)| op == "set_registry_value")
                .count(),
            1,
            "only the earlier version's write is logged: nothing was written"
        );
        assert!(!rows.iter().any(|(op, _)| op == "rollback_registry"));
    }

    #[test]
    fn unmet_windows_range_skips_apply_before_anything_is_recorded() {
        let new_builds = &RANGE_SANDBOX_TWEAKS[0];
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let safety = test_safety(journal.clone(), "engine unit", false);
        // The range is checked before the account: the reason is the Windows version.
        safety.assume_other_user();
        let TweakApply { result, written } = apply_tweak(
            &safety,
            new_builds,
            &TaskConnection::new(),
            &Requirements::new(HOME_22H2),
        );
        assert!(written.is_empty());
        assert_eq!(result.outcome, ItemOutcome::Skipped);
        assert_eq!(result.details, vec![TOO_OLD.to_string()]);
        assert_eq!(
            journal.summary().unwrap().registry_total,
            0,
            "nothing journaled"
        );
        assert!(journal.ops(10).unwrap().is_empty(), "nothing logged");

        // A whole apply refuses it before a session is opened.
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::with_system(journal.clone(), test_system(HOME_22H2, no_refresh));
        let report = engine
            .apply_selected(
                Selection {
                    tweaks: vec![new_builds],
                    appx_names: Vec::new(),
                    results: Vec::new(),
                },
                "apply: selftest.new_builds".into(),
                &ApplyOptions {
                    restore_point: RestorePointPolicy::Skip,
                    dry_run: false,
                },
            )
            .unwrap();
        assert_eq!(report.session_id, None);
        assert!(report.restore_point.is_none());
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].outcome, ItemOutcome::Skipped);
        // A per-user tweak is withheld first when the accounts differ or cannot be compared;
        // either way nothing is recorded.
        let reason = match safety::check_interactive_user() {
            Ok(()) => TOO_OLD.to_string(),
            Err(e) => e.to_string(),
        };
        assert_eq!(report.results[0].details, vec![reason]);
        let summary = journal.summary().unwrap();
        assert_eq!((summary.sessions, summary.registry_total), (0, 0));
        assert!(journal.ops(10).unwrap().is_empty());
        assert!(!exists(Hive::CurrentUser, RANGE_SANDBOX).unwrap());
    }

    #[test]
    fn unreadable_windows_facts_warn_once_and_keep_tweaks_available() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_system(
            temp_journal(&dir),
            test_system(
                Probes {
                    windows: || Err(Error::Other("no build number".into())),
                    ..ALL_MET
                },
                no_refresh,
            ),
        );
        let report = engine.scan_with(&RANGE_SANDBOX_TWEAKS, false).unwrap();
        for t in &RANGE_SANDBOX_TWEAKS {
            let item = report.item(t.id).unwrap();
            assert_eq!(item.unavailable, None, "{}", t.id);
            assert_eq!(item.state, ItemState::NotApplied, "{}", t.id);
        }
        assert_eq!(
            report.warnings,
            vec![
                "Cannot read the Windows version (no build number); tweaks meant for some \
                 Windows versions or editions are shown as available."
                    .to_string()
            ]
        );
        assert_eq!(report.windows, None);

        // Without a tweak for some Windows versions the facts are never needed.
        let report = engine
            .scan_with(std::slice::from_ref(&SANDBOX_TWEAK), false)
            .unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.windows, None);
    }

    #[test]
    fn unknown_edition_warns_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_system(
            temp_journal(&dir),
            test_system(
                Probes {
                    windows: || {
                        Ok(WindowsFacts {
                            edition_id: "Weird".into(),
                            edition: EditionClass::Unknown,
                            name: "Windows 11 Weird".into(),
                            ..home_26h2()
                        })
                    },
                    ..ALL_MET
                },
                no_refresh,
            ),
        );
        let report = engine.scan_with(&RANGE_SANDBOX_TWEAKS, false).unwrap();
        let state = |id: &str| {
            let item = report.item(id).unwrap();
            (item.state, item.unavailable)
        };
        assert_eq!(
            state("selftest.pro_only"),
            (ItemState::NotApplied, None),
            "read normally"
        );
        assert_eq!(state("selftest.new_builds"), (ItemState::NotApplied, None));
        assert_eq!(
            state("selftest.old_builds"),
            (ItemState::Unavailable, Some(Unavailable::WindowsVersion)),
            "the build decides without the edition"
        );
        assert_eq!(
            report.warnings,
            vec![
                "selftest.pro_only: cannot check whether this edition of Windows uses this \
                 setting: Cairn does not know the edition \"Weird\""
                    .to_string()
            ]
        );
        assert_eq!(
            report.windows.as_ref().map(|w| w.edition),
            Some(EditionClass::Unknown)
        );
    }

    /// Written by the former-value tests, one at a time (see [`former_sandbox`]).
    const FORMER_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitFormer";
    const FORMER_REASON: &str = "Cairn 0.2 also set this value, so Cairn no longer sets it.";

    /// Serializes the tests that use [`FORMER_SANDBOX`].
    static FORMER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Exclusive use of [`FORMER_SANDBOX`], which is empty at the start and removed at the end.
    struct FormerSandbox {
        _key: SandboxKey,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn former_sandbox() -> FormerSandbox {
        let lock = FORMER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        delete_sandbox_tree(FORMER_SANDBOX).unwrap();
        FormerSandbox {
            _key: SandboxKey(FORMER_SANDBOX),
            _lock: lock,
        }
    }

    /// A recommended tweak with one current value and one value an earlier version also set.
    static FORMER_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.former",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Sandbox value with a former value",
        description: "A value under the self-test key; an earlier version also set another.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Registry(reg_dword(
            Hive::CurrentUser,
            FORMER_SANDBOX,
            "Current",
            1,
        ))],
        former: &[Former {
            action: reg_dword(Hive::CurrentUser, FORMER_SANDBOX, "Former", 0),
            reason: FORMER_REASON,
        }],
    };

    fn former_target() -> RegistryTarget {
        RegistryTarget {
            hive: Hive::CurrentUser,
            key_path: FORMER_SANDBOX.to_string(),
            value_name: "Former".to_string(),
        }
    }

    fn former_selection() -> Selection {
        Selection {
            tweaks: vec![&FORMER_SANDBOX_TWEAK],
            appx_names: Vec::new(),
            results: Vec::new(),
        }
    }

    #[test]
    fn former_records_make_the_tweak_revertible_and_listed() {
        let _sandbox = former_sandbox();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let engine = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let tweaks = std::slice::from_ref(&FORMER_SANDBOX_TWEAK);

        let report = engine.scan_with(tweaks, false).unwrap();
        let item = report.item(FORMER_SANDBOX_TWEAK.id).unwrap();
        assert!(
            !item.revertible && item.former.is_empty(),
            "nothing recorded"
        );

        record_earlier_change(&journal, FORMER_SANDBOX, "Former", Some(7));
        let report = engine.scan_with(tweaks, false).unwrap();
        let item = report.item(FORMER_SANDBOX_TWEAK.id).unwrap();
        assert!(
            item.revertible,
            "the former value's record undoes with the tweak"
        );
        assert_eq!(item.state, ItemState::NotApplied, "from the current value");
        assert_eq!(
            item.former,
            vec![FormerRecord {
                target: format!(r"HKCU\{FORMER_SANDBOX}\Former"),
                original: "0x00000007 (7)".to_string(),
                reason: FORMER_REASON.to_string(),
            }]
        );
        assert!(ActiveRecords::load(&journal)
            .unwrap()
            .covers(&FORMER_SANDBOX_TWEAK));

        let mut filter = RollbackFilter::default();
        add_tweak_targets(&FORMER_SANDBOX_TWEAK, &mut filter);
        assert_eq!(filter.registry.len(), 2);
        assert_eq!(filter.registry[1], former_target());

        // A tweak whose only per-user value is a former one still needs the signed-in user.
        const USER_FORMER: &[Former] = &[Former {
            action: reg_dword(Hive::CurrentUser, FORMER_SANDBOX, "Former", 0),
            reason: FORMER_REASON,
        }];
        let former_only = Tweak {
            id: "selftest.former_only",
            category: Category::Privacy,
            topic: Topic::Activity,
            title: "Sandbox former value",
            description: "A value an earlier version set.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: None,
            windows: ANY_WINDOWS,
            retired: None,
            undo_limit: None,
            actions: &[],
            former: USER_FORMER,
        };
        assert!(touches_current_user(&former_only));
        assert!(touches_current_user(&FORMER_SANDBOX_TWEAK));
        assert!(!exists(Hive::CurrentUser, FORMER_SANDBOX).unwrap());
    }

    /// The (operation, outcome) rows of `session`, oldest first.
    fn session_rows(journal: &Journal, session: i64) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|op| op.session_id == Some(session))
            .map(|op| (op.op, op.outcome))
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn apply_restores_former_records_in_the_same_session() {
        let _sandbox = former_sandbox();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier_change(&journal, FORMER_SANDBOX, "Former", Some(7));
        set_sandbox_dword(FORMER_SANDBOX, "Former", 0);
        let engine = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let safety = test_safety(journal.clone(), "engine unit", false);

        let report = apply_own(&engine, &safety, former_selection());
        let result = &report.results[0];
        assert_eq!(result.outcome, ItemOutcome::Applied, "{:?}", result.details);
        let target = format!(r"HKCU\{FORMER_SANDBOX}");
        assert_eq!(
            result.details,
            vec![
                format!(
                    "{target}\\Former: restored to 0x00000007 (7) (no longer part of this tweak)"
                ),
                format!("{target}\\Current: applied"),
            ]
        );
        assert_eq!(report.restart, RestartNeed::SignOut);
        let rows = session_rows(&journal, safety.session_id());
        let row = |op: &str, outcome: &str| (op.to_string(), outcome.to_string());
        assert_eq!(
            rows[..2],
            [
                row("rollback_registry", "restored"),
                row("set_registry_value", "applied")
            ],
            "{rows:?}"
        );

        assert_eq!(
            read_value(Hive::CurrentUser, FORMER_SANDBOX, "Former").unwrap(),
            Some(RegValue::Dword(7))
        );
        assert_eq!(
            read_value(Hive::CurrentUser, FORMER_SANDBOX, "Current").unwrap(),
            Some(RegValue::Dword(1))
        );
        let active: Vec<String> = journal
            .active_registry()
            .unwrap()
            .into_iter()
            .map(|r| r.value_name)
            .collect();
        assert_eq!(active, ["Current"]);
        let former = journal
            .all_registry()
            .unwrap()
            .into_iter()
            .find(|r| r.value_name == "Former")
            .unwrap();
        assert!(!former.active && former.reverted_at.is_some());
        assert_eq!(
            journal.summary().unwrap().sessions,
            2,
            "the earlier version's session and this apply's"
        );
    }

    #[test]
    fn former_only_apply_is_applied_and_raises_the_restart() {
        let _sandbox = former_sandbox();
        let target = format!(r"HKCU\{FORMER_SANDBOX}");
        let expected = vec![
            format!("{target}\\Former: restored to not set (no longer part of this tweak)"),
            format!("{target}\\Current: already set"),
        ];
        // The current value is already in place; only the former value changes.
        let prepare = |journal: &Journal| {
            delete_sandbox_tree(FORMER_SANDBOX).unwrap();
            record_earlier_change(journal, FORMER_SANDBOX, "Former", None);
            set_sandbox_dword(FORMER_SANDBOX, "Former", 0);
            set_sandbox_dword(FORMER_SANDBOX, "Current", 1);
        };

        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        prepare(&journal);
        let safety = test_safety(journal.clone(), "engine unit", false);
        let TweakApply { result, written } = apply_tweak(
            &safety,
            &FORMER_SANDBOX_TWEAK,
            &TaskConnection::new(),
            &Requirements::new(ALL_MET),
        );
        assert_eq!(result.outcome, ItemOutcome::Applied, "{:?}", result.details);
        assert_eq!(result.details, expected);
        assert_eq!(written.len(), 1, "only the restored value was written");
        match &written[0] {
            WrittenTarget::Former { target, original } => {
                assert_eq!(*target, former_target());
                assert_eq!(*original, None, "the value did not exist");
            }
            WrittenTarget::Action(a) => panic!("{a:?} was already set"),
        }
        assert_eq!(
            read_value(Hive::CurrentUser, FORMER_SANDBOX, "Former").unwrap(),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        prepare(&journal);
        let engine = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let safety = test_safety(journal.clone(), "engine unit", false);
        let report = apply_own(&engine, &safety, former_selection());
        assert_eq!(report.results[0].outcome, ItemOutcome::Applied);
        assert_eq!(report.results[0].details, expected);
        assert_eq!(report.restart, RestartNeed::SignOut);
        // The current value's baseline is recorded although it was already set; the former
        // value's record is reverted.
        let active: Vec<String> = journal
            .active_registry()
            .unwrap()
            .into_iter()
            .map(|r| r.value_name)
            .collect();
        assert_eq!(active, ["Current"]);
    }

    #[test]
    fn dry_run_plans_the_former_restore_and_opens_no_session() {
        let _sandbox = former_sandbox();
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier_change(&journal, FORMER_SANDBOX, "Former", Some(7));
        set_sandbox_dword(FORMER_SANDBOX, "Former", 0);
        set_sandbox_dword(FORMER_SANDBOX, "Current", 1);
        let sessions = journal.summary().unwrap().sessions;
        let engine = Engine::with_system(journal.clone(), test_system(ALL_MET, no_refresh));
        let options = ApplyOptions {
            restore_point: RestorePointPolicy::Skip,
            dry_run: true,
        };

        let report = engine
            .apply_selected(
                former_selection(),
                "apply: selftest.former".into(),
                &options,
            )
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.session_id, None);
        let result = &report.results[0];
        assert_eq!(result.outcome, ItemOutcome::Planned, "{:?}", result.details);
        assert_eq!(
            result.details[0],
            format!(
                r"restore HKCU\{FORMER_SANDBOX}\Former to 0x00000007 (7) (no longer part of this tweak)"
            )
        );
        assert!(
            result.details[1].starts_with(&format!(r"HKCU\{FORMER_SANDBOX}\Current = ")),
            "{:?}",
            result.details
        );
        assert_eq!(journal.summary().unwrap().sessions, sessions);
        assert_eq!(journal.summary().unwrap().registry_active, 1);
        assert_eq!(
            read_value(Hive::CurrentUser, FORMER_SANDBOX, "Former").unwrap(),
            Some(RegValue::Dword(0)),
            "a dry run changes nothing"
        );

        // Without a former record the applied tweak is already applied.
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_system(temp_journal(&dir), test_system(ALL_MET, no_refresh));
        let report = engine
            .apply_selected(
                former_selection(),
                "apply: selftest.former".into(),
                &options,
            )
            .unwrap();
        assert_eq!(report.results[0].outcome, ItemOutcome::AlreadyApplied);
    }

    /// Written by the test of a former value that cannot be restored.
    const FORMER_FAIL_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitFormerFail";
    /// A key whose last name is longer than the registry allows (255 characters), so a value
    /// cannot be written under it.
    const UNWRITABLE_SANDBOX: &str = concat!(
        r"Software\PCOptimizer\SelfTest\EngineUnitFormerFail\",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );

    static FAILING_FORMER_TWEAK: Tweak = Tweak {
        id: "selftest.former_fails",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Sandbox value with a former value that cannot be restored",
        description: "A value under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Registry(reg_dword(
            Hive::CurrentUser,
            FORMER_FAIL_SANDBOX,
            "Current",
            1,
        ))],
        former: &[Former {
            action: reg_dword(Hive::CurrentUser, UNWRITABLE_SANDBOX, "Former", 0),
            reason: FORMER_REASON,
        }],
    };

    #[test]
    fn failed_former_restore_fails_the_item_and_still_applies_the_actions() {
        let _cleanup = SandboxKey(FORMER_FAIL_SANDBOX);
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        record_earlier_change(&journal, UNWRITABLE_SANDBOX, "Former", Some(7));
        let safety = test_safety(journal.clone(), "engine unit", false);
        let TweakApply { result, written } = apply_tweak(
            &safety,
            &FAILING_FORMER_TWEAK,
            &TaskConnection::new(),
            &Requirements::new(ALL_MET),
        );
        assert_eq!(result.outcome, ItemOutcome::Failed, "{:?}", result.details);
        assert_eq!(result.details.len(), 2, "{:?}", result.details);
        assert!(
            result.details[0].starts_with(&format!(
                r"HKCU\{UNWRITABLE_SANDBOX}\Former: could not be restored ("
            )),
            "{}",
            result.details[0]
        );
        assert_eq!(
            result.details[1],
            format!(r"HKCU\{FORMER_FAIL_SANDBOX}\Current: applied")
        );
        assert!(
            matches!(written.as_slice(), [WrittenTarget::Action(_)]),
            "only the current value is read back"
        );
        let active: Vec<String> = journal
            .active_registry()
            .unwrap()
            .into_iter()
            .map(|r| r.value_name)
            .collect();
        assert_eq!(
            active.len(),
            2,
            "the former record stays active: {active:?}"
        );
        assert_eq!(
            read_value(Hive::CurrentUser, FORMER_FAIL_SANDBOX, "Current").unwrap(),
            Some(RegValue::Dword(1))
        );
        assert!(!exists(Hive::CurrentUser, UNWRITABLE_SANDBOX).unwrap_or(false));
    }

    #[test]
    fn category_status_ignores_unavailable_items_with_records() {
        let item = |id: &str, state: ItemState| ScanItem {
            id: id.to_string(),
            kind: ItemKind::Tweak,
            category: Category::Privacy,
            title: id.to_string(),
            description: String::new(),
            risk: Risk::Low,
            recommended: true,
            state,
            restart: RestartNeed::None,
            actions: Vec::new(),
            revertible: false,
            note: None,
            topic: Some(Topic::Activity),
            unavailable: None,
            undo_limit: None,
            former: Vec::new(),
            mode_undo: true,
            drift: Vec::new(),
        };
        let counts = |items: &[ScanItem]| {
            let s = category_status(Category::Privacy, items);
            (
                s.items,
                s.applied,
                s.recommended,
                s.recommended_applied,
                s.active,
            )
        };
        let mut recorded_unmet = item("selftest.unmet", ItemState::Applied);
        recorded_unmet.revertible = true;
        recorded_unmet.unavailable = Some(Unavailable::WindowsVersion);
        assert_eq!(
            counts(std::slice::from_ref(&recorded_unmet)),
            (1, 1, 0, 0, false),
            "an item that is not offered never makes the mode look on"
        );
        let applied = item("selftest.applied", ItemState::Applied);
        assert_eq!(
            counts(&[recorded_unmet.clone(), applied.clone()]),
            (2, 2, 1, 1, true)
        );
        let missing = item("selftest.missing", ItemState::NotApplied);
        assert_eq!(
            counts(&[recorded_unmet, applied.clone(), missing]),
            (3, 2, 2, 1, false)
        );
        // An applied item that still holds a former value is not done.
        let mut former = item("selftest.former", ItemState::Applied);
        former.former = vec![FormerRecord {
            target: format!(r"HKCU\{FORMER_SANDBOX}\Former"),
            original: "not set".to_string(),
            reason: FORMER_REASON.to_string(),
        }];
        assert_eq!(counts(&[applied.clone(), former]), (2, 2, 2, 1, false));
        // Nothing of it on this PC: counted nowhere, as before.
        let absent = item("selftest.absent", ItemState::Unavailable);
        assert_eq!(counts(&[applied, absent]), (2, 1, 1, 1, true));
    }

    #[test]
    fn catalog_json_carries_windows_retired_and_former_targets() {
        let [new_builds, old_builds, pro_only] = &RANGE_SANDBOX_TWEAKS;
        let windows = |t: &Tweak| tweak_entry(t).windows;
        assert_eq!(
            windows(new_builds).as_deref(),
            Some("Windows 11 24H2 (build 26100.3915) or later")
        );
        assert_eq!(
            windows(old_builds).as_deref(),
            Some("Windows 11 23H2 and earlier")
        );
        assert_eq!(
            windows(pro_only).as_deref(),
            Some("Windows 11 Pro, Enterprise or Education")
        );
        assert_eq!(windows(&SANDBOX_TWEAK), None);

        let retired = tweak_entry(&RETIRED_SANDBOX_TWEAK);
        assert_eq!(retired.retired.as_deref(), RETIRED_SANDBOX_TWEAK.retired);
        let former = tweak_entry(&FORMER_SANDBOX_TWEAK);
        assert_eq!(
            former.targets,
            vec![format!(r"HKCU\{FORMER_SANDBOX}\Current")]
        );
        assert_eq!(
            former.former_targets,
            vec![format!(r"HKCU\{FORMER_SANDBOX}\Former")]
        );

        let json = serde_json::to_value(&former).unwrap();
        assert_eq!(
            json["former_targets"],
            serde_json::json!([format!(r"HKCU\{FORMER_SANDBOX}\Former")])
        );
        assert_eq!(json["windows"], serde_json::Value::Null);
        let json = serde_json::to_value(tweak_entry(new_builds)).unwrap();
        assert_eq!(
            json["windows"],
            "Windows 11 24H2 (build 26100.3915) or later"
        );
        let mut old = json;
        for key in ["windows", "retired", "former_targets"] {
            old.as_object_mut().unwrap().remove(key);
        }
        let parsed: CatalogEntry = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.windows, None);
        assert!(parsed.retired.is_none() && parsed.former_targets.is_empty());
    }

    /// Never written: the tweak is only scanned.
    const NOTEPAD_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitNotepad";

    static NOTEPAD_SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.notepad",
        category: Category::Privacy,
        topic: Topic::Ai,
        title: "Sandbox Notepad policy",
        description: "A value under the self-test key that stands in for a Notepad policy.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::NotepadAi),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Registry(reg_dword(
            Hive::CurrentUser,
            NOTEPAD_SANDBOX,
            "DisableAIFeatures",
            1,
        ))],
        former: &[],
    };

    #[test]
    fn packages_lookup_failure_warns_and_keeps_the_tweak() {
        let reqs = Requirements::new(Probes {
            packages: |_| Err(Error::Other("lookup failed".into())),
            ..ALL_MET
        });
        let mut warnings = Vec::new();
        let item = scan_tweak(
            &NOTEPAD_SANDBOX_TWEAK,
            false,
            &TaskConnection::new(),
            &reqs,
            true,
            &mut warnings,
        );
        assert_eq!(item.state, ItemState::NotApplied, "read normally");
        assert_eq!((item.unavailable, item.note.as_deref()), (None, None));
        assert_eq!(
            warnings,
            vec![
                "selftest.notepad: cannot check whether Notepad 11.2503.16 or later is \
                 installed: lookup failed"
                    .to_string()
            ]
        );
        let mut tweaks: Vec<&'static Tweak> = vec![&NOTEPAD_SANDBOX_TWEAK];
        let mut results = Vec::new();
        withhold_unavailable(&mut tweaks, &mut results, &reqs);
        assert_eq!(tweaks.len(), 1, "a failed check keeps the tweak");
        assert!(results.is_empty());

        // An old Notepad: not on this PC.
        let old = Requirements::new(Probes {
            packages: |_| Ok(vec![[11, 2402, 22, 0]]),
            ..ALL_MET
        });
        let item = scan_tweak(
            &NOTEPAD_SANDBOX_TWEAK,
            false,
            &TaskConnection::new(),
            &old,
            true,
            &mut warnings,
        );
        assert_eq!(item.unavailable, Some(Unavailable::NotOnThisPc));
        assert_eq!(
            item.note.as_deref(),
            Some("Notepad 11.2503.16 or later is not installed.")
        );
        assert!(!exists(Hive::CurrentUser, NOTEPAD_SANDBOX).unwrap());
    }
}
