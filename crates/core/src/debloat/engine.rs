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
//! A tweak whose requirement (Office, Edge, GPU scheduling support) is known to be missing
//! scans as unavailable with a note and is skipped on apply before a session is opened or
//! anything is recorded; while it has an active journal record it is read normally, so it
//! stays undoable. Settings Windows reads only at sign-in (the mouse) are pushed to the
//! running session after they are applied.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::appx::{self, AppxPackage};
use super::catalog::{
    self, Action, Category, Requirement, RestartNeed, Risk, Tweak, GRAPHICS_DRIVERS,
};
use super::live::{self, LiveSetting};
use super::requirements::{gpu_note, Availability, Probes, Requirements, SYSTEM_PROBES};
use super::scheduled_tasks::{self, TaskConnection};
use super::{power, registry, services, ActionState, ActionStatus};
use crate::safety::rollback::{self, RollbackFilter, RollbackReport};
use crate::safety::state_log::{AppxRecord, Journal};
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
}

/// Every tweak followed by every bloatware package pattern, in catalog order.
pub fn catalog_view() -> Vec<CatalogEntry> {
    let tweaks = catalog::TWEAKS.iter().map(|t| CatalogEntry {
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
    });
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
    });
    tweaks.chain(packages).collect()
}

/// The parts of this PC the engine reaches through a seam (requirement probes, the live
/// settings a tweak feeds and their push to the session); tests substitute fakes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EngineSystem {
    pub(crate) probes: Probes,
    pub(crate) refresh: fn(LiveSetting) -> Result<()>,
    /// The live settings the values of a tweak feed.
    pub(crate) live_settings: fn(&Tweak) -> Vec<LiveSetting>,
}

impl EngineSystem {
    pub(crate) const LIVE: EngineSystem = EngineSystem {
        probes: SYSTEM_PROBES,
        refresh: live::refresh,
        live_settings: live::settings_of,
    };
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

    // ───────────────────────────── scan ─────────────────────────────

    pub fn scan(&self) -> Result<ScanReport> {
        let started = Instant::now();
        let mut warnings = Vec::new();
        let has_battery = power::has_system_battery();
        let active = ActiveRecords::load(&self.journal)?;
        let tasks = TaskConnection::new();
        let reqs = Requirements::new(self.system.probes);
        let mut items: Vec<ScanItem> = catalog::TWEAKS
            .iter()
            .map(|t| {
                let revertible = active.covers(t);
                // A tweak with an active record is read normally, so it stays undoable.
                let mut item =
                    scan_tweak(t, has_battery, &tasks, &reqs, !revertible, &mut warnings);
                item.revertible = revertible;
                item.note = item
                    .note
                    .take()
                    .or_else(|| active.reenabled_note(t, &item.actions))
                    .or_else(|| requirement_note(t, &reqs, revertible));
                item
            })
            .collect();
        let mut appx_unavailable = None;
        items.extend(self.scan_appx(&active.appx, &mut warnings, &mut appx_unavailable)?);
        let categories = Category::ALL
            .iter()
            .map(|&c| category_status(c, &items))
            .collect();
        Ok(ScanReport {
            elevated: is_elevated(),
            has_battery,
            items,
            categories,
            warnings,
            duration_ms: started.elapsed().as_millis() as u64,
            appx_unavailable,
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
        let mut sel = select(ids);
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

        if opts.dry_run {
            let tasks = TaskConnection::new();
            let has_battery = power::has_system_battery();
            let reqs = Requirements::new(self.system.probes);
            let mut warnings = Vec::new();
            for t in &tweaks {
                let item = scan_tweak(t, has_battery, &tasks, &reqs, true, &mut warnings);
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
                        ItemResult {
                            id,
                            outcome: ItemOutcome::AlreadyApplied,
                            details: vec!["not installed".into()],
                        }
                    } else {
                        ItemResult {
                            id,
                            outcome: ItemOutcome::Planned,
                            details: matches
                                .iter()
                                .map(|p| format!("remove {}", p.full_name))
                                .collect(),
                        }
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

        withhold_unavailable(
            &mut tweaks,
            &mut results,
            &Requirements::new(self.system.probes),
        );
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

        let label = format!("apply: {}", ids.join(", "));
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
        Ok(self.apply_selection(safety, sel))
    }

    /// Applies a resolved selection under `safety`; every path that applies ends here, so
    /// [`Engine::apply`] and [`Engine::apply_in`] behave the same once a session exists.
    /// Tweaks with a missing requirement are skipped; the live settings of applied tweaks
    /// are pushed to the running session after the tweaks and before the Store packages.
    fn apply_selection(&self, safety: &Safety, sel: Selection) -> ApplyReport {
        let Selection {
            tweaks,
            appx_names,
            mut results,
        } = sel;
        let tasks = TaskConnection::new();
        let reqs = Requirements::new(self.system.probes);
        let mut restart = RestartNeed::None;
        let mut applied_live: Vec<LiveSetting> = Vec::new();

        for t in &tweaks {
            let result = apply_tweak(safety, t, &tasks, &reqs);
            if result.outcome == ItemOutcome::Applied {
                restart = restart.max(t.restart);
                for setting in (self.system.live_settings)(t) {
                    if !applied_live.contains(&setting) {
                        applied_live.push(setting);
                    }
                }
            }
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

    /// Applies every recommended item of `category` that is not applied yet.
    pub fn apply_category(&self, category: Category, opts: &ApplyOptions) -> Result<ApplyReport> {
        let scan = self.scan()?;
        let ids: Vec<String> = scan
            .items
            .iter()
            .filter(|i| {
                i.category == category
                    && i.recommended
                    && matches!(i.state, ItemState::NotApplied | ItemState::Partial)
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

    /// Journal targets of `ids`: every target of each tweak, and the recorded families of each
    /// removed bloatware package. Unknown ids add nothing.
    pub fn revert_filter(&self, ids: &[String]) -> Result<RollbackFilter> {
        let mut filter = RollbackFilter::default();
        let mut appx_journal = None;
        for id in ids {
            if let Some(t) = catalog::tweak(id) {
                for action in t.actions {
                    match action {
                        Action::Registry(r) => filter.registry.push(registry::target(r)),
                        Action::Service(s) => filter.services.push(s.name.to_string()),
                        Action::ScheduledTask(s) => filter.scheduled_tasks.push(s.path.to_string()),
                        Action::Power(_) => filter.power = true,
                    }
                }
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

/// Note on an installed bloatware package that still has an active removal record.
pub const REINSTALLED_NOTE: &str = "Windows reinstalled this app after it was removed";

/// Targets with an active journal record, loaded once per scan.
struct ActiveRecords {
    registry: HashSet<(Hive, String, String)>,
    services: HashSet<String>,
    /// Lowercase task paths.
    scheduled_tasks: HashSet<String>,
    power: bool,
    appx: Vec<AppxRecord>,
}

impl ActiveRecords {
    fn load(journal: &Journal) -> Result<ActiveRecords> {
        Ok(ActiveRecords {
            registry: journal
                .active_registry()?
                .into_iter()
                .map(|r| {
                    (
                        r.hive,
                        r.key_path.to_ascii_lowercase(),
                        r.value_name.to_ascii_lowercase(),
                    )
                })
                .collect(),
            services: journal
                .active_services()?
                .into_iter()
                .map(|r| r.name.to_ascii_lowercase())
                .collect(),
            scheduled_tasks: journal
                .active_scheduled_tasks()?
                .into_iter()
                .map(|r| r.path.to_ascii_lowercase())
                .collect(),
            power: !journal.active_power()?.is_empty(),
            appx: journal.active_appx()?,
        })
    }

    /// True when any action of `t` has an active record.
    fn covers(&self, t: &Tweak) -> bool {
        t.actions.iter().any(|a| match a {
            Action::Registry(r) => self.registry.contains(&(
                r.hive,
                r.path.to_ascii_lowercase(),
                r.name.to_ascii_lowercase(),
            )),
            Action::Service(s) => self.services.contains(&s.name.to_ascii_lowercase()),
            Action::ScheduledTask(s) => self.scheduled_tasks.contains(&s.path.to_ascii_lowercase()),
            Action::Power(_) => self.power,
        })
    }

    /// Note for a tweak whose scheduled tasks this tool turned off (they have an active
    /// record) but that are no longer at the target, because Windows or the user turned
    /// them back on. `actions` are the tweak's scanned statuses, in action order.
    fn reenabled_note(&self, t: &Tweak, actions: &[ActionStatus]) -> Option<String> {
        let count = t
            .actions
            .iter()
            .zip(actions)
            .filter(|(action, status)| {
                matches!(action, Action::ScheduledTask(s)
                    if self.scheduled_tasks.contains(&s.path.to_ascii_lowercase()))
                    && status.state == ActionState::NotApplied
            })
            .count();
        match count {
            0 => None,
            1 => Some(
                "1 scheduled task this tool turned off was turned back on; apply again to \
                 turn it off."
                    .to_string(),
            ),
            n => Some(format!(
                "{n} scheduled tasks this tool turned off were turned back on; apply again \
                 to turn them off."
            )),
        }
    }
}

/// Package Name part of a family name (`Name_PublisherId`). Package names cannot contain
/// underscores, so the last underscore separates the publisher id.
pub fn package_name_from_family(family: &str) -> &str {
    family.rsplit_once('_').map_or(family, |(name, _)| name)
}

/// Reads one action's state. Scheduled tasks are read through `tasks`, which connects to
/// Task Scheduler on first use.
fn action_status(action: &Action, tasks: &TaskConnection) -> Result<ActionStatus> {
    match action {
        Action::Registry(r) => registry::status(r),
        Action::Service(s) => services::status(s),
        Action::ScheduledTask(s) => scheduled_tasks::status_in(tasks, s),
        Action::Power(p) => power::status(*p),
    }
}

fn action_label(action: &Action) -> String {
    match action {
        Action::Registry(r) => registry::describe(r),
        Action::Service(s) => services::describe(s),
        Action::ScheduledTask(s) => scheduled_tasks::describe(s),
        Action::Power(p) => power::describe(*p),
    }
}

/// Reads the state of a tweak. With `check_requirement`, a tweak whose requirement is known
/// to be missing is reported unavailable with the reason as its note, without reading
/// anything; a requirement that cannot be checked adds a warning routed to the item
/// (`<id>: …`) and the state is read normally.
fn scan_tweak(
    t: &'static Tweak,
    has_battery: bool,
    tasks: &TaskConnection,
    reqs: &Requirements,
    check_requirement: bool,
    warnings: &mut Vec<String>,
) -> ScanItem {
    let power_on_battery = has_battery && t.actions.iter().any(|a| matches!(a, Action::Power(_)));
    let item = |actions: Vec<ActionStatus>, state: ItemState, note: Option<String>| ScanItem {
        id: t.id.to_string(),
        kind: ItemKind::Tweak,
        category: t.category,
        title: t.title.to_string(),
        description: t.description.to_string(),
        risk: t.risk,
        recommended: t.default_on && !power_on_battery,
        state,
        restart: t.restart,
        actions,
        revertible: false,
        note,
    };

    if let Some(req) = t.requires.filter(|_| check_requirement) {
        match reqs.check(req) {
            Availability::Met => {}
            Availability::Missing(text) => {
                let actions = t
                    .actions
                    .iter()
                    .map(|a| {
                        ActionStatus::new(
                            ActionState::Unavailable,
                            format!("{}: {text}", action_label(a)),
                        )
                    })
                    .collect();
                return item(actions, ItemState::Unavailable, Some(text.to_string()));
            }
            Availability::Unknown(e) => {
                warn!(item = t.id, error = %e, "cannot check the requirement");
                warnings.push(format!(
                    "{}: cannot check whether {}: {e}",
                    t.id,
                    req.subject()
                ));
            }
        }
    }

    let actions: Vec<ActionStatus> = t
        .actions
        .iter()
        .map(|a| {
            action_status(a, tasks).unwrap_or_else(|e| {
                warn!(item = t.id, error = %e, "cannot read state");
                warnings.push(format!("{}: cannot read {}: {e}", t.id, action_label(a)));
                ActionStatus::new(
                    ActionState::Unavailable,
                    format!("{}: {e}", action_label(a)),
                )
            })
        })
        .collect();
    let state = combine_states(&actions);
    item(actions, state, None)
}

/// Note a tweak's requirement adds to its scanned item: for GPU scheduling, what the GPU
/// runs with now when that differs from the stored setting; for a missing requirement of an
/// item that still has journal records, that undo is still possible.
fn requirement_note(t: &Tweak, reqs: &Requirements, revertible: bool) -> Option<String> {
    let req = t.requires?;
    match reqs.check(req) {
        Availability::Met if req == Requirement::GpuScheduling => {
            let stored = match read_value(Hive::LocalMachine, GRAPHICS_DRIVERS, "HwSchMode") {
                Ok(Some(RegValue::Dword(v))) => Some(v),
                _ => None,
            };
            gpu_note(stored, reqs.gpu()?).map(str::to_string)
        }
        Availability::Missing(text) if revertible => Some(format!(
            "{text} Undo removes the settings {} stored.",
            crate::APP_NAME
        )),
        _ => None,
    }
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
    }
}

fn category_status(category: Category, items: &[ScanItem]) -> CategoryStatus {
    let in_cat: Vec<&ScanItem> = items.iter().filter(|i| i.category == category).collect();
    let recommended: Vec<&&ScanItem> = in_cat
        .iter()
        .filter(|i| i.recommended && i.state != ItemState::Unavailable)
        .collect();
    let recommended_applied = recommended
        .iter()
        .filter(|i| i.state == ItemState::Applied)
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
    ItemResult {
        id: id.to_string(),
        outcome: ItemOutcome::Failed,
        details: vec![reason.into()],
    }
}

fn plan_result(item: &ScanItem) -> ItemResult {
    let outcome = match item.state {
        ItemState::Applied => ItemOutcome::AlreadyApplied,
        ItemState::Unavailable => ItemOutcome::Skipped,
        ItemState::NotApplied | ItemState::Partial => ItemOutcome::Planned,
    };
    ItemResult {
        id: item.id.clone(),
        outcome,
        details: item.actions.iter().map(|a| a.detail.clone()).collect(),
    }
}

/// True when the tweak writes a value under HKEY_CURRENT_USER, which has to land in the
/// signed-in user's profile.
pub(crate) fn touches_current_user(t: &Tweak) -> bool {
    t.actions
        .iter()
        .any(|a| matches!(a, Action::Registry(r) if r.hive == Hive::CurrentUser))
}

fn skipped(id: &str, reason: &str) -> ItemResult {
    ItemResult {
        id: id.to_string(),
        outcome: ItemOutcome::Skipped,
        details: vec![reason.to_string()],
    }
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

/// Moves every tweak whose requirement is known to be missing from the selection into
/// `results` as skipped with the missing text. A requirement that cannot be checked keeps its
/// tweak selected.
fn withhold_unavailable(
    tweaks: &mut Vec<&'static Tweak>,
    results: &mut Vec<ItemResult>,
    reqs: &Requirements,
) {
    tweaks.retain(|t| match t.requires.map(|req| reqs.check(req)) {
        Some(Availability::Missing(text)) => {
            results.push(skipped(t.id, text));
            false
        }
        _ => true,
    });
}

/// Applies every action of a tweak, continuing past failures so one broken value does not
/// block the rest. Applied wins over already-applied; any failure marks the item failed.
/// A tweak whose requirement is known to be missing is skipped first, before anything is
/// checked or recorded. A tweak with an HKCU value is skipped whole, with nothing changed,
/// when [`Safety::ensure_interactive_user`] fails, so it is never left half applied.
/// Scheduled tasks are changed through `tasks`, which connects to Task Scheduler on first
/// use.
fn apply_tweak(
    safety: &Safety,
    t: &'static Tweak,
    tasks: &TaskConnection,
    reqs: &Requirements,
) -> ItemResult {
    if let Some(req) = t.requires {
        if let Availability::Missing(text) = reqs.check(req) {
            return skipped(t.id, text);
        }
    }
    if touches_current_user(t) {
        if let Err(e) = safety.ensure_interactive_user() {
            return skipped(t.id, &e.to_string());
        }
    }
    let mut details = Vec::new();
    let mut any_applied = false;
    let mut any_failed = false;
    let mut all_skipped = true;

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
    ItemResult {
        id: t.id.to_string(),
        outcome,
        details,
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
            results.push(ItemResult {
                id,
                outcome: ItemOutcome::AlreadyApplied,
                details: vec!["not installed".into()],
            });
            continue;
        }
        let slot = results.len();
        results.push(ItemResult {
            id,
            outcome: ItemOutcome::AlreadyApplied,
            details: Vec::new(),
        });
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
    use crate::debloat::catalog::{RegData, RegistryAction};
    use crate::debloat::requirements::GpuScheduling;
    use crate::safety::state_log::{JournalTable, NewRegistryRecord, NewScheduledTaskRecord};
    use crate::safety::test_safety;
    use crate::win::registry::{delete_sandbox_tree, exists};

    fn status(state: ActionState) -> ActionStatus {
        ActionStatus::new(state, "")
    }

    /// Probes that report every requirement as met.
    const ALL_MET: Probes = Probes {
        office: || Ok(true),
        edge: || Ok(true),
        gpu: || {
            Ok(GpuScheduling {
                supported: true,
                enabled: true,
            })
        },
    };

    /// Probes of a PC without Office.
    const NO_OFFICE: Probes = Probes {
        office: || Ok(false),
        ..ALL_MET
    };

    /// A live-settings push that must not run: the tweaks it is given feed no live setting.
    fn no_refresh(setting: LiveSetting) -> Result<()> {
        panic!("{setting:?} was pushed to the session")
    }

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
        title: "Sandbox Office policies",
        description: "Two values under the self-test key that stand in for Office policies.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
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
    };

    static OFFICE_APPLY_TWEAK: Tweak = Tweak {
        id: "selftest.office_apply",
        category: Category::Privacy,
        title: "Sandbox Office policies to apply",
        description: "Two values under the self-test key that stand in for Office policies.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
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
    };

    /// Written by the test of the live-settings push.
    const LIVE_SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnitLive";

    /// Two tweaks that need Office, with one value each in the self-test sandbox.
    static LIVE_SANDBOX_TWEAKS: [Tweak; 2] = [
        Tweak {
            id: "selftest.live_first",
            category: Category::Gaming,
            title: "Sandbox live value, first",
            description: "A value under the self-test key that stands in for a mouse value.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: Some(Requirement::Office),
            actions: &[Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: LIVE_SANDBOX,
                name: "First",
                data: RegData::Dword(1),
            })],
        },
        Tweak {
            id: "selftest.live_second",
            category: Category::Gaming,
            title: "Sandbox live value, second",
            description: "A value under the self-test key that stands in for a mouse value.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires: Some(Requirement::Office),
            actions: &[Action::Registry(RegistryAction {
                hive: Hive::CurrentUser,
                path: LIVE_SANDBOX,
                name: "Second",
                data: RegData::Dword(2),
            })],
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
        assert_eq!(item.actions.len(), 2);
        for action in &item.actions {
            assert_eq!(action.state, ActionState::Unavailable);
            assert!(
                action.detail.ends_with(&format!(": {text}")),
                "{}",
                action.detail
            );
        }
        assert_eq!(item.recommended, OFFICE_SANDBOX_TWEAK.default_on);
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
        let result = apply_tweak(
            &safety,
            &OFFICE_SANDBOX_TWEAK,
            &TaskConnection::new(),
            &Requirements::new(NO_OFFICE),
        );
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
        assert_eq!(requirement_note(&OFFICE_SANDBOX_TWEAK, &reqs, true), None);
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
        let engine = Engine::with_system(
            journal.clone(),
            EngineSystem {
                probes: NO_OFFICE,
                refresh: no_refresh,
                live_settings: live::settings_of,
            },
        );
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
        let text = Requirement::Office.missing_text();
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

        let without = Engine::with_system(
            journal.clone(),
            EngineSystem {
                probes: NO_OFFICE,
                refresh: no_refresh,
                live_settings: live::settings_of,
            },
        );
        let safety = test_safety(journal.clone(), "engine unit", false);
        let report = without.apply_selection(&safety, selection());
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].outcome, ItemOutcome::Skipped);
        assert_eq!(
            report.results[0].details,
            vec![Requirement::Office.missing_text().to_string()]
        );
        assert_eq!(journal.summary().unwrap().registry_total, 0);
        assert!(!exists(Hive::CurrentUser, OFFICE_APPLY_SANDBOX).unwrap());

        let with = Engine::with_system(
            journal.clone(),
            EngineSystem {
                probes: ALL_MET,
                refresh: no_refresh,
                live_settings: live::settings_of,
            },
        );
        let report = with.apply_selection(&safety, selection());
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
            EngineSystem {
                probes: Probes {
                    edge: || Ok(false),
                    ..ALL_MET
                },
                refresh: no_refresh,
                live_settings: live::settings_of,
            },
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
                    probes,
                    refresh,
                    live_settings: |_| vec![LiveSetting::Mouse],
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
        let report = engine(NO_OFFICE, count_push).apply_selection(&safety, selection());
        assert_eq!(outcomes(&report), vec![ItemOutcome::Skipped; 2]);
        assert_eq!(pushes(), 0);
        assert!(op_rows(&journal).is_empty());

        // Two applied tweaks that feed one setting push it once, after their values are
        // written.
        let report = engine(ALL_MET, count_push).apply_selection(&safety, selection());
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
        let report = engine(ALL_MET, count_push).apply_selection(&safety, selection());
        assert_eq!(outcomes(&report), vec![ItemOutcome::AlreadyApplied; 2]);
        assert_eq!(pushes(), 1);
        assert_eq!(refresh_rows(&journal), vec![pushed.clone()]);

        // A push that fails reaches the report: the values are stored, so signing out
        // applies them.
        delete_sandbox_tree(LIVE_SANDBOX).unwrap();
        let report = engine(ALL_MET, |_| Err(Error::Other("refused".into())))
            .apply_selection(&safety, selection());
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
        // A journal record (no registry write) makes the tweak revertible.
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
        journal.end_session(session).unwrap();

        let engine = Engine::with_system(
            journal.clone(),
            EngineSystem {
                probes: NO_OFFICE,
                refresh: no_refresh,
                live_settings: live::settings_of,
            },
        );
        let report = engine.scan().unwrap();
        let text = Requirement::Office.missing_text();
        let recorded = report.item("privacy.office_telemetry").unwrap();
        assert!(recorded.revertible);
        assert_ne!(recorded.state, ItemState::Unavailable, "read normally");
        assert_eq!(
            recorded.note,
            Some(format!("{text} Undo removes the settings Cairn stored."))
        );
        for id in [
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
        ] {
            let item = report.item(id).unwrap();
            assert_eq!(item.state, ItemState::Unavailable, "{id}");
            assert_eq!(item.note.as_deref(), Some(text), "{id}");
            assert!(!item.revertible, "{id}");
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
        assert_eq!(requires("privacy.office_telemetry"), "office");
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
    fn reenabled_note_counts_only_recorded_enabled_tasks() {
        use ActionState::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = temp_journal(&dir);
        let ceip = catalog::tweak("privacy.ceip_tasks").unwrap();
        let paths = task_paths(ceip);
        // The first three tasks were turned off by this tool; the last two were not.
        record_tasks(&journal, &paths[..3]);
        let active = ActiveRecords::load(&journal).unwrap();
        let note = |states: [ActionState; 5]| active.reenabled_note(ceip, &states.map(status));

        assert_eq!(
            note([NotApplied, Applied, Applied, NotApplied, NotApplied]),
            Some(
                "1 scheduled task this tool turned off was turned back on; apply again to \
                 turn it off."
                    .to_string()
            )
        );
        assert_eq!(
            note([NotApplied, NotApplied, Unavailable, NotApplied, Applied]),
            Some(
                "2 scheduled tasks this tool turned off were turned back on; apply again to \
                 turn them off."
                    .to_string()
            )
        );
        assert_eq!(
            note([Applied, Applied, Unavailable, NotApplied, NotApplied]),
            None
        );
        let registry_only = catalog::tweak("privacy.ceip").unwrap();
        assert_eq!(
            active.reenabled_note(registry_only, &[status(NotApplied)]),
            None
        );
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
        let result = apply_tweak(&safety, ceip, &TaskConnection::failed(error), &reqs);
        assert_eq!(result.outcome, ItemOutcome::Failed);
        assert_eq!(result.details.len(), 5);
        assert!(result
            .details
            .iter()
            .all(|d| d.ends_with(&format!(": failed ({error})"))));
        assert_eq!(journal.summary().unwrap().scheduled_tasks_total, 0);
    }

    const SANDBOX: &str = r"Software\PCOptimizer\SelfTest\EngineUnit";

    /// A per-user tweak whose values live in the self-test sandbox.
    static SANDBOX_TWEAK: Tweak = Tweak {
        id: "selftest.per_user",
        category: Category::Privacy,
        title: "Sandbox values",
        description: "Two values under the self-test key.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
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
    };

    #[test]
    fn per_user_tweaks_are_skipped_whole_when_the_user_check_fails() {
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
            );
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
}
