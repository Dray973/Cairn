//! Planning, applying and exporting profiles against a [`ProfileSystem`].
//!
//! A plan reads only the sources the profile's sections need and turns every entry into a
//! row: a change, a setting already in place, or a skipped entry with its reason. A source
//! that cannot be read skips its section's rows and adds a warning; it never fails the plan.
//! Every write target comes from the catalog, the DNS presets or an entry, package or
//! adapter listed on this PC; the profile's own strings are only compared with those.
//!
//! Applying plans again, keeps the chosen change rows and applies them in one session in the
//! order tweaks, startup, DNS, Windows Update, maintenance, Store apps. The `apply_profile`
//! "started" row is written before the first change and one final row after the last. A row
//! that fails does not stop the others; a journal error does, and the records written so far
//! stay active.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::format::{
    is_startup_id, validate, windows_update_fields, DnsChoices, DnsFamilies, Profile,
    SectionCounts, StartupChoice, FORMAT, MAX_STARTUP_NAME, SCHEMA,
};
use super::step::{
    MaintenanceChoice, SettingStep, StepOutcome, StepReason, StepResult, StepStatus,
    WindowsUpdateChoice,
};
use super::system::ProfileSystem;
use super::OP_APPLY_PROFILE;
use crate::debloat::catalog::{self, Action, Category, RestartNeed, Risk, BLOAT_PACKAGES};
use crate::debloat::engine::touches_current_user;
use crate::debloat::{ItemKind, ItemOutcome, ItemState, ScanItem, ScanReport};
use crate::network::{
    self, canonical_guid, Adapter, AdapterKind, ChangeOutcome, DnsChoice, DnsConfig, DnsMode,
    DnsReport, DnsRequest, IpFamily, LinkStatus, NetworkReport, PRESETS,
};
use crate::safety::rollback::{RegistryTarget, RollbackFilter};
use crate::safety::{MutationOutcome, RestorePoint, Safety};
use crate::startup::{self, StartupEntry, StartupSource};
use crate::{Error, Result, VERSION};

// ───────────────────────────── output types ─────────────────────────────

/// Profile section of a row, in apply order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Tweaks,
    Apps,
    Startup,
    Dns,
    WindowsUpdate,
    Maintenance,
}

impl Section {
    /// Section of a row key by its prefix (`tweak:`, `app:`, `startup:`, `dns:`,
    /// `windows_update:`, `maintenance`); Tweaks for anything else.
    pub fn of_key(key: &str) -> Section {
        let prefix = key.split_once(':').map_or(key, |(p, _)| p);
        match prefix {
            "app" => Section::Apps,
            "startup" => Section::Startup,
            "dns" => Section::Dns,
            "windows_update" => Section::WindowsUpdate,
            "maintenance" => Section::Maintenance,
            _ => Section::Tweaks,
        }
    }
}

/// One entry of a profile as it applies to this PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRow {
    /// `tweak:<id>`, `app:<Name or pattern>`, `startup:<id>`, `dns:<canonical guid>`,
    /// `dns:ethernet` or `dns:wifi` (no adapter of that kind), `windows_update:<field>` or
    /// `maintenance`.
    pub key: String,
    pub section: Section,
    pub title: String,
    pub status: StepStatus,
    pub detail: String,
    /// Set when the row is skipped.
    pub reason: Option<StepReason>,
    /// Opt-in note; a row with a caution starts unselected.
    pub caution: Option<String>,
    /// Tweaks and Store apps.
    pub risk: Option<Risk>,
    /// Tweaks; none otherwise.
    pub restart: RestartNeed,
    pub per_user: bool,
    /// A change row without a caution.
    pub selected: bool,
}

/// What applying a profile would do on this PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilePlan {
    /// Always true: a plan changes nothing.
    pub dry_run: bool,
    pub name: String,
    /// In apply order: tweaks, startup, DNS, Windows Update, maintenance, Store apps.
    pub rows: Vec<PlanRow>,
    pub changes: usize,
    pub already: usize,
    pub skipped: usize,
    /// Strongest restart need of the selected change rows.
    pub restart: RestartNeed,
    pub elevated: bool,
    /// Why per-user rows are skipped; None when this process runs as the signed-in user.
    pub other_account: Option<String>,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

/// What applying one row did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowResult {
    pub key: String,
    pub section: Section,
    pub title: String,
    pub outcome: StepOutcome,
    pub details: Vec<String>,
}

/// What applying a profile did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileApplyReport {
    /// Always false.
    pub dry_run: bool,
    pub name: String,
    /// None when nothing was applied (no session was opened).
    pub session_id: Option<i64>,
    pub restore_point: Option<RestorePoint>,
    pub results: Vec<RowResult>,
    pub applied: usize,
    pub already: usize,
    pub skipped: usize,
    pub failed: usize,
    pub restart: RestartNeed,
    pub warnings: Vec<String>,
    /// Selects the journal records of what this apply changed; `revert_targets` undoes it.
    pub undo: RollbackFilter,
}

/// One setting of this PC that an exported profile can hold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRow {
    pub key: String,
    pub section: Section,
    pub title: String,
    pub detail: String,
    pub selected: bool,
    pub caution: Option<String>,
    pub per_user: bool,
}

/// This PC's exportable settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCandidates {
    pub rows: Vec<ExportRow>,
    pub other_account: Option<String>,
    pub warnings: Vec<String>,
}

/// What an export wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportReport {
    pub path: String,
    pub name: String,
    pub counts: SectionCounts,
    /// Chosen keys that were no longer settings of this PC when the file was written.
    pub missing: Vec<String>,
}

// ───────────────────────────── texts ─────────────────────────────

const OTHER_ACCOUNT_PLAN: &str = "Cairn is running as a different account than the signed-in \
     user, so settings that belong to a user account are skipped. Reopen Cairn as yourself to \
     apply them.";
const OTHER_ACCOUNT_EXPORT: &str = "Cairn is running as a different account than the \
     signed-in user, so settings that belong to a user account were read from that account, \
     not yours.";
const OTHER_ACCOUNT_ROW: &str =
    "Belongs to your user account, but Cairn is running as another account.";
const OTHER_ACCOUNT_CAUTION: &str = "Read from the account Cairn runs as, not yours.";
/// Added to a change row whose target already has an active journal record from an earlier
/// session: undo goes back to that record's baseline, not to the current value.
pub(crate) const EARLIER_CHANGE: &str =
    " · Undo returns it to how it was before Cairn first changed it";
const BATTERY_CAUTION: &str = "This PC has a battery: the Ultimate Performance plan keeps the \
     processor at full speed and shortens battery life.";
const NOT_INSTALLED: &str = "Not installed for your account.";
const DNS_POLICY: &str =
    "DNS servers are set by Group Policy on this PC, so adapter settings have no effect.";
const NOT_IN_PLAN: &str = "Not part of this profile's plan on this PC.";
const NOTHING_SELECTED: &str = "Nothing was selected, so no profile was written.";
const STARTUP_IDENTIFIER: &str =
    "can't be saved in a profile: its name contains an identifier of this PC or account.";
/// Hex digits after the last `_` that make a startup name end in an identifier.
const IDENTIFIER_DIGITS: usize = 16;

/// Short text of a skip reason, as the Profiles section shows it.
pub fn reason_text(reason: StepReason) -> &'static str {
    match reason {
        StepReason::Unsupported => "Not supported by this version",
        StepReason::Unreadable => "Couldn't be read",
        StepReason::CannotChange => "Can't be changed here",
        StepReason::OtherAccount => "Belongs to another account",
        StepReason::Edition => "Not on this edition of Windows",
        StepReason::NotOnThisPc => "Not on this PC",
        StepReason::UnknownId => "Unknown to this version of Cairn",
    }
}

fn category_label(category: Category) -> &'static str {
    match category {
        Category::Privacy => "Privacy",
        Category::Gaming => "Gaming",
        Category::Performance => "Performance",
        Category::Interface => "Interface",
        Category::Bloatware => "Store apps",
    }
}

fn restart_phrase(restart: RestartNeed) -> Option<&'static str> {
    match restart {
        RestartNeed::None => None,
        RestartNeed::Explorer => Some("File Explorer restarts"),
        RestartNeed::SignOut => Some("takes effect after signing out"),
        RestartNeed::Restart => Some("takes effect after a restart"),
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Location text of a startup source key, as `startup::list` words it.
pub(super) fn source_location(source: &str) -> &str {
    match source {
        "user_run" => "HKCU Run",
        "machine_run" => "HKLM Run",
        "machine_run32" => "HKLM Run (32-bit)",
        "user_folder" => "Startup folder",
        "common_folder" => "Startup folder (all users)",
        "packaged_task" => "Packaged app",
        "policy_user_run" => "HKCU Run (Group Policy)",
        "policy_machine_run" => "HKLM Run (Group Policy)",
        other => other,
    }
}

/// Startup sources whose entries are the same program listed in another place.
const SIBLING_SOURCES: &[&[&str]] = &[
    &["user_run", "machine_run", "machine_run32"],
    &["user_folder", "common_folder"],
];

fn split_startup_id(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or((id, ""))
}

fn kind_label(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::Wifi => "Wi-Fi",
        _ => "Ethernet",
    }
}

fn kind_key(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::Wifi => "wifi",
        _ => "ethernet",
    }
}

const DNS_KINDS: [AdapterKind; 2] = [AdapterKind::Ethernet, AdapterKind::Wifi];

fn dns_families(choices: &DnsChoices, kind: AdapterKind) -> Option<&DnsFamilies> {
    match kind {
        AdapterKind::Wifi => choices.wifi.as_ref(),
        _ => choices.ethernet.as_ref(),
    }
}

// ───────────────────────────── rows ─────────────────────────────

fn row(
    key: String,
    section: Section,
    title: String,
    status: StepStatus,
    detail: String,
) -> PlanRow {
    PlanRow {
        key,
        section,
        title,
        status,
        detail,
        reason: None,
        caution: None,
        risk: None,
        restart: RestartNeed::None,
        per_user: false,
        selected: false,
    }
}

fn skipped_row(
    key: String,
    section: Section,
    title: String,
    reason: StepReason,
    detail: String,
) -> PlanRow {
    let mut r = row(key, section, title, StepStatus::Skipped, detail);
    r.reason = Some(reason);
    r
}

/// Whether per-user changes reach the signed-in user, probed at most once.
struct Account<'a> {
    sys: &'a dyn ProfileSystem,
    other: OnceCell<Option<String>>,
}

impl<'a> Account<'a> {
    fn new(sys: &'a dyn ProfileSystem) -> Account<'a> {
        Account {
            sys,
            other: OnceCell::new(),
        }
    }

    /// Why per-user changes are refused, probing on first use; None when they are allowed.
    fn other(&self) -> Option<&str> {
        self.other
            .get_or_init(|| self.sys.per_user_allowed().err().map(|e| e.to_string()))
            .as_deref()
    }

    fn probed_other(&self) -> bool {
        matches!(self.other.get(), Some(Some(_)))
    }

    /// Skips a per-user row that would change or is already set when this process runs as
    /// another account.
    fn check(&self, row: &mut PlanRow) {
        if row.per_user && row.status != StepStatus::Skipped && self.other().is_some() {
            row.status = StepStatus::Skipped;
            row.reason = Some(StepReason::OtherAccount);
            row.detail = OTHER_ACCOUNT_ROW.to_string();
            row.caution = None;
        }
    }
}

/// Active registry records and the adapters with active DNS records, read once when an
/// earlier change is looked up.
struct ActiveRecords<'a> {
    sys: &'a dyn ProfileSystem,
    registry: OnceCell<Vec<RegistryTarget>>,
    dns: OnceCell<Vec<String>>,
}

impl<'a> ActiveRecords<'a> {
    fn new(sys: &'a dyn ProfileSystem) -> ActiveRecords<'a> {
        ActiveRecords {
            sys,
            registry: OnceCell::new(),
            dns: OnceCell::new(),
        }
    }

    fn has_registry(&self, target: &RegistryTarget) -> bool {
        self.registry
            .get_or_init(|| {
                self.sys
                    .journal()
                    .active_registry()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| RegistryTarget {
                        hive: r.hive,
                        key_path: r.key_path,
                        value_name: r.value_name,
                    })
                    .collect()
            })
            .iter()
            .any(|t| same_registry_target(t, target))
    }

    /// Whether the adapter has an active DNS record of either address family. An undo filter
    /// names the adapter, which selects its records of both families.
    fn has_dns(&self, guid: &str) -> bool {
        let guid = canonical_guid(guid);
        self.dns
            .get_or_init(|| {
                self.sys
                    .journal()
                    .active_dns()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| canonical_guid(&r.interface_guid))
                    .collect()
            })
            .contains(&guid)
    }
}

fn same_registry_target(a: &RegistryTarget, b: &RegistryTarget) -> bool {
    a.hive == b.hive
        && a.key_path.eq_ignore_ascii_case(&b.key_path)
        && a.value_name.eq_ignore_ascii_case(&b.value_name)
}

/// A plan plus what applying needs besides the rows: the DNS request of each adapter row.
struct Planned {
    plan: ProfilePlan,
    dns_requests: HashMap<String, DnsRequest>,
}

/// What applying `profile` would change on this PC. Read-only.
pub(crate) fn plan_with(sys: &dyn ProfileSystem, profile: &Profile) -> Result<ProfilePlan> {
    Ok(plan_rows(sys, profile).plan)
}

fn plan_rows(sys: &dyn ProfileSystem, profile: &Profile) -> Planned {
    let started = Instant::now();
    let account = Account::new(sys);
    let records = ActiveRecords::new(sys);
    let mut warnings = Vec::new();
    let mut rows = Vec::new();
    let mut dns_requests = HashMap::new();

    let scan = if profile.tweaks.is_empty() && profile.apps.is_empty() {
        None
    } else {
        let scan = sys.scan().map_err(|e| e.to_string());
        if let Err(e) = &scan {
            let what = match (profile.tweaks.is_empty(), profile.apps.is_empty()) {
                (false, false) => "Optimize settings and Store apps",
                (false, true) => "Optimize settings",
                _ => "Store apps",
            };
            warnings.push(format!("{what} could not be read: {e}"));
        }
        Some(scan)
    };

    if let Some(scan) = &scan {
        rows.extend(tweak_rows(profile, scan, &account));
    }
    if !profile.startup.is_empty() {
        match sys.startup() {
            Ok(entries) => rows.extend(startup_rows(profile, &entries, &account, &records)),
            Err(e) => {
                warnings.push(format!("Startup apps could not be read: {e}"));
                rows.extend(profile.startup.iter().map(|choice| {
                    skipped_row(
                        format!("startup:{}", choice.id),
                        Section::Startup,
                        startup_fallback_title(choice),
                        StepReason::Unreadable,
                        format!("Couldn't read: {e}"),
                    )
                }));
            }
        }
    }
    if !profile.dns.is_empty() {
        match sys.network() {
            Ok(report) => rows.extend(dns_rows(
                sys,
                &profile.dns,
                &report,
                &records,
                &mut dns_requests,
            )),
            Err(e) => {
                warnings.push(format!("Network adapters could not be read: {e}"));
                for kind in DNS_KINDS {
                    if dns_families(&profile.dns, kind).is_some() {
                        rows.push(skipped_row(
                            format!("dns:{}", kind_key(kind)),
                            Section::Dns,
                            format!("DNS servers ({})", kind_label(kind)),
                            StepReason::Unreadable,
                            format!("Couldn't read: {e}"),
                        ));
                    }
                }
            }
        }
    }
    if let Some(want) = &profile.windows_update {
        match sys.wu_plan(want) {
            Ok(steps) => {
                let mut wu_rows: Vec<PlanRow> = steps
                    .into_iter()
                    .map(|s| step_row(s, Section::WindowsUpdate))
                    .collect();
                if wu_rows.iter().any(|r| r.status == StepStatus::Change) {
                    if let Ok(recorded) = sys.wu_recorded() {
                        for r in wu_rows
                            .iter_mut()
                            .filter(|r| r.status == StepStatus::Change)
                        {
                            if recorded.iter().any(|k| k.eq_ignore_ascii_case(&r.key)) {
                                r.detail.push_str(EARLIER_CHANGE);
                            }
                        }
                    }
                }
                rows.extend(wu_rows);
            }
            Err(e) => {
                warnings.push(format!("Windows Update settings could not be read: {e}"));
                rows.extend(wu_fields(want).into_iter().map(|(key, title)| {
                    skipped_row(
                        key.to_string(),
                        Section::WindowsUpdate,
                        title.to_string(),
                        StepReason::Unreadable,
                        format!("Couldn't read: {e}"),
                    )
                }));
            }
        }
    }
    if let Some(want) = &profile.maintenance {
        match sys.maintenance_plan(want) {
            Ok(steps) => rows.extend(steps.into_iter().map(|s| step_row(s, Section::Maintenance))),
            Err(e) => {
                warnings.push(format!("Scheduled maintenance could not be read: {e}"));
                rows.push(skipped_row(
                    MAINTENANCE_KEY.to_string(),
                    Section::Maintenance,
                    MAINTENANCE_TITLE.to_string(),
                    StepReason::Unreadable,
                    format!("Couldn't read: {e}"),
                ));
            }
        }
    }
    if let Some(scan) = &scan {
        rows.extend(app_rows(profile, scan, &account));
    }

    for r in &mut rows {
        r.selected = r.status == StepStatus::Change && r.caution.is_none();
    }
    let count = |status: StepStatus| rows.iter().filter(|r| r.status == status).count();
    let restart = rows
        .iter()
        .filter(|r| r.selected)
        .map(|r| r.restart)
        .max()
        .unwrap_or_default();
    let plan = ProfilePlan {
        dry_run: true,
        name: profile.name.clone(),
        changes: count(StepStatus::Change),
        already: count(StepStatus::Already),
        skipped: count(StepStatus::Skipped),
        restart,
        elevated: sys.elevated(),
        other_account: account
            .probed_other()
            .then(|| OTHER_ACCOUNT_PLAN.to_string()),
        warnings,
        duration_ms: started.elapsed().as_millis() as u64,
        rows,
    };
    Planned { plan, dns_requests }
}

// ── tweaks ──

fn tweak_rows(
    profile: &Profile,
    scan: &std::result::Result<ScanReport, String>,
    account: &Account<'_>,
) -> Vec<PlanRow> {
    let mut rows = Vec::new();
    for id in &profile.tweaks {
        let key = format!("tweak:{id}");
        let Some(tweak) = catalog::tweak(id) else {
            rows.push(skipped_row(
                key,
                Section::Tweaks,
                id.clone(),
                StepReason::UnknownId,
                format!("Cairn {VERSION} doesn't know this setting; a newer version may."),
            ));
            continue;
        };
        let title = tweak.title.to_string();
        let scan = match scan {
            Ok(scan) => scan,
            Err(e) => {
                rows.push(skipped_row(
                    key,
                    Section::Tweaks,
                    title,
                    StepReason::Unreadable,
                    format!("Couldn't read: {e}"),
                ));
                continue;
            }
        };
        let Some(item) = scan.item(id) else {
            rows.push(skipped_row(
                key,
                Section::Tweaks,
                title,
                StepReason::Unreadable,
                "Couldn't read this setting.".into(),
            ));
            continue;
        };
        let mut r = match item.state {
            ItemState::Applied => row(
                key,
                Section::Tweaks,
                title,
                StepStatus::Already,
                if item.revertible {
                    "Already applied".into()
                } else {
                    "Already set on this PC".into()
                },
            ),
            ItemState::Unavailable => skipped_row(
                key,
                Section::Tweaks,
                title,
                StepReason::NotOnThisPc,
                item.note
                    .clone()
                    .or_else(|| item.actions.first().map(|a| a.detail.clone()))
                    .unwrap_or_else(|| "Not on this PC.".into()),
            ),
            ItemState::NotApplied | ItemState::Partial => {
                let mut detail = format!(
                    "{} · {}",
                    category_label(tweak.category),
                    if item.state == ItemState::Partial {
                        "partly applied"
                    } else {
                        "not applied"
                    }
                );
                if let Some(phrase) = restart_phrase(tweak.restart) {
                    detail.push_str(" · ");
                    detail.push_str(phrase);
                }
                if tweak.risk == Risk::Medium {
                    detail.push_str(" · Medium risk");
                }
                if item.revertible {
                    detail.push_str(EARLIER_CHANGE);
                }
                let mut r = row(key, Section::Tweaks, title, StepStatus::Change, detail);
                let power = tweak.actions.iter().any(|a| matches!(a, Action::Power(_)));
                r.caution = if power && scan.has_battery {
                    Some(BATTERY_CAUTION.to_string())
                } else if tweak.risk == Risk::High {
                    Some(format!("High risk: {}", tweak.description))
                } else {
                    None
                };
                r
            }
        };
        r.per_user = touches_current_user(tweak);
        r.risk = Some(tweak.risk);
        r.restart = tweak.restart;
        account.check(&mut r);
        rows.push(r);
    }
    rows
}

// ── Store apps ──

fn app_rows(
    profile: &Profile,
    scan: &std::result::Result<ScanReport, String>,
    account: &Account<'_>,
) -> Vec<PlanRow> {
    let mut rows: Vec<PlanRow> = Vec::new();
    let push = |mut r: PlanRow, rows: &mut Vec<PlanRow>| {
        if !rows.iter().any(|x| x.key.eq_ignore_ascii_case(&r.key)) {
            account.check(&mut r);
            rows.push(r);
        }
    };
    let unreadable = match scan {
        Err(e) => Some(format!("Couldn't read: {e}")),
        Ok(scan) => scan
            .appx_unavailable
            .as_ref()
            .map(|e| format!("The Store app list couldn't be read: {e}")),
    };
    for entry in &profile.apps {
        let key = format!("app:{entry}");
        if let Some(detail) = &unreadable {
            let mut r = skipped_row(
                key,
                Section::Apps,
                entry.clone(),
                StepReason::Unreadable,
                detail.clone(),
            );
            r.per_user = true;
            push(r, &mut rows);
            continue;
        }
        let Ok(scan) = scan else { continue };
        let installed: Vec<(&str, &ScanItem)> = scan
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Appx)
            .filter_map(|i| catalog::appx_name_from_item_id(&i.id).map(|n| (n, i)))
            .collect();

        if entry.ends_with('*') {
            let Some(package) = BLOAT_PACKAGES
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case(entry))
            else {
                push(
                    skipped_row(
                        key,
                        Section::Apps,
                        entry.clone(),
                        StepReason::UnknownId,
                        "Cairn doesn't remove these apps.".into(),
                    ),
                    &mut rows,
                );
                continue;
            };
            let matches: Vec<&(&str, &ScanItem)> = installed
                .iter()
                .filter(|(name, _)| catalog::name_matches(package.name, name))
                .collect();
            if matches.is_empty() {
                push(
                    app_row(key, package.title.to_string(), package.risk, None),
                    &mut rows,
                );
            }
            for (name, item) in matches {
                push(
                    app_row(
                        format!("app:{name}"),
                        item.title.clone(),
                        package.risk,
                        Some((name, item)),
                    ),
                    &mut rows,
                );
            }
            continue;
        }

        if catalog::is_protected_package(entry) {
            push(
                skipped_row(
                    key,
                    Section::Apps,
                    entry.clone(),
                    StepReason::CannotChange,
                    "Cairn never removes this app.".into(),
                ),
                &mut rows,
            );
            continue;
        }
        let Some(package) = catalog::bloat_entry_for(entry) else {
            push(
                skipped_row(
                    key,
                    Section::Apps,
                    entry.clone(),
                    StepReason::UnknownId,
                    "Cairn doesn't remove this app.".into(),
                ),
                &mut rows,
            );
            continue;
        };
        match installed
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(entry))
        {
            Some((name, item)) => push(
                app_row(
                    format!("app:{name}"),
                    item.title.clone(),
                    package.risk,
                    Some((name, item)),
                ),
                &mut rows,
            ),
            None => push(
                app_row(key, package.title.to_string(), package.risk, None),
                &mut rows,
            ),
        }
    }
    rows
}

/// Row of a catalog package: listed on this PC (`listed`) or not installed.
fn app_row(key: String, title: String, risk: Risk, listed: Option<(&str, &ScanItem)>) -> PlanRow {
    let mut r = match listed {
        Some((name, item)) if matches!(item.state, ItemState::NotApplied | ItemState::Partial) => {
            let mut detail = format!("Store app {name} · removed for your account");
            if risk == Risk::Medium {
                detail.push_str(" · Medium risk");
            }
            row(key, Section::Apps, title, StepStatus::Change, detail)
        }
        Some((_, item)) if item.state == ItemState::Applied => row(
            key,
            Section::Apps,
            title,
            StepStatus::Already,
            "Removed by Cairn".into(),
        ),
        _ => row(
            key,
            Section::Apps,
            title,
            StepStatus::Already,
            NOT_INSTALLED.into(),
        ),
    };
    r.per_user = true;
    r.risk = Some(risk);
    r
}

// ── startup ──

fn startup_fallback_title(choice: &StartupChoice) -> String {
    if choice.name.is_empty() {
        split_startup_id(&choice.id).1.to_string()
    } else {
        choice.name.clone()
    }
}

fn startup_rows(
    profile: &Profile,
    entries: &[StartupEntry],
    account: &Account<'_>,
    records: &ActiveRecords<'_>,
) -> Vec<PlanRow> {
    let mut rows: Vec<PlanRow> = Vec::new();
    for choice in &profile.startup {
        let (source, name) = split_startup_id(&choice.id);
        let r = match StartupSource::of_id(&choice.id) {
            Some(s) if s.is_policy() => skipped_row(
                format!("startup:{}", choice.id),
                Section::Startup,
                startup_fallback_title(choice),
                StepReason::CannotChange,
                "Set by Group Policy.".into(),
            ),
            None => skipped_row(
                format!("startup:{}", choice.id),
                Section::Startup,
                startup_fallback_title(choice),
                StepReason::NotOnThisPc,
                "Not listed on this PC.".into(),
            ),
            Some(_) => {
                let exact = entries
                    .iter()
                    .find(|e| e.id.eq_ignore_ascii_case(&choice.id));
                let sibling = || {
                    let group = SIBLING_SOURCES.iter().find(|g| g.contains(&source))?;
                    let found: Vec<&StartupEntry> = entries
                        .iter()
                        .filter(|e| {
                            let (s, k) = split_startup_id(&e.id);
                            s != source && group.contains(&s) && k.eq_ignore_ascii_case(name)
                        })
                        .collect();
                    match found.as_slice() {
                        [one] => Some(*one),
                        _ => None,
                    }
                };
                match exact {
                    Some(entry) => startup_row(entry, None, account, records),
                    None => match sibling() {
                        Some(entry) => {
                            let note = format!(
                                "Listed under {}; the profile has it under {}",
                                entry.location,
                                source_location(source)
                            );
                            startup_row(entry, Some(note), account, records)
                        }
                        None => skipped_row(
                            format!("startup:{}", choice.id),
                            Section::Startup,
                            startup_fallback_title(choice),
                            StepReason::NotOnThisPc,
                            "Not listed on this PC.".into(),
                        ),
                    },
                }
            }
        };
        if !rows.iter().any(|x| x.key.eq_ignore_ascii_case(&r.key)) {
            rows.push(r);
        }
    }
    rows
}

fn startup_row(
    entry: &StartupEntry,
    note: Option<String>,
    account: &Account<'_>,
    records: &ActiveRecords<'_>,
) -> PlanRow {
    let key = format!("startup:{}", entry.id);
    let title = entry.name.clone();
    let per_user = entry.source.is_per_user();
    let with_note = |detail: String| match &note {
        Some(note) => format!("{detail} · {note}"),
        None => detail,
    };
    let mut r = if entry.source.is_policy() {
        skipped_row(
            key,
            Section::Startup,
            title,
            StepReason::CannotChange,
            "Set by Group Policy.".into(),
        )
    } else if !entry.can_toggle {
        if per_user && account.other().is_some() {
            skipped_row(
                key,
                Section::Startup,
                title,
                StepReason::OtherAccount,
                OTHER_ACCOUNT_ROW.into(),
            )
        } else {
            let detail = if entry.note.is_empty() {
                "This startup entry can't be changed here.".to_string()
            } else {
                entry.note.clone()
            };
            skipped_row(
                key,
                Section::Startup,
                title,
                StepReason::CannotChange,
                detail,
            )
        }
    } else if !entry.enabled {
        row(
            key,
            Section::Startup,
            title,
            StepStatus::Already,
            with_note("Already turned off".into()),
        )
    } else {
        let publisher = if entry.publisher.is_empty() {
            "Unknown publisher"
        } else {
            &entry.publisher
        };
        let mut detail = format!("{} · {publisher} · turned off at sign-in", entry.location);
        if startup::registry_target(&entry.id).is_some_and(|t| records.has_registry(&t)) {
            detail.push_str(EARLIER_CHANGE);
        }
        row(
            key,
            Section::Startup,
            title,
            StepStatus::Change,
            with_note(detail),
        )
    };
    r.per_user = per_user;
    account.check(&mut r);
    r
}

// ── DNS ──

/// The DNS request of a profile's families: a missing family is left unchanged; a preset's
/// servers, or automatic when it has none.
pub(crate) fn request_for(families: &DnsFamilies) -> Result<DnsRequest> {
    let choice = |value: &Option<String>, family: IpFamily| -> Result<DnsChoice> {
        let Some(id) = value else {
            return Ok(DnsChoice::Unchanged);
        };
        let preset = PRESETS
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::Other(format!("unknown DNS choice {id:?}")))?;
        let servers: Vec<String> = preset
            .servers(family)
            .iter()
            .map(|s| s.to_string())
            .collect();
        if servers.is_empty() {
            Ok(DnsChoice::Automatic)
        } else {
            Ok(DnsChoice::Servers(network::parse_servers(
                &servers, family,
            )?))
        }
    };
    Ok(DnsRequest {
        ipv4: choice(&families.ipv4, IpFamily::Ipv4)?,
        ipv6: choice(&families.ipv6, IpFamily::Ipv6)?,
    })
}

fn preset_title(id: &str) -> Option<&'static str> {
    PRESETS.iter().find(|p| p.id == id).map(|p| p.title)
}

/// A DNS configuration in words: a preset's title, the servers, "Automatic" and so on.
fn config_text(config: &DnsConfig) -> String {
    match config.mode {
        DnsMode::Automatic => "Automatic".into(),
        DnsMode::Manual => config
            .preset
            .as_deref()
            .and_then(preset_title)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if config.servers.is_empty() {
                    "none".into()
                } else {
                    config.servers.join(", ")
                }
            }),
        DnsMode::Profile => "set for this Wi-Fi network".into(),
        DnsMode::Unknown => "unknown".into(),
    }
}

fn change_text(report: &DnsReport) -> String {
    report
        .changes
        .iter()
        .map(|c| {
            let label = c.family.label();
            match c.outcome {
                ChangeOutcome::Planned | ChangeOutcome::Applied => format!(
                    "{label}: {} → {}",
                    config_text(&c.previous),
                    config_text(&c.target)
                ),
                ChangeOutcome::AlreadySet => {
                    format!("{label}: {} (already set)", config_text(&c.target))
                }
                ChangeOutcome::Skipped | ChangeOutcome::Failed => {
                    format!("{label}: {}", c.detail.as_deref().unwrap_or("unchanged"))
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn dns_candidates(report: &NetworkReport, kind: AdapterKind) -> Vec<&Adapter> {
    report
        .adapters
        .iter()
        .filter(|a| a.kind == kind && a.hardware && a.status != LinkStatus::NotPresent)
        .collect()
}

fn dns_rows(
    sys: &dyn ProfileSystem,
    choices: &DnsChoices,
    report: &NetworkReport,
    records: &ActiveRecords<'_>,
    requests: &mut HashMap<String, DnsRequest>,
) -> Vec<PlanRow> {
    let mut rows = Vec::new();
    for kind in DNS_KINDS {
        let Some(families) = dns_families(choices, kind) else {
            continue;
        };
        let label = kind_label(kind);
        let adapters = dns_candidates(report, kind);
        if adapters.is_empty() {
            rows.push(skipped_row(
                format!("dns:{}", kind_key(kind)),
                Section::Dns,
                format!("DNS servers ({label})"),
                StepReason::NotOnThisPc,
                format!("This PC has no {label} adapter."),
            ));
            continue;
        }
        let unknown = [&families.ipv4, &families.ipv6]
            .into_iter()
            .flatten()
            .find(|v| !PRESETS.iter().any(|p| p.id == v.as_str()));
        for adapter in adapters {
            let key = format!("dns:{}", canonical_guid(&adapter.id));
            let title = format!("DNS servers of {} ({label})", adapter.name);
            let skip = |reason: StepReason, detail: String| {
                skipped_row(key.clone(), Section::Dns, title.clone(), reason, detail)
            };
            let r = if !report.dns_policy.is_empty() {
                skip(StepReason::CannotChange, DNS_POLICY.into())
            } else if let Some(value) = unknown {
                let offered: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
                skip(
                    StepReason::UnknownId,
                    format!(
                        "Unknown DNS choice “{value}”; this version offers: {}.",
                        offered.join(", ")
                    ),
                )
            } else if !adapter.can_change_dns {
                skip(
                    StepReason::CannotChange,
                    adapter
                        .note
                        .clone()
                        .unwrap_or_else(|| "DNS servers can't be changed on this adapter.".into()),
                )
            } else {
                match request_for(families) {
                    Err(e) => skip(StepReason::Unreadable, format!("Couldn't read: {e}")),
                    Ok(request) => {
                        let r = match sys.plan_dns(&adapter.id, &request) {
                            Err(e) => skip(StepReason::Unreadable, format!("Couldn't read: {e}")),
                            Ok(plan) => dns_plan_row(&key, &title, &plan, records),
                        };
                        if r.status == StepStatus::Change {
                            requests.insert(key.clone(), request);
                        }
                        r
                    }
                }
            };
            rows.push(r);
        }
    }
    rows
}

fn dns_plan_row(key: &str, title: &str, plan: &DnsReport, records: &ActiveRecords<'_>) -> PlanRow {
    let has = |outcome: ChangeOutcome| plan.changes.iter().any(|c| c.outcome == outcome);
    let text = change_text(plan);
    if has(ChangeOutcome::Planned) {
        let mut detail = text;
        if records.has_dns(&plan.adapter_id) {
            detail.push_str(EARLIER_CHANGE);
        }
        row(
            key.into(),
            Section::Dns,
            title.into(),
            StepStatus::Change,
            detail,
        )
    } else if has(ChangeOutcome::Failed) {
        skipped_row(
            key.into(),
            Section::Dns,
            title.into(),
            StepReason::Unreadable,
            text,
        )
    } else if !plan.changes.is_empty()
        && plan
            .changes
            .iter()
            .all(|c| c.outcome == ChangeOutcome::AlreadySet)
    {
        row(
            key.into(),
            Section::Dns,
            title.into(),
            StepStatus::Already,
            text,
        )
    } else {
        let detail = if text.is_empty() {
            "Nothing to change on this adapter.".to_string()
        } else {
            text
        };
        skipped_row(
            key.into(),
            Section::Dns,
            title.into(),
            StepReason::CannotChange,
            detail,
        )
    }
}

// ── Windows Update and maintenance ──

const WU_ACTIVE_HOURS: &str = "windows_update:active_hours";
const WU_RESTART_NOTIFY: &str = "windows_update:restart_notify";
const WU_EXCLUDE_DRIVERS: &str = "windows_update:exclude_drivers";
const WU_DEFER_FEATURE: &str = "windows_update:defer_feature";
const MAINTENANCE_KEY: &str = "maintenance";
const MAINTENANCE_TITLE: &str = "Scheduled maintenance";

/// Keys and titles of the fields a Windows Update choice sets.
fn wu_fields(want: &WindowsUpdateChoice) -> Vec<(&'static str, &'static str)> {
    let mut fields = Vec::new();
    if want.active_hours.is_some() {
        fields.push((WU_ACTIVE_HOURS, "Active hours"));
    }
    if want.restart_notify.is_some() {
        fields.push((WU_RESTART_NOTIFY, "Restart notifications"));
    }
    if want.exclude_drivers {
        fields.push((WU_EXCLUDE_DRIVERS, "Exclude drivers from quality updates"));
    }
    if want.defer_feature_days.is_some() {
        fields.push((WU_DEFER_FEATURE, "Defer feature updates"));
    }
    fields
}

fn step_row(step: SettingStep, section: Section) -> PlanRow {
    let mut r = row(step.key, section, step.title, step.status, step.detail);
    r.reason = step.reason;
    r.caution = step.caution;
    r
}

// ───────────────────────────── apply ─────────────────────────────

/// Unions `from` into `into`, dropping duplicates (strings compared ignoring ASCII case, DNS
/// GUIDs in canonical form); `power` is set when either sets it.
pub(crate) fn merge_filter(into: &mut RollbackFilter, from: RollbackFilter) {
    let RollbackFilter {
        registry,
        services,
        appx_families,
        power,
        scheduled_tasks,
        dns,
        task_definitions,
    } = from;
    for target in registry {
        if !into
            .registry
            .iter()
            .any(|t| same_registry_target(t, &target))
        {
            into.registry.push(target);
        }
    }
    let union = |into: &mut Vec<String>, from: Vec<String>| {
        for value in from {
            if !into.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
                into.push(value);
            }
        }
    };
    union(&mut into.services, services);
    union(&mut into.appx_families, appx_families);
    union(&mut into.scheduled_tasks, scheduled_tasks);
    union(&mut into.task_definitions, task_definitions);
    for guid in dns {
        let guid = canonical_guid(&guid);
        if !into.dns.iter().any(|g| canonical_guid(g) == guid) {
            into.dns.push(guid);
        }
    }
    into.power |= power;
}

fn item_outcome(outcome: ItemOutcome) -> (StepOutcome, Option<&'static str>) {
    match outcome {
        ItemOutcome::Applied => (StepOutcome::Applied, None),
        ItemOutcome::AlreadyApplied => (StepOutcome::AlreadySet, None),
        ItemOutcome::Skipped => (StepOutcome::Skipped, None),
        ItemOutcome::Failed => (StepOutcome::Failed, None),
        ItemOutcome::Planned => (StepOutcome::Failed, Some("unexpected planned outcome")),
    }
}

fn result_of(r: &PlanRow, outcome: StepOutcome, details: Vec<String>) -> RowResult {
    RowResult {
        key: r.key.clone(),
        section: r.section,
        title: r.title.clone(),
        outcome,
        details,
    }
}

/// "12 changes: 7 settings, 2 startup apps, 1 DNS, 2 apps".
fn selection_detail(rows: &[&PlanRow]) -> String {
    let count = |section: Section| rows.iter().filter(|r| r.section == section).count();
    let mut parts = Vec::new();
    for (section, noun) in [
        (Section::Tweaks, "setting"),
        (Section::Startup, "startup app"),
        (Section::Dns, ""),
        (Section::WindowsUpdate, "Windows Update setting"),
        (Section::Maintenance, ""),
        (Section::Apps, "app"),
    ] {
        let n = count(section);
        if n == 0 {
            continue;
        }
        parts.push(match section {
            Section::Dns => format!("{n} DNS"),
            Section::Maintenance => "scheduled maintenance".to_string(),
            _ => plural(n, noun),
        });
    }
    format!("{}: {}", plural(rows.len(), "change"), parts.join(", "))
}

/// Label of the journal session a profile applies in.
pub(crate) fn session_label(name: &str) -> String {
    format!("profile: {}", name.trim())
}

/// A plan or the report of an apply, serialized as the one it holds.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PlanOrApply {
    Plan(ProfilePlan),
    Apply(ProfileApplyReport),
}

/// [`plan_with`] when `dry_run` is set, which never calls `begin`; else [`apply_with`].
pub(crate) fn plan_or_apply_with(
    sys: &dyn ProfileSystem,
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
    profile: &Profile,
    keys: Option<&[String]>,
) -> Result<PlanOrApply> {
    if dry_run {
        let profile = validate(profile)?;
        return plan_with(sys, &profile).map(PlanOrApply::Plan);
    }
    apply_with(sys, begin, profile, keys).map(PlanOrApply::Apply)
}

/// Applies the rows `keys` selects (None: every change row whose `selected` is true) in one
/// session opened by `begin`. An unelevated process is refused before anything is read.
/// Plans again first, so rows that no longer need a change are reported, not applied.
/// Nothing chosen opens no session; per-user rows another account may not change are
/// withheld before `begin`.
pub(crate) fn apply_with(
    sys: &dyn ProfileSystem,
    begin: impl FnOnce() -> Result<Safety>,
    profile: &Profile,
    keys: Option<&[String]>,
) -> Result<ProfileApplyReport> {
    let profile = validate(profile)?;
    if !sys.elevated() {
        return Err(Error::NotElevated);
    }
    let Planned { plan, dns_requests } = plan_rows(sys, &profile);

    let mut results: Vec<RowResult> = Vec::new();
    let mut chosen: Vec<&PlanRow> = Vec::new();
    match keys {
        None => chosen.extend(plan.rows.iter().filter(|r| r.selected)),
        Some(keys) => {
            let mut seen: Vec<&str> = Vec::new();
            for key in keys {
                if seen.iter().any(|k| k.eq_ignore_ascii_case(key)) {
                    continue;
                }
                seen.push(key);
                let found = plan
                    .rows
                    .iter()
                    .find(|r| r.key == *key)
                    .or_else(|| plan.rows.iter().find(|r| r.key.eq_ignore_ascii_case(key)));
                match found {
                    None => results.push(RowResult {
                        key: key.clone(),
                        section: Section::of_key(key),
                        title: key.clone(),
                        outcome: StepOutcome::Skipped,
                        details: vec![NOT_IN_PLAN.into()],
                    }),
                    Some(r) => match r.status {
                        StepStatus::Change => {
                            if !chosen.iter().any(|c| c.key == r.key) {
                                chosen.push(r);
                            }
                        }
                        StepStatus::Already => results.push(result_of(
                            r,
                            StepOutcome::AlreadySet,
                            vec![r.detail.clone()],
                        )),
                        StepStatus::Skipped => {
                            let reason = r.reason.map_or("Skipped", reason_text);
                            results.push(result_of(
                                r,
                                StepOutcome::Skipped,
                                vec![format!("{reason}: {}", r.detail)],
                            ));
                        }
                    },
                }
            }
            // Chosen rows keep plan order.
            chosen.sort_by_key(|c| plan.rows.iter().position(|r| r.key == c.key));
        }
    }

    let mut warnings = plan.warnings.clone();
    if chosen.iter().any(|r| r.per_user) {
        if let Err(e) = sys.per_user_allowed() {
            let reason = e.to_string();
            for r in chosen.iter().filter(|r| r.per_user) {
                results.push(result_of(r, StepOutcome::Skipped, vec![reason.clone()]));
            }
            chosen.retain(|r| !r.per_user);
        }
    }
    if chosen.is_empty() {
        return Ok(apply_report(
            &profile,
            None,
            None,
            results,
            RestartNeed::None,
            warnings,
            RollbackFilter::default(),
        ));
    }

    let safety = begin()?;
    let target = format!("profile \"{}\"", profile.name);
    safety.log_op(
        OP_APPLY_PROFILE,
        &target,
        "started",
        Some(&selection_detail(&chosen)),
    )?;

    let mut undo = RollbackFilter::default();
    let mut restart = RestartNeed::None;
    let in_section = |section: Section| -> Vec<&PlanRow> {
        chosen
            .iter()
            .copied()
            .filter(|r| r.section == section)
            .collect()
    };

    // Tweaks.
    let tweaks = in_section(Section::Tweaks);
    if !tweaks.is_empty() {
        let ids: Vec<String> = tweaks
            .iter()
            .map(|r| r.key.trim_start_matches("tweak:").to_string())
            .collect();
        let (mut section_results, changed, need) =
            apply_items(sys, &safety, &tweaks, &ids, "tweak:", &mut warnings);
        restart = restart.max(need);
        results.append(&mut section_results);
        if !changed.is_empty() {
            merge_filter(&mut undo, sys.revert_filter(&changed)?);
        }
    }

    // Startup entries.
    for r in in_section(Section::Startup) {
        let id = r.key.trim_start_matches("startup:");
        let result = match sys.set_startup(&safety, id, false) {
            Ok(MutationOutcome::Applied) => {
                if let Some(target) = startup::registry_target(id) {
                    merge_filter(
                        &mut undo,
                        RollbackFilter {
                            registry: vec![target],
                            ..Default::default()
                        },
                    );
                }
                result_of(
                    r,
                    StepOutcome::Applied,
                    vec!["Turned off at sign-in".into()],
                )
            }
            Ok(MutationOutcome::AlreadyInDesiredState) => result_of(
                r,
                StepOutcome::AlreadySet,
                vec!["Already turned off".into()],
            ),
            Ok(MutationOutcome::Skipped(reason)) => {
                result_of(r, StepOutcome::Skipped, vec![reason])
            }
            Err(e) => result_of(r, StepOutcome::Failed, vec![e.to_string()]),
        };
        results.push(result);
    }

    // DNS servers.
    for r in in_section(Section::Dns) {
        let guid = r.key.trim_start_matches("dns:");
        let Some(request) = dns_requests.get(&r.key) else {
            results.push(result_of(r, StepOutcome::Failed, vec![NOT_IN_PLAN.into()]));
            continue;
        };
        let result = match sys.set_dns(&safety, guid, request) {
            Ok(report) => {
                let applied = report
                    .changes
                    .iter()
                    .any(|c| c.outcome == ChangeOutcome::Applied);
                let failed = report
                    .changes
                    .iter()
                    .any(|c| c.outcome == ChangeOutcome::Failed);
                if applied || failed {
                    merge_filter(
                        &mut undo,
                        RollbackFilter {
                            dns: vec![guid.to_string()],
                            ..Default::default()
                        },
                    );
                }
                for w in &report.warnings {
                    warnings.push(w.clone());
                }
                let details = vec![change_text(&report)];
                let outcome = if failed {
                    StepOutcome::Failed
                } else if applied {
                    StepOutcome::Applied
                } else if !report.changes.is_empty()
                    && report
                        .changes
                        .iter()
                        .all(|c| c.outcome == ChangeOutcome::AlreadySet)
                {
                    StepOutcome::AlreadySet
                } else {
                    StepOutcome::Skipped
                };
                result_of(r, outcome, details)
            }
            Err(e) => result_of(r, StepOutcome::Failed, vec![e.to_string()]),
        };
        results.push(result);
    }

    // Windows Update settings.
    let wu = in_section(Section::WindowsUpdate);
    if let (false, Some(want)) = (wu.is_empty(), &profile.windows_update) {
        let keys: Vec<String> = wu.iter().map(|r| r.key.clone()).collect();
        match sys.wu_apply(&safety, want, &keys) {
            Ok((steps, filter)) => {
                results.extend(step_results(&wu, steps));
                merge_filter(&mut undo, filter);
            }
            Err(e) => results.extend(
                wu.iter()
                    .map(|r| result_of(r, StepOutcome::Failed, vec![e.to_string()])),
            ),
        }
    }

    // Scheduled maintenance.
    let maintenance = in_section(Section::Maintenance);
    if let (false, Some(want)) = (maintenance.is_empty(), &profile.maintenance) {
        match sys.maintenance_apply(&safety, want) {
            Ok((steps, filter)) => {
                results.extend(step_results(&maintenance, steps));
                merge_filter(&mut undo, filter);
            }
            Err(e) => results.extend(
                maintenance
                    .iter()
                    .map(|r| result_of(r, StepOutcome::Failed, vec![e.to_string()])),
            ),
        }
    }

    // Store apps.
    let apps = in_section(Section::Apps);
    if !apps.is_empty() {
        let ids: Vec<String> = apps
            .iter()
            .map(|r| catalog::appx_item_id(r.key.trim_start_matches("app:")))
            .collect();
        let (mut section_results, changed, _) =
            apply_items(sys, &safety, &apps, &ids, "app:", &mut warnings);
        results.append(&mut section_results);
        if !changed.is_empty() {
            merge_filter(&mut undo, sys.revert_filter(&changed)?);
        }
    }

    let done = apply_report(
        &profile,
        Some(safety.session_id()),
        safety.restore_point().cloned(),
        results,
        restart,
        [warnings, safety.warnings().to_vec()].concat(),
        undo,
    );
    safety.log_op(
        OP_APPLY_PROFILE,
        &target,
        if done.failed == 0 {
            "applied"
        } else {
            "failed"
        },
        Some(&format!(
            "{} applied, {} already set, {} skipped, {} failed",
            done.applied, done.already, done.skipped, done.failed
        )),
    )?;
    Ok(done)
}

/// Applies catalog items (tweaks, or Store apps as `appx.<Name>`) for `rows` and maps each
/// item result back to its row. Returns the results, the ids that were applied or failed
/// (a failed item may have changed and recorded some of its targets before the failure) and
/// the restart they need.
fn apply_items(
    sys: &dyn ProfileSystem,
    safety: &Safety,
    rows: &[&PlanRow],
    ids: &[String],
    prefix: &str,
    warnings: &mut Vec<String>,
) -> (Vec<RowResult>, Vec<String>, RestartNeed) {
    let row_of = |id: &str| {
        let name = catalog::appx_name_from_item_id(id).unwrap_or(id);
        rows.iter()
            .position(|r| r.key[prefix.len()..].eq_ignore_ascii_case(name))
    };
    match sys.apply_items(safety, ids) {
        Ok(report) => {
            let mut results: Vec<Option<RowResult>> = vec![None; rows.len()];
            let mut changed = Vec::new();
            for item in report.results {
                let Some(index) = row_of(&item.id) else {
                    continue;
                };
                let (outcome, note) = item_outcome(item.outcome);
                let mut details = item.details;
                if let Some(note) = note {
                    details.push(note.to_string());
                }
                if matches!(outcome, StepOutcome::Applied | StepOutcome::Failed) {
                    changed.push(item.id.clone());
                }
                results[index] = Some(result_of(rows[index], outcome, details));
            }
            warnings.extend(report.warnings);
            let results = results
                .into_iter()
                .zip(rows)
                .map(|(result, r)| {
                    result.unwrap_or_else(|| {
                        result_of(
                            r,
                            StepOutcome::Failed,
                            vec!["No result was reported.".into()],
                        )
                    })
                })
                .collect();
            (results, changed, report.restart)
        }
        Err(e) => (
            rows.iter()
                .map(|r| result_of(r, StepOutcome::Failed, vec![e.to_string()]))
                .collect(),
            Vec::new(),
            RestartNeed::None,
        ),
    }
}

/// Maps the step results of a Windows Update or maintenance apply to their rows; a chosen
/// row without a step result failed.
fn step_results(rows: &[&PlanRow], steps: Vec<StepResult>) -> Vec<RowResult> {
    let mut results: Vec<RowResult> = Vec::new();
    for step in steps {
        let (section, title) = rows
            .iter()
            .find(|r| r.key == step.key)
            .map_or((Section::of_key(&step.key), step.key.clone()), |r| {
                (r.section, r.title.clone())
            });
        results.push(RowResult {
            key: step.key,
            section,
            title,
            outcome: step.outcome,
            details: step.details,
        });
    }
    for r in rows {
        if !results.iter().any(|x| x.key == r.key) {
            results.push(result_of(
                r,
                StepOutcome::Failed,
                vec!["No result was reported.".into()],
            ));
        }
    }
    results
}

fn apply_report(
    profile: &Profile,
    session_id: Option<i64>,
    restore_point: Option<RestorePoint>,
    results: Vec<RowResult>,
    restart: RestartNeed,
    warnings: Vec<String>,
    undo: RollbackFilter,
) -> ProfileApplyReport {
    let count = |outcome: StepOutcome| results.iter().filter(|r| r.outcome == outcome).count();
    let mut unique: Vec<String> = Vec::new();
    for w in warnings {
        if !unique.contains(&w) {
            unique.push(w);
        }
    }
    ProfileApplyReport {
        dry_run: false,
        name: profile.name.clone(),
        session_id,
        restore_point,
        applied: count(StepOutcome::Applied),
        already: count(StepOutcome::AlreadySet),
        skipped: count(StepOutcome::Skipped),
        failed: count(StepOutcome::Failed),
        results,
        restart,
        warnings: unique,
        undo,
    }
}

// ───────────────────────────── export ─────────────────────────────

/// What a candidate row puts into a profile.
#[derive(Debug, Clone)]
enum CandidateValue {
    Tweak(String),
    App(String),
    Startup(StartupChoice),
    Dns(AdapterKind, DnsFamilies),
    WindowsUpdate(WindowsUpdateChoice),
    Maintenance(MaintenanceChoice),
}

#[derive(Debug, Clone)]
struct Candidate {
    row: ExportRow,
    value: CandidateValue,
}

struct CandidateList {
    candidates: Vec<Candidate>,
    other_account: Option<String>,
    warnings: Vec<String>,
}

fn export_row(
    key: String,
    section: Section,
    title: String,
    detail: String,
    per_user: bool,
) -> ExportRow {
    ExportRow {
        key,
        section,
        title,
        detail,
        selected: true,
        caution: None,
        per_user,
    }
}

/// A name for a profile file: control characters removed, at most the profile limit.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_STARTUP_NAME)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `text` without its trailing `_` and 16 or more ASCII hex digits; None when it does not end
/// that way. Programs name startup entries this way after a hash of something on this PC or
/// account: a Chromium-based browser names its Run value `<Browser>AutoLaunch_<hash>` after
/// its profile folder, whose path holds the user name.
fn without_identifier(text: &str) -> Option<&str> {
    let (base, suffix) = text.rsplit_once('_')?;
    (suffix.len() >= IDENTIFIER_DIGITS && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(base)
}

/// Whether the key of startup entry `id` (after `<source>:`) ends in an identifier of this
/// PC or account, which a profile must not carry.
fn identifying_startup_key(id: &str) -> bool {
    id.split_once(':')
        .is_some_and(|(_, key)| without_identifier(key).is_some())
}

fn family_export(config: &DnsConfig) -> Option<String> {
    match config.mode {
        DnsMode::Manual => config.preset.clone(),
        _ => None,
    }
}

fn candidate_list(sys: &dyn ProfileSystem) -> CandidateList {
    let other = sys.per_user_allowed().is_err();
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();

    match sys.scan() {
        Ok(scan) => {
            for item in scan
                .items
                .iter()
                .filter(|i| i.kind == ItemKind::Tweak && i.state == ItemState::Applied)
            {
                let Some(tweak) = catalog::tweak(&item.id) else {
                    continue;
                };
                candidates.push(Candidate {
                    row: export_row(
                        format!("tweak:{}", item.id),
                        Section::Tweaks,
                        item.title.clone(),
                        if item.revertible {
                            "Changed by Cairn".into()
                        } else {
                            "Already set on this PC".into()
                        },
                        touches_current_user(tweak),
                    ),
                    value: CandidateValue::Tweak(item.id.clone()),
                });
            }
            for item in scan
                .items
                .iter()
                .filter(|i| i.kind == ItemKind::Appx && i.state == ItemState::Applied)
            {
                let Some(name) = catalog::appx_name_from_item_id(&item.id) else {
                    continue;
                };
                candidates.push(Candidate {
                    row: export_row(
                        format!("app:{name}"),
                        Section::Apps,
                        item.title.clone(),
                        "Removed by Cairn".into(),
                        true,
                    ),
                    value: CandidateValue::App(name.to_string()),
                });
            }
        }
        Err(e) => warnings.push(format!(
            "Optimize settings and Store apps could not be read: {e}"
        )),
    }

    match sys.startup() {
        Ok(entries) => {
            for entry in entries
                .iter()
                .filter(|e| !e.enabled && !e.source.is_policy())
            {
                if !is_startup_id(&entry.id) {
                    warnings.push(format!(
                        "The startup app {} can't be saved in a profile.",
                        clean_name(&entry.name)
                    ));
                    continue;
                }
                if identifying_startup_key(&entry.id) {
                    let name = clean_name(without_identifier(&entry.name).unwrap_or(&entry.name));
                    warnings.push(if name.is_empty() {
                        format!("A startup app {STARTUP_IDENTIFIER}")
                    } else {
                        format!("The startup app {name} {STARTUP_IDENTIFIER}")
                    });
                    continue;
                }
                candidates.push(Candidate {
                    row: export_row(
                        format!("startup:{}", entry.id),
                        Section::Startup,
                        entry.name.clone(),
                        format!("{} · turned off", entry.location),
                        entry.source.is_per_user(),
                    ),
                    value: CandidateValue::Startup(StartupChoice {
                        id: entry.id.clone(),
                        name: clean_name(&entry.name),
                    }),
                });
            }
        }
        Err(e) => warnings.push(format!("Startup apps could not be read: {e}")),
    }

    match sys.network() {
        Ok(report) if report.dns_policy.is_empty() => {
            for kind in DNS_KINDS {
                if let Some(candidate) = dns_candidate(&report, kind, &mut warnings) {
                    candidates.push(candidate);
                }
            }
        }
        Ok(_) => {}
        Err(e) => warnings.push(format!("Network adapters could not be read: {e}")),
    }

    match sys.wu_current() {
        Ok(current) => candidates.extend(wu_candidates(&current)),
        Err(e) => warnings.push(format!("Windows Update settings could not be read: {e}")),
    }

    match sys.maintenance_current() {
        Ok(Some(choice)) if choice.enabled => {
            candidates.push(Candidate {
                row: export_row(
                    MAINTENANCE_KEY.into(),
                    Section::Maintenance,
                    MAINTENANCE_TITLE.into(),
                    maintenance_text(&choice),
                    false,
                ),
                value: CandidateValue::Maintenance(choice),
            });
        }
        Ok(_) => {}
        Err(e) => warnings.push(format!("Scheduled maintenance could not be read: {e}")),
    }

    if other {
        for c in candidates.iter_mut().filter(|c| c.row.per_user) {
            c.row.selected = false;
            c.row.caution = Some(OTHER_ACCOUNT_CAUTION.into());
        }
    }
    CandidateList {
        candidates,
        other_account: other.then(|| OTHER_ACCOUNT_EXPORT.to_string()),
        warnings,
    }
}

/// The DNS choice of one adapter kind: the primary adapter of that kind, else the first
/// connected one, else the first. Only presets are exported; automatic only beside a preset.
fn dns_candidate(
    report: &NetworkReport,
    kind: AdapterKind,
    warnings: &mut Vec<String>,
) -> Option<Candidate> {
    let adapters = dns_candidates(report, kind);
    let adapter = adapters
        .iter()
        .find(|a| a.primary)
        .or_else(|| adapters.iter().find(|a| a.status == LinkStatus::Connected))
        .or_else(|| adapters.first())?;
    let label = kind_label(kind);
    let v4 = family_export(&adapter.dns_ipv4);
    let v6 = family_export(&adapter.dns_ipv6);
    for config in [&adapter.dns_ipv4, &adapter.dns_ipv6] {
        if config.mode == DnsMode::Manual && config.preset.is_none() {
            warnings.push(format!(
                "Custom DNS servers on {label} are not saved in profiles."
            ));
            break;
        }
    }
    let pick = |own: &Option<String>, config: &DnsConfig, other: &Option<String>| match own {
        Some(preset) => Some(preset.clone()),
        None if config.mode == DnsMode::Automatic && other.is_some() => Some("automatic".into()),
        None => None,
    };
    let families = DnsFamilies {
        ipv4: pick(&v4, &adapter.dns_ipv4, &v6),
        ipv6: pick(&v6, &adapter.dns_ipv6, &v4),
    };
    if families.ipv4.is_none() && families.ipv6.is_none() {
        return None;
    }
    for other in adapters.iter().filter(|a| a.id != adapter.id) {
        if other.dns_ipv4 != adapter.dns_ipv4 || other.dns_ipv6 != adapter.dns_ipv6 {
            warnings.push(format!(
                "{} uses other DNS servers; the profile keeps these.",
                other.name
            ));
        }
    }
    let part = |family: IpFamily, value: &Option<String>| {
        value.as_deref().map(|id| {
            format!(
                "{}: {}",
                family.label(),
                if id == "automatic" {
                    "Automatic"
                } else {
                    preset_title(id).unwrap_or(id)
                }
            )
        })
    };
    let parts: Vec<String> = [
        part(IpFamily::Ipv4, &families.ipv4),
        part(IpFamily::Ipv6, &families.ipv6),
    ]
    .into_iter()
    .flatten()
    .collect();
    Some(Candidate {
        row: export_row(
            format!("dns:{}", kind_key(kind)),
            Section::Dns,
            format!("DNS servers ({label})"),
            format!("{} (from {})", parts.join(" · "), adapter.name),
            false,
        ),
        value: CandidateValue::Dns(kind, families),
    })
}

fn wu_candidates(current: &WindowsUpdateChoice) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut push = |key: &str, title: &str, detail: String, value: WindowsUpdateChoice| {
        out.push(Candidate {
            row: export_row(
                key.into(),
                Section::WindowsUpdate,
                title.into(),
                detail,
                false,
            ),
            value: CandidateValue::WindowsUpdate(value),
        });
    };
    if let Some(hours) = current.active_hours {
        let detail = match (hours.automatic, hours.start, hours.end) {
            (true, _, _) => "Set automatically".to_string(),
            (false, Some(start), Some(end)) => format!("{start:02}:00 to {end:02}:00"),
            _ => "Set".to_string(),
        };
        push(
            WU_ACTIVE_HOURS,
            "Active hours",
            detail,
            WindowsUpdateChoice {
                active_hours: Some(hours),
                ..Default::default()
            },
        );
    }
    if let Some(notify) = current.restart_notify {
        push(
            WU_RESTART_NOTIFY,
            "Restart notifications",
            if notify { "On" } else { "Off" }.into(),
            WindowsUpdateChoice {
                restart_notify: Some(notify),
                ..Default::default()
            },
        );
    }
    if current.exclude_drivers {
        push(
            WU_EXCLUDE_DRIVERS,
            "Exclude drivers from quality updates",
            "Drivers are left out of quality updates".into(),
            WindowsUpdateChoice {
                exclude_drivers: true,
                ..Default::default()
            },
        );
    }
    if let Some(days) = current.defer_feature_days {
        push(
            WU_DEFER_FEATURE,
            "Defer feature updates",
            plural(days as usize, "day"),
            WindowsUpdateChoice {
                defer_feature_days: Some(days),
                ..Default::default()
            },
        );
    }
    out
}

fn maintenance_text(choice: &MaintenanceChoice) -> String {
    let mut parts = vec![format!(
        "Every {} at {}",
        choice.day.map_or("week", |d| d.label()),
        choice.time.as_deref().unwrap_or("--:--")
    )];
    if !choice.clean.is_empty() {
        parts.push(plural(choice.clean.len(), "cleanup target"));
    }
    if choice.sfc_verify {
        parts.push("system file check".into());
    }
    if choice.dism_check {
        parts.push("component store check".into());
    }
    parts.join(" · ")
}

/// This PC's exportable settings. Read-only.
pub(crate) fn candidates_with(sys: &dyn ProfileSystem) -> Result<ExportCandidates> {
    let list = candidate_list(sys);
    Ok(ExportCandidates {
        rows: list.candidates.into_iter().map(|c| c.row).collect(),
        other_account: list.other_account,
        warnings: list.warnings,
    })
}

/// The profile made of the candidate rows `keys` selects (None: rows whose `selected` is
/// true), dated `today`, and the chosen keys that are no longer candidates.
pub(crate) fn build_with(
    sys: &dyn ProfileSystem,
    name: &str,
    description: &str,
    keys: Option<&[String]>,
    today: &str,
) -> Result<(Profile, Vec<String>)> {
    let list = candidate_list(sys);
    let mut missing: Vec<String> = Vec::new();
    let chosen: Vec<&Candidate> = match keys {
        None => list.candidates.iter().filter(|c| c.row.selected).collect(),
        Some(keys) => {
            for key in keys {
                if !list.candidates.iter().any(|c| c.row.key == *key) && !missing.contains(key) {
                    missing.push(key.clone());
                }
            }
            list.candidates
                .iter()
                .filter(|c| keys.contains(&c.row.key))
                .collect()
        }
    };
    if chosen.is_empty() {
        return Err(Error::Other(NOTHING_SELECTED.into()));
    }
    let mut profile = Profile {
        format: FORMAT.to_string(),
        schema: SCHEMA,
        name: name.to_string(),
        description: description.to_string(),
        created: Some(today.to_string()),
        created_with: Some(format!("Cairn {VERSION}")),
        tweaks: Vec::new(),
        apps: Vec::new(),
        startup: Vec::new(),
        dns: DnsChoices::default(),
        windows_update: None,
        maintenance: None,
    };
    for c in chosen {
        match &c.value {
            CandidateValue::Tweak(id) => profile.tweaks.push(id.clone()),
            CandidateValue::App(name) => profile.apps.push(name.clone()),
            CandidateValue::Startup(choice) => profile.startup.push(choice.clone()),
            CandidateValue::Dns(kind, families) => match kind {
                AdapterKind::Wifi => profile.dns.wifi = Some(families.clone()),
                _ => profile.dns.ethernet = Some(families.clone()),
            },
            CandidateValue::WindowsUpdate(part) => {
                let wu = profile.windows_update.get_or_insert_with(Default::default);
                if part.active_hours.is_some() {
                    wu.active_hours = part.active_hours;
                }
                if part.restart_notify.is_some() {
                    wu.restart_notify = part.restart_notify;
                }
                wu.exclude_drivers |= part.exclude_drivers;
                if part.defer_feature_days.is_some() {
                    wu.defer_feature_days = part.defer_feature_days;
                }
            }
            CandidateValue::Maintenance(choice) => profile.maintenance = Some(choice.clone()),
        }
    }
    if profile
        .windows_update
        .as_ref()
        .is_some_and(|wu| windows_update_fields(wu) == 0)
    {
        profile.windows_update = None;
    }
    let profile = validate(&profile)?;
    Ok((profile, missing))
}
