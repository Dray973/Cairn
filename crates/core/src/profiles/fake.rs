//! In-memory [`ProfileSystem`] for the profile tests, with fixture builders over real catalog
//! ids and generic names.
//!
//! Reads are recorded in `reads`, mutations in `calls`. Every mutation first asserts that the
//! `apply_profile` "started" row is already in the journal, and none of them touches the
//! system. `revert_filter` delegates to an [`Engine`] over the temporary journal, which reads
//! only the catalog and the journal.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::plan::source_location;
use super::step::{
    MaintenanceChoice, SettingStep, StepOutcome, StepReason, StepResult, StepStatus,
    WindowsUpdateChoice,
};
use super::system::ProfileSystem;
use super::OP_APPLY_PROFILE;
use crate::debloat::catalog::{self, Action, RestartNeed, Tweak};
use crate::debloat::{
    ActionState, ActionStatus, ApplyReport, Category, Engine, ItemKind, ItemOutcome, ItemResult,
    ItemState, ScanItem, ScanReport,
};
use crate::network::{
    canonical_guid, Adapter, AdapterKind, ChangeOutcome, DnsChange, DnsConfig, DnsMode, DnsReport,
    DnsRequest, IpFamily, NetworkReport, PRESETS,
};
use crate::safety::rollback::RollbackFilter;
use crate::safety::state_log::{Journal, NewAppxRecord, NewRegistryRecord};
use crate::safety::{MutationOutcome, Safety};
use crate::startup::{StartupEntry, StartupSource};
use crate::{Error, Result};

pub(crate) struct FakeProfiles {
    _dir: tempfile::TempDir,
    pub journal: Arc<Journal>,
    engine: Engine,
    pub elevated: bool,
    /// The refusal `per_user_allowed` returns; None allows per-user changes.
    pub other_account: Option<String>,
    /// `per_user_allowed` refuses only from this call on (1 = the first call).
    pub other_account_from_call: usize,
    per_user_calls: Cell<usize>,
    pub scan: std::result::Result<ScanReport, String>,
    pub startup: std::result::Result<Vec<StartupEntry>, String>,
    pub network: std::result::Result<NetworkReport, String>,
    /// Canonical adapter GUID -> what planning its DNS change reports.
    pub dns_plans: HashMap<String, DnsReport>,
    pub wu_steps: std::result::Result<Vec<SettingStep>, String>,
    pub wu_current: std::result::Result<WindowsUpdateChoice, String>,
    /// Row keys of the Windows Update settings with an active journal record.
    pub wu_recorded: std::result::Result<Vec<String>, String>,
    pub wu_filter: RollbackFilter,
    pub maintenance_steps: std::result::Result<Vec<SettingStep>, String>,
    pub maintenance_current: std::result::Result<Option<MaintenanceChoice>, String>,
    pub maintenance_filter: RollbackFilter,
    /// Row keys whose change fails.
    pub fail: HashSet<String>,
    /// Row keys of tweaks and Store apps whose change records a baseline and then fails: a
    /// tweak's first registry value, or the app's package.
    pub partial: HashSet<String>,
    /// Startup ids whose toggle reports this outcome instead of Applied.
    pub startup_outcomes: HashMap<String, MutationOutcome>,
    pub reads: RefCell<Vec<&'static str>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeProfiles {
    /// An elevated system with an empty scan, no startup entries, no adapters and no
    /// Windows Update or maintenance steps.
    pub(crate) fn new() -> FakeProfiles {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        FakeProfiles {
            _dir: dir,
            engine: Engine::new(journal.clone()),
            journal,
            elevated: true,
            other_account: None,
            other_account_from_call: 1,
            per_user_calls: Cell::new(0),
            scan: Ok(scan_report(Vec::new())),
            startup: Ok(Vec::new()),
            network: Ok(network_report(Vec::new())),
            dns_plans: HashMap::new(),
            wu_steps: Ok(Vec::new()),
            wu_current: Ok(WindowsUpdateChoice::default()),
            wu_recorded: Ok(Vec::new()),
            wu_filter: RollbackFilter::default(),
            maintenance_steps: Ok(Vec::new()),
            maintenance_current: Ok(None),
            maintenance_filter: RollbackFilter::default(),
            fail: HashSet::new(),
            partial: HashSet::new(),
            startup_outcomes: HashMap::new(),
            reads: RefCell::new(Vec::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// A session of this fake's journal, as `apply` would open it.
    pub(crate) fn session(&self, label: &str) -> Safety {
        crate::safety::test_safety(self.journal.clone(), label, false)
    }

    fn read(&self, what: &'static str) {
        self.reads.borrow_mut().push(what);
    }

    fn assert_started(&self) {
        let ops = self.journal.ops(1000).unwrap();
        assert!(
            ops.iter()
                .any(|o| o.op == OP_APPLY_PROFILE && o.outcome == "started"),
            "a change ran before the apply_profile started row"
        );
    }

    fn call(&self, text: String) {
        self.assert_started();
        self.calls.borrow_mut().push(text);
    }
}

impl ProfileSystem for FakeProfiles {
    fn elevated(&self) -> bool {
        self.elevated
    }

    fn per_user_allowed(&self) -> Result<()> {
        self.read("per_user_allowed");
        let n = self.per_user_calls.get() + 1;
        self.per_user_calls.set(n);
        match &self.other_account {
            Some(reason) if n >= self.other_account_from_call => Err(Error::Other(reason.clone())),
            _ => Ok(()),
        }
    }

    fn journal(&self) -> &Journal {
        &self.journal
    }

    fn scan(&self) -> Result<ScanReport> {
        self.read("scan");
        self.scan.clone().map_err(Error::Other)
    }

    fn startup(&self) -> Result<Vec<StartupEntry>> {
        self.read("startup");
        self.startup.clone().map_err(Error::Other)
    }

    fn network(&self) -> Result<NetworkReport> {
        self.read("network");
        self.network.clone().map_err(Error::Other)
    }

    fn plan_dns(&self, adapter_id: &str, _request: &DnsRequest) -> Result<DnsReport> {
        self.read("plan_dns");
        self.dns_plans
            .get(&canonical_guid(adapter_id))
            .cloned()
            .ok_or_else(|| Error::Other("the adapter's DNS settings could not be read".into()))
    }

    fn wu_current(&self) -> Result<WindowsUpdateChoice> {
        self.read("wu_current");
        self.wu_current.clone().map_err(Error::Other)
    }

    fn wu_recorded(&self) -> Result<Vec<String>> {
        self.read("wu_recorded");
        self.wu_recorded.clone().map_err(Error::Other)
    }

    fn wu_plan(&self, _want: &WindowsUpdateChoice) -> Result<Vec<SettingStep>> {
        self.read("wu_plan");
        self.wu_steps.clone().map_err(Error::Other)
    }

    fn maintenance_current(&self) -> Result<Option<MaintenanceChoice>> {
        self.read("maintenance_current");
        self.maintenance_current.clone().map_err(Error::Other)
    }

    fn maintenance_plan(&self, _want: &MaintenanceChoice) -> Result<Vec<SettingStep>> {
        self.read("maintenance_plan");
        self.maintenance_steps.clone().map_err(Error::Other)
    }

    fn apply_items(&self, safety: &Safety, ids: &[String]) -> Result<ApplyReport> {
        self.call(format!("apply_items {}", ids.join(",")));
        let mut restart = RestartNeed::None;
        let mut results = Vec::new();
        for id in ids {
            let (key, appx) = match catalog::appx_name_from_item_id(id) {
                Some(name) => (format!("app:{name}"), Some(name)),
                None => (format!("tweak:{id}"), None),
            };
            if self.fail.contains(&key) {
                results.push(ItemResult {
                    id: id.clone(),
                    outcome: ItemOutcome::Failed,
                    details: vec!["access denied".into()],
                });
                continue;
            }
            let partial = self.partial.contains(&key);
            if let Some(name) = appx {
                safety.record_appx(&NewAppxRecord {
                    package_full_name: format!("{name}_1.0.0.0_x64__8wekyb3d8bbwe"),
                    package_family: format!("{name}_8wekyb3d8bbwe"),
                    install_location: format!(
                        r"C:\Program Files\WindowsApps\{name}_1.0.0.0_x64__8wekyb3d8bbwe"
                    ),
                    all_users: false,
                })?;
            } else if let Some(t) = catalog::tweak(id) {
                if partial {
                    record_first_value(safety, t)?;
                } else {
                    restart = restart.max(t.restart);
                }
            }
            results.push(if partial {
                ItemResult {
                    id: id.clone(),
                    outcome: ItemOutcome::Failed,
                    details: vec![
                        "first change: applied".into(),
                        "next change: failed (access denied)".into(),
                    ],
                }
            } else {
                ItemResult {
                    id: id.clone(),
                    outcome: ItemOutcome::Applied,
                    details: vec!["applied".into()],
                }
            });
        }
        Ok(ApplyReport {
            dry_run: false,
            session_id: Some(safety.session_id()),
            restore_point: None,
            results,
            warnings: Vec::new(),
            restart,
        })
    }

    fn revert_filter(&self, ids: &[String]) -> Result<RollbackFilter> {
        self.engine.revert_filter(ids)
    }

    fn set_startup(&self, _safety: &Safety, id: &str, enabled: bool) -> Result<MutationOutcome> {
        self.call(format!("set_startup {id} {enabled}"));
        if self.fail.contains(&format!("startup:{id}")) {
            return Err(Error::Other("access denied".into()));
        }
        Ok(self
            .startup_outcomes
            .get(id)
            .cloned()
            .unwrap_or(MutationOutcome::Applied))
    }

    fn set_dns(
        &self,
        safety: &Safety,
        adapter_id: &str,
        _request: &DnsRequest,
    ) -> Result<DnsReport> {
        let guid = canonical_guid(adapter_id);
        self.call(format!("set_dns {guid}"));
        let mut report = self
            .dns_plans
            .get(&guid)
            .cloned()
            .ok_or_else(|| Error::Other("network adapter is not on this PC".into()))?;
        let failing = self.fail.contains(&format!("dns:{guid}"));
        for change in &mut report.changes {
            if change.outcome == ChangeOutcome::Planned {
                change.outcome = if failing {
                    ChangeOutcome::Failed
                } else {
                    ChangeOutcome::Applied
                };
            }
        }
        report.dry_run = false;
        report.session_id = Some(safety.session_id());
        Ok(report)
    }

    fn wu_apply(
        &self,
        _safety: &Safety,
        _want: &WindowsUpdateChoice,
        keys: &[String],
    ) -> Result<(Vec<StepResult>, RollbackFilter)> {
        self.call(format!("wu_apply {}", keys.join(",")));
        Ok((
            keys.iter().map(|k| self.step_result(k)).collect(),
            self.wu_filter.clone(),
        ))
    }

    fn maintenance_apply(
        &self,
        _safety: &Safety,
        _want: &MaintenanceChoice,
    ) -> Result<(Vec<StepResult>, RollbackFilter)> {
        self.call("maintenance_apply".into());
        Ok((
            vec![self.step_result("maintenance")],
            self.maintenance_filter.clone(),
        ))
    }
}

/// Records the baseline of the first registry value of `t` under `safety`'s session, as a
/// write that went through before a later action of the tweak failed. Tweaks without a
/// registry value record nothing.
fn record_first_value(safety: &Safety, t: &Tweak) -> Result<()> {
    let first = t.actions.iter().find_map(|action| match action {
        Action::Registry(r) => Some(r),
        _ => None,
    });
    if let Some(r) = first {
        safety.journal().record_registry(
            safety.session_id(),
            &NewRegistryRecord {
                hive: r.hive,
                key_path: r.path.to_string(),
                value_name: r.name.to_string(),
                key_existed: true,
                value_existed: false,
                original: None,
                created_root: None,
            },
        )?;
    }
    Ok(())
}

impl FakeProfiles {
    fn step_result(&self, key: &str) -> StepResult {
        let failed = self.fail.contains(key);
        StepResult {
            key: key.to_string(),
            outcome: if failed {
                StepOutcome::Failed
            } else {
                StepOutcome::Applied
            },
            details: vec![if failed { "access denied" } else { "set" }.to_string()],
        }
    }
}

// ───────────────────────────── fixtures ─────────────────────────────

/// Scan item of catalog tweak `id` in `state`.
pub(crate) fn scan_item(id: &str, state: ItemState) -> ScanItem {
    let t = catalog::tweak(id).unwrap_or_else(|| panic!("{id} is not a catalog tweak"));
    let action = match state {
        ItemState::Applied => ActionState::Applied,
        ItemState::Unavailable => ActionState::Unavailable,
        ItemState::NotApplied | ItemState::Partial => ActionState::NotApplied,
    };
    ScanItem {
        id: t.id.to_string(),
        kind: ItemKind::Tweak,
        category: t.category,
        title: t.title.to_string(),
        description: t.description.to_string(),
        risk: t.risk,
        recommended: t.default_on,
        state,
        restart: t.restart,
        actions: vec![ActionStatus::new(action, format!("{id} action"))],
        revertible: false,
        note: None,
    }
}

/// Scan item of the installed (NotApplied) or removed (Applied) Store package `name`.
pub(crate) fn appx_item(name: &str, state: ItemState) -> ScanItem {
    let entry = catalog::bloat_entry_for(name).unwrap_or_else(|| panic!("{name} is not bloat"));
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
        actions: vec![ActionStatus::new(
            ActionState::NotApplied,
            format!("installed: {name}_1.0.0.0_x64__8wekyb3d8bbwe"),
        )],
        revertible: false,
        note: None,
    }
}

pub(crate) fn scan_report(items: Vec<ScanItem>) -> ScanReport {
    ScanReport {
        elevated: true,
        has_battery: false,
        items,
        categories: Vec::new(),
        warnings: Vec::new(),
        duration_ms: 1,
        appx_unavailable: None,
    }
}

/// Startup entry `<source>:<key>` named `name`, toggleable, with a generic publisher and path.
pub(crate) fn startup_entry(
    source: StartupSource,
    key: &str,
    name: &str,
    enabled: bool,
) -> StartupEntry {
    let source_key = serde_json::to_value(source).unwrap();
    let source_key = source_key.as_str().unwrap();
    StartupEntry {
        id: format!("{source_key}:{key}"),
        name: name.to_string(),
        source,
        location: source_location(source_key).to_string(),
        command: format!(r#""C:\Users\Test\AppData\Local\{name}\{name}.exe" --minimized"#),
        path: format!(r"C:\Users\Test\AppData\Local\{name}\{name}.exe"),
        publisher: "Contoso Ltd.".into(),
        exists: true,
        enabled,
        requires_admin: !source.is_per_user(),
        can_toggle: !source.is_policy(),
        note: String::new(),
    }
}

/// Hardware adapter `n` of `kind`, connected, whose DNS servers can be changed and are
/// obtained automatically.
pub(crate) fn adapter(n: u32, name: &str, kind: AdapterKind) -> Adapter {
    let mut a = crate::network::tests::adapter(n, name, kind);
    a.can_change_dns = true;
    a.dns_ipv4 = automatic();
    a.dns_ipv6 = automatic();
    a
}

pub(crate) fn automatic() -> DnsConfig {
    DnsConfig {
        mode: DnsMode::Automatic,
        servers: Vec::new(),
        preset: None,
        profile_servers: Vec::new(),
    }
}

/// Manual servers of `preset` for `family`, as the adapter list reports them.
pub(crate) fn preset_config(preset: &str, family: IpFamily) -> DnsConfig {
    let p = PRESETS.iter().find(|p| p.id == preset).unwrap();
    DnsConfig {
        mode: DnsMode::Manual,
        servers: p.servers(family).iter().map(|s| s.to_string()).collect(),
        preset: Some(preset.to_string()),
        profile_servers: Vec::new(),
    }
}

pub(crate) fn network_report(adapters: Vec<Adapter>) -> NetworkReport {
    NetworkReport {
        adapters,
        dns_policy: Vec::new(),
        vpn_connected: false,
        warnings: Vec::new(),
        duration_ms: 1,
    }
}

/// What planning `preset` on `adapter` reports, with one change per family in `outcomes`.
pub(crate) fn dns_plan(
    adapter: &Adapter,
    preset: &str,
    outcomes: &[(IpFamily, ChangeOutcome)],
) -> DnsReport {
    DnsReport {
        dry_run: true,
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        session_id: None,
        changes: outcomes
            .iter()
            .map(|&(family, outcome)| DnsChange {
                family,
                previous: adapter.dns(family).clone(),
                target: preset_config(preset, family),
                outcome,
                detail: match outcome {
                    ChangeOutcome::Skipped => {
                        Some(format!("{} is turned off on this adapter", family.label()))
                    }
                    ChangeOutcome::Failed => Some("cannot read the current DNS servers".into()),
                    _ => None,
                },
            })
            .collect(),
        warnings: Vec::new(),
    }
}

/// A Windows Update or maintenance step.
pub(crate) fn step(
    key: &str,
    title: &str,
    status: StepStatus,
    caution: Option<&str>,
) -> SettingStep {
    SettingStep {
        key: key.to_string(),
        title: title.to_string(),
        status,
        detail: format!("{title} detail"),
        reason: (status == StepStatus::Skipped).then_some(StepReason::Edition),
        caution: caution.map(str::to_string),
    }
}
