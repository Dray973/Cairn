//! Boot history: start and shutdown durations from the Diagnostics-Performance log, the
//! start type of each start (Kernel-Boot event 27), unexpected shutdowns (Kernel-Power event
//! 41), what slowed starts down, and the startup entries those slow apps belong to.
//!
//! Windows times only full starts: a Fast Startup start logs its Kernel-Boot event but no
//! Diagnostics-Performance event, so such starts are only counted, in a note.
//!
//! The Diagnostics-Performance channel is readable by administrators only; without rights
//! the history reports `needs_admin` and reads nothing else. Fields are read by name, never
//! from message text.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use super::text::{local_date, plural};
use crate::startup::StartupEntry;
use crate::win::event_log::{
    self, data_path, xpath_time, EvtValue, LogError, RenderContext, SYSTEM_EVENT_ID,
    SYSTEM_RECORD_ID, SYSTEM_TIME_CREATED,
};
use crate::{Error, Result};

/// Performance events read at most.
pub(crate) const MAX_PERF_EVENTS: usize = 4000;
/// Largest `limit` the history accepts.
pub const MAX_BOOT_LIMIT: usize = 500;

const PERF_CHANNEL: &str = "Microsoft-Windows-Diagnostics-Performance/Operational";
const PERF_QUERY: &str = "*[System[(EventID=100 or (EventID>=101 and EventID<=110) or \
    EventID=200 or (EventID>=201 and EventID<=203))]]";
const PERF_CHANNEL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\WINEVT\Channels\Microsoft-Windows-Diagnostics-Performance/Operational";
const STARTUP_LIST_NOTE: &str =
    "The startup list could not be read, so slow apps are not matched to startup entries.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogAccess {
    Ok,
    /// The log's access list admits administrators only.
    NeedsAdmin,
    /// The Diagnostics-Performance log is turned off; events read before are still listed.
    LogDisabled,
    /// This copy of Windows has no Diagnostics-Performance log.
    LogMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootType {
    Full,
    FastStartup,
    /// Resume from hibernation.
    Hibernate,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowKind {
    App,
    Driver,
    Service,
    Device,
    Windows,
    Prefetch,
    Policy,
}

impl SlowKind {
    /// Kind of a degradation event by its id; `None` for other events.
    pub fn of_event(id: u16) -> Option<SlowKind> {
        Some(match id {
            101 | 201 => SlowKind::App,
            102 => SlowKind::Driver,
            103 | 203 => SlowKind::Service,
            109 | 202 => SlowKind::Device,
            104 | 110 => SlowKind::Windows,
            105 | 106 => SlowKind::Prefetch,
            107 | 108 => SlowKind::Policy,
            _ => return None,
        })
    }

    fn key(self) -> &'static str {
        match self {
            SlowKind::App => "app",
            SlowKind::Driver => "driver",
            SlowKind::Service => "service",
            SlowKind::Device => "device",
            SlowKind::Windows => "windows",
            SlowKind::Prefetch => "prefetch",
            SlowKind::Policy => "policy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Startup,
    Shutdown,
}

/// Something that took longer than usual during one start or shutdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlowEvent {
    pub event_id: u16,
    pub kind: SlowKind,
    pub name: String,
    /// Friendly name, else product name, else name.
    pub title: String,
    pub path: Option<String>,
    pub company: Option<String>,
    pub version: Option<String>,
    pub total_ms: u64,
    pub degradation_ms: u64,
    pub at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BootPhases {
    pub kernel_ms: Option<u64>,
    pub drivers_ms: Option<u64>,
    pub devices_ms: Option<u64>,
    pub user_profile_ms: Option<u64>,
    pub explorer_ms: Option<u64>,
}

/// One start of Windows (event 100).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootRecord {
    pub record_id: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub logged_at: DateTime<Utc>,
    /// Until Windows settled.
    pub boot_ms: u64,
    /// Until the desktop appeared.
    pub main_path_ms: u64,
    pub post_boot_ms: u64,
    pub startup_apps: Option<u32>,
    /// The first start after installing updates.
    pub after_update: bool,
    /// Windows marked the start slower than usual.
    pub degraded: bool,
    pub boot_type: BootType,
    pub after_unexpected_shutdown: bool,
    pub phases: BootPhases,
    pub slow: Vec<SlowEvent>,
}

/// One shutdown (event 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRecord {
    pub record_id: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub logged_at: DateTime<Utc>,
    pub shutdown_ms: u64,
    pub degraded: bool,
    pub slow: Vec<SlowEvent>,
}

/// Everything that slowed starts or shutdowns, grouped by what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlowItem {
    /// "{phase}:{kind}:{lower-case name}".
    pub key: String,
    pub phase: Phase,
    pub kind: SlowKind,
    pub name: String,
    pub title: String,
    pub path: Option<String>,
    pub company: Option<String>,
    /// Events of this item.
    pub count: u32,
    pub last_seen: DateTime<Utc>,
    pub median_degradation_ms: u64,
    pub max_degradation_ms: u64,
    /// The startup entry that starts this app, when exactly one does; empty otherwise.
    pub startup_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trend {
    /// `Full` when only full starts were compared; `None` when every start was.
    pub boot_type: Option<BootType>,
    pub recent_median_ms: u64,
    pub earlier_median_ms: u64,
    pub change_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootStats {
    pub count: u32,
    pub latest_ms: Option<u64>,
    pub median_ms: Option<u64>,
    pub full_count: u32,
    pub fast_count: u32,
    pub trend: Option<Trend>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootHistory {
    pub read_at: DateTime<Utc>,
    pub access: LogAccess,
    /// Newest first.
    pub boots: Vec<BootRecord>,
    /// Newest first.
    pub shutdowns: Vec<ShutdownRecord>,
    pub slow_items: Vec<SlowItem>,
    /// Newest first.
    pub unexpected_shutdowns: Vec<DateTime<Utc>>,
    pub stats: BootStats,
    pub fast_startup: Option<bool>,
    /// The startup entries some slow app matched.
    pub startup_entries: Vec<StartupEntry>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

// ───────────────────────────── Raw events ─────────────────────────────

/// Event 100.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct BootData {
    pub(crate) start: Option<DateTime<Utc>>,
    pub(crate) end: Option<DateTime<Utc>>,
    pub(crate) boot_ms: u64,
    pub(crate) main_path_ms: u64,
    pub(crate) post_boot_ms: u64,
    pub(crate) phases: BootPhases,
    pub(crate) startup_apps: Option<u32>,
    pub(crate) after_update: bool,
    pub(crate) degraded: bool,
}

/// Event 200.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ShutdownData {
    pub(crate) start: Option<DateTime<Utc>>,
    pub(crate) end: Option<DateTime<Utc>>,
    pub(crate) shutdown_ms: u64,
    pub(crate) degraded: bool,
}

/// Events 101 to 110 and 201 to 203.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SlowData {
    pub(crate) start: Option<DateTime<Utc>>,
    pub(crate) name: String,
    pub(crate) friendly_name: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) product: Option<String>,
    pub(crate) company: Option<String>,
    pub(crate) total_ms: u64,
    pub(crate) degradation_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PerfData {
    Boot(BootData),
    Shutdown(ShutdownData),
    Slow(SlowData),
}

/// One Diagnostics-Performance event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PerfEvent {
    pub(crate) event_id: u16,
    pub(crate) record_id: u64,
    pub(crate) logged_at: DateTime<Utc>,
    pub(crate) data: PerfData,
}

/// The event logs behind the history; the live source reads this PC.
pub(crate) trait EventSource {
    /// The Diagnostics-Performance log's `Enabled` setting; `None` when unknown.
    fn log_enabled(&self) -> Option<bool>;
    /// Performance events, newest first, until `boots` start events or `max_events` events
    /// were read.
    fn performance(
        &self,
        boots: usize,
        max_events: usize,
    ) -> std::result::Result<Vec<PerfEvent>, LogError>;
    /// Kernel-Boot event 27 since `since`: (time, BootType).
    fn boot_types(&self, since: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, u32)>>;
    /// Kernel-Power event 41 since `since`.
    fn unexpected_shutdowns(&self, since: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>>;
}

// ───────────────────────────── Builder ─────────────────────────────

fn empty(now: DateTime<Utc>, access: LogAccess, fast_startup: Option<bool>) -> BootHistory {
    BootHistory {
        read_at: now,
        access,
        boots: Vec::new(),
        shutdowns: Vec::new(),
        slow_items: Vec::new(),
        unexpected_shutdowns: Vec::new(),
        stats: stats(&[]),
        fast_startup,
        startup_entries: Vec::new(),
        notes: Vec::new(),
        errors: Vec::new(),
    }
}

fn title_of(data: &SlowData) -> String {
    [&data.friendly_name, &data.product]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or(data.name.trim())
        .to_string()
}

fn slow_event(event_id: u16, kind: SlowKind, data: &SlowData) -> SlowEvent {
    SlowEvent {
        event_id,
        kind,
        name: data.name.trim().to_string(),
        title: title_of(data),
        path: data.path.clone(),
        company: data.company.clone(),
        version: data.version.clone(),
        total_ms: data.total_ms,
        degradation_ms: data.degradation_ms,
        at: data.start,
    }
}

/// When a start or shutdown was logged, and its start and end.
type Window = (DateTime<Utc>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Index of the record an event belongs to: the one logged nearest in time, within
/// ±10 minutes; ties go to the record whose [start, end] holds the event's start time.
fn nearest(
    logged_at: DateTime<Utc>,
    start: Option<DateTime<Utc>>,
    records: &[Window],
) -> Option<usize> {
    let window = ChronoDuration::minutes(10);
    records
        .iter()
        .enumerate()
        .filter(|(_, (logged, _, _))| (logged_at - *logged).abs() <= window)
        .min_by_key(|(_, (logged, begin, end))| {
            let inside = match (start, begin, end) {
                (Some(t), Some(b), Some(e)) => *b <= t && t <= *e,
                _ => false,
            };
            ((logged_at - *logged).abs(), !inside)
        })
        .map(|(i, _)| i)
}

fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    })
}

fn boot_type(raw: u32) -> BootType {
    match raw {
        0 => BootType::Full,
        1 => BootType::FastStartup,
        2 => BootType::Hibernate,
        _ => BootType::Unknown,
    }
}

/// Whether a Kernel-Boot event at `t` belongs to the start timed from `start` to `end`: it
/// lies between two minutes before the start and ten minutes after it, and not after the end.
fn in_start_window(t: DateTime<Utc>, start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> bool {
    t >= start - ChronoDuration::minutes(2)
        && t <= start + ChronoDuration::minutes(10)
        && end.map_or(true, |end| t <= end)
}

/// Kernel-Boot `BootType` of a Fast Startup start.
const FAST_STARTUP_BOOT_TYPE: u32 = 1;
/// Days of Kernel-Boot events counted when no start was timed.
const UNTIMED_DAYS: i64 = 30;

/// The note on the Fast Startup starts Windows did not time since `since`.
fn untimed_note(count: usize, since: DateTime<Utc>) -> String {
    format!(
        "Windows times only full starts, such as restarts: {} since {} {} not listed.",
        plural(count as u64, "Fast Startup start"),
        local_date(since),
        if count == 1 { "is" } else { "are" }
    )
}

/// Start times compared by the trend: full starts when there are at least 8, else all
/// starts when there are at least 8.
fn trend(boots: &[BootRecord]) -> Option<Trend> {
    let full: Vec<u64> = boots
        .iter()
        .filter(|b| b.boot_type == BootType::Full)
        .map(|b| b.boot_ms)
        .collect();
    let (set, kind) = if full.len() >= 8 {
        (full, Some(BootType::Full))
    } else if boots.len() >= 8 {
        (boots.iter().map(|b| b.boot_ms).collect(), None)
    } else {
        return None;
    };
    let mut recent: Vec<u64> = set.iter().take(5).copied().collect();
    let mut earlier: Vec<u64> = set.iter().skip(5).take(10).copied().collect();
    if earlier.len() < 3 {
        return None;
    }
    let recent_median = median(&mut recent)?;
    let earlier_median = median(&mut earlier)?;
    if earlier_median == 0 {
        return None;
    }
    let change_pct = (recent_median as f64 - earlier_median as f64) / earlier_median as f64 * 100.0;
    Some(Trend {
        boot_type: kind,
        recent_median_ms: recent_median,
        earlier_median_ms: earlier_median,
        change_pct,
    })
}

fn stats(boots: &[BootRecord]) -> BootStats {
    let mut all: Vec<u64> = boots.iter().map(|b| b.boot_ms).collect();
    BootStats {
        count: boots.len() as u32,
        latest_ms: boots.first().map(|b| b.boot_ms),
        median_ms: median(&mut all),
        full_count: boots
            .iter()
            .filter(|b| b.boot_type == BootType::Full)
            .count() as u32,
        fast_count: boots
            .iter()
            .filter(|b| b.boot_type == BootType::FastStartup)
            .count() as u32,
        trend: trend(boots),
    }
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

fn file_name(path: &str) -> Option<String> {
    Path::new(strip_verbatim(path))
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
}

/// Programs that start many unrelated things: a slow one says nothing about which startup
/// entry started it.
const SHARED_HOSTS: [&str; 12] = [
    "rundll32.exe",
    "svchost.exe",
    "dllhost.exe",
    "explorer.exe",
    "cmd.exe",
    "conhost.exe",
    "powershell.exe",
    "pwsh.exe",
    "wscript.exe",
    "cscript.exe",
    "mshta.exe",
    "msiexec.exe",
];

/// The startup entry that starts the program of a slow app: the only entry with the same
/// path (ignoring case and a `\\?\` prefix), else the only one with the same file name.
/// Several candidates match none, and so does a program that hosts many others, such as
/// rundll32.exe: turning off a guessed entry could stop another program from starting.
fn matching_entry<'a>(
    entries: &'a [StartupEntry],
    item_path: Option<&str>,
    item_name: &str,
) -> Option<&'a StartupEntry> {
    let item_file = item_path
        .and_then(file_name)
        .or_else(|| file_name(item_name))?;
    if SHARED_HOSTS.contains(&item_file.as_str()) {
        return None;
    }
    let only = |found: Vec<&'a StartupEntry>| match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    };
    let usable = || entries.iter().filter(|e| !e.path.trim().is_empty());
    if let Some(path) = item_path {
        let wanted = strip_verbatim(path.trim()).to_lowercase();
        let same_path: Vec<&StartupEntry> = usable()
            .filter(|e| strip_verbatim(e.path.trim()).to_lowercase() == wanted)
            .collect();
        if !same_path.is_empty() {
            return only(same_path);
        }
    }
    only(
        usable()
            .filter(|e| file_name(e.path.trim()).as_deref() == Some(item_file.as_str()))
            .collect(),
    )
}

/// Builds the history from `source`. Pure given its inputs: `startup` lists the startup
/// entries (read only when a slow app needs matching), `fast_startup` is Windows' setting.
/// Fast Startup starts that no listed start accounts for are counted in a note: those since
/// one hour before the oldest listed record, or over the last 30 days when none is listed.
pub(crate) fn history_with(
    source: &dyn EventSource,
    startup: &dyn Fn() -> Result<Vec<StartupEntry>>,
    fast_startup: Option<bool>,
    limit: usize,
    now: DateTime<Utc>,
) -> Result<BootHistory> {
    let enabled = source.log_enabled();
    let events = match source.performance(limit + 1, MAX_PERF_EVENTS) {
        Ok(events) => events,
        Err(LogError::AccessDenied) => return Ok(empty(now, LogAccess::NeedsAdmin, fast_startup)),
        Err(LogError::ChannelNotFound) => {
            return Ok(empty(now, LogAccess::LogMissing, fast_startup))
        }
        Err(LogError::Other(e)) => return Err(e),
    };
    let access = if enabled == Some(false) {
        LogAccess::LogDisabled
    } else {
        LogAccess::Ok
    };
    let mut history = empty(now, access, fast_startup);

    let mut boot_events: Vec<(u64, DateTime<Utc>, BootData)> = Vec::new();
    let mut shutdown_events: Vec<(u64, DateTime<Utc>, ShutdownData)> = Vec::new();
    let mut slow_events: Vec<(u16, DateTime<Utc>, SlowData)> = Vec::new();
    for event in events {
        match event.data {
            PerfData::Boot(data) => boot_events.push((event.record_id, event.logged_at, data)),
            PerfData::Shutdown(data) => {
                shutdown_events.push((event.record_id, event.logged_at, data))
            }
            PerfData::Slow(data) => slow_events.push((event.event_id, event.logged_at, data)),
        }
    }
    boot_events.sort_by_key(|e| Reverse(e.1));
    boot_events.truncate(limit);
    shutdown_events.sort_by_key(|e| Reverse(e.1));
    shutdown_events.truncate(limit);

    let boot_oldest = boot_events
        .iter()
        .map(|(_, logged, data)| data.start.unwrap_or(*logged))
        .min();
    let shutdown_oldest = shutdown_events
        .iter()
        .map(|(_, logged, data)| data.start.unwrap_or(*logged))
        .min();
    let oldest = boot_oldest.into_iter().chain(shutdown_oldest).min();
    let since = oldest.map(|t| t - ChronoDuration::hours(1));
    let (boot_types, unexpected) = match since {
        Some(since) => {
            let types = source.boot_types(since).unwrap_or_else(|e| {
                history
                    .errors
                    .push(format!("Could not read the start types: {e}"));
                Vec::new()
            });
            let mut unexpected = source.unexpected_shutdowns(since).unwrap_or_else(|e| {
                history
                    .errors
                    .push(format!("Could not read the unexpected shutdowns: {e}"));
                Vec::new()
            });
            unexpected.sort_by(|a, b| b.cmp(a));
            (types, unexpected)
        }
        None => (Vec::new(), Vec::new()),
    };

    let mut boots: Vec<BootRecord> = boot_events
        .iter()
        .map(|(record_id, logged_at, data)| {
            let kind = data
                .start
                .and_then(|start| {
                    boot_types
                        .iter()
                        .filter(|(t, _)| in_start_window(*t, start, data.end))
                        .max_by_key(|(t, _)| *t)
                })
                .map(|(_, raw)| boot_type(*raw))
                .unwrap_or(BootType::Unknown);
            let after_crash = data.start.is_some_and(|start| {
                let end = data.end.unwrap_or(*logged_at);
                unexpected
                    .iter()
                    .any(|t| *t >= start - ChronoDuration::minutes(2) && *t <= end)
            });
            BootRecord {
                record_id: *record_id,
                started_at: data.start,
                logged_at: *logged_at,
                boot_ms: data.boot_ms,
                main_path_ms: data.main_path_ms,
                post_boot_ms: data.post_boot_ms,
                startup_apps: data.startup_apps,
                after_update: data.after_update,
                degraded: data.degraded,
                boot_type: kind,
                after_unexpected_shutdown: after_crash,
                phases: data.phases.clone(),
                slow: Vec::new(),
            }
        })
        .collect();
    let mut shutdowns: Vec<ShutdownRecord> = shutdown_events
        .iter()
        .map(|(record_id, logged_at, data)| ShutdownRecord {
            record_id: *record_id,
            started_at: data.start,
            logged_at: *logged_at,
            shutdown_ms: data.shutdown_ms,
            degraded: data.degraded,
            slow: Vec::new(),
        })
        .collect();

    let boot_windows: Vec<_> = boot_events
        .iter()
        .map(|(_, logged, data)| (*logged, data.start, data.end))
        .collect();
    let shutdown_windows: Vec<_> = shutdown_events
        .iter()
        .map(|(_, logged, data)| (*logged, data.start, data.end))
        .collect();
    // Slow events logged well before the oldest listed start (or shutdown) are left out.
    let boot_cutoff = boot_oldest.map(|t| t - ChronoDuration::minutes(10));
    let shutdown_cutoff = shutdown_oldest.map(|t| t - ChronoDuration::minutes(10));
    let mut groups: HashMap<String, Vec<(DateTime<Utc>, SlowEvent)>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (event_id, logged_at, data) in &slow_events {
        let Some(kind) = SlowKind::of_event(*event_id) else {
            continue;
        };
        let phase = if *event_id >= 200 {
            Phase::Shutdown
        } else {
            Phase::Startup
        };
        let event = slow_event(*event_id, kind, data);
        match phase {
            Phase::Startup => {
                if let Some(i) = nearest(*logged_at, data.start, &boot_windows) {
                    boots[i].slow.push(event.clone());
                }
            }
            Phase::Shutdown => {
                if let Some(i) = nearest(*logged_at, data.start, &shutdown_windows) {
                    shutdowns[i].slow.push(event.clone());
                }
            }
        }
        let cutoff = match phase {
            Phase::Startup => boot_cutoff,
            Phase::Shutdown => shutdown_cutoff,
        };
        if cutoff.is_some_and(|c| *logged_at < c) {
            continue;
        }
        let key = format!(
            "{}:{}:{}",
            match phase {
                Phase::Startup => "startup",
                Phase::Shutdown => "shutdown",
            },
            kind.key(),
            event.name.to_lowercase()
        );
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push((*logged_at, event));
    }

    let mut items: Vec<SlowItem> = order
        .into_iter()
        .filter_map(|key| {
            let mut events = groups.remove(&key)?;
            events.sort_by_key(|e| Reverse(e.0));
            let (last_seen, newest) = events.first()?.clone();
            let mut degradations: Vec<u64> = events.iter().map(|(_, e)| e.degradation_ms).collect();
            let max_degradation_ms = degradations.iter().copied().max().unwrap_or(0);
            let median_degradation_ms = median(&mut degradations).unwrap_or(0);
            let phase = if key.starts_with("shutdown:") {
                Phase::Shutdown
            } else {
                Phase::Startup
            };
            Some(SlowItem {
                key,
                phase,
                kind: newest.kind,
                name: newest.name,
                title: newest.title,
                path: newest.path,
                company: newest.company,
                count: events.len() as u32,
                last_seen,
                median_degradation_ms,
                max_degradation_ms,
                startup_ids: Vec::new(),
            })
        })
        .collect();
    items.sort_by(|a, b| {
        let weight = |i: &SlowItem| u64::from(i.count).saturating_mul(i.median_degradation_ms);
        weight(b)
            .cmp(&weight(a))
            .then_with(|| a.title.cmp(&b.title))
    });

    if items
        .iter()
        .any(|i| i.kind == SlowKind::App && i.phase == Phase::Startup)
    {
        match startup() {
            Ok(entries) => {
                for item in items
                    .iter_mut()
                    .filter(|i| i.kind == SlowKind::App && i.phase == Phase::Startup)
                {
                    if let Some(entry) = matching_entry(&entries, item.path.as_deref(), &item.name)
                    {
                        item.startup_ids.push(entry.id.clone());
                        if !history.startup_entries.iter().any(|e| e.id == entry.id) {
                            history.startup_entries.push(entry.clone());
                        }
                    }
                }
            }
            Err(_) => history.notes.push(STARTUP_LIST_NOTE.to_string()),
        }
    }

    // Windows times only full starts: a Fast Startup start leaves its Kernel-Boot event and
    // no Diagnostics-Performance event, so it is missing from the list.
    let (fast_since, start_types) = match since {
        Some(since) => (since, boot_types),
        None => {
            let since = now - ChronoDuration::days(UNTIMED_DAYS);
            let types = source.boot_types(since).unwrap_or_else(|e| {
                history
                    .errors
                    .push(format!("Could not read the start types: {e}"));
                Vec::new()
            });
            (since, types)
        }
    };
    let untimed = start_types
        .iter()
        .filter(|(t, raw)| {
            *raw == FAST_STARTUP_BOOT_TYPE
                && !boot_events.iter().any(|(_, _, data)| {
                    data.start
                        .is_some_and(|start| in_start_window(*t, start, data.end))
                })
        })
        .count();
    if untimed > 0 {
        history.notes.push(untimed_note(untimed, fast_since));
    }

    history.stats = stats(&boots);
    history.boots = boots;
    history.shutdowns = shutdowns;
    history.slow_items = items;
    history.unexpected_shutdowns = unexpected;
    Ok(history)
}

// ───────────────────────────── Live source ─────────────────────────────

const BOOT_FIELDS: [&str; 13] = [
    "BootStartTime",
    "BootEndTime",
    "BootTime",
    "MainPathBootTime",
    "BootPostBootTime",
    "BootKernelInitTime",
    "BootDriverInitTime",
    "BootDevicesInitTime",
    "BootUserProfileProcessingTime",
    "BootExplorerInitTime",
    "BootNumStartupApps",
    "BootIsRebootAfterInstall",
    "BootIsDegradation",
];
const SLOW_FIELDS: [&str; 9] = [
    "StartTime",
    "Name",
    "FriendlyName",
    "Version",
    "TotalTime",
    "DegradationTime",
    "Path",
    "ProductName",
    "CompanyName",
];
const SHUTDOWN_FIELDS: [&str; 4] = [
    "ShutdownStartTime",
    "ShutdownEndTime",
    "ShutdownTime",
    "ShutdownIsDegradation",
];

fn value(values: &[EvtValue], i: usize) -> &EvtValue {
    values.get(i).unwrap_or(&EvtValue::Null)
}

fn text(values: &[EvtValue], i: usize) -> Option<String> {
    value(values, i)
        .as_text()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Event 100 from its `BOOT_FIELDS` values; missing fields are zero or `None`.
pub(crate) fn boot_data(values: &[EvtValue]) -> BootData {
    let ms = |i| value(values, i).as_u64();
    BootData {
        start: value(values, 0).as_time(),
        end: value(values, 1).as_time(),
        boot_ms: ms(2).unwrap_or(0),
        main_path_ms: ms(3).unwrap_or(0),
        post_boot_ms: ms(4).unwrap_or(0),
        phases: BootPhases {
            kernel_ms: ms(5),
            drivers_ms: ms(6),
            devices_ms: ms(7),
            user_profile_ms: ms(8),
            explorer_ms: ms(9),
        },
        startup_apps: ms(10).and_then(|v| u32::try_from(v).ok()),
        after_update: value(values, 11).as_bool().unwrap_or(false),
        degraded: value(values, 12).as_bool().unwrap_or(false),
    }
}

/// Events 101-110 and 201-203 from their `SLOW_FIELDS` values.
pub(crate) fn slow_data(values: &[EvtValue]) -> SlowData {
    SlowData {
        start: value(values, 0).as_time(),
        name: text(values, 1).unwrap_or_default(),
        friendly_name: text(values, 2),
        version: text(values, 3),
        total_ms: value(values, 4).as_u64().unwrap_or(0),
        degradation_ms: value(values, 5).as_u64().unwrap_or(0),
        path: text(values, 6),
        product: text(values, 7),
        company: text(values, 8),
    }
}

/// Event 200 from its `SHUTDOWN_FIELDS` values.
pub(crate) fn shutdown_data(values: &[EvtValue]) -> ShutdownData {
    ShutdownData {
        start: value(values, 0).as_time(),
        end: value(values, 1).as_time(),
        shutdown_ms: value(values, 2).as_u64().unwrap_or(0),
        degraded: value(values, 3).as_bool().unwrap_or(false),
    }
}

/// The event logs of this PC.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveEvents;

fn other(e: Error) -> LogError {
    LogError::Other(e)
}

/// Times (and the first data field) of the System log events matching `xpath`, newest first.
fn system_events(xpath: &str, field: Option<&str>) -> Result<Vec<(DateTime<Utc>, u32)>> {
    let mut query =
        event_log::query("System", xpath, true).map_err(|e| Error::Other(e.to_string()))?;
    let system = RenderContext::system()?;
    let values = match field {
        Some(name) => Some(RenderContext::values(&[&data_path(name)])?),
        None => None,
    };
    let mut out = Vec::new();
    loop {
        let batch = query.next_batch(64)?;
        if batch.is_empty() || out.len() >= MAX_PERF_EVENTS {
            break;
        }
        for event in &batch {
            let sys = system.render(event)?;
            let Some(time) = value(&sys, SYSTEM_TIME_CREATED).as_time() else {
                continue;
            };
            let data = match &values {
                Some(ctx) => ctx
                    .render(event)?
                    .first()
                    .and_then(EvtValue::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(u32::MAX),
                None => 0,
            };
            out.push((time, data));
        }
    }
    Ok(out)
}

impl EventSource for LiveEvents {
    fn log_enabled(&self) -> Option<bool> {
        match crate::win::registry::Key::open(
            crate::win::registry::Hive::LocalMachine,
            PERF_CHANNEL_KEY,
            false,
        ) {
            Ok(Some(key)) => super::probe::key_dword(Some(&key), "Enabled").map(|v| v != 0),
            _ => None,
        }
    }

    fn performance(
        &self,
        boots: usize,
        max_events: usize,
    ) -> std::result::Result<Vec<PerfEvent>, LogError> {
        let mut query = event_log::query(PERF_CHANNEL, PERF_QUERY, true)?;
        let system = RenderContext::system().map_err(other)?;
        let boot_ctx =
            RenderContext::values(&BOOT_FIELDS.map(data_path).each_ref().map(String::as_str))
                .map_err(other)?;
        let slow_ctx =
            RenderContext::values(&SLOW_FIELDS.map(data_path).each_ref().map(String::as_str))
                .map_err(other)?;
        let shutdown_ctx = RenderContext::values(
            &SHUTDOWN_FIELDS
                .map(data_path)
                .each_ref()
                .map(String::as_str),
        )
        .map_err(other)?;
        let mut out = Vec::new();
        let mut seen_boots = 0usize;
        'read: loop {
            let batch = query.next_batch(64).map_err(LogError::from_error)?;
            if batch.is_empty() {
                break;
            }
            for event in &batch {
                let sys = system.render(event).map_err(LogError::from_error)?;
                let Some(event_id) = value(&sys, SYSTEM_EVENT_ID)
                    .as_u64()
                    .and_then(|v| u16::try_from(v).ok())
                else {
                    continue;
                };
                let Some(logged_at) = value(&sys, SYSTEM_TIME_CREATED).as_time() else {
                    continue;
                };
                let record_id = value(&sys, SYSTEM_RECORD_ID).as_u64().unwrap_or(0);
                let data = match event_id {
                    100 => {
                        seen_boots += 1;
                        PerfData::Boot(boot_data(&boot_ctx.render(event).map_err(other)?))
                    }
                    200 => PerfData::Shutdown(shutdown_data(
                        &shutdown_ctx.render(event).map_err(other)?,
                    )),
                    id if SlowKind::of_event(id).is_some() => {
                        PerfData::Slow(slow_data(&slow_ctx.render(event).map_err(other)?))
                    }
                    _ => continue,
                };
                out.push(PerfEvent {
                    event_id,
                    record_id,
                    logged_at,
                    data,
                });
                if seen_boots >= boots || out.len() >= max_events {
                    break 'read;
                }
            }
        }
        Ok(out)
    }

    fn boot_types(&self, since: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, u32)>> {
        system_events(
            &format!(
                "*[System[Provider[@Name='Microsoft-Windows-Kernel-Boot'] and EventID=27 and \
                 TimeCreated[@SystemTime>='{}']]]",
                xpath_time(since)
            ),
            Some("BootType"),
        )
    }

    fn unexpected_shutdowns(&self, since: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>> {
        Ok(system_events(
            &format!(
                "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and EventID=41 and \
                 TimeCreated[@SystemTime>='{}']]]",
                xpath_time(since)
            ),
            None,
        )?
        .into_iter()
        .map(|(t, _)| t)
        .collect())
    }
}

/// The boot history of this PC: at most `limit` starts (1 to 500).
pub(crate) fn live_history(limit: usize) -> Result<BootHistory> {
    if !(1..=MAX_BOOT_LIMIT).contains(&limit) {
        return Err(Error::Other(format!(
            "limit must be between 1 and {MAX_BOOT_LIMIT}, got {limit}"
        )));
    }
    let fast_startup = crate::sysinfo::os_info()
        .ok()
        .and_then(|os| os.fast_startup);
    history_with(
        &LiveEvents,
        &crate::startup::list,
        fast_startup,
        limit,
        Utc::now(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::sync::Mutex;

    use super::*;
    use crate::startup::StartupSource;

    pub(crate) fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn secs(n: i64) -> ChronoDuration {
        ChronoDuration::seconds(n)
    }

    fn mins(n: i64) -> ChronoDuration {
        ChronoDuration::minutes(n)
    }

    /// Event 100 of a start at `start` that took `boot_ms`, logged 30 s after it ended.
    pub(crate) fn boot(record_id: u64, start: DateTime<Utc>, boot_ms: u64) -> PerfEvent {
        let end = start + ChronoDuration::milliseconds(boot_ms as i64);
        PerfEvent {
            event_id: 100,
            record_id,
            logged_at: end + secs(30),
            data: PerfData::Boot(BootData {
                start: Some(start),
                end: Some(end),
                boot_ms,
                main_path_ms: boot_ms / 2,
                post_boot_ms: boot_ms - boot_ms / 2,
                phases: BootPhases {
                    kernel_ms: Some(1500),
                    ..BootPhases::default()
                },
                startup_apps: Some(12),
                after_update: false,
                degraded: false,
            }),
        }
    }

    pub(crate) fn slow(
        event_id: u16,
        logged_at: DateTime<Utc>,
        start: DateTime<Utc>,
        name: &str,
        path: Option<&str>,
        degradation_ms: u64,
    ) -> PerfEvent {
        PerfEvent {
            event_id,
            record_id: 0,
            logged_at,
            data: PerfData::Slow(SlowData {
                start: Some(start),
                name: name.into(),
                friendly_name: None,
                version: Some("1.0".into()),
                path: path.map(str::to_string),
                product: None,
                company: Some("Contoso".into()),
                total_ms: degradation_ms * 2,
                degradation_ms,
            }),
        }
    }

    pub(crate) fn shutdown(record_id: u64, start: DateTime<Utc>, shutdown_ms: u64) -> PerfEvent {
        let end = start + ChronoDuration::milliseconds(shutdown_ms as i64);
        PerfEvent {
            event_id: 200,
            record_id,
            logged_at: end + secs(5),
            data: PerfData::Shutdown(ShutdownData {
                start: Some(start),
                end: Some(end),
                shutdown_ms,
                degraded: true,
            }),
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeSource {
        pub(crate) enabled: Option<bool>,
        pub(crate) events: Vec<PerfEvent>,
        /// "denied", "missing" or "broken" makes the performance read fail.
        pub(crate) failure: Option<&'static str>,
        pub(crate) boot_types: Vec<(DateTime<Utc>, u32)>,
        pub(crate) crashes: Vec<DateTime<Utc>>,
        pub(crate) since: Mutex<Vec<DateTime<Utc>>>,
    }

    impl EventSource for FakeSource {
        fn log_enabled(&self) -> Option<bool> {
            self.enabled
        }

        fn performance(
            &self,
            _boots: usize,
            _max_events: usize,
        ) -> std::result::Result<Vec<PerfEvent>, LogError> {
            match self.failure {
                Some("denied") => Err(LogError::AccessDenied),
                Some("missing") => Err(LogError::ChannelNotFound),
                Some(_) => Err(LogError::Other(Error::Other("the log is corrupt".into()))),
                None => Ok(self.events.clone()),
            }
        }

        fn boot_types(&self, since: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, u32)>> {
            self.since.lock().unwrap().push(since);
            Ok(self.boot_types.clone())
        }

        fn unexpected_shutdowns(&self, since: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>> {
            self.since.lock().unwrap().push(since);
            Ok(self.crashes.clone())
        }
    }

    pub(crate) fn entry(id: &str, path: &str, enabled: bool) -> StartupEntry {
        StartupEntry {
            id: id.into(),
            name: id.split(':').nth(1).unwrap_or(id).into(),
            source: StartupSource::UserRun,
            location: "HKCU Run".into(),
            command: format!("\"{path}\""),
            path: path.into(),
            publisher: "Contoso".into(),
            exists: true,
            enabled,
            requires_admin: false,
            can_toggle: true,
            note: String::new(),
        }
    }

    fn now() -> DateTime<Utc> {
        at("2026-09-28T12:00:00Z")
    }

    fn no_entries() -> Result<Vec<StartupEntry>> {
        Ok(Vec::new())
    }

    fn build(source: &FakeSource, limit: usize) -> BootHistory {
        history_with(source, &no_entries, Some(true), limit, now()).unwrap()
    }

    /// Ten full starts, one per day from 2026-09-01, each slowed by an app; three shutdowns.
    pub(crate) fn sample_source() -> FakeSource {
        let mut events = Vec::new();
        let mut boot_types = Vec::new();
        for day in 0..10i64 {
            let start = at("2026-09-01T08:00:00Z") + ChronoDuration::days(day);
            let event = boot(100 + day as u64, start, 40_000 + day as u64 * 1000);
            let logged = event.logged_at;
            events.push(event);
            events.push(slow(
                101,
                logged,
                start + secs(10),
                "Discord.exe",
                Some(r"C:\Apps\Discord.exe"),
                3000,
            ));
            boot_types.push((start + secs(2), 0));
        }
        for day in 0..3i64 {
            let start = at("2026-09-01T23:00:00Z") + ChronoDuration::days(day);
            let event = shutdown(900 + day as u64, start, 12_300);
            let logged = event.logged_at;
            events.push(event);
            events.push(slow(
                203,
                logged,
                start + secs(1),
                "Contoso Service",
                None,
                800,
            ));
        }
        FakeSource {
            events,
            boot_types,
            ..FakeSource::default()
        }
    }

    pub(crate) fn sample_history() -> BootHistory {
        let entries = || -> Result<Vec<StartupEntry>> {
            Ok(vec![entry(
                "user_run:Discord",
                r"C:\Apps\Discord.exe",
                true,
            )])
        };
        history_with(&sample_source(), &entries, Some(true), 60, now()).unwrap()
    }

    #[test]
    fn a_sample_history_is_complete() {
        let history = sample_history();
        assert_eq!(history.access, LogAccess::Ok);
        assert_eq!(history.boots.len(), 10);
        assert_eq!(history.shutdowns.len(), 3);
        // Newest first.
        assert_eq!(history.boots[0].record_id, 109);
        assert_eq!(history.boots[0].boot_ms, 49_000);
        assert!(history.boots.iter().all(|b| b.boot_type == BootType::Full));
        assert!(history.boots.iter().all(|b| b.slow.len() == 1));
        assert!(history.shutdowns.iter().all(|s| s.slow.len() == 1));
        assert_eq!(history.slow_items.len(), 2);
        let app = &history.slow_items[0];
        assert_eq!(app.key, "startup:app:discord.exe");
        assert_eq!((app.count, app.median_degradation_ms), (10, 3000));
        assert_eq!(app.startup_ids, vec!["user_run:Discord".to_string()]);
        assert_eq!(history.startup_entries.len(), 1);
        let service = &history.slow_items[1];
        assert_eq!(service.key, "shutdown:service:contoso service");
        assert_eq!(service.phase, Phase::Shutdown);
        assert_eq!(history.stats.count, 10);
        assert_eq!(history.stats.full_count, 10);
        assert_eq!(history.stats.latest_ms, Some(49_000));
        assert_eq!(history.stats.median_ms, Some(44_500));
        let trend = history.stats.trend.clone().unwrap();
        assert_eq!(trend.boot_type, Some(BootType::Full));
        assert_eq!(trend.recent_median_ms, 47_000);
        assert_eq!(trend.earlier_median_ms, 42_000);
        assert_eq!(history.fast_startup, Some(true));
        assert!(history.notes.is_empty() && history.errors.is_empty());
    }

    #[test]
    fn boot_keys_match_the_contract() {
        let json = serde_json::to_value(sample_history()).unwrap();
        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let sorted = |names: &[&str]| -> Vec<String> {
            let mut k: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        assert_eq!(
            keys(&json),
            sorted(&[
                "read_at",
                "access",
                "boots",
                "shutdowns",
                "slow_items",
                "unexpected_shutdowns",
                "stats",
                "fast_startup",
                "startup_entries",
                "notes",
                "errors"
            ])
        );
        assert_eq!(
            keys(&json["boots"][0]),
            sorted(&[
                "record_id",
                "started_at",
                "logged_at",
                "boot_ms",
                "main_path_ms",
                "post_boot_ms",
                "startup_apps",
                "after_update",
                "degraded",
                "boot_type",
                "after_unexpected_shutdown",
                "phases",
                "slow"
            ])
        );
        assert_eq!(
            keys(&json["boots"][0]["phases"]),
            sorted(&[
                "kernel_ms",
                "drivers_ms",
                "devices_ms",
                "user_profile_ms",
                "explorer_ms"
            ])
        );
        assert_eq!(
            keys(&json["boots"][0]["slow"][0]),
            sorted(&[
                "event_id",
                "kind",
                "name",
                "title",
                "path",
                "company",
                "version",
                "total_ms",
                "degradation_ms",
                "at"
            ])
        );
        assert_eq!(
            keys(&json["shutdowns"][0]),
            sorted(&[
                "record_id",
                "started_at",
                "logged_at",
                "shutdown_ms",
                "degraded",
                "slow"
            ])
        );
        assert_eq!(
            keys(&json["slow_items"][0]),
            sorted(&[
                "key",
                "phase",
                "kind",
                "name",
                "title",
                "path",
                "company",
                "count",
                "last_seen",
                "median_degradation_ms",
                "max_degradation_ms",
                "startup_ids"
            ])
        );
        assert_eq!(
            keys(&json["stats"]),
            sorted(&[
                "count",
                "latest_ms",
                "median_ms",
                "full_count",
                "fast_count",
                "trend"
            ])
        );
        assert_eq!(
            keys(&json["stats"]["trend"]),
            sorted(&[
                "boot_type",
                "recent_median_ms",
                "earlier_median_ms",
                "change_pct"
            ])
        );
        assert_eq!(json["access"], "ok");
        assert_eq!(json["boots"][0]["boot_type"], "full");
        assert_eq!(json["slow_items"][0]["kind"], "app");
        assert_eq!(json["slow_items"][1]["phase"], "shutdown");
        assert_eq!(json["startup_entries"][0]["id"], "user_run:Discord");
    }

    #[test]
    fn slow_events_join_the_start_logged_nearest() {
        let a_start = at("2026-09-20T07:59:00Z");
        let b_start = at("2026-09-20T08:11:00Z");
        // A is logged at 07:59:30 + 30 s = 08:00:00, B at 08:11:40 + 30 s = 08:12:10.
        let events = vec![
            boot(1, a_start, 30_000),
            boot(2, b_start, 40_000),
            // 6 min 5 s from both: its start time lies within A.
            slow(
                101,
                at("2026-09-20T08:06:05Z"),
                a_start + secs(10),
                "A.exe",
                None,
                100,
            ),
            // Same distance, start time within B.
            slow(
                101,
                at("2026-09-20T08:06:05Z"),
                b_start + secs(10),
                "B.exe",
                None,
                100,
            ),
            // Nearer to B.
            slow(
                102,
                at("2026-09-20T08:10:00Z"),
                a_start,
                "Contoso Driver",
                None,
                100,
            ),
            // More than 10 minutes from both: not assigned, still counted.
            slow(
                101,
                at("2026-09-20T08:40:00Z"),
                b_start,
                "Late.exe",
                None,
                100,
            ),
        ];
        let history = build(
            &FakeSource {
                events,
                ..FakeSource::default()
            },
            60,
        );
        let names = |i: usize| -> Vec<String> {
            history.boots[i]
                .slow
                .iter()
                .map(|s| s.name.clone())
                .collect()
        };
        // boots[0] is B (newest).
        assert_eq!(names(0), vec!["B.exe", "Contoso Driver"]);
        assert_eq!(names(1), vec!["A.exe"]);
        assert!(history.slow_items.iter().any(|i| i.name == "Late.exe"));
        let driver = history
            .slow_items
            .iter()
            .find(|i| i.kind == SlowKind::Driver)
            .unwrap();
        assert_eq!(driver.key, "startup:driver:contoso driver");
    }

    #[test]
    fn start_types_come_from_kernel_boot_events_near_the_start() {
        let s1 = at("2026-09-20T08:00:00Z");
        let s2 = at("2026-09-21T08:00:00Z");
        let s3 = at("2026-09-22T08:00:00Z");
        let s4 = at("2026-09-23T08:00:00Z");
        let source = FakeSource {
            events: vec![
                boot(1, s1, 400_000),
                boot(2, s2, 60_000),
                boot(3, s3, 60_000),
                boot(4, s4, 60_000),
            ],
            boot_types: vec![
                // s1: the latest inside [start - 2 min, start + 10 min] and before the end wins.
                (s1 - mins(3), 2),
                (s1 - mins(1), 1),
                (s1 + mins(5), 0),
                // s2: after the end of the start.
                (s2 + mins(3), 1),
                // s3: resume from hibernation.
                (s3 + secs(1), 2),
            ],
            ..FakeSource::default()
        };
        let history = build(&source, 60);
        let kinds: Vec<BootType> = history.boots.iter().map(|b| b.boot_type).collect();
        assert_eq!(
            kinds,
            vec![
                BootType::Unknown,
                BootType::Hibernate,
                BootType::Unknown,
                BootType::Full
            ]
        );
        assert_eq!(history.stats.full_count, 1);
        assert_eq!(history.stats.fast_count, 0);
        // Queried from one hour before the oldest start.
        let since = source.since.lock().unwrap().clone();
        assert_eq!(since, vec![s1 - mins(60), s1 - mins(60)]);
    }

    #[test]
    fn unexpected_shutdowns_mark_the_next_start() {
        let s1 = at("2026-09-20T08:00:00Z");
        let s2 = at("2026-09-21T08:00:00Z");
        let source = FakeSource {
            events: vec![boot(1, s1, 30_000), boot(2, s2, 30_000)],
            crashes: vec![s1 - mins(5), s2 - mins(1)],
            ..FakeSource::default()
        };
        let history = build(&source, 60);
        assert!(history.boots[0].after_unexpected_shutdown);
        assert!(!history.boots[1].after_unexpected_shutdown);
        assert_eq!(
            history.unexpected_shutdowns,
            vec![s2 - mins(1), s1 - mins(5)]
        );
    }

    fn boots_of(kinds: &[u32], first_ms: u64) -> FakeSource {
        let mut events = Vec::new();
        let mut boot_types = Vec::new();
        for (i, kind) in kinds.iter().enumerate() {
            // Older starts are faster: index 0 is the newest.
            let start = at("2026-09-27T08:00:00Z") - ChronoDuration::days(i as i64);
            events.push(boot(i as u64, start, first_ms - i as u64 * 1000));
            boot_types.push((start + secs(1), *kind));
        }
        FakeSource {
            events,
            boot_types,
            ..FakeSource::default()
        }
    }

    #[test]
    fn a_trend_needs_eight_starts() {
        assert!(build(&boots_of(&[1; 7], 40_000), 60).stats.trend.is_none());
        let trend = build(&boots_of(&[1; 8], 40_000), 60).stats.trend.unwrap();
        assert_eq!(trend.boot_type, None);
        assert_eq!(trend.recent_median_ms, 38_000);
        assert_eq!(trend.earlier_median_ms, 34_000);
        assert!((trend.change_pct - 400.0 / 34.0).abs() < 1e-9);
    }

    #[test]
    fn the_trend_prefers_full_starts() {
        // Eight full starts among fast ones: only the full starts are compared.
        let mut kinds = vec![0u32; 8];
        kinds.extend([1, 1, 1, 1]);
        let history = build(&boots_of(&kinds, 60_000), 60);
        let trend = history.stats.trend.unwrap();
        assert_eq!(trend.boot_type, Some(BootType::Full));
        assert_eq!(history.stats.fast_count, 4);
        // Seven full starts: every start is compared.
        let mut kinds = vec![0u32; 7];
        kinds.extend([1, 1, 1]);
        let trend = build(&boots_of(&kinds, 60_000), 60).stats.trend.unwrap();
        assert_eq!(trend.boot_type, None);
    }

    #[test]
    fn slow_apps_match_startup_entries_by_path_or_file_name() {
        let start = at("2026-09-20T08:00:00Z");
        let event = boot(1, start, 30_000);
        let logged = event.logged_at;
        let source = FakeSource {
            events: vec![
                event,
                slow(
                    101,
                    logged,
                    start,
                    "discord.exe",
                    Some(r"\\?\C:\APPS\discord.exe"),
                    900,
                ),
                slow(
                    101,
                    logged,
                    start,
                    "CONTOSO.EXE",
                    Some(r"C:\Program Files\Contoso\CONTOSO.EXE"),
                    800,
                ),
                slow(101, logged, start, "Fabrikam.exe", None, 700),
                slow(
                    101,
                    logged,
                    start,
                    "Northwind.exe",
                    Some(r"C:\N\Northwind.exe"),
                    600,
                ),
                slow(
                    102,
                    logged,
                    start,
                    "discord.exe",
                    Some(r"C:\Apps\Discord.exe"),
                    500,
                ),
            ],
            ..FakeSource::default()
        };
        let calls = Cell::new(0);
        let entries = || -> Result<Vec<StartupEntry>> {
            calls.set(calls.get() + 1);
            Ok(vec![
                entry("user_run:Discord", r"C:\Apps\Discord.exe", true),
                entry("user_folder:Contoso.lnk", r"D:\Other\contoso.exe", false),
                entry("machine_run:Fabrikam", r"C:\Tools\fabrikam.EXE", true),
                entry("user_run:Empty", "", true),
            ])
        };
        let history = history_with(&source, &entries, None, 60, now()).unwrap();
        assert_eq!(calls.get(), 1);
        let ids = |name: &str| -> Vec<String> {
            history
                .slow_items
                .iter()
                .find(|i| i.name == name && i.kind == SlowKind::App)
                .unwrap()
                .startup_ids
                .clone()
        };
        assert_eq!(ids("discord.exe"), vec!["user_run:Discord"]);
        assert_eq!(ids("CONTOSO.EXE"), vec!["user_folder:Contoso.lnk"]);
        assert_eq!(ids("Fabrikam.exe"), vec!["machine_run:Fabrikam"]);
        assert!(ids("Northwind.exe").is_empty());
        // Drivers are never matched.
        let driver = history
            .slow_items
            .iter()
            .find(|i| i.kind == SlowKind::Driver)
            .unwrap();
        assert!(driver.startup_ids.is_empty());
        let listed: Vec<&str> = history
            .startup_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(
            listed,
            vec![
                "user_run:Discord",
                "user_folder:Contoso.lnk",
                "machine_run:Fabrikam"
            ]
        );
        // Sorted by count times the median delay.
        assert_eq!(history.slow_items[0].name, "discord.exe");
    }

    #[test]
    fn a_slow_app_matches_only_its_own_startup_entry() {
        let start = at("2026-09-20T08:00:00Z");
        let event = boot(1, start, 30_000);
        let logged = event.logged_at;
        let source = FakeSource {
            events: vec![
                event,
                // Its path names one entry; another program's Update.exe shares only its name.
                slow(
                    101,
                    logged,
                    start,
                    "Update.exe",
                    Some(r"C:\Users\Test\AppData\Local\Contoso\Update.exe"),
                    900,
                ),
                // No path, and two entries share its file name.
                slow(101, logged, start, "Updater.exe", None, 800),
                // Two entries start this very program.
                slow(
                    101,
                    logged,
                    start,
                    "Tailspin.exe",
                    Some(r"C:\Tools\Tailspin.exe"),
                    700,
                ),
                // A shared host starts many unrelated things.
                slow(
                    101,
                    logged,
                    start,
                    "rundll32.exe",
                    Some(r"C:\Windows\System32\rundll32.exe"),
                    600,
                ),
            ],
            ..FakeSource::default()
        };
        let entries = || -> Result<Vec<StartupEntry>> {
            Ok(vec![
                entry(
                    "user_run:Fabrikam",
                    r"C:\Users\Test\AppData\Local\Fabrikam\Update.exe",
                    true,
                ),
                entry(
                    "user_run:Contoso",
                    r"C:\Users\Test\AppData\Local\Contoso\Update.exe",
                    true,
                ),
                entry("user_run:Northwind", r"C:\Tools\A\Updater.exe", true),
                entry("machine_run:Northwind", r"C:\Tools\B\Updater.exe", true),
                entry("user_run:Tailspin", r"C:\Tools\Tailspin.exe", true),
                entry("user_folder:Tailspin.lnk", r"C:\Tools\Tailspin.exe", true),
                entry(
                    "user_run:Wingtip",
                    r"C:\Windows\System32\rundll32.exe",
                    true,
                ),
            ])
        };
        let history = history_with(&source, &entries, None, 60, now()).unwrap();
        let ids = |name: &str| -> Vec<String> {
            history
                .slow_items
                .iter()
                .find(|i| i.name == name)
                .unwrap()
                .startup_ids
                .clone()
        };
        assert_eq!(ids("Update.exe"), vec!["user_run:Contoso"]);
        assert!(ids("Updater.exe").is_empty());
        assert!(ids("Tailspin.exe").is_empty());
        assert!(ids("rundll32.exe").is_empty());
        let listed: Vec<&str> = history
            .startup_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(listed, vec!["user_run:Contoso"]);
    }

    #[test]
    fn fast_startup_starts_that_windows_does_not_time_are_counted() {
        let older = at("2026-09-24T08:00:00Z");
        let newer = at("2026-09-28T08:00:00Z");
        let fast = |text: &str| (at(text), 1);
        let source = FakeSource {
            events: vec![boot(2, newer, 44_000), boot(1, older, 31_000)],
            boot_types: vec![
                (older + secs(1), 0),
                fast("2026-09-25T08:00:00Z"),
                fast("2026-09-26T08:00:00Z"),
                fast("2026-09-27T08:00:00Z"),
                // Inside a listed start's window: that start's own event.
                (newer - mins(1), 1),
                (newer + secs(1), 0),
                fast("2026-09-29T08:00:00Z"),
                fast("2026-09-30T08:00:00Z"),
                // A resume from hibernation is not a start.
                (at("2026-09-30T20:00:00Z"), 2),
            ],
            ..FakeSource::default()
        };
        let read_at = at("2026-10-01T00:00:00Z");
        let history = history_with(&source, &no_entries, Some(true), 60, read_at).unwrap();
        assert_eq!(history.boots.len(), 2);
        assert!(history.boots.iter().all(|b| b.boot_type == BootType::Full));
        assert_eq!(
            history.notes,
            vec![format!(
                "Windows times only full starts, such as restarts: 5 Fast Startup starts since \
                 {} are not listed.",
                super::super::text::local_date(older - mins(60))
            )]
        );
        // Only full starts: nothing to add.
        let history = build(&sample_source(), 60);
        assert!(history.notes.is_empty());
    }

    #[test]
    fn without_timed_starts_the_last_thirty_days_are_counted() {
        let read_at = at("2026-10-01T00:00:00Z");
        let source = FakeSource {
            boot_types: vec![(at("2026-09-29T08:00:00Z"), 1)],
            ..FakeSource::default()
        };
        let history = history_with(&source, &no_entries, Some(true), 60, read_at).unwrap();
        assert!(history.boots.is_empty());
        assert_eq!(
            history.notes,
            vec![format!(
                "Windows times only full starts, such as restarts: 1 Fast Startup start since {} \
                 is not listed.",
                super::super::text::local_date(read_at - ChronoDuration::days(30))
            )]
        );
        assert_eq!(
            source.since.lock().unwrap().clone(),
            vec![read_at - ChronoDuration::days(30)]
        );
        // Nothing recorded at all: no note.
        assert!(build(&FakeSource::default(), 60).notes.is_empty());
    }

    #[test]
    fn an_unreadable_startup_list_is_a_note() {
        let start = at("2026-09-20T08:00:00Z");
        let event = boot(1, start, 30_000);
        let logged = event.logged_at;
        let source = FakeSource {
            events: vec![event, slow(101, logged, start, "App.exe", None, 900)],
            ..FakeSource::default()
        };
        let failing =
            || -> Result<Vec<StartupEntry>> { Err(Error::Other("registry unreadable".into())) };
        let history = history_with(&source, &failing, None, 60, now()).unwrap();
        assert_eq!(history.notes, vec![STARTUP_LIST_NOTE.to_string()]);
        assert!(history.slow_items[0].startup_ids.is_empty());
        // Without slow apps the list is never read.
        let source = FakeSource {
            events: vec![boot(1, start, 30_000)],
            ..FakeSource::default()
        };
        let panicking = || -> Result<Vec<StartupEntry>> { panic!("not needed") };
        let history = history_with(&source, &panicking, None, 60, now()).unwrap();
        assert!(history.notes.is_empty());
    }

    #[test]
    fn access_errors_map_to_their_states() {
        for (failure, access) in [
            ("denied", LogAccess::NeedsAdmin),
            ("missing", LogAccess::LogMissing),
        ] {
            let source = FakeSource {
                failure: Some(failure),
                ..sample_source()
            };
            let history = build(&source, 60);
            assert_eq!(history.access, access);
            assert!(history.boots.is_empty() && history.slow_items.is_empty());
            assert_eq!(history.stats.count, 0);
            assert!(source.since.lock().unwrap().is_empty());
        }
        let source = FakeSource {
            failure: Some("broken"),
            ..FakeSource::default()
        };
        let err = history_with(&source, &no_entries, None, 60, now()).unwrap_err();
        assert_eq!(err.to_string(), "the log is corrupt");
    }

    #[test]
    fn a_disabled_log_still_lists_what_it_recorded() {
        let source = FakeSource {
            enabled: Some(false),
            ..sample_source()
        };
        let history = build(&source, 60);
        assert_eq!(history.access, LogAccess::LogDisabled);
        assert_eq!(history.boots.len(), 10);
    }

    #[test]
    fn the_limit_keeps_the_newest_starts() {
        let history = build(&sample_source(), 3);
        let ids: Vec<u64> = history.boots.iter().map(|b| b.record_id).collect();
        assert_eq!(ids, vec![109, 108, 107]);
        assert_eq!(history.stats.count, 3);
        // Slow events older than the oldest listed start are not counted.
        assert_eq!(history.slow_items[0].count, 3);
    }

    #[test]
    fn no_events_give_an_empty_history() {
        let history = build(&FakeSource::default(), 60);
        assert_eq!(history.access, LogAccess::Ok);
        assert!(history.boots.is_empty());
        assert_eq!(history.stats.latest_ms, None);
        assert!(history.stats.trend.is_none());
    }

    #[test]
    fn rendered_values_decode_with_missing_fields() {
        let ft = |t: &str| {
            let t = at(t);
            EvtValue::FileTime(
                crate::win::filetime::UNIX_EPOCH_FILETIME + t.timestamp() as u64 * 10_000_000,
            )
        };
        // An event 100 whose later fields are missing or null.
        let values = vec![
            ft("2026-09-20T08:00:00Z"),
            ft("2026-09-20T08:00:41Z"),
            EvtValue::UInt(41_200),
            EvtValue::UInt(18_000),
            EvtValue::UInt(23_200),
            EvtValue::Null,
        ];
        let data = boot_data(&values);
        assert_eq!(data.start, Some(at("2026-09-20T08:00:00Z")));
        assert_eq!(data.end, Some(at("2026-09-20T08:00:41Z")));
        assert_eq!(
            (data.boot_ms, data.main_path_ms, data.post_boot_ms),
            (41_200, 18_000, 23_200)
        );
        assert_eq!(data.phases, BootPhases::default());
        assert_eq!(data.startup_apps, None);
        assert!(!data.after_update && !data.degraded);
        let mut full = values.clone();
        full.truncate(5);
        full.extend([
            EvtValue::UInt(1),
            EvtValue::UInt(2),
            EvtValue::UInt(3),
            EvtValue::UInt(4),
            EvtValue::UInt(5),
            EvtValue::UInt(12),
            EvtValue::Bool(true),
            EvtValue::Bool(true),
        ]);
        let data = boot_data(&full);
        assert_eq!(data.phases.explorer_ms, Some(5));
        assert_eq!(data.startup_apps, Some(12));
        assert!(data.after_update && data.degraded);

        let slow = slow_data(&[
            ft("2026-09-20T08:00:05Z"),
            EvtValue::Text("OneDrive.exe".into()),
            EvtValue::Text("Microsoft OneDrive".into()),
            EvtValue::Null,
            EvtValue::UInt(5000),
            EvtValue::UInt(2500),
        ]);
        assert_eq!(slow.name, "OneDrive.exe");
        assert_eq!(title_of(&slow), "Microsoft OneDrive");
        assert_eq!((slow.total_ms, slow.degradation_ms), (5000, 2500));
        assert_eq!(slow.path, None);
        let shutdown = shutdown_data(&[EvtValue::Null, EvtValue::Null, EvtValue::UInt(12_300)]);
        assert_eq!(shutdown.shutdown_ms, 12_300);
        assert!(!shutdown.degraded);
    }

    #[test]
    fn titles_and_kinds() {
        let data = |friendly: Option<&str>, product: Option<&str>| SlowData {
            name: "svc.exe".into(),
            friendly_name: friendly.map(str::to_string),
            product: product.map(str::to_string),
            ..SlowData::default()
        };
        assert_eq!(
            title_of(&data(Some("Friendly"), Some("Product"))),
            "Friendly"
        );
        assert_eq!(title_of(&data(Some(" "), Some("Product"))), "Product");
        assert_eq!(title_of(&data(None, None)), "svc.exe");
        for (id, kind) in [
            (101, SlowKind::App),
            (201, SlowKind::App),
            (102, SlowKind::Driver),
            (103, SlowKind::Service),
            (203, SlowKind::Service),
            (109, SlowKind::Device),
            (202, SlowKind::Device),
            (104, SlowKind::Windows),
            (110, SlowKind::Windows),
            (105, SlowKind::Prefetch),
            (106, SlowKind::Prefetch),
            (107, SlowKind::Policy),
            (108, SlowKind::Policy),
        ] {
            assert_eq!(SlowKind::of_event(id), Some(kind), "{id}");
        }
        assert_eq!(SlowKind::of_event(100), None);
        assert_eq!(SlowKind::of_event(200), None);
    }

    #[test]
    fn limits_outside_one_to_five_hundred_are_refused() {
        assert!(live_history(0).is_err());
        assert!(live_history(MAX_BOOT_LIMIT + 1).is_err());
    }
}
