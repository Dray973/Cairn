//! Windows Update: the service, policies, pauses and history (check 10), the updates waiting
//! to install (check 11, from the update scanner) and a pending restart (check 12).
//!
//! The update scanner runs one Windows Update search at a time on its own engine thread, so
//! the window never waits for it: `start` returns at once, `view` clones in-memory state and
//! `cancel` sets a flag. Locks are never held across a COM call.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::checkup::{unreadable, Check, CheckId, FixAction, Severity};
use super::network::worst;
use super::probe::{hklm, hklm_exists, key_dword, key_text, CheckupRaw};
use super::text::{age_text, days_between, local_date, plural};
use crate::win::event_log::{self, RenderContext, SYSTEM_TIME_CREATED};
use crate::win::scm::{self, Scm, StartType};
use crate::win::update_agent::{self, FoundUpdate, HistoryEntry, SearchOutcome, UpdateStatus};
use crate::{Error, Result};

const AU_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU";
const WU_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
const UX_SETTINGS: &str = r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings";
const PAUSE_VALUES: [&str; 3] = [
    "PauseUpdatesExpiryTime",
    "PauseQualityUpdatesEndTime",
    "PauseFeatureUpdatesEndTime",
];
const REBOOT_REQUIRED_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired";
const REBOOT_PENDING_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending";
/// Windows Update client log and the id of "Windows Update successfully found {n} updates",
/// logged at every search, whichever component started it.
const WU_CLIENT_LOG: &str = "Microsoft-Windows-WindowsUpdateClient/Operational";
const SEARCH_FOUND_EVENT: &str = "*[System[EventID=26]]";

/// Category of Defender definition updates.
const DEFINITION_CATEGORY: &str = "e0789628-ce08-4437-be74-2495b842f43b";
/// Categories "Security Updates" and "Critical Updates".
const SECURITY_CATEGORIES: [&str; 2] = [
    "0fa1201d-4330-4fa8-8ae9-b877473b6441",
    "e6cf1350-c01b-414d-a61f-263d14d133b4",
];
/// The Windows Update service that delivers Windows 11's own monthly and critical updates.
/// The agent records their installations without category ids.
const QUALITY_SERVICE: &str = "8b24b027-1dee-babb-9a95-3517dfb9c552";
/// Knowledge-base numbers of updates that keep installing while Windows' own updates fail:
/// Microsoft Defender's definitions (2267602, 915597) and antimalware platform (4052623), the
/// Windows Security platform (5007651) and the monthly Malicious Software Removal Tool
/// (890830).
const OTHER_KBS: [&str; 5] = ["2267602", "915597", "4052623", "5007651", "890830"];
/// Host of the support pages named after a knowledge-base number ("/help/5030000").
const SUPPORT_HOST: &str = "support.microsoft.com";
/// How much the event log and the Automatic Updates record may disagree before both show.
const LAST_CHECK_DISAGREEMENT_DAYS: i64 = 7;
/// A security update published longer ago than this is overdue.
const SECURITY_OVERDUE_DAYS: i64 = 14;
/// History entries read at most while looking for the last security update.
const HISTORY_MAX: i32 = 1000;
/// Oldest the last security update gets on a PC that keeps up: the monthly updates come up to
/// 35 days apart, and installing one can wait a week for a restart. Older, the latest monthly
/// update is late.
const INSTALL_LATE_DAYS: i64 = 42;
/// Age of the last security update from which the latest monthly update is three weeks late
/// or more.
const INSTALL_STALLED_DAYS: i64 = 56;

/// How long an offline and an online search may run.
const OFFLINE_DEADLINE: Duration = Duration::from_secs(3 * 60);
const ONLINE_DEADLINE: Duration = Duration::from_secs(15 * 60);
/// A search that finished longer ago than this is repeated when the checkup loads.
const SCAN_FRESH_MINUTES: i64 = 30;

const WINDOWS_UPDATE_PAGE: &str = "ms-settings:windowsupdate";

/// Windows Update's configuration and records.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WuRaw {
    pub(crate) service_start: Option<StartType>,
    pub(crate) no_auto_update: Option<u32>,
    pub(crate) wu_server: Option<String>,
    pub(crate) use_wu_server: Option<u32>,
    /// Pause end times as stored (RFC 3339 text).
    pub(crate) pause_values: Vec<String>,
    /// The Windows Update Agent's status, or why it could not be read.
    pub(crate) status: std::result::Result<UpdateStatus, String>,
    /// Newest "search found updates" event (26) of the Windows Update client log.
    pub(crate) last_scan_event: Option<DateTime<Utc>>,
    /// One of the reboot-pending registry keys exists.
    pub(crate) reboot_pending_keys: bool,
}

impl Default for WuRaw {
    /// Nothing configured and an empty agent status.
    fn default() -> WuRaw {
        WuRaw {
            service_start: None,
            no_auto_update: None,
            wu_server: None,
            use_wu_server: None,
            pause_values: Vec::new(),
            status: Ok(UpdateStatus::default()),
            last_scan_event: None,
            reboot_pending_keys: false,
        }
    }
}

/// Reads Windows Update's service, policies, pauses, event log and agent status. Creating
/// the agent's objects can start the demand-start Windows Update service.
pub(crate) fn read_windows_update() -> Result<WuRaw> {
    let service_start = Scm::connect()
        .ok()
        .and_then(|scm| scm.open("wuauserv", scm::READ_ACCESS).ok().flatten())
        .and_then(|service| service.config().ok())
        .map(|config| config.start_type);
    let au = hklm(AU_POLICY);
    let policy = hklm(WU_POLICY);
    let ux = hklm(UX_SETTINGS);
    let pause_values = PAUSE_VALUES
        .iter()
        .filter_map(|name| key_text(ux.as_ref(), name))
        .filter(|v| !v.is_empty())
        .collect();
    let last_scan_event = newest_search_event();
    let status = update_agent::status(HISTORY_MAX, &is_security_install)
        .map_err(|e| update_agent::error_text(&e));
    Ok(WuRaw {
        service_start,
        no_auto_update: key_dword(au.as_ref(), "NoAutoUpdate"),
        wu_server: key_text(policy.as_ref(), "WUServer").filter(|s| !s.is_empty()),
        use_wu_server: key_dword(au.as_ref(), "UseWUServer"),
        pause_values,
        status,
        last_scan_event,
        reboot_pending_keys: hklm_exists(REBOOT_REQUIRED_KEY) || hklm_exists(REBOOT_PENDING_KEY),
    })
}

/// Time of the newest event 26 of the Windows Update client log; `None` when the log holds
/// none or cannot be read.
fn newest_search_event() -> Option<DateTime<Utc>> {
    let mut query = event_log::query(WU_CLIENT_LOG, SEARCH_FOUND_EVENT, true).ok()?;
    let events = query.next_batch(1).ok()?;
    let event = events.first()?;
    let values = RenderContext::system().ok()?.render(event).ok()?;
    values.get(SYSTEM_TIME_CREATED)?.as_time()
}

fn guid_eq(a: &str, b: &str) -> bool {
    a.trim().trim_matches(['{', '}']).eq_ignore_ascii_case(b)
}

/// Whether one of `category_ids` is "Security Updates" or "Critical Updates".
fn security_category(category_ids: &[String]) -> bool {
    category_ids
        .iter()
        .any(|c| SECURITY_CATEGORIES.iter().any(|s| guid_eq(c, s)))
}

/// Whether the agent recorded a category id for the entry: a GUID other than the nil GUID.
/// An empty id, as Windows 11 records for its own updates, is none.
fn has_category_ids(entry: &HistoryEntry) -> bool {
    entry.category_ids.iter().any(|id| {
        let id = id.trim().trim_matches(['{', '}']);
        id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
            && id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
            && id.bytes().any(|b| b != b'-' && b != b'0')
    })
}

/// The digits after each "KB" (any case) that starts a word of `title`. Titles are
/// translated, but their knowledge-base numbers are not.
fn title_kbs(title: &str) -> Vec<&str> {
    let bytes = title.as_bytes();
    let mut numbers = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        let starts_word = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if starts_word && bytes[i..i + 2].eq_ignore_ascii_case(b"kb") {
            let digits = bytes[i + 2..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if digits > 0 {
                // ASCII bytes are character boundaries, so the slice is valid.
                numbers.push(&title[i + 2..i + 2 + digits]);
                i += 2 + digits;
                continue;
            }
        }
        i += 1;
    }
    numbers
}

/// The knowledge-base number of a support page named after it:
/// "https://support.microsoft.com/help/5030000", also with a language segment or "/kb/".
fn support_page_kb(url: &str) -> Option<&str> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")) {
        return None;
    }
    let (host, path) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case(SUPPORT_HOST) {
        return None;
    }
    let mut segments = path.split(['?', '#']).next().unwrap_or("").split('/');
    segments
        .by_ref()
        .find(|s| s.eq_ignore_ascii_case("help") || s.eq_ignore_ascii_case("kb"))?;
    segments
        .next()
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The knowledge-base numbers an entry names in its title or support page.
fn kb_numbers(entry: &HistoryEntry) -> Vec<&str> {
    let mut numbers = title_kbs(&entry.title);
    numbers.extend(support_page_kb(&entry.support_url));
    numbers
}

/// A successful installation of a security or critical update. An entry with category ids
/// counts when one of them is Security Updates or Critical Updates and none is Definition
/// Updates. Windows 11 records its own updates without category ids; such an entry counts
/// when it came from [`QUALITY_SERVICE`] and names a knowledge-base number. The agent records
/// other installations too (Microsoft Defender's definition and platform updates, Microsoft
/// Store apps, the monthly removal tool), and those keep coming while Windows' own updates
/// fail, so they never count: [`OTHER_KBS`] are left out whatever their categories or service.
pub(crate) fn is_security_install(entry: &HistoryEntry) -> bool {
    if !(entry.installation && entry.succeeded && entry.date.is_some()) {
        return false;
    }
    let kbs = kb_numbers(entry);
    if kbs
        .iter()
        .any(|kb| OTHER_KBS.contains(&kb.trim_start_matches('0')))
    {
        return false;
    }
    if has_category_ids(entry) {
        security_category(&entry.category_ids)
            && !entry
                .category_ids
                .iter()
                .any(|c| guid_eq(c, DEFINITION_CATEGORY))
    } else {
        guid_eq(&entry.service_id, QUALITY_SERVICE) && !kbs.is_empty()
    }
}

/// The newest successful installation of a security or critical update.
pub(crate) fn last_installed(status: &UpdateStatus) -> Option<&HistoryEntry> {
    status
        .history
        .iter()
        .filter(|e| is_security_install(e))
        .max_by_key(|e| e.date)
}

/// The latest pause end in the future; `Err` with the raw text when a value cannot be
/// parsed.
fn paused_until(values: &[String], now: DateTime<Utc>) -> (Option<DateTime<Utc>>, Vec<String>) {
    let mut latest: Option<DateTime<Utc>> = None;
    let mut unparsed = Vec::new();
    for value in values {
        match DateTime::parse_from_rfc3339(value.trim()) {
            Ok(t) => {
                let t = t.with_timezone(&Utc);
                if t > now && latest.map_or(true, |l| t > l) {
                    latest = Some(t);
                }
            }
            Err(_) => unparsed.push(value.clone()),
        }
    }
    (latest, unparsed)
}

/// When Windows Update last searched: the newest event 26, else the Automatic Updates
/// record, and the Automatic Updates record when it disagrees by more than a week.
fn last_check(wu: &WuRaw) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let au = wu.status.as_ref().ok().and_then(|s| s.last_search);
    match (wu.last_scan_event, au) {
        (Some(event), Some(au)) if (event - au).num_days().abs() > LAST_CHECK_DISAGREEMENT_DAYS => {
            (Some(event), Some(au))
        }
        (Some(event), _) => (Some(event), None),
        (None, au) => (au, None),
    }
}

const WU_DETAIL: &str = "Monthly updates fix security holes that attackers already know about.";

/// Check 10: Windows Update runs and installs updates.
pub(crate) fn windows_update(raw: &CheckupRaw, now: DateTime<Utc>) -> Check {
    let wu = match &raw.windows_update {
        Ok(wu) => wu,
        Err(e) => {
            return unreadable(CheckId::WindowsUpdate, WU_DETAIL, "Windows Update", e)
                .uri("Open Windows Update", WINDOWS_UPDATE_PAGE)
        }
    };
    let status = wu.status.as_ref().ok();
    let installed = status.and_then(last_installed);
    let (checked, au_record) = last_check(wu);
    let (paused, unparsed) = paused_until(&wu.pause_values, now);

    let mut check = Check::new(CheckId::WindowsUpdate, WU_DETAIL).fact(
        "Last check",
        checked.map(local_date).unwrap_or_else(|| "unknown".into()),
    );
    if let Some(au) = au_record {
        check = check.line(format!(
            "Windows Update's own record says the last check was {}",
            local_date(au)
        ));
    }
    if let Some(entry) = installed {
        let date = entry.date.map(local_date).unwrap_or_default();
        check = check.fact("Last installed", format!("{} ({date})", entry.title));
    }
    if let Some(until) = paused {
        check = check.fact("Paused until", local_date(until));
    }
    for raw_value in &unparsed {
        check = check.fact("Paused until", raw_value.clone());
    }
    if wu.wu_server.is_some() && wu.use_wu_server == Some(1) {
        check = check.line("Updates come from your organization's update server");
    }
    check = check.uri("Open Windows Update", WINDOWS_UPDATE_PAGE);
    let disabled = wu.service_start == Some(StartType::Disabled);
    if disabled {
        check = check.tool("Open Services", "services");
    }

    let mut causes: Vec<(Severity, String)> = Vec::new();
    if disabled {
        causes.push((
            Severity::High,
            "The Windows Update service is disabled".into(),
        ));
    }
    if wu.no_auto_update == Some(1) {
        causes.push((
            Severity::High,
            "Automatic updates are turned off by policy".into(),
        ));
    }
    let install_days = installed.and_then(|e| e.date).map(|d| days_between(d, now));
    if let Some(days) = install_days.filter(|d| *d > INSTALL_STALLED_DAYS) {
        causes.push((
            Severity::High,
            format!("No updates installed for {}", plural(days as u64, "day")),
        ));
    }
    if let Some(until) = paused {
        causes.push((
            Severity::Medium,
            format!("Updates are paused until {}", local_date(until)),
        ));
    }
    if let Some(days) = checked.map(|t| days_between(t, now)).filter(|d| *d > 7) {
        causes.push((
            Severity::Medium,
            format!(
                "Windows Update hasn't checked for {}",
                plural(days as u64, "day")
            ),
        ));
    }
    if let Some(days) =
        install_days.filter(|d| *d > INSTALL_LATE_DAYS && *d <= INSTALL_STALLED_DAYS)
    {
        causes.push((
            Severity::Medium,
            format!("Last update was {} ago", plural(days as u64, "day")),
        ));
    }
    match (worst(causes), &wu.status) {
        (Some((severity, summary)), _) => check.attention(severity, summary),
        (None, Err(e)) => check.unknown(format!("Could not read Windows Update: {e}")),
        (None, Ok(_)) => match installed.and_then(|e| e.date) {
            Some(date) => check.good(format!("Updates installed {}", age_text(date, now))),
            None => check.unknown("No security update was found in Windows Update's history"),
        },
    }
}

const PENDING_DETAIL: &str =
    "Windows installs these on its own schedule; installing them now closes the gaps sooner.";

/// Titles listed under the pending-updates check.
const LISTED_TITLES: usize = 5;

/// Check 11: updates waiting to install, from the latest search of this process.
pub(crate) fn pending_updates(
    raw: &CheckupRaw,
    scan: &UpdateScanView,
    now: DateTime<Utc>,
) -> Check {
    let check = Check::new(CheckId::PendingUpdates, PENDING_DETAIL)
        .fix("Check online now", FixAction::UpdateScan { online: true })
        .uri("Open Windows Update", WINDOWS_UPDATE_PAGE);
    match scan.state {
        ScanState::Running if scan.online => check.checking("Checking Windows Update online…"),
        ScanState::Running => check.checking("Checking for waiting updates…"),
        ScanState::Idle => check.unknown("Not checked yet"),
        ScanState::Cancelled => check.unknown("The check was stopped"),
        ScanState::Failed => check.unknown(
            scan.error
                .clone()
                .unwrap_or_else(|| "The check failed".into()),
        ),
        ScanState::Done => {
            let mut check = check;
            for update in scan.updates.iter().take(LISTED_TITLES) {
                check = check.line(format!("• {}", update.title));
            }
            if scan.updates.len() > LISTED_TITLES {
                check = check.line(format!("and {} more", scan.updates.len() - LISTED_TITLES));
            }
            let security: Vec<&PendingUpdate> =
                scan.updates.iter().filter(|u| u.security).collect();
            let others = scan.updates.len() - security.len();
            let overdue = security
                .iter()
                .filter_map(|u| u.released_at)
                .map(|t| days_between(t, now))
                .filter(|d| *d > SECURITY_OVERDUE_DAYS)
                .max();
            let n = security.len() as u64;
            if let Some(days) = overdue {
                check.attention(
                    Severity::High,
                    format!(
                        "{} waiting for {}",
                        plural(n, "security update"),
                        plural(days as u64, "day")
                    ),
                )
            } else if n > 0 {
                check.attention(
                    Severity::Medium,
                    format!("{} waiting", plural(n, "security update")),
                )
            } else if others > 0 {
                check.attention(
                    Severity::Low,
                    format!("{} available", plural(others as u64, "other update")),
                )
            } else if scan.online {
                check.good("None found online just now")
            } else {
                // An offline search reads Windows Update's cached data, so only Windows
                // Update's own record dates the check it reflects.
                let checked = raw
                    .windows_update
                    .as_ref()
                    .ok()
                    .and_then(|wu| last_check(wu).0);
                match checked {
                    Some(t) => check.good(format!(
                        "None found in Windows Update's last check ({})",
                        local_date(t)
                    )),
                    None => check.good("None found in Windows Update's last check"),
                }
            }
        }
    }
}

/// Check 12: a restart pending for updates.
pub(crate) fn update_restart(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Updates that wait for a restart don't protect the PC yet.";
    let wu = match &raw.windows_update {
        Ok(wu) => wu,
        Err(e) => {
            return unreadable(CheckId::UpdateRestart, DETAIL, "Windows Update", e)
                .uri("Open Windows Update", WINDOWS_UPDATE_PAGE)
        }
    };
    let check =
        Check::new(CheckId::UpdateRestart, DETAIL).uri("Open Windows Update", WINDOWS_UPDATE_PAGE);
    let agent = wu
        .status
        .as_ref()
        .ok()
        .and_then(|s| s.reboot_required)
        .unwrap_or(false);
    if agent || wu.reboot_pending_keys {
        check.attention(Severity::Medium, "Restart to finish installing updates")
    } else {
        check.good("No restart pending")
    }
}

// ───────────────────────────── Update scanner ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// No search ran in this process yet.
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// An update a search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub title: String,
    /// Knowledge-base article numbers.
    pub kb: Vec<String>,
    pub msrc_severity: Option<String>,
    /// A security or critical update, or one with an MSRC severity.
    pub security: bool,
    pub downloaded: bool,
    /// When the update was last published or changed.
    pub released_at: Option<DateTime<Utc>>,
}

impl PendingUpdate {
    fn from_found(found: FoundUpdate) -> PendingUpdate {
        let security = security_category(&found.category_ids)
            || found
                .msrc_severity
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
        PendingUpdate {
            title: found.title,
            kb: found.kb,
            msrc_severity: found.msrc_severity,
            security,
            downloaded: found.downloaded,
            released_at: found.released_at,
        }
    }
}

/// State of the latest Windows Update search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateScanView {
    pub state: ScanState,
    pub online: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Time the search ran, up to now while it runs.
    pub elapsed_ms: u64,
    pub updates: Vec<PendingUpdate>,
    pub error: Option<String>,
}

/// Runs Windows Update searches; `LiveAgent` searches this PC.
pub(crate) trait UpdateAgent: Send + Sync + fmt::Debug {
    fn search(
        &self,
        online: bool,
        cancel: &AtomicBool,
        deadline: Duration,
    ) -> Result<SearchOutcome>;
}

/// Searches through the Windows Update Agent (refused while searches are forbidden).
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveAgent;

impl UpdateAgent for LiveAgent {
    fn search(
        &self,
        online: bool,
        cancel: &AtomicBool,
        deadline: Duration,
    ) -> Result<SearchOutcome> {
        update_agent::search(online, cancel, deadline)
    }
}

#[derive(Debug, Clone)]
struct ScanShared {
    state: ScanState,
    online: bool,
    started_at: Option<DateTime<Utc>>,
    started: Option<Instant>,
    finished_at: Option<DateTime<Utc>>,
    elapsed_ms: u64,
    updates: Vec<PendingUpdate>,
    error: Option<String>,
}

impl ScanShared {
    fn idle() -> ScanShared {
        ScanShared {
            state: ScanState::Idle,
            online: false,
            started_at: None,
            started: None,
            finished_at: None,
            elapsed_ms: 0,
            updates: Vec::new(),
            error: None,
        }
    }
}

/// One Windows Update search at a time on an engine thread.
pub(crate) struct UpdateScanner {
    agent: Arc<dyn UpdateAgent>,
    shared: Arc<Mutex<ScanShared>>,
    cancel: Arc<AtomicBool>,
    thread_alive: Arc<AtomicBool>,
}

impl fmt::Debug for UpdateScanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateScanner")
            .field("agent", &self.agent)
            .field("state", &self.shared.lock().state)
            .finish_non_exhaustive()
    }
}

fn minutes(d: Duration) -> u64 {
    d.as_secs().div_ceil(60)
}

impl UpdateScanner {
    pub(crate) fn new(agent: Arc<dyn UpdateAgent>) -> UpdateScanner {
        UpdateScanner {
            agent,
            shared: Arc::new(Mutex::new(ScanShared::idle())),
            cancel: Arc::new(AtomicBool::new(false)),
            thread_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts a search on the "health-update-scan" thread and returns its view at once.
    /// Refused while a search runs or its thread is still ending.
    pub(crate) fn start(&self, online: bool) -> Result<UpdateScanView> {
        // Claiming the thread slot atomically makes concurrent starts refuse all but one.
        if self
            .thread_alive
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let running = self.shared.lock().state == ScanState::Running;
            return Err(Error::Other(if running {
                "Windows Update is already being checked.".into()
            } else {
                "The previous check is still ending; try again in a minute.".into()
            }));
        }
        self.cancel.store(false, Ordering::SeqCst);
        *self.shared.lock() = ScanShared {
            state: ScanState::Running,
            online,
            started_at: Some(Utc::now()),
            started: Some(Instant::now()),
            ..ScanShared::idle()
        };
        let agent = Arc::clone(&self.agent);
        let shared = Arc::clone(&self.shared);
        let cancel = Arc::clone(&self.cancel);
        let alive = Arc::clone(&self.thread_alive);
        let spawned = thread::Builder::new()
            .name("health-update-scan".into())
            .spawn(move || {
                let _com = crate::win::com::enter_mta();
                let deadline = if online {
                    ONLINE_DEADLINE
                } else {
                    OFFLINE_DEADLINE
                };
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    agent.search(online, &cancel, deadline)
                }));
                {
                    let mut s = shared.lock();
                    s.elapsed_ms = s
                        .started
                        .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
                        .unwrap_or(0);
                    s.finished_at = Some(Utc::now());
                    match outcome {
                        Ok(Ok(SearchOutcome::Found(found))) => {
                            s.state = ScanState::Done;
                            s.updates = found.into_iter().map(PendingUpdate::from_found).collect();
                        }
                        Ok(Ok(SearchOutcome::Cancelled)) => s.state = ScanState::Cancelled,
                        Ok(Ok(SearchOutcome::TimedOut)) => {
                            s.state = ScanState::Failed;
                            s.error = Some(format!(
                                "Windows Update did not finish checking within {}.",
                                plural(minutes(deadline), "minute")
                            ));
                        }
                        Ok(Err(e)) => {
                            s.state = ScanState::Failed;
                            s.error = Some(update_agent::error_text(&e));
                        }
                        Err(payload) => {
                            let text = payload
                                .downcast_ref::<&str>()
                                .map(|s| (*s).to_string())
                                .or_else(|| payload.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "unknown panic".to_string());
                            s.state = ScanState::Failed;
                            s.error = Some(format!("internal error: {text}"));
                        }
                    }
                }
                alive.store(false, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            {
                let mut s = self.shared.lock();
                s.state = ScanState::Failed;
                s.finished_at = Some(Utc::now());
                s.error = Some(format!("could not start the check: {e}"));
            }
            self.thread_alive.store(false, Ordering::SeqCst);
            return Err(e.into());
        }
        Ok(self.view())
    }

    /// The latest search's state; clones in-memory state only.
    pub(crate) fn view(&self) -> UpdateScanView {
        let s = self.shared.lock();
        let elapsed_ms = match (s.state, s.started) {
            (ScanState::Running, Some(t)) => {
                u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
            }
            _ => s.elapsed_ms,
        };
        UpdateScanView {
            state: s.state,
            online: s.online,
            started_at: s.started_at,
            finished_at: s.finished_at,
            elapsed_ms,
            updates: s.updates.clone(),
            error: s.error.clone(),
        }
    }

    /// Asks a running search to stop; false when none runs.
    pub(crate) fn cancel(&self) -> bool {
        if self.shared.lock().state != ScanState::Running {
            return false;
        }
        self.cancel.store(true, Ordering::SeqCst);
        true
    }

    /// Whether the checkup should start an offline search: none runs, and none finished in
    /// this process or the last one finished more than 30 minutes before `now`.
    pub(crate) fn due(&self, now: DateTime<Utc>) -> bool {
        if self.thread_alive.load(Ordering::SeqCst) {
            return false;
        }
        let s = self.shared.lock();
        s.state != ScanState::Running
            && s.finished_at
                .map_or(true, |t| (now - t).num_minutes() > SCAN_FRESH_MINUTES)
    }

    /// Whether the search thread is still running.
    #[cfg(test)]
    pub(crate) fn busy(&self) -> bool {
        self.thread_alive.load(Ordering::SeqCst)
    }
}

/// The scanner of this process.
pub(crate) fn scanner() -> &'static UpdateScanner {
    static SCANNER: OnceLock<UpdateScanner> = OnceLock::new();
    SCANNER.get_or_init(|| UpdateScanner::new(Arc::new(LiveAgent)))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, Sender};

    use chrono::Duration as ChronoDuration;

    use super::super::checkup::{evaluate, CheckState};
    use super::super::probe::fixtures::{self, days_ago, installed, now};
    use super::*;

    fn wu(change: impl FnOnce(&mut WuRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(wu) = &mut raw.windows_update {
            change(wu);
        }
        raw
    }

    fn status(wu: &mut WuRaw) -> &mut UpdateStatus {
        wu.status.as_mut().unwrap()
    }

    #[test]
    fn a_healthy_windows_update_reports_the_last_install() {
        let check = windows_update(&fixtures::raw(), now());
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(check.summary, "Updates installed 10 days ago");
        assert_eq!(check.facts[0].label, "Last check");
        assert!(check
            .facts
            .iter()
            .any(|f| f.label == "Last installed" && f.value.starts_with("2026-09 Cumulative")));
        assert_eq!(
            check.fixes[0].action,
            FixAction::Uri {
                uri: WINDOWS_UPDATE_PAGE.into()
            }
        );
    }

    #[test]
    fn a_disabled_service_and_the_policy_are_high() {
        let raw = wu(|wu| wu.service_start = Some(StartType::Disabled));
        let check = windows_update(&raw, now());
        assert_eq!(
            (check.state, check.severity, check.summary.as_str()),
            (
                CheckState::Attention,
                Severity::High,
                "The Windows Update service is disabled"
            )
        );
        assert!(check.fixes.iter().any(|f| matches!(
            &f.action,
            FixAction::WindowsTool { tool, requires_admin: true } if tool == "services"
        )));
        let raw = wu(|wu| wu.no_auto_update = Some(1));
        let check = windows_update(&raw, now());
        assert_eq!(check.summary, "Automatic updates are turned off by policy");
        assert_eq!(check.severity, Severity::High);
    }

    #[test]
    fn pauses_in_the_future_count_and_past_ones_do_not() {
        let until = now() + ChronoDuration::days(5);
        let raw = wu(|wu| wu.pause_values = vec![until.to_rfc3339()]);
        let check = windows_update(&raw, now());
        assert_eq!(check.severity, Severity::Medium);
        assert!(check.summary.starts_with("Updates are paused until "));
        assert!(check.facts.iter().any(|f| f.label == "Paused until"));
        let raw = wu(|wu| wu.pause_values = vec![days_ago(3).to_rfc3339()]);
        assert_eq!(windows_update(&raw, now()).state, CheckState::Good);
        let raw = wu(|wu| wu.pause_values = vec!["next week".into()]);
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Good);
        assert!(check
            .facts
            .iter()
            .any(|f| f.label == "Paused until" && f.value == "next week"));
    }

    #[test]
    fn install_age_boundaries() {
        for (days, state, severity, summary) in [
            (42, CheckState::Good, None, "Updates installed 42 days ago"),
            (
                43,
                CheckState::Attention,
                Some(Severity::Medium),
                "Last update was 43 days ago",
            ),
            (
                56,
                CheckState::Attention,
                Some(Severity::Medium),
                "Last update was 56 days ago",
            ),
            (
                57,
                CheckState::Attention,
                Some(Severity::High),
                "No updates installed for 57 days",
            ),
        ] {
            let raw = wu(|wu| status(wu).history = vec![installed("Cumulative Update", days)]);
            let check = windows_update(&raw, now());
            assert_eq!(check.state, state, "{days}");
            assert_eq!(check.summary, summary, "{days}");
            if let Some(severity) = severity {
                assert_eq!(check.severity, severity, "{days}");
            }
        }
    }

    #[test]
    fn definition_updates_are_not_the_last_install() {
        let mut by_category = installed("Security Intelligence Update", 0);
        by_category.category_ids = vec!["E0789628-CE08-4437-BE74-2495B842F43B".into()];
        // Listed as a security update, but its knowledge-base number is the definitions'.
        let by_kb = installed("Update for Microsoft Defender Antivirus - KB2267602", 0);
        let raw = wu(|wu| {
            status(wu).history = vec![by_category, by_kb, installed("Cumulative Update", 60)]
        });
        let check = windows_update(&raw, now());
        assert_eq!(check.summary, "No updates installed for 60 days");
        let mut failed = installed("Cumulative Update", 1);
        failed.succeeded = false;
        let raw = wu(|wu| status(wu).history = vec![failed, installed("Older update", 2)]);
        assert_eq!(
            windows_update(&raw, now()).summary,
            "Updates installed 2 days ago"
        );
    }

    /// A successful installation the agent recorded with these categories.
    fn installed_in(title: &str, days: i64, categories: &[&str]) -> HistoryEntry {
        HistoryEntry {
            category_ids: categories.iter().map(|c| c.to_string()).collect(),
            ..installed(title, days)
        }
    }

    /// A successful installation as Windows 11 records it: `categories` as category ids (an
    /// empty id for a named category without one), the update service and the support page.
    fn recorded(
        title: &str,
        at: DateTime<Utc>,
        categories: &[&str],
        service: &str,
        support: &str,
    ) -> HistoryEntry {
        HistoryEntry {
            title: title.into(),
            date: Some(at),
            installation: true,
            succeeded: true,
            hresult: 0,
            category_ids: categories.iter().map(|c| c.to_string()).collect(),
            service_id: service.into(),
            support_url: support.into(),
        }
    }

    /// The monthly security update: no categories, from the quality-update service.
    fn monthly_update(at: DateTime<Utc>) -> HistoryEntry {
        recorded(
            "2026-09 Security Update (KB5129195) (26200.9457)",
            at,
            &[],
            QUALITY_SERVICE,
            "https://support.microsoft.com/help/5129195",
        )
    }

    /// The critical update Windows setup installs: one category ("OOBE ZDP") with an empty id.
    fn setup_critical_update(at: DateTime<Utc>) -> HistoryEntry {
        recorded(
            "2026-09 Critical Update (KB5128942)",
            at,
            &[""],
            QUALITY_SERVICE,
            "https://support.microsoft.com/help/5128942",
        )
    }

    /// Microsoft Defender's definition update: one category ("Microsoft Defender
    /// Antivirus") with an empty id, and no service.
    fn definition_update(at: DateTime<Utc>) -> HistoryEntry {
        recorded(
            "Security Intelligence Update for Microsoft Defender Antivirus - KB2267602 \
             (Version 1.459.491.0) - Current Channel (Broad)",
            at,
            &[""],
            "",
            "https://go.microsoft.com/fwlink/?LinkId=52661",
        )
    }

    /// Microsoft Defender's antimalware platform update, recorded like its definitions.
    fn platform_update(at: DateTime<Utc>) -> HistoryEntry {
        recorded(
            "Update for Microsoft Defender Antivirus antimalware platform - KB4052623 \
             (Version 4.18.26080.4) - Current Channel (Broad)",
            at,
            &[""],
            "",
            "https://learn.microsoft.com/en-us/defender-endpoint/\
             microsoft-defender-antivirus-updates",
        )
    }

    /// Entries the agent records besides Windows' own updates, with category ids and
    /// without: definition and platform updates of Microsoft Defender, a Microsoft Store app
    /// and the monthly removal tool (Update Rollups, or no categories from the quality-update
    /// service).
    fn other_installs() -> Vec<HistoryEntry> {
        vec![
            installed_in(
                "Security Intelligence Update for Microsoft Defender Antivirus - KB2267602",
                0,
                &["E0789628-CE08-4437-BE74-2495B842F43B"],
            ),
            installed_in("9AAAAAAAAAAA-CONTOSO.NOTES", 1, &[]),
            installed_in(
                "Windows Malicious Software Removal Tool x64 (KB890830)",
                5,
                &["28BC880E-0592-4CBF-8F95-C79B17911D5F"],
            ),
            definition_update(days_ago(0)),
            platform_update(days_ago(2)),
            recorded(
                "9AAAAAAAAAAA-CONTOSO.NOTES",
                days_ago(1),
                &[],
                QUALITY_SERVICE,
                "",
            ),
            recorded(
                "Windows Malicious Software Removal Tool x64 - v5.145 (KB890830)",
                days_ago(4),
                &[],
                QUALITY_SERVICE,
                "",
            ),
        ]
    }

    #[test]
    fn entries_without_category_ids_count_by_service_and_knowledge_base_number() {
        let at = days_ago(3);
        // Windows' own updates: the monthly update (no categories) and the critical update
        // setup installs (a category with an empty id).
        assert!(is_security_install(&monthly_update(at)));
        assert!(is_security_install(&setup_critical_update(at)));
        // Microsoft Defender's updates, the removal tool and Store apps never count, not even
        // from the quality-update service.
        let from_quality_service = |entry: HistoryEntry| HistoryEntry {
            service_id: QUALITY_SERVICE.into(),
            ..entry
        };
        for entry in other_installs().into_iter().chain([
            from_quality_service(definition_update(at)),
            from_quality_service(platform_update(at)),
            recorded(
                "Update for Windows Security platform - KB5007651 (Version 1.0.2507.26001)",
                at,
                &[""],
                QUALITY_SERVICE,
                "",
            ),
            recorded("Definition Update - kb915597", at, &[], QUALITY_SERVICE, ""),
        ]) {
            assert!(!is_security_install(&entry), "{}", entry.title);
        }
        // Another service, or no knowledge-base number: a driver, a feature update.
        let other_service = HistoryEntry {
            service_id: String::new(),
            ..monthly_update(at)
        };
        let driver = recorded(
            "Contoso - Display - 31.0.101.5592",
            at,
            &[],
            QUALITY_SERVICE,
            "https://support.microsoft.com/select/?target=hub",
        );
        let feature = recorded("Windows 11, version 26H1", at, &[], QUALITY_SERVICE, "");
        for entry in [other_service, driver, feature] {
            assert!(!is_security_install(&entry), "{}", entry.title);
        }
        // The number may come from the support page alone; the service id's form is ignored.
        let untitled = HistoryEntry {
            title: "2026-09 Sicherheitsupdate (26200.9457)".into(),
            service_id: "{8B24B027-1DEE-BABB-9A95-3517DFB9C552}".into(),
            ..monthly_update(at)
        };
        assert!(is_security_install(&untitled));
        // Failed, removed or undated installations never count.
        for entry in [
            HistoryEntry {
                succeeded: false,
                ..monthly_update(at)
            },
            HistoryEntry {
                installation: false,
                ..monthly_update(at)
            },
            HistoryEntry {
                date: None,
                ..monthly_update(at)
            },
        ] {
            assert!(!is_security_install(&entry));
        }
        // Real category ids decide on their own: Update Rollups never counts, Security
        // Updates counts from any service; a nil id is none.
        let rollup = HistoryEntry {
            category_ids: vec!["28bc880e-0592-4cbf-8f95-c79b17911d5f".into()],
            ..monthly_update(at)
        };
        assert!(!is_security_install(&rollup));
        let classic = installed_in(
            "2026-08 Cumulative Update for Windows 11 (KB5000003)",
            3,
            &["0fa1201d-4330-4fa8-8ae9-b877473b6441"],
        );
        assert!(is_security_install(&classic));
        let nil = HistoryEntry {
            category_ids: vec!["{00000000-0000-0000-0000-000000000000}".into()],
            ..monthly_update(at)
        };
        assert!(is_security_install(&nil));
    }

    #[test]
    fn knowledge_base_numbers_come_from_titles_and_support_pages() {
        assert_eq!(
            title_kbs("2026-09 Security Update (KB5129195) (26200.9457)"),
            vec!["5129195"]
        );
        assert_eq!(
            title_kbs("Security Intelligence Update - KB2267602 (Version 1.459.491.0)"),
            vec!["2267602"]
        );
        assert_eq!(
            title_kbs("kb890830 and KB5000001."),
            vec!["890830", "5000001"]
        );
        assert_eq!(
            title_kbs("Mise à jour cumulative (KB5129195)"),
            vec!["5129195"]
        );
        for none in [
            "",
            "KB",
            "KB-5129195",
            "WEBKB123",
            "Contoso - Display - 31.0.101",
        ] {
            assert!(title_kbs(none).is_empty(), "{none}");
        }
        for (url, kb) in [
            (
                "https://support.microsoft.com/help/5129195",
                Some("5129195"),
            ),
            (
                "HTTPS://Support.Microsoft.com/en-us/help/5128942?x=1",
                Some("5128942"),
            ),
            ("http://support.microsoft.com/kb/890830/", Some("890830")),
            ("https://go.microsoft.com/fwlink/?LinkId=52661", None),
            (
                "https://learn.microsoft.com/en-us/defender-endpoint/\
                 microsoft-defender-antivirus-updates",
                None,
            ),
            ("https://support.microsoft.com/select/?target=hub", None),
            (
                "https://support.microsoft.com.contoso.example/help/5129195",
                None,
            ),
            ("https://support.microsoft.com/help/", None),
            ("support.microsoft.com/help/5129195", None),
            ("", None),
        ] {
            assert_eq!(support_page_kb(url), kb, "{url}");
        }
    }

    /// `hours`, `minutes` and `seconds` before [`now`].
    fn before_now(hours: i64, minutes: i64, seconds: i64) -> DateTime<Utc> {
        now() - ChronoDuration::seconds(hours * 3600 + minutes * 60 + seconds)
    }

    /// The whole Windows Update history of a Windows 11 Home 25H2 PC (build 26200.9457) as
    /// the agent returns it, newest first, each entry dated by its time before [`now`]:
    /// Microsoft Defender's definition updates about twice a day and its platform update, all
    /// recorded with one category whose id is empty and without a service; the 2026-09
    /// monthly security update, installed 7 days and 10 hours before [`now`] and recorded
    /// without categories from the quality-update service; and the critical update installed
    /// ten minutes before it.
    fn home_25h2_history() -> Vec<HistoryEntry> {
        let definitions = |version: &str, at: DateTime<Utc>| HistoryEntry {
            title: format!(
                "Security Intelligence Update for Microsoft Defender Antivirus - KB2267602 \
                 (Version {version}) - Current Channel (Broad)"
            ),
            ..definition_update(at)
        };
        vec![
            definitions("1.459.491.0", before_now(10, 55, 41)),
            definitions("1.459.484.0", before_now(22, 58, 15)),
            definitions("1.459.468.0", before_now(35, 1, 57)),
            definitions("1.459.456.0", before_now(49, 25, 25)),
            definitions("1.459.450.0", before_now(61, 26, 37)),
            definitions("1.459.440.0", before_now(73, 50, 27)),
            definitions("1.459.432.0", before_now(86, 10, 10)),
            definitions("1.459.424.0", before_now(98, 34, 21)),
            definitions("1.459.417.0", before_now(110, 58, 7)),
            definitions("1.459.408.0", before_now(123, 27, 36)),
            definitions("1.459.403.0", before_now(132, 14, 50)),
            definitions("1.459.401.0", before_now(135, 34, 36)),
            definitions("1.459.389.0", before_now(147, 38, 57)),
            definitions("1.459.384.0", before_now(159, 46, 23)),
            platform_update(before_now(159, 46, 29)),
            definitions("1.459.364.0", before_now(178, 27, 57)),
            monthly_update(before_now(178, 36, 28)),
            setup_critical_update(before_now(178, 46, 54)),
        ]
    }

    #[test]
    fn a_windows_11_home_25h2_history_names_the_2026_09_security_update_and_its_age() {
        let history = home_25h2_history();
        let counted: Vec<&str> = history
            .iter()
            .filter(|e| is_security_install(e))
            .map(|e| e.title.as_str())
            .collect();
        assert_eq!(
            counted,
            [
                "2026-09 Security Update (KB5129195) (26200.9457)",
                "2026-09 Critical Update (KB5128942)"
            ]
        );
        let installed_at = history[16].date.unwrap();
        let searched_at = before_now(2, 53, 14);
        let mut raw = fixtures::raw();
        raw.windows_update = Ok(WuRaw {
            status: Ok(UpdateStatus {
                last_search: Some(before_now(10, 55, 49)),
                last_install: history[0].date,
                reboot_required: Some(false),
                history,
            }),
            last_scan_event: Some(searched_at),
            ..fixtures::windows_update()
        });
        let checks = evaluate(&raw, &fixtures::ctx(), &fixtures::scan_done(), now());
        let row = checks
            .iter()
            .find(|c| c.id == CheckId::WindowsUpdate)
            .unwrap();
        assert_eq!(
            (row.title.as_str(), row.state, row.summary.as_str()),
            (
                "Windows Update",
                CheckState::Good,
                "Updates installed 7 days ago"
            )
        );
        let checked = local_date(searched_at);
        let installed = format!(
            "2026-09 Security Update (KB5129195) (26200.9457) ({})",
            local_date(installed_at)
        );
        let facts: Vec<(&str, &str)> = row
            .facts
            .iter()
            .map(|f| (f.label.as_str(), f.value.as_str()))
            .collect();
        assert_eq!(
            facts,
            [
                ("Last check", checked.as_str()),
                ("Last installed", installed.as_str())
            ]
        );
        // While Windows' own updates stall, Microsoft Defender's keep coming and do not hide
        // it.
        let mut stalled = home_25h2_history();
        for entry in &mut stalled[16..] {
            entry.date = entry.date.map(|d| d - ChronoDuration::days(53));
        }
        let raw = wu(|wu| status(wu).history = stalled);
        let check = windows_update(&raw, now());
        assert_eq!(
            (check.state, check.severity, check.summary.as_str()),
            (
                CheckState::Attention,
                Severity::High,
                "No updates installed for 60 days"
            )
        );
    }

    #[test]
    fn only_security_and_critical_updates_count_as_installed() {
        for (days, severity, summary) in [
            (50, Severity::Medium, "Last update was 50 days ago"),
            (60, Severity::High, "No updates installed for 60 days"),
        ] {
            let mut history = other_installs();
            history.push(installed("2026-07 Security Update (KB5000001)", days));
            let raw = wu(|wu| status(wu).history = history);
            let check = windows_update(&raw, now());
            assert_eq!(
                (check.state, check.severity, check.summary.as_str()),
                (CheckState::Attention, severity, summary)
            );
            assert!(check.facts.iter().any(|f| f.label == "Last installed"
                && f.value.starts_with("2026-07 Security Update (KB5000001) (")));
        }
        let mut history = other_installs();
        history.push(installed_in(
            "2026-09 Critical Update (KB5000002)",
            3,
            &["{E6CF1350-C01B-414D-A61F-263D14D133B4}"],
        ));
        let raw = wu(|wu| status(wu).history = history);
        assert_eq!(
            windows_update(&raw, now()).summary,
            "Updates installed 3 days ago"
        );
    }

    #[test]
    fn no_security_update_in_the_history_is_unknown() {
        for history in [other_installs(), Vec::new()] {
            let raw = wu(|wu| status(wu).history = history);
            let check = windows_update(&raw, now());
            assert_eq!(
                (check.state, check.summary.as_str()),
                (
                    CheckState::Unknown,
                    "No security update was found in Windows Update's history"
                )
            );
            assert!(!check.facts.iter().any(|f| f.label == "Last installed"));
        }
        // A finding still stands.
        let raw = wu(|wu| {
            status(wu).history = Vec::new();
            wu.no_auto_update = Some(1);
        });
        assert_eq!(windows_update(&raw, now()).state, CheckState::Attention);
    }

    #[test]
    fn an_unknown_last_check_is_not_dated_by_the_search() {
        // The offline search reads Windows Update's cached data; when it finished says nothing
        // about when Windows Update last checked.
        let mut unread = fixtures::raw();
        unread.windows_update = Err("Windows Update did not answer within 8 s".into());
        let undated = wu(|wu| {
            wu.last_scan_event = None;
            status(wu).last_search = None;
        });
        for raw in [unread, undated] {
            let check = pending_updates(&raw, &fixtures::scan_done(), now());
            assert_eq!(
                (check.state, check.summary.as_str()),
                (
                    CheckState::Good,
                    "None found in Windows Update's last check"
                )
            );
        }
    }

    #[test]
    fn the_last_check_prefers_the_event_log() {
        // Event only.
        let raw = wu(|wu| {
            wu.last_scan_event = Some(days_ago(9));
            status(wu).last_search = None;
        });
        let check = windows_update(&raw, now());
        assert_eq!(check.summary, "Windows Update hasn't checked for 9 days");
        assert_eq!(
            check.facts[0].value,
            super::super::text::local_date(days_ago(9))
        );
        // Automatic Updates only.
        let raw = wu(|wu| {
            wu.last_scan_event = None;
            status(wu).last_search = Some(days_ago(8));
        });
        assert_eq!(
            windows_update(&raw, now()).summary,
            "Windows Update hasn't checked for 8 days"
        );
        // Both agree: no extra fact.
        let raw = wu(|wu| {
            wu.last_scan_event = Some(days_ago(2));
            status(wu).last_search = Some(days_ago(4));
        });
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Good);
        assert!(!check
            .facts
            .iter()
            .any(|f| f.value.starts_with("Windows Update's own record")));
        // They disagree by more than a week: the event wins and the record is shown.
        let raw = wu(|wu| {
            wu.last_scan_event = Some(days_ago(1));
            status(wu).last_search = Some(days_ago(30));
        });
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Good);
        assert!(check.facts.iter().any(|f| f.value
            == format!(
                "Windows Update's own record says the last check was {}",
                super::super::text::local_date(days_ago(30))
            )));
        // Neither: no finding, the last check is unknown.
        let raw = wu(|wu| {
            wu.last_scan_event = None;
            status(wu).last_search = None;
        });
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(check.facts[0].value, "unknown");
    }

    #[test]
    fn an_organization_update_server_is_a_fact() {
        let raw = wu(|wu| {
            wu.wu_server = Some("https://wsus.contoso.example".into());
            wu.use_wu_server = Some(1);
        });
        let check = windows_update(&raw, now());
        assert!(check
            .facts
            .iter()
            .any(|f| f.value == "Updates come from your organization's update server"));
    }

    #[test]
    fn an_unreadable_agent_is_unknown_unless_a_finding_stands() {
        let raw = wu(|wu| wu.status = Err("The Windows Update service is disabled.".into()));
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read Windows Update: The Windows Update service is disabled."
        );
        let raw = wu(|wu| {
            wu.status = Err("x".into());
            wu.service_start = Some(StartType::Disabled);
        });
        assert_eq!(windows_update(&raw, now()).state, CheckState::Attention);
        let mut raw = fixtures::raw();
        raw.windows_update = Err("Windows Update did not answer within 8 s".into());
        let check = windows_update(&raw, now());
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read Windows Update: Windows Update did not answer within 8 s"
        );
    }

    #[test]
    fn restart_needs_come_from_the_agent_or_the_registry() {
        assert_eq!(update_restart(&fixtures::raw()).state, CheckState::Good);
        let raw = wu(|wu| wu.reboot_pending_keys = true);
        let check = update_restart(&raw);
        assert_eq!(
            (check.state, check.severity, check.summary.as_str()),
            (
                CheckState::Attention,
                Severity::Medium,
                "Restart to finish installing updates"
            )
        );
        let raw = wu(|wu| status(wu).reboot_required = Some(true));
        assert_eq!(update_restart(&raw).state, CheckState::Attention);
    }

    fn pending(title: &str, security: bool, released_days: Option<i64>) -> PendingUpdate {
        PendingUpdate {
            title: title.into(),
            kb: Vec::new(),
            msrc_severity: None,
            security,
            downloaded: false,
            released_at: released_days.map(days_ago),
        }
    }

    fn scan(state: ScanState, online: bool, updates: Vec<PendingUpdate>) -> UpdateScanView {
        UpdateScanView {
            state,
            online,
            updates,
            ..fixtures::scan_done()
        }
    }

    #[test]
    fn pending_updates_follow_the_scan() {
        let raw = fixtures::raw();
        let cases = [
            (
                scan(ScanState::Idle, false, vec![]),
                CheckState::Unknown,
                "Not checked yet",
            ),
            (
                scan(ScanState::Running, false, vec![]),
                CheckState::Checking,
                "Checking for waiting updates…",
            ),
            (
                scan(ScanState::Running, true, vec![]),
                CheckState::Checking,
                "Checking Windows Update online…",
            ),
            (
                scan(ScanState::Cancelled, false, vec![]),
                CheckState::Unknown,
                "The check was stopped",
            ),
            (
                scan(ScanState::Done, true, vec![]),
                CheckState::Good,
                "None found online just now",
            ),
        ];
        for (view, state, summary) in cases {
            let check = pending_updates(&raw, &view, now());
            assert_eq!((check.state, check.summary.as_str()), (state, summary));
        }
        let failed = UpdateScanView {
            error: Some(
                "Windows Update could not be reached; check the internet connection.".into(),
            ),
            ..scan(ScanState::Failed, true, vec![])
        };
        let check = pending_updates(&raw, &failed, now());
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Windows Update could not be reached; check the internet connection."
        );
        let check = pending_updates(&raw, &fixtures::scan_done(), now());
        assert_eq!(check.state, CheckState::Good);
        assert!(check
            .summary
            .starts_with("None found in Windows Update's last check ("));
        assert_eq!(
            check.fixes[0].action,
            FixAction::UpdateScan { online: true }
        );
    }

    #[test]
    fn waiting_security_updates_are_overdue_after_14_days() {
        let raw = fixtures::raw();
        let view = scan(
            ScanState::Done,
            false,
            vec![pending("Security update", true, Some(14))],
        );
        let check = pending_updates(&raw, &view, now());
        assert_eq!(
            (check.severity, check.summary.as_str()),
            (Severity::Medium, "1 security update waiting")
        );
        let view = scan(
            ScanState::Done,
            false,
            vec![
                pending("Security update", true, Some(15)),
                pending("Another security update", true, Some(2)),
                pending("Driver", false, Some(40)),
            ],
        );
        let check = pending_updates(&raw, &view, now());
        assert_eq!(
            (check.severity, check.summary.as_str()),
            (Severity::High, "2 security updates waiting for 15 days")
        );
        let view = scan(
            ScanState::Done,
            false,
            vec![
                pending("Driver", false, None),
                pending("Feature", false, None),
            ],
        );
        let check = pending_updates(&raw, &view, now());
        assert_eq!(
            (check.severity, check.summary.as_str()),
            (Severity::Low, "2 other updates available")
        );
    }

    #[test]
    fn pending_titles_are_listed_up_to_five() {
        let updates = (1..=7)
            .map(|n| pending(&format!("Update {n}"), false, None))
            .collect();
        let check = pending_updates(
            &fixtures::raw(),
            &scan(ScanState::Done, false, updates),
            now(),
        );
        let lines: Vec<&str> = check.facts.iter().map(|f| f.value.as_str()).collect();
        assert_eq!(
            lines,
            vec![
                "• Update 1",
                "• Update 2",
                "• Update 3",
                "• Update 4",
                "• Update 5",
                "and 2 more"
            ]
        );
    }

    #[test]
    fn security_updates_are_recognized_by_category_or_severity() {
        let found = |categories: &[&str], severity: Option<&str>| {
            PendingUpdate::from_found(FoundUpdate {
                title: "Update".into(),
                category_ids: categories.iter().map(|s| s.to_string()).collect(),
                msrc_severity: severity.map(str::to_string),
                ..FoundUpdate::default()
            })
            .security
        };
        assert!(found(&["0FA1201D-4330-4FA8-8AE9-B877473B6441"], None));
        assert!(found(&["{e6cf1350-c01b-414d-a61f-263d14d133b4}"], None));
        assert!(found(&[], Some("Critical")));
        assert!(!found(&["28bc880e-0592-4cbf-8f95-c79b17911d5f"], None));
        assert!(!found(&[], None));
    }

    // ───────────── Update scanner ─────────────

    /// A scripted agent: each search waits for the outcome the test sends, or for the cancel
    /// flag.
    #[derive(Debug)]
    struct FakeAgent {
        outcomes: Mutex<Receiver<Option<Result<SearchOutcome>>>>,
        calls: Mutex<Vec<(bool, Duration)>>,
    }

    fn fake() -> (Arc<FakeAgent>, Sender<Option<Result<SearchOutcome>>>) {
        let (tx, rx) = mpsc::channel();
        let agent = Arc::new(FakeAgent {
            outcomes: Mutex::new(rx),
            calls: Mutex::new(Vec::new()),
        });
        (agent, tx)
    }

    impl UpdateAgent for FakeAgent {
        fn search(
            &self,
            online: bool,
            cancel: &AtomicBool,
            deadline: Duration,
        ) -> Result<SearchOutcome> {
            self.calls.lock().push((online, deadline));
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(SearchOutcome::Cancelled);
                }
                match self.outcomes.lock().recv_timeout(Duration::from_millis(5)) {
                    Ok(Some(outcome)) => return outcome,
                    Ok(None) => panic!("agent crashed"),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Error::Other("no outcome".into()))
                    }
                }
            }
        }
    }

    fn wait_until_finished(scanner: &UpdateScanner) -> UpdateScanView {
        let started = Instant::now();
        while scanner.busy() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "scan never ended"
            );
            thread::sleep(Duration::from_millis(5));
        }
        scanner.view()
    }

    #[test]
    fn a_scan_runs_to_done() {
        let (agent, tx) = fake();
        let scanner = UpdateScanner::new(agent.clone());
        let view = scanner.start(false).unwrap();
        assert_eq!(view.state, ScanState::Running);
        assert!(!view.online);
        assert!(view.started_at.is_some());
        assert!(!scanner.due(Utc::now()));
        tx.send(Some(Ok(SearchOutcome::Found(vec![FoundUpdate {
            title: "2026-09 Security Update".into(),
            kb: vec!["5030000".into()],
            msrc_severity: Some("Important".into()),
            ..FoundUpdate::default()
        }]))))
        .unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(view.state, ScanState::Done);
        assert_eq!(view.updates.len(), 1);
        assert!(view.updates[0].security);
        assert!(view.finished_at.is_some());
        assert_eq!(agent.calls.lock().as_slice(), &[(false, OFFLINE_DEADLINE)]);
        // Fresh for 30 minutes.
        assert!(!scanner.due(Utc::now()));
        assert!(scanner.due(Utc::now() + ChronoDuration::minutes(31)));
    }

    #[test]
    fn a_second_start_is_refused_while_one_runs() {
        let (agent, tx) = fake();
        let scanner = UpdateScanner::new(agent.clone());
        scanner.start(true).unwrap();
        let err = scanner.start(false).unwrap_err();
        assert_eq!(err.to_string(), "Windows Update is already being checked.");
        tx.send(Some(Ok(SearchOutcome::Found(Vec::new())))).unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(view.state, ScanState::Done);
        assert!(view.online);
        assert_eq!(agent.calls.lock().as_slice(), &[(true, ONLINE_DEADLINE)]);
        // After it ended, a new one starts.
        scanner.start(false).unwrap();
        tx.send(Some(Ok(SearchOutcome::Found(Vec::new())))).unwrap();
        wait_until_finished(&scanner);
        assert_eq!(agent.calls.lock().len(), 2);
    }

    #[test]
    fn concurrent_starts_run_one_search() {
        let (agent, tx) = fake();
        let scanner = UpdateScanner::new(agent.clone());
        let barrier = std::sync::Barrier::new(8);
        let started = thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        scanner.start(false).is_ok()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|ok| *ok)
                .count()
        });
        assert_eq!(started, 1);
        tx.send(Some(Ok(SearchOutcome::Found(Vec::new())))).unwrap();
        assert_eq!(wait_until_finished(&scanner).state, ScanState::Done);
        assert_eq!(agent.calls.lock().len(), 1);
    }

    #[test]
    fn a_scan_whose_thread_is_still_ending_is_refused_with_its_own_text() {
        let (agent, _tx) = fake();
        let scanner = UpdateScanner::new(agent);
        scanner.thread_alive.store(true, Ordering::SeqCst);
        scanner.shared.lock().state = ScanState::Failed;
        let err = scanner.start(true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "The previous check is still ending; try again in a minute."
        );
        assert!(!scanner.due(Utc::now()));
        scanner.thread_alive.store(false, Ordering::SeqCst);
    }

    #[test]
    fn cancel_stops_a_running_scan() {
        let (agent, _tx) = fake();
        let scanner = UpdateScanner::new(agent);
        assert!(!scanner.cancel());
        scanner.start(true).unwrap();
        assert!(scanner.cancel());
        let view = wait_until_finished(&scanner);
        assert_eq!(view.state, ScanState::Cancelled);
        assert!(!scanner.cancel());
        // A stopped search counts as finished: no automatic search for 30 minutes.
        assert!(view.finished_at.is_some());
        assert!(!scanner.due(Utc::now()));
        assert!(scanner.due(Utc::now() + ChronoDuration::minutes(31)));
    }

    #[test]
    fn timeouts_errors_and_panics_fail_the_scan() {
        let (agent, tx) = fake();
        let scanner = UpdateScanner::new(agent);
        scanner.start(false).unwrap();
        tx.send(Some(Ok(SearchOutcome::TimedOut))).unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(view.state, ScanState::Failed);
        assert_eq!(
            view.error.as_deref(),
            Some("Windows Update did not finish checking within 3 minutes.")
        );
        scanner.start(true).unwrap();
        tx.send(Some(Ok(SearchOutcome::TimedOut))).unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(
            view.error.as_deref(),
            Some("Windows Update did not finish checking within 15 minutes.")
        );
        scanner.start(false).unwrap();
        tx.send(Some(Err(Error::Win32(windows::core::Error::from_hresult(
            windows::core::HRESULT(0x8007_0422u32 as i32),
        )))))
        .unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(
            view.error.as_deref(),
            Some("The Windows Update service is disabled.")
        );
        scanner.start(false).unwrap();
        tx.send(None).unwrap();
        let view = wait_until_finished(&scanner);
        assert_eq!(view.state, ScanState::Failed);
        assert_eq!(view.error.as_deref(), Some("internal error: agent crashed"));
        // A failed search is not repeated automatically at once either.
        assert!(!scanner.due(Utc::now()));
        assert!(scanner.due(Utc::now() + ChronoDuration::minutes(31)));
    }

    #[test]
    fn the_view_does_not_wait_for_a_blocked_search() {
        let (agent, tx) = fake();
        let scanner = UpdateScanner::new(agent);
        scanner.start(false).unwrap();
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        for _ in 0..100 {
            let view = scanner.view();
            assert_eq!(view.state, ScanState::Running);
        }
        assert!(!scanner.due(Utc::now()));
        assert!(started.elapsed() < Duration::from_millis(50));
        let view = scanner.view();
        assert!(view.elapsed_ms >= 20, "{}", view.elapsed_ms);
        tx.send(Some(Ok(SearchOutcome::Found(Vec::new())))).unwrap();
        wait_until_finished(&scanner);
    }

    #[test]
    fn a_new_scanner_is_idle_and_due() {
        let (agent, _tx) = fake();
        let scanner = UpdateScanner::new(agent);
        let view = scanner.view();
        assert_eq!(view.state, ScanState::Idle);
        assert_eq!(view.elapsed_ms, 0);
        assert!(scanner.due(Utc::now()));
    }

    #[test]
    fn the_live_agent_refuses_to_search_in_tests() {
        assert!(update_agent::search_forbidden());
        let cancel = AtomicBool::new(false);
        let err = LiveAgent
            .search(false, &cancel, Duration::from_secs(1))
            .unwrap_err();
        assert!(err.to_string().contains(update_agent::FORBID_ENV), "{err}");
    }
}
