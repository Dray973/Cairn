//! The security checkup: check types, the score, and the run that reads every source under
//! one deadline and evaluates the 24 checks.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::probe::{CheckupRaw, Context, Probe};
use super::updates::{UpdateScanView, UpdateScanner};
use super::{accounts, apps, device, network, protection, updates};
use crate::win::deadline;

/// Shared deadline of the checkup's source reads.
pub(crate) const CHECKUP_DEADLINE: Duration = Duration::from_secs(8);

/// Windows Security deep links that only navigate (the first path segment after
/// `windowsdefender://`). Links that start scans, updates or restarts are never used.
const SAFE_DEFENDER_PAGES: [&str; 13] = [
    "threat",
    "threatsettings",
    "history",
    "network",
    "appbrowser",
    "smartscreenpua",
    "smartapp",
    "devicesecurity",
    "coreisolation",
    "securityprocessor",
    "providers",
    "accountprotection",
    "administratorprotection",
];

/// Control Panel page of BitLocker Drive Encryption.
pub(crate) const BITLOCKER_URI: &str = "shell:::{D9EF8727-CAC2-4E60-809E-86F80A666C91}";

/// Whether a fix may open `uri`: an `ms-settings:` page, a navigating Windows Security page,
/// or the BitLocker Control Panel page.
pub fn allowed_uri(uri: &str) -> bool {
    if uri == BITLOCKER_URI {
        return true;
    }
    let lower = uri.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("windowsdefender://") {
        let page = rest.split(['/', '?', '#']).next().unwrap_or("");
        return SAFE_DEFENDER_PAGES.contains(&page);
    }
    lower.starts_with("ms-settings:") && lower.len() > "ms-settings:".len()
}

// ───────────────────────────── Types ─────────────────────────────

/// One check of the checkup, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    Antivirus,
    RealtimeProtection,
    SecurityIntelligence,
    TamperProtection,
    ThreatActions,
    Firewall,
    RemoteDesktop,
    RemoteAssistance,
    Smb1,
    WindowsUpdate,
    PendingUpdates,
    UpdateRestart,
    Encryption,
    SecureBoot,
    Tpm,
    MemoryIntegrity,
    Uac,
    AdminAccount,
    BuiltinAccounts,
    AutoSignIn,
    Smartscreen,
    EdgeSmartscreen,
    SmartAppControl,
    FileExtensions,
}

impl CheckId {
    pub const ALL: [CheckId; 24] = [
        CheckId::Antivirus,
        CheckId::RealtimeProtection,
        CheckId::SecurityIntelligence,
        CheckId::TamperProtection,
        CheckId::ThreatActions,
        CheckId::Firewall,
        CheckId::RemoteDesktop,
        CheckId::RemoteAssistance,
        CheckId::Smb1,
        CheckId::WindowsUpdate,
        CheckId::PendingUpdates,
        CheckId::UpdateRestart,
        CheckId::Encryption,
        CheckId::SecureBoot,
        CheckId::Tpm,
        CheckId::MemoryIntegrity,
        CheckId::Uac,
        CheckId::AdminAccount,
        CheckId::BuiltinAccounts,
        CheckId::AutoSignIn,
        CheckId::Smartscreen,
        CheckId::EdgeSmartscreen,
        CheckId::SmartAppControl,
        CheckId::FileExtensions,
    ];

    pub fn group(self) -> CheckGroup {
        use CheckId::*;
        match self {
            Antivirus | RealtimeProtection | SecurityIntelligence | TamperProtection
            | ThreatActions => CheckGroup::Protection,
            Firewall | RemoteDesktop | RemoteAssistance | Smb1 => CheckGroup::Network,
            WindowsUpdate | PendingUpdates | UpdateRestart => CheckGroup::Updates,
            Encryption | SecureBoot | Tpm | MemoryIntegrity => CheckGroup::Device,
            Uac | AdminAccount | BuiltinAccounts | AutoSignIn => CheckGroup::Accounts,
            Smartscreen | EdgeSmartscreen | SmartAppControl | FileExtensions => CheckGroup::Apps,
        }
    }

    /// Title of the check ("Drive encryption" reads "Device encryption" on Home).
    pub fn title(self) -> &'static str {
        use CheckId::*;
        match self {
            Antivirus => "Antivirus",
            RealtimeProtection => "Real-time protection",
            SecurityIntelligence => "Virus definitions",
            TamperProtection => "Tamper Protection",
            ThreatActions => "Threat actions",
            Firewall => "Firewall",
            RemoteDesktop => "Remote Desktop",
            RemoteAssistance => "Remote Assistance",
            Smb1 => "SMB 1.0 file sharing",
            WindowsUpdate => "Windows Update",
            PendingUpdates => "Waiting updates",
            UpdateRestart => "Restart for updates",
            Encryption => "Drive encryption",
            SecureBoot => "Secure Boot",
            Tpm => "TPM",
            MemoryIntegrity => "Memory integrity",
            Uac => "User Account Control",
            AdminAccount => "Your account",
            BuiltinAccounts => "Built-in accounts",
            AutoSignIn => "Automatic sign-in",
            Smartscreen => "SmartScreen for apps and files",
            EdgeSmartscreen => "SmartScreen in Microsoft Edge",
            SmartAppControl => "Smart App Control",
            FileExtensions => "File name extensions",
        }
    }

    /// The most severe finding this check can report; passed and unchecked rows carry it
    /// for sorting.
    pub fn max_severity(self) -> Severity {
        use CheckId::*;
        match self {
            Antivirus | RealtimeProtection | ThreatActions | Firewall | Uac => Severity::Critical,
            SecurityIntelligence | RemoteDesktop | Smb1 | WindowsUpdate | PendingUpdates
            | Encryption | SecureBoot | BuiltinAccounts | AutoSignIn | Smartscreen => {
                Severity::High
            }
            TamperProtection | UpdateRestart | Tpm | MemoryIntegrity | EdgeSmartscreen => {
                Severity::Medium
            }
            RemoteAssistance | AdminAccount | FileExtensions => Severity::Low,
            SmartAppControl => Severity::Info,
        }
    }

    /// The check reads the signed-in user's own settings.
    pub fn per_user(self) -> bool {
        matches!(self, CheckId::EdgeSmartscreen | CheckId::FileExtensions)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckGroup {
    Protection,
    Network,
    Updates,
    Device,
    Accounts,
    Apps,
}

impl CheckGroup {
    pub fn title(self) -> &'static str {
        match self {
            CheckGroup::Protection => "Virus & threat protection",
            CheckGroup::Network => "Firewall & network",
            CheckGroup::Updates => "Windows Update",
            CheckGroup::Device => "Device security",
            CheckGroup::Accounts => "Accounts & sign-in",
            CheckGroup::Apps => "Apps & browser",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Good,
    Attention,
    /// A Windows Update search is running for this check.
    Checking,
    /// The check could not run; `summary` says why.
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Points an attention finding of this severity takes off the score.
    pub fn penalty(self) -> u32 {
        match self {
            Severity::Critical => 40,
            Severity::High => 20,
            Severity::Medium => 8,
            Severity::Low => 3,
            Severity::Info => 0,
        }
    }

    /// Label of the severity chip.
    pub fn chip(self) -> &'static str {
        match self {
            Severity::Critical => "Critical",
            Severity::High => "Important",
            Severity::Medium => "Recommended",
            Severity::Low => "Optional",
            Severity::Info => "Info",
        }
    }
}

/// A "label: value" line under a check; an empty label shows the value alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// What a fix button does. Every mutating fix reuses a journaled flow of the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FixAction {
    /// Opens a settings or Windows Security page (see [`allowed_uri`]).
    Uri { uri: String },
    /// Opens a built-in Windows tool by its id (`tools::windows_tools`).
    WindowsTool { tool: String, requires_admin: bool },
    /// Applies a catalog tweak through the journaled apply flow.
    Tweak { id: String },
    /// Searches Windows Update (online or offline).
    UpdateScan { online: bool },
    /// Restarts Cairn as administrator.
    Elevate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub label: String,
    pub action: FixAction,
    /// Extra instructions shown under the buttons.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: CheckId,
    pub group: CheckGroup,
    pub title: String,
    pub state: CheckState,
    /// For an attention finding its severity; otherwise the most severe outcome this check
    /// can have (used for sorting only).
    pub severity: Severity,
    pub summary: String,
    pub detail: String,
    pub facts: Vec<Fact>,
    pub fixes: Vec<Fix>,
    /// Checking needs administrator rights.
    pub needs_admin: bool,
    /// The check reads the signed-in user's settings.
    pub per_user: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    Good,
    Fair,
    AtRisk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    /// 0 to 100.
    pub value: u8,
    pub grade: Grade,
    /// Attention findings of severity low or higher.
    pub to_fix: u32,
    pub critical: u32,
    /// Checks that could not run or are still running.
    pub unknown: u32,
    /// Checks with a result (good or attention).
    pub checked: u32,
}

/// A group of reads that ran on its own thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Os,
    Device,
    SecurityCenter,
    Defender,
    Firewall,
    WindowsUpdate,
    Encryption,
    Accounts,
    Remote,
    Apps,
}

impl Source {
    pub const ALL: [Source; 10] = [
        Source::Os,
        Source::Device,
        Source::SecurityCenter,
        Source::Defender,
        Source::Firewall,
        Source::WindowsUpdate,
        Source::Encryption,
        Source::Accounts,
        Source::Remote,
        Source::Apps,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Source::Os => "Windows edition",
            Source::Device => "Device security",
            Source::SecurityCenter => "Windows Security Center",
            Source::Defender => "Microsoft Defender",
            Source::Firewall => "Windows Firewall",
            Source::WindowsUpdate => "Windows Update",
            Source::Encryption => "Drive encryption",
            Source::Accounts => "Account settings",
            Source::Remote => "Remote access settings",
            Source::Apps => "App and browser settings",
        }
    }

    fn thread_name(self) -> &'static str {
        match self {
            Source::Os => "health-os",
            Source::Device => "health-device",
            Source::SecurityCenter => "health-security-center",
            Source::Defender => "health-defender",
            Source::Firewall => "health-firewall",
            Source::WindowsUpdate => "health-windows-update",
            Source::Encryption => "health-encryption",
            Source::Accounts => "health-accounts",
            Source::Remote => "health-remote",
            Source::Apps => "health-apps",
        }
    }
}

/// A source that failed, panicked or missed the deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceError {
    pub source: Source,
    pub message: String,
}

/// The result of one security checkup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkup {
    pub taken_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub elevated: bool,
    /// Whether Cairn runs as another account than the signed-in user; `None` when that
    /// could not be decided.
    pub other_user: Option<bool>,
    pub home_edition: bool,
    pub score: Score,
    /// In `CheckId::ALL` order.
    pub checks: Vec<Check>,
    pub update_scan: UpdateScanView,
    /// No Windows Update search finished recently and none runs: the window starts an
    /// offline one.
    pub update_scan_due: bool,
    pub notes: Vec<String>,
    pub errors: Vec<SourceError>,
}

// ───────────────────────────── Check builder ─────────────────────────────

impl Check {
    /// A check in state unknown with `detail` and its default title and flags.
    pub(crate) fn new(id: CheckId, detail: impl Into<String>) -> Check {
        Check {
            id,
            group: id.group(),
            title: id.title().to_string(),
            state: CheckState::Unknown,
            severity: id.max_severity(),
            summary: String::new(),
            detail: detail.into(),
            facts: Vec::new(),
            fixes: Vec::new(),
            needs_admin: false,
            per_user: id.per_user(),
        }
    }

    pub(crate) fn good(mut self, summary: impl Into<String>) -> Check {
        self.state = CheckState::Good;
        self.severity = self.id.max_severity();
        self.summary = summary.into();
        self
    }

    pub(crate) fn attention(mut self, severity: Severity, summary: impl Into<String>) -> Check {
        self.state = CheckState::Attention;
        self.severity = severity;
        self.summary = summary.into();
        self
    }

    pub(crate) fn unknown(mut self, summary: impl Into<String>) -> Check {
        self.state = CheckState::Unknown;
        self.severity = self.id.max_severity();
        self.summary = summary.into();
        self
    }

    pub(crate) fn checking(mut self, summary: impl Into<String>) -> Check {
        self.state = CheckState::Checking;
        self.severity = self.id.max_severity();
        self.summary = summary.into();
        self
    }

    pub(crate) fn not_applicable(mut self, summary: impl Into<String>) -> Check {
        self.state = CheckState::NotApplicable;
        self.severity = self.id.max_severity();
        self.summary = summary.into();
        self
    }

    pub(crate) fn titled(mut self, title: impl Into<String>) -> Check {
        self.title = title.into();
        self
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Check {
        self.detail = detail.into();
        self
    }

    pub(crate) fn fact(mut self, label: impl Into<String>, value: impl Into<String>) -> Check {
        self.facts.push(Fact {
            label: label.into(),
            value: value.into(),
        });
        self
    }

    /// A fact shown without a label.
    pub(crate) fn line(self, value: impl Into<String>) -> Check {
        self.fact("", value)
    }

    pub(crate) fn needs_admin(mut self) -> Check {
        self.needs_admin = true;
        self
    }

    pub(crate) fn fix(mut self, label: impl Into<String>, action: FixAction) -> Check {
        self.fixes.push(Fix {
            label: label.into(),
            action,
            note: None,
        });
        self
    }

    pub(crate) fn fix_with_note(
        mut self,
        label: impl Into<String>,
        action: FixAction,
        note: impl Into<String>,
    ) -> Check {
        self.fixes.push(Fix {
            label: label.into(),
            action,
            note: Some(note.into()),
        });
        self
    }

    /// A fix that opens `uri`.
    pub(crate) fn uri(self, label: &str, uri: &str) -> Check {
        self.fix(label, FixAction::Uri { uri: uri.into() })
    }

    /// A fix that opens the Windows tool `tool`; whether it needs administrator rights comes
    /// from the tool list.
    pub(crate) fn tool(self, label: &str, tool: &str) -> Check {
        let action = tool_action(tool);
        self.fix(label, action)
    }

    pub(crate) fn tool_with_note(self, label: &str, tool: &str, note: &str) -> Check {
        let action = tool_action(tool);
        self.fix_with_note(label, action, note)
    }
}

fn tool_action(tool: &str) -> FixAction {
    let requires_admin = crate::tools::windows_tools::windows_tool(tool)
        .map(|t| t.requires_admin)
        .unwrap_or(true);
    FixAction::WindowsTool {
        tool: tool.into(),
        requires_admin,
    }
}

/// Check state for a source that could not be read: "{what}: {error}".
pub(crate) fn unreadable(id: CheckId, detail: &str, what: &str, error: &str) -> Check {
    Check::new(id, detail).unknown(format!("Could not read {what}: {error}"))
}

// ───────────────────────────── Score ─────────────────────────────

/// Score of a set of checks: 100 minus the penalty of every attention finding, and a grade.
pub(crate) fn score(checks: &[Check]) -> Score {
    let attention: Vec<&Check> = checks
        .iter()
        .filter(|c| c.state == CheckState::Attention)
        .collect();
    let penalty: u32 = attention.iter().map(|c| c.severity.penalty()).sum();
    let value = 100u32.saturating_sub(penalty) as u8;
    let critical = attention
        .iter()
        .filter(|c| c.severity == Severity::Critical)
        .count() as u32;
    let high = attention.iter().any(|c| c.severity == Severity::High);
    let grade = if critical > 0 || value < 60 {
        Grade::AtRisk
    } else if high || value < 90 {
        Grade::Fair
    } else {
        Grade::Good
    };
    let count =
        |states: &[CheckState]| checks.iter().filter(|c| states.contains(&c.state)).count() as u32;
    Score {
        value,
        grade,
        to_fix: attention
            .iter()
            .filter(|c| c.severity >= Severity::Low)
            .count() as u32,
        critical,
        unknown: count(&[CheckState::Unknown, CheckState::Checking]),
        checked: count(&[CheckState::Good, CheckState::Attention]),
    }
}

// ───────────────────────────── Evaluation ─────────────────────────────

/// Every check from the raw readings, in `CheckId::ALL` order. Pure.
pub(crate) fn evaluate(
    raw: &CheckupRaw,
    ctx: &Context,
    scan: &UpdateScanView,
    now: DateTime<Utc>,
) -> Vec<Check> {
    let home = raw.os.as_ref().ok().map(|os| os.home());
    vec![
        protection::antivirus(raw),
        protection::realtime(raw),
        protection::intelligence(raw, now),
        protection::tamper(raw),
        protection::threat_actions(raw),
        network::firewall(raw),
        network::remote_desktop(raw, home),
        network::remote_assistance(raw),
        network::smb1(raw),
        updates::windows_update(raw, now),
        updates::pending_updates(raw, scan, now),
        updates::update_restart(raw),
        device::encryption(raw, ctx, home),
        device::secure_boot(raw),
        device::tpm(raw),
        device::memory_integrity(raw),
        accounts::uac(raw),
        accounts::admin_account(raw, ctx),
        accounts::builtin_accounts(raw),
        accounts::auto_sign_in(raw),
        apps::smartscreen(raw),
        apps::edge_smartscreen(raw),
        apps::smart_app_control(raw),
        apps::file_extensions(raw),
    ]
}

/// Remarks about the whole checkup: what could not be checked and why.
pub(crate) fn notes(raw: &CheckupRaw, ctx: &Context) -> Vec<String> {
    let mut notes = Vec::new();
    if let super::probe::UserHive::Unavailable(note) = &ctx.hive {
        notes.push(note.clone());
    }
    if raw.security_center.is_err() && raw.defender.is_ok() {
        notes.push(
            "Windows Security Center could not be read, so the antivirus state comes from \
             Microsoft Defender."
                .to_string(),
        );
    }
    if !ctx.elevated {
        notes.push("Drive encryption can only be checked with administrator rights.".to_string());
    }
    notes
}

/// Result of one source thread.
enum Reading {
    Os(Result<super::probe::OsRaw, String>),
    Device(Result<crate::sysinfo::SecurityInfo, String>),
    SecurityCenter(Result<protection::WscRaw, String>),
    Defender(Result<protection::DefenderRead, String>),
    Firewall(Result<network::FirewallRaw, String>),
    WindowsUpdate(Result<updates::WuRaw, String>),
    Encryption(Result<device::EncryptionRaw, String>),
    Accounts(Result<accounts::AccountsRaw, String>),
    Remote(Result<network::RemoteRaw, String>),
    Apps(Result<apps::AppsRaw, String>),
}

/// Runs `read` in the calling thread's COM apartment without critical-error dialogs; an
/// error or a panic becomes its message.
fn read_source<T>(read: impl FnOnce() -> crate::Result<T>) -> Result<T, String> {
    let _com = crate::win::com::enter_mta();
    let _mode = crate::win::error_mode::ErrorModeGuard::new();
    match panic::catch_unwind(AssertUnwindSafe(read)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("internal error: {text}"))
        }
    }
}

/// The reading of a source that did not answer in time.
fn late<T>(source: Source, deadline: Duration) -> Result<T, String> {
    Err(format!(
        "{} did not answer within {}",
        source.title(),
        deadline_text(deadline)
    ))
}

fn deadline_text(deadline: Duration) -> String {
    if deadline >= Duration::from_secs(1) {
        format!("{} s", deadline.as_secs())
    } else {
        format!("{} ms", deadline.as_millis())
    }
}

/// Reads every source through `probe` on its own thread under `deadline`, then evaluates the
/// checks. Never fails as a whole: a source that errors, panics or misses the deadline makes
/// its checks unknown and is listed in `errors`.
pub(crate) fn checkup_with(
    probe: Arc<dyn Probe>,
    scanner: &UpdateScanner,
    now: DateTime<Utc>,
    deadline: Duration,
) -> Checkup {
    let started = Instant::now();
    let ctx = probe.context();
    let mut jobs: Vec<deadline::Job<Reading>> = Vec::with_capacity(Source::ALL.len());
    for source in Source::ALL {
        let probe = Arc::clone(&probe);
        let ctx = ctx.clone();
        let work: Box<dyn FnOnce() -> Reading + Send> = Box::new(move || match source {
            Source::Os => Reading::Os(read_source(|| probe.os())),
            Source::Device => Reading::Device(read_source(|| probe.device())),
            Source::SecurityCenter => {
                Reading::SecurityCenter(read_source(|| probe.security_center()))
            }
            Source::Defender => Reading::Defender(read_source(|| probe.defender())),
            Source::Firewall => Reading::Firewall(read_source(|| probe.firewall())),
            Source::WindowsUpdate => Reading::WindowsUpdate(read_source(|| probe.windows_update())),
            Source::Encryption => {
                Reading::Encryption(read_source(|| probe.encryption(ctx.elevated)))
            }
            Source::Accounts => Reading::Accounts(read_source(|| probe.accounts(&ctx))),
            Source::Remote => Reading::Remote(read_source(|| probe.remote())),
            Source::Apps => Reading::Apps(read_source(|| probe.apps(&ctx.hive))),
        });
        jobs.push((source.thread_name(), work));
    }
    let results = deadline::run_all(jobs, deadline);

    let mut raw = CheckupRaw {
        os: late(Source::Os, deadline),
        device: late(Source::Device, deadline),
        security_center: late(Source::SecurityCenter, deadline),
        defender: late(Source::Defender, deadline),
        firewall: late(Source::Firewall, deadline),
        windows_update: late(Source::WindowsUpdate, deadline),
        encryption: late(Source::Encryption, deadline),
        accounts: late(Source::Accounts, deadline),
        remote: late(Source::Remote, deadline),
        apps: late(Source::Apps, deadline),
    };
    for reading in results.into_iter().flatten() {
        match reading {
            Reading::Os(r) => raw.os = r,
            Reading::Device(r) => raw.device = r,
            Reading::SecurityCenter(r) => raw.security_center = r,
            Reading::Defender(r) => raw.defender = r,
            Reading::Firewall(r) => raw.firewall = r,
            Reading::WindowsUpdate(r) => raw.windows_update = r,
            Reading::Encryption(r) => raw.encryption = r,
            Reading::Accounts(r) => raw.accounts = r,
            Reading::Remote(r) => raw.remote = r,
            Reading::Apps(r) => raw.apps = r,
        }
    }

    let scan = scanner.view();
    let checks = evaluate(&raw, &ctx, &scan, now);
    let score = score(&checks);
    let errors = raw.errors();
    let notes = notes(&raw, &ctx);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::debug!(
        duration_ms,
        failed_sources = errors.len(),
        "security checkup read"
    );
    Checkup {
        taken_at: now,
        duration_ms,
        elevated: ctx.elevated,
        other_user: ctx.other_user,
        home_edition: raw.os.as_ref().map(|os| os.home()).unwrap_or(false),
        score,
        checks,
        update_scan: scan,
        update_scan_due: scanner.due(now),
        notes,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use serde_json::Value;

    use super::super::probe::fixtures::{self, now};
    use super::super::probe::{OsRaw, UserHive};
    use super::super::updates::{PendingUpdate, ScanState, UpdateAgent};
    use super::*;
    use crate::win::update_agent::SearchOutcome;

    fn check(id: CheckId, state: CheckState, severity: Severity) -> Check {
        Check {
            state,
            severity,
            ..Check::new(id, "")
        }
    }

    fn attention(severity: Severity) -> Check {
        check(CheckId::Firewall, CheckState::Attention, severity)
    }

    #[test]
    fn score_vectors() {
        let one_high = score(&[attention(Severity::High)]);
        assert_eq!((one_high.value, one_high.grade), (80, Grade::Fair));
        let one_medium = score(&[attention(Severity::Medium)]);
        assert_eq!((one_medium.value, one_medium.grade), (92, Grade::Good));
        let one_critical = score(&[attention(Severity::Critical)]);
        assert_eq!(
            (one_critical.value, one_critical.grade),
            (60, Grade::AtRisk)
        );
        let two_high = score(&[attention(Severity::High), attention(Severity::High)]);
        assert_eq!((two_high.value, two_high.grade), (60, Grade::Fair));
        let unknown = score(&[
            check(CheckId::Tpm, CheckState::Unknown, Severity::Critical),
            check(CheckId::Tpm, CheckState::Checking, Severity::Critical),
            check(CheckId::Tpm, CheckState::NotApplicable, Severity::Critical),
            check(CheckId::Tpm, CheckState::Good, Severity::Critical),
        ]);
        assert_eq!((unknown.value, unknown.grade), (100, Grade::Good));
        assert_eq!((unknown.unknown, unknown.checked), (2, 1));
    }

    #[test]
    fn grade_rules() {
        // Below 60 is at risk even without a critical finding.
        let low = score(&[
            attention(Severity::High),
            attention(Severity::High),
            attention(Severity::Low),
        ]);
        assert_eq!((low.value, low.grade), (57, Grade::AtRisk));
        // Below 90 is fair even without a high finding.
        let mediums = score(&[attention(Severity::Medium), attention(Severity::Medium)]);
        assert_eq!((mediums.value, mediums.grade), (84, Grade::Fair));
        // Info costs nothing and is not "to fix".
        let info = score(&[attention(Severity::Info), attention(Severity::Low)]);
        assert_eq!((info.value, info.grade, info.to_fix), (97, Grade::Good, 1));
        let floor = score(&vec![attention(Severity::Critical); 4]);
        assert_eq!((floor.value, floor.critical, floor.to_fix), (0, 4, 4));
    }

    #[test]
    fn every_check_id_is_listed_once_and_every_group_is_used() {
        let ids: HashSet<CheckId> = CheckId::ALL.into_iter().collect();
        assert_eq!(ids.len(), 24);
        let groups: HashSet<CheckGroup> = CheckId::ALL.into_iter().map(CheckId::group).collect();
        assert_eq!(groups.len(), 6);
        for id in CheckId::ALL {
            assert!(!id.title().is_empty());
            assert!(!id.group().title().is_empty());
        }
        assert_eq!(CheckId::SmartAppControl.max_severity(), Severity::Info);
        let sources: HashSet<Source> = Source::ALL.into_iter().collect();
        assert_eq!(sources.len(), 10);
    }

    #[test]
    fn a_well_protected_pc_passes_every_check() {
        let checks = evaluate(
            &fixtures::raw(),
            &fixtures::ctx(),
            &fixtures::scan_done(),
            now(),
        );
        let ids: Vec<CheckId> = checks.iter().map(|c| c.id).collect();
        assert_eq!(ids, CheckId::ALL.to_vec());
        for check in &checks {
            assert_eq!(
                check.state,
                CheckState::Good,
                "{:?}: {}",
                check.id,
                check.summary
            );
            assert_eq!(check.group, check.id.group());
            assert_eq!(check.severity, check.id.max_severity());
        }
        let score = score(&checks);
        assert_eq!(
            (score.value, score.grade, score.to_fix),
            (100, Grade::Good, 0)
        );
        assert_eq!(score.checked, 24);
        assert!(notes(&fixtures::raw(), &fixtures::ctx())
            .contains(&"Drive encryption can only be checked with administrator rights.".into()));
    }

    #[test]
    fn allowed_uris() {
        for uri in [
            "ms-settings:windowsupdate",
            "windowsdefender://threat/",
            "windowsdefender://history",
            "WindowsDefender://Network/",
            BITLOCKER_URI,
        ] {
            assert!(allowed_uri(uri), "{uri}");
        }
        for uri in [
            "ms-settings:",
            "http://example.com",
            "file:///C:/Windows",
            "windowsdefender://quickscan/",
            "windowsdefender://fullscan/",
            "windowsdefender://enablertp/",
            "windowsdefender://update/",
            "windowsdefender://updateandquickscan/",
            "windowsdefender://enableandupdate/",
            "windowsdefender://wdoscan/",
            "windowsdefender://reboot/",
            "windowsdefender://",
            "shell:::{00000000-0000-0000-0000-000000000000}",
            "shell:startup",
        ] {
            assert!(!allowed_uri(uri), "{uri}");
        }
    }

    /// Readings that drive most checks into attention or unknown, on Home and Pro.
    fn troubled() -> Vec<CheckupRaw> {
        let mut bad = fixtures::raw();
        fixtures::with_defender(&mut bad, |d| {
            d.realtime = Some(false);
            d.tamper_protected = Some(false);
            d.computer_state = Some(4);
            d.signature_age_days = Some(9);
        });
        bad.os = Ok(OsRaw {
            edition_id: "Core".into(),
            build: 26200,
            fast_startup: None,
        });
        if let Ok(fw) = &mut bad.firewall {
            fw.state.public.enabled = false;
        }
        if let Ok(accounts) = &mut bad.accounts {
            accounts.enable_lua = Some(0);
            accounts.auto_logon = Some("1".into());
            accounts.admin_approval_mode = Some(2);
        }
        if let Ok(remote) = &mut bad.remote {
            remote.assistance = Some(1);
            remote.smb1_client_key = true;
        }
        if let Ok(wu) = &mut bad.windows_update {
            wu.service_start = Some(crate::win::scm::StartType::Disabled);
        }
        if let Ok(super::super::device::EncryptionRaw::Volumes { volumes, .. }) =
            &mut bad.encryption
        {
            volumes[0].protection = Some(0);
            volumes[0].conversion = Some(0);
        }
        let mut pro = bad.clone();
        pro.os = fixtures::raw().os;
        if let Ok(remote) = &mut pro.remote {
            remote.deny_connections = Some(0);
        }
        if let Ok(security_center) = &mut pro.security_center {
            security_center.antivirus.push(fixtures::product(
                "Contoso Antivirus",
                crate::win::security_center::ProductState::Off,
                true,
            ));
        }
        let mut failed = fixtures::raw();
        failed.os = Err("x".into());
        failed.device = Err("x".into());
        failed.security_center = Err("x".into());
        failed.defender = Err("x".into());
        failed.firewall = Err("x".into());
        failed.windows_update = Err("x".into());
        failed.encryption = Err("x".into());
        failed.accounts = Err("x".into());
        failed.remote = Err("x".into());
        failed.apps = Err("x".into());
        let mut not_elevated = fixtures::raw();
        not_elevated.encryption = Ok(super::super::device::EncryptionRaw::NotElevated);
        vec![fixtures::raw(), bad, pro, failed, not_elevated]
    }

    #[test]
    fn every_fix_uri_is_allowed() {
        let mut seen = BTreeSet::new();
        for raw in troubled() {
            for check in evaluate(&raw, &fixtures::ctx(), &fixtures::scan_done(), now()) {
                assert!(!check.summary.is_empty(), "{:?}", check.id);
                for fix in &check.fixes {
                    match &fix.action {
                        FixAction::Uri { uri } => {
                            assert!(allowed_uri(uri), "{:?}: {uri}", check.id);
                            seen.insert(uri.clone());
                        }
                        FixAction::WindowsTool { tool, .. } => {
                            assert!(
                                crate::tools::windows_tools::windows_tool(tool).is_some(),
                                "{tool}"
                            );
                        }
                        FixAction::Tweak { id } => {
                            assert!(crate::debloat::catalog::tweak(id).is_some(), "{id}");
                        }
                        FixAction::UpdateScan { .. } | FixAction::Elevate => {}
                    }
                }
            }
        }
        assert!(seen.len() >= 15, "{seen:?}");
        assert!(seen.contains(BITLOCKER_URI));
        assert!(seen.contains("ms-settings:deviceencryption"));
    }

    #[test]
    fn fix_tools_carry_their_admin_need() {
        let check = Check::new(CheckId::Uac, "")
            .tool("Open", "uac_settings")
            .tool("Open", "remote_settings");
        assert_eq!(
            check.fixes[0].action,
            FixAction::WindowsTool {
                tool: "uac_settings".into(),
                requires_admin: false
            }
        );
        assert_eq!(
            check.fixes[1].action,
            FixAction::WindowsTool {
                tool: "remote_settings".into(),
                requires_admin: true
            }
        );
    }

    fn keys(value: &Value) -> BTreeSet<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn checkup_keys_match_the_contract() {
        let checks = evaluate(
            &fixtures::raw(),
            &fixtures::ctx(),
            &fixtures::scan_done(),
            now(),
        );
        let checkup = Checkup {
            taken_at: now(),
            duration_ms: 1300,
            elevated: false,
            other_user: Some(false),
            home_edition: false,
            score: score(&checks),
            checks,
            update_scan: fixtures::scan_done(),
            update_scan_due: false,
            notes: Vec::new(),
            errors: vec![SourceError {
                source: Source::SecurityCenter,
                message: "x".into(),
            }],
        };
        let json = serde_json::to_value(&checkup).unwrap();
        assert_eq!(
            keys(&json),
            set(&[
                "taken_at",
                "duration_ms",
                "elevated",
                "other_user",
                "home_edition",
                "score",
                "checks",
                "update_scan",
                "update_scan_due",
                "notes",
                "errors"
            ])
        );
        assert_eq!(
            keys(&json["score"]),
            set(&["value", "grade", "to_fix", "critical", "unknown", "checked"])
        );
        assert_eq!(json["score"]["grade"], "good");
        assert_eq!(keys(&json["errors"][0]), set(&["source", "message"]));
        assert_eq!(json["errors"][0]["source"], "security_center");
        assert_eq!(
            keys(&json["checks"][0]),
            set(&[
                "id",
                "group",
                "title",
                "state",
                "severity",
                "summary",
                "detail",
                "facts",
                "fixes",
                "needs_admin",
                "per_user"
            ])
        );
        assert_eq!(json["checks"][0]["id"], "antivirus");
        assert_eq!(json["checks"][0]["group"], "protection");
        assert_eq!(json["checks"][0]["state"], "good");
        assert_eq!(json["checks"][0]["severity"], "critical");
        assert_eq!(json["checks"][21]["id"], "edge_smartscreen");
        assert_eq!(json["checks"][21]["per_user"], true);
        assert_eq!(
            keys(&json["checks"][0]["facts"][0]),
            set(&["label", "value"])
        );
        assert_eq!(
            keys(&json["checks"][0]["fixes"][0]),
            set(&["label", "action", "note"])
        );
        assert_eq!(
            keys(&json["update_scan"]),
            set(&[
                "state",
                "online",
                "started_at",
                "finished_at",
                "elapsed_ms",
                "updates",
                "error"
            ])
        );
        assert_eq!(json["update_scan"]["state"], "done");
        let actions = [
            (
                FixAction::Uri {
                    uri: "ms-settings:windowsupdate".into(),
                },
                vec!["kind", "uri"],
                "uri",
            ),
            (
                FixAction::WindowsTool {
                    tool: "services".into(),
                    requires_admin: true,
                },
                vec!["kind", "tool", "requires_admin"],
                "windows_tool",
            ),
            (
                FixAction::Tweak {
                    id: "interface.file_extensions".into(),
                },
                vec!["kind", "id"],
                "tweak",
            ),
            (
                FixAction::UpdateScan { online: true },
                vec!["kind", "online"],
                "update_scan",
            ),
            (FixAction::Elevate, vec!["kind"], "elevate"),
        ];
        for (action, names, kind) in actions {
            let json = serde_json::to_value(&action).unwrap();
            assert_eq!(keys(&json), set(&names));
            assert_eq!(json["kind"], kind);
            let back: FixAction = serde_json::from_value(json).unwrap();
            assert_eq!(back, action);
        }
        let update = serde_json::to_value(PendingUpdate {
            title: "Update".into(),
            kb: vec!["5030000".into()],
            msrc_severity: None,
            security: true,
            downloaded: false,
            released_at: None,
        })
        .unwrap();
        assert_eq!(
            keys(&update),
            set(&[
                "title",
                "kb",
                "msrc_severity",
                "security",
                "downloaded",
                "released_at"
            ])
        );
        let back: Checkup =
            serde_json::from_value(serde_json::to_value(&checkup).unwrap()).unwrap();
        assert_eq!(back, checkup);
    }

    // ───────────── checkup_with over a scripted probe ─────────────

    #[derive(Debug)]
    struct Idle;

    impl UpdateAgent for Idle {
        fn search(
            &self,
            _online: bool,
            _cancel: &std::sync::atomic::AtomicBool,
            _deadline: Duration,
        ) -> crate::Result<SearchOutcome> {
            Ok(SearchOutcome::Found(Vec::new()))
        }
    }

    /// The fixture readings, except that the firewall read fails, Defender panics and the
    /// Windows Update read takes longer than the deadline.
    struct Scripted;

    impl Probe for Scripted {
        fn context(&self) -> Context {
            fixtures::ctx()
        }
        fn os(&self) -> crate::Result<OsRaw> {
            fixtures::raw().os.map_err(crate::Error::Other)
        }
        fn device(&self) -> crate::Result<crate::sysinfo::SecurityInfo> {
            Ok(fixtures::device())
        }
        fn security_center(&self) -> crate::Result<protection::WscRaw> {
            fixtures::raw().security_center.map_err(crate::Error::Other)
        }
        fn defender(&self) -> crate::Result<protection::DefenderRead> {
            panic!("WMI crashed")
        }
        fn firewall(&self) -> crate::Result<network::FirewallRaw> {
            Err(crate::Error::Other(
                "the firewall service is not running".into(),
            ))
        }
        fn windows_update(&self) -> crate::Result<updates::WuRaw> {
            std::thread::sleep(Duration::from_millis(1500));
            Ok(fixtures::windows_update())
        }
        fn encryption(&self, elevated: bool) -> crate::Result<device::EncryptionRaw> {
            assert!(!elevated);
            Ok(device::EncryptionRaw::NotElevated)
        }
        fn accounts(&self, _ctx: &Context) -> crate::Result<accounts::AccountsRaw> {
            Ok(fixtures::accounts())
        }
        fn remote(&self) -> crate::Result<network::RemoteRaw> {
            Ok(fixtures::remote())
        }
        fn apps(&self, hive: &UserHive) -> crate::Result<apps::AppsRaw> {
            assert_eq!(*hive, UserHive::Current);
            Ok(fixtures::apps())
        }
    }

    #[test]
    fn failing_sources_make_their_checks_unknown() {
        let scanner = UpdateScanner::new(Arc::new(Idle));
        let started = Instant::now();
        let checkup = checkup_with(
            Arc::new(Scripted),
            &scanner,
            now(),
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_millis(1400));
        assert_eq!(checkup.checks.len(), 24);
        let by_id = |id: CheckId| checkup.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(by_id(CheckId::Firewall).state, CheckState::Unknown);
        assert_eq!(
            by_id(CheckId::Firewall).summary,
            "Could not read Windows Firewall: the firewall service is not running"
        );
        assert_eq!(by_id(CheckId::WindowsUpdate).state, CheckState::Unknown);
        assert_eq!(
            by_id(CheckId::WindowsUpdate).summary,
            "Could not read Windows Update: Windows Update did not answer within 300 ms"
        );
        // Security Center still says Defender is on.
        assert_eq!(by_id(CheckId::Antivirus).state, CheckState::Good);
        assert_eq!(
            by_id(CheckId::RealtimeProtection).state,
            CheckState::Unknown
        );
        assert_eq!(by_id(CheckId::Encryption).state, CheckState::Unknown);
        assert!(by_id(CheckId::Encryption).needs_admin);
        let sources: Vec<Source> = checkup.errors.iter().map(|e| e.source).collect();
        assert_eq!(
            sources,
            vec![Source::Defender, Source::Firewall, Source::WindowsUpdate]
        );
        assert_eq!(checkup.errors[0].message, "internal error: WMI crashed");
        assert_eq!(checkup.update_scan.state, ScanState::Idle);
        assert!(checkup.update_scan_due);
        assert!(checkup
            .notes
            .contains(&"Drive encryption can only be checked with administrator rights.".into()));
        assert_eq!(checkup.other_user, Some(false));
        assert!(!checkup.home_edition);
        assert_eq!(checkup.taken_at, now());
    }

    #[test]
    fn deadlines_read_in_seconds_or_milliseconds() {
        assert_eq!(deadline_text(CHECKUP_DEADLINE), "8 s");
        assert_eq!(deadline_text(Duration::from_millis(300)), "300 ms");
    }
}
