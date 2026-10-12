//! Secure Boot certificates: whether Windows replaced Microsoft's 2011 Secure Boot
//! certificates in the firmware with the 2023 ones; check 15.
//!
//! Windows' servicing record in the registry and the Microsoft-Windows-TPM-WMI events in the
//! System log decide the state; both are readable without administrator rights. With them,
//! whether the Secure-Boot-Update task is turned on is read as well. Nothing here writes to
//! the registry or the task, and the firmware's variables are never read: what the firmware
//! holds is known only through Windows' record.

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDate, Utc};

use super::checkup::{
    unreadable, Check, CheckId, Severity, RECOVERY_KEY, SECURE_BOOT_EVENTS, SECURE_BOOT_GUIDANCE,
    SECURE_BOOT_MANUFACTURERS, SECURE_BOOT_PREVENTED, SECURE_BOOT_TROUBLESHOOTING,
};
use super::probe::{key_dword, key_text, CheckupRaw, Context};
use super::text::local_date;
use super::updates::{paused_until, WINDOWS_UPDATE_PAGE};
use crate::sysinfo::{clean_oem, SecureBoot};
use crate::win::event_log::{
    self, data_path, EvtValue, RenderContext, SYSTEM_EVENT_ID, SYSTEM_TIME_CREATED,
};
use crate::win::registry::{Hive, Key, RegValue};
use crate::win::task_scheduler::TaskScheduler;
use crate::Result;

/// Windows' Secure Boot key; its servicing record and the device attributes Windows recorded
/// for the rollout are subkeys.
const SECURE_BOOT_KEY: &str = r"SYSTEM\CurrentControlSet\Control\SecureBoot";
const SERVICING: &str = "Servicing";
const DEVICE_ATTRIBUTES: &str = r"Servicing\DeviceAttributes";
/// The scheduled task that installs the certificates and later Secure Boot updates.
pub(crate) const UPDATE_TASK: &str = r"\Microsoft\Windows\PI\Secure-Boot-Update";

const SYSTEM_LOG: &str = "System";
/// The servicing events by id; the provider's other ids (1038 to 1041) are attestation
/// checks logged at every start.
const SERVICING_QUERY: &str = "*[System[Provider[@Name='Microsoft-Windows-TPM-WMI'] and \
    (EventID=1032 or EventID=1036 or EventID=1043 or EventID=1044 or EventID=1045 or \
    EventID=1795 or EventID=1796 or EventID=1799 or EventID=1800 or EventID=1801 or \
    EventID=1802 or EventID=1803 or EventID=1808)]]";
/// Events read at most, newest first.
const MAX_EVENTS: usize = 256;
const BATCH: usize = 64;

/// The certificates and the boot manager are updated.
const UPDATED: u16 = 1808;
/// The certificates are available but not yet applied to the firmware.
const NOT_YET: u16 = 1801;
/// A restart is needed before the update can go on.
const RESTART: u16 = 1800;
// The steps: the boot manager signed with Windows UEFI CA 2023, a DB update, Microsoft UEFI
// CA 2023, Option ROM UEFI CA 2023 and KEK 2K CA 2023.
const BOOT_MANAGER: u16 = 1799;
const DB_UPDATE: u16 = 1036;
const UEFI_CA: u16 = 1045;
const OPTION_ROM_CA: u16 = 1044;
const KEK: u16 = 1043;
const PROGRESS: [u16; 5] = [DB_UPDATE, KEK, OPTION_ROM_CA, UEFI_CA, BOOT_MANAGER];
// Problems: the firmware returned an error, an unexpected error, a known firmware issue, no
// PK-signed KEK, and BitLocker's configuration.
const FIRMWARE_ERROR: u16 = 1795;
const UPDATE_ERROR: u16 = 1796;
const KNOWN_ISSUE: u16 = 1802;
const NO_PK_SIGNED_KEK: u16 = 1803;
const BITLOCKER: u16 = 1032;
const PROBLEMS: [u16; 5] = [
    NO_PK_SIGNED_KEK,
    KNOWN_ISSUE,
    FIRMWARE_ERROR,
    BITLOCKER,
    UPDATE_ERROR,
];
/// Every id of [`SERVICING_QUERY`].
const EVENT_IDS: [u16; 13] = [
    BITLOCKER,
    DB_UPDATE,
    KEK,
    OPTION_ROM_CA,
    UEFI_CA,
    FIRMWARE_ERROR,
    UPDATE_ERROR,
    BOOT_MANAGER,
    RESTART,
    NOT_YET,
    KNOWN_ISSUE,
    NO_PK_SIGNED_KEK,
    UPDATED,
];

/// A problem event older than this no longer counts.
const PROBLEM_DAYS: i64 = 30;
/// An install whose newest step is older than this has stalled; Microsoft estimates about
/// 48 hours.
const STALL_DAYS: i64 = 14;

// ───────────────────────────── Raw readings ─────────────────────────────

/// `UEFICA2023Status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateStatus {
    NotStarted,
    InProgress,
    Updated,
    /// Any other text, or the value of another registry type as Cairn shows values.
    Other(String),
}

/// Microsoft's assessment of the group of PCs this one belongs to (`ConfidenceLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    HighConfidence,
    Paused,
    NotSupported,
    UnderObservation,
    NoData,
}

/// The firmware Windows' servicing recorded (`Servicing\DeviceAttributes`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FirmwareId {
    /// The PC's maker, or the board's maker when the PC's is a placeholder.
    pub(crate) maker: Option<String>,
    pub(crate) version: Option<String>,
    /// `FirmwareReleaseDate` as YYYY-MM-DD when it parses, else as stored.
    pub(crate) released: Option<String>,
}

/// Windows' Secure Boot servicing record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ServicingRaw {
    pub(crate) status: Option<UpdateStatus>,
    /// `UEFICA2023Error`: the first error the update ran into, 0 after a success.
    pub(crate) error: Option<u32>,
    /// `UEFICA2023ErrorEvent`: the id of the event that reported that error.
    pub(crate) error_event: Option<u32>,
    pub(crate) bucket: Option<Bucket>,
    /// `HighConfidenceOptOut` of the Secure Boot key.
    pub(crate) opt_out: Option<u32>,
    /// `AvailableUpdatesPolicy` exists in the Secure Boot key.
    pub(crate) policy_managed: bool,
    pub(crate) firmware: FirmwareId,
}

/// The newest event of one id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServicingEvent {
    pub(crate) id: u16,
    pub(crate) at: DateTime<Utc>,
    /// `HResult` of 1795 and 1796.
    pub(crate) hresult: Option<u32>,
}

/// Whether the Secure-Boot-Update task is turned on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskRead {
    /// Not read: its access list admits administrators only.
    NotRead,
    Enabled,
    Disabled,
    Missing,
    Failed(String),
}

/// Everything the check reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertificatesRaw {
    pub(crate) servicing: std::result::Result<ServicingRaw, String>,
    /// Newest event of each id, newest first.
    pub(crate) events: std::result::Result<Vec<ServicingEvent>, String>,
    pub(crate) task: TaskRead,
}

// ───────────────────────────── Reader ─────────────────────────────

/// Reads the servicing record and events; the update task only when `elevated`.
pub(crate) fn read_certificates(elevated: bool) -> Result<CertificatesRaw> {
    let servicing = read_servicing_at(Hive::LocalMachine, SECURE_BOOT_KEY);
    let events = read_events();
    let task = if elevated {
        read_task()
    } else {
        TaskRead::NotRead
    };
    tracing::debug!(
        record = servicing.is_ok(),
        events = events.as_ref().map(Vec::len).unwrap_or(0),
        "secure boot certificates read"
    );
    Ok(CertificatesRaw {
        servicing,
        events,
        task,
    })
}

/// The servicing record below the Secure Boot key `root` of `hive`. A missing key is an
/// empty record; a key that cannot be opened or a status that cannot be read is an error.
/// The opt-out and the policy are read from the Secure Boot key itself, so they count even
/// without a servicing record.
fn read_servicing_at(hive: Hive, root: &str) -> std::result::Result<ServicingRaw, String> {
    let open = |path: &str| Key::open(hive, path, false).map_err(|e| e.to_string());
    let Some(parent) = open(root)? else {
        return Ok(ServicingRaw::default());
    };
    let servicing = open(&format!(r"{root}\{SERVICING}"))?;
    let status = match &servicing {
        Some(key) => key
            .query("UEFICA2023Status")
            .map_err(|e| e.to_string())?
            .and_then(status_of),
        None => None,
    };
    let attributes = match &servicing {
        Some(_) => open(&format!(r"{root}\{DEVICE_ATTRIBUTES}")).ok().flatten(),
        None => None,
    };
    Ok(ServicingRaw {
        status,
        error: key_dword(servicing.as_ref(), "UEFICA2023Error"),
        error_event: key_dword(servicing.as_ref(), "UEFICA2023ErrorEvent"),
        bucket: key_text(servicing.as_ref(), "ConfidenceLevel")
            .as_deref()
            .and_then(parse_bucket),
        opt_out: key_dword(Some(&parent), "HighConfidenceOptOut"),
        policy_managed: matches!(parent.value_info("AvailableUpdatesPolicy"), Ok(Some(_))),
        firmware: firmware_id(attributes.as_ref()),
    })
}

/// The status of a `UEFICA2023Status` value; an empty text is no status.
fn status_of(value: RegValue) -> Option<UpdateStatus> {
    match value {
        RegValue::Sz(text) | RegValue::ExpandSz(text) if text.trim().is_empty() => None,
        RegValue::Sz(text) | RegValue::ExpandSz(text) => Some(parse_status(&text)),
        other => Some(UpdateStatus::Other(other.display())),
    }
}

/// A status token, compared ASCII case-insensitively after trimming.
pub(crate) fn parse_status(text: &str) -> UpdateStatus {
    let text = text.trim();
    if text.eq_ignore_ascii_case("NotStarted") {
        UpdateStatus::NotStarted
    } else if text.eq_ignore_ascii_case("InProgress") {
        UpdateStatus::InProgress
    } else if text.eq_ignore_ascii_case("Updated") {
        UpdateStatus::Updated
    } else {
        UpdateStatus::Other(text.to_string())
    }
}

/// The bucket a `ConfidenceLevel` text starts with, ignoring case; the dash in "Not
/// Supported – Known Limitation" may be any character. Any other text is no bucket.
pub(crate) fn parse_bucket(text: &str) -> Option<Bucket> {
    const PREFIXES: [(&str, Bucket); 5] = [
        ("high confidence", Bucket::HighConfidence),
        ("temporarily paused", Bucket::Paused),
        ("not supported", Bucket::NotSupported),
        ("under observation", Bucket::UnderObservation),
        ("no data observed", Bucket::NoData),
    ];
    let lower = text.trim().to_lowercase();
    PREFIXES
        .iter()
        .find(|(prefix, _)| lower.starts_with(prefix))
        .map(|(_, bucket)| *bucket)
}

/// `FirmwareReleaseDate` (MM/DD/YYYY) as YYYY-MM-DD, or the trimmed text when it does not
/// parse.
pub(crate) fn parse_release_date(text: &str) -> String {
    let text = text.trim();
    NaiveDate::parse_from_str(text, "%m/%d/%Y")
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| text.to_string())
}

/// The firmware of the device attributes; a placeholder maker is replaced by the board's.
fn firmware_id(attributes: Option<&Key>) -> FirmwareId {
    let text = |name: &str| key_text(attributes, name).as_deref().and_then(clean_oem);
    FirmwareId {
        maker: text("OEMManufacturerName").or_else(|| text("BaseBoardManufacturer")),
        version: text("FirmwareVersion"),
        released: key_text(attributes, "FirmwareReleaseDate")
            .filter(|date| !date.is_empty())
            .map(|date| parse_release_date(&date)),
    }
}

/// The newest servicing event of each id, newest first; an error when the System log cannot
/// be read.
fn read_events() -> std::result::Result<Vec<ServicingEvent>, String> {
    let text = |e: crate::Error| e.to_string();
    let mut query =
        event_log::query(SYSTEM_LOG, SERVICING_QUERY, true).map_err(|e| e.to_string())?;
    let system = RenderContext::system().map_err(text)?;
    let codes = RenderContext::values(&[&data_path("HResult")]).map_err(text)?;
    let mut events = Vec::new();
    let mut seen: HashSet<u16> = HashSet::new();
    let mut read = 0;
    // Stops early once every id was seen.
    while read < MAX_EVENTS && seen.len() < EVENT_IDS.len() {
        let batch = query.next_batch(BATCH).map_err(text)?;
        if batch.is_empty() {
            break;
        }
        read += batch.len();
        for event in &batch {
            let values = system.render(event).map_err(text)?;
            let id = values
                .get(SYSTEM_EVENT_ID)
                .and_then(EvtValue::as_u64)
                .and_then(|id| u16::try_from(id).ok());
            let at = values.get(SYSTEM_TIME_CREATED).and_then(EvtValue::as_time);
            let (Some(id), Some(at)) = (id, at) else {
                continue;
            };
            if !EVENT_IDS.contains(&id) {
                continue;
            }
            // Only the newest event of an id is kept, so only its code is rendered.
            let hresult = if seen.insert(id) && matches!(id, FIRMWARE_ERROR | UPDATE_ERROR) {
                codes
                    .render(event)
                    .ok()
                    .and_then(|values| values.first().and_then(hresult_of))
            } else {
                None
            };
            events.push(ServicingEvent { id, at, hresult });
        }
    }
    Ok(newest_per_id(events))
}

/// An `HResult` field: `win:Int32` in the provider's templates, shown as unsigned hex.
fn hresult_of(value: &EvtValue) -> Option<u32> {
    match value {
        EvtValue::Int(code) => i32::try_from(*code).ok().map(|code| code as u32),
        EvtValue::UInt(code) => u32::try_from(*code).ok(),
        _ => None,
    }
}

/// The first event of each id in `events`, which come newest first; order is kept.
pub(crate) fn newest_per_id(events: Vec<ServicingEvent>) -> Vec<ServicingEvent> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .filter(|event| seen.insert(event.id))
        .collect()
}

/// Whether the update task is turned on; needs administrator rights.
fn read_task() -> TaskRead {
    let scheduler = match TaskScheduler::connect() {
        Ok(scheduler) => scheduler,
        Err(e) => return TaskRead::Failed(e.to_string()),
    };
    match scheduler.task(UPDATE_TASK) {
        Ok(None) => TaskRead::Missing,
        Ok(Some(task)) => match task.enabled() {
            Ok(true) => TaskRead::Enabled,
            Ok(false) => TaskRead::Disabled,
            Err(e) => TaskRead::Failed(e.to_string()),
        },
        Err(e) => TaskRead::Failed(e.to_string()),
    }
}

// ───────────────────────────── Rules ─────────────────────────────

/// Why the state is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnknownReason {
    /// The record or the log could not be read, and the other has no status.
    Unreadable(String),
    UnknownStatus(String),
    /// Secure Boot's state is unknown and Windows has no record.
    NoRecord,
}

/// Why the firmware or its maker has to help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirmwareReason {
    NotSupported,
    KnownIssue,
    NoPkSignedKek,
}

/// The result of the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    LegacyBios,
    SecureBootOff {
        installed: bool,
    },
    Unknown(UnknownReason),
    Updated,
    UpdatedTaskOff {
        missing: bool,
    },
    TaskOff {
        missing: bool,
    },
    OptedOut,
    NeedsFirmware(FirmwareReason),
    Paused,
    FirmwareError {
        hresult: Option<u32>,
    },
    BitLockerHold,
    RetryAfterError {
        hresult: Option<u32>,
    },
    /// In progress, but its newest step is old or no longer in the log.
    Stalled {
        since: Option<DateTime<Utc>>,
    },
    RestartNeeded,
    InProgress,
    WaitingForData,
    NoData,
    /// Not started yet: the status or a 1801 says so, or servicing events were logged but no
    /// status was recorded.
    NotStarted,
    /// Secure Boot is on, and Windows recorded neither a status nor an event.
    NoRecordYet,
}

/// A recent problem: the newest event of a problem id, or the error Windows recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) id: u16,
    pub(crate) hresult: Option<u32>,
}

/// Events that tell how far the update got: 1800, 1801, 1808 and the steps.
fn is_state(id: u16) -> bool {
    matches!(id, RESTART | NOT_YET | UPDATED) || PROGRESS.contains(&id)
}

/// Time of the newest event of one of `ids`.
fn newest_of(events: &[ServicingEvent], ids: &[u16]) -> Option<DateTime<Utc>> {
    events
        .iter()
        .filter(|event| ids.contains(&event.id))
        .map(|event| event.at)
        .max()
}

/// The newest of 1800, 1801, 1808 and the steps.
pub(crate) fn newest_state_event(events: &[ServicingEvent]) -> Option<&ServicingEvent> {
    events
        .iter()
        .filter(|event| is_state(event.id))
        .max_by_key(|event| event.at)
}

/// The status Windows recorded; without one, what the events show: a 1808 that no 1801
/// follows is Updated, a step newer than the newest 1801 is InProgress, and a 1801 alone is
/// NotStarted.
pub(crate) fn effective_status(
    recorded: Option<&UpdateStatus>,
    events: &[ServicingEvent],
) -> Option<UpdateStatus> {
    if let Some(status) = recorded {
        return Some(status.clone());
    }
    let updated = newest_of(events, &[UPDATED]);
    let not_yet = newest_of(events, &[NOT_YET]);
    let progress = newest_of(events, &PROGRESS);
    match (updated, not_yet, progress) {
        (Some(done), not_yet, _) if not_yet.map_or(true, |n| n <= done) => {
            Some(UpdateStatus::Updated)
        }
        (_, not_yet, Some(step)) if not_yet.map_or(true, |n| step > n) => {
            Some(UpdateStatus::InProgress)
        }
        (_, Some(_), _) => Some(UpdateStatus::NotStarted),
        _ => None,
    }
}

/// The problems that still count: the newest event of each problem id that is at most
/// `PROBLEM_DAYS` old and newer than the newest 1808 and the newest step. The error Windows
/// recorded stands in for an event that is no longer in the log, unless an event shows the
/// update moved on since.
pub(crate) fn recent_problems(raw: &CertificatesRaw, now: DateTime<Utc>) -> Vec<Problem> {
    let events = raw.events.as_deref().unwrap_or(&[]);
    let moved_on = newest_of(events, &[UPDATED]).max(newest_of(events, &PROGRESS));
    let mut problems: Vec<Problem> = events
        .iter()
        .filter(|event| PROBLEMS.contains(&event.id))
        .filter(|event| now - event.at <= Duration::days(PROBLEM_DAYS))
        .filter(|event| moved_on.map_or(true, |at| event.at > at))
        .map(|event| Problem {
            id: event.id,
            hresult: event.hresult,
        })
        .collect();
    let Ok(servicing) = &raw.servicing else {
        return problems;
    };
    let recorded = servicing
        .error_event
        .and_then(|id| u16::try_from(id).ok())
        .filter(|id| PROBLEMS.contains(id));
    if let Some(id) = recorded {
        let in_log = events.iter().any(|event| event.id == id);
        if !in_log && moved_on.is_none() && servicing.error != Some(0) {
            let hresult = servicing
                .error
                .filter(|_| matches!(id, FIRMWARE_ERROR | UPDATE_ERROR));
            problems.push(Problem { id, hresult });
        }
    }
    problems
}

/// When an install Windows records as in progress stopped moving: `Some(Some(time))` of its
/// newest step or state event other than 1801 when that is older than `STALL_DAYS`,
/// `Some(None)` when none is left in the log, `None` while it moves or the log cannot be
/// read.
fn stalled(raw: &CertificatesRaw, now: DateTime<Utc>) -> Option<Option<DateTime<Utc>>> {
    let events = raw.events.as_ref().ok()?;
    let newest = events
        .iter()
        .filter(|event| event.id != NOT_YET && is_state(event.id))
        .map(|event| event.at)
        .max();
    match newest {
        None => Some(None),
        Some(at) if now - at > Duration::days(STALL_DAYS) => Some(Some(at)),
        Some(_) => None,
    }
}

/// The verdict; the first rule that applies wins. Pure.
pub(crate) fn verdict(
    raw: &CertificatesRaw,
    secure_boot: SecureBoot,
    now: DateTime<Utc>,
) -> Verdict {
    let servicing = raw.servicing.as_ref().ok();
    let events = raw.events.as_deref().unwrap_or(&[]);
    let status = effective_status(servicing.and_then(|s| s.status.as_ref()), events);
    match secure_boot {
        SecureBoot::Unsupported => return Verdict::LegacyBios,
        SecureBoot::Off => {
            return Verdict::SecureBootOff {
                installed: status == Some(UpdateStatus::Updated),
            }
        }
        SecureBoot::On | SecureBoot::Unknown => {}
    }
    if status.is_none() {
        // The record's error first: it holds the status.
        if let Err(error) = &raw.servicing {
            return Verdict::Unknown(UnknownReason::Unreadable(error.clone()));
        }
        if let Err(error) = &raw.events {
            return Verdict::Unknown(UnknownReason::Unreadable(error.clone()));
        }
    }
    if let Some(UpdateStatus::Other(text)) = &status {
        return Verdict::Unknown(UnknownReason::UnknownStatus(text.clone()));
    }
    let task_off = match raw.task {
        TaskRead::Disabled => Some(false),
        TaskRead::Missing => Some(true),
        TaskRead::NotRead | TaskRead::Enabled | TaskRead::Failed(_) => None,
    };
    if status == Some(UpdateStatus::Updated) {
        return match task_off {
            Some(missing) => Verdict::UpdatedTaskOff { missing },
            None => Verdict::Updated,
        };
    }
    if secure_boot == SecureBoot::Unknown && status.is_none() {
        return Verdict::Unknown(UnknownReason::NoRecord);
    }
    if let Some(missing) = task_off {
        return Verdict::TaskOff { missing };
    }
    if servicing.and_then(|s| s.opt_out).is_some_and(|v| v != 0) {
        return Verdict::OptedOut;
    }
    let bucket = servicing.and_then(|s| s.bucket);
    let problems = recent_problems(raw, now);
    let problem = |id: u16| problems.iter().find(|p| p.id == id);
    if problem(NO_PK_SIGNED_KEK).is_some() {
        return Verdict::NeedsFirmware(FirmwareReason::NoPkSignedKek);
    }
    // A paused group is Microsoft's own hold; a 1802 is then probably how it shows.
    if bucket != Some(Bucket::Paused) && problem(KNOWN_ISSUE).is_some() {
        return Verdict::NeedsFirmware(FirmwareReason::KnownIssue);
    }
    if bucket == Some(Bucket::NotSupported) {
        return Verdict::NeedsFirmware(FirmwareReason::NotSupported);
    }
    if bucket == Some(Bucket::Paused) {
        return Verdict::Paused;
    }
    if let Some(p) = problem(FIRMWARE_ERROR) {
        return Verdict::FirmwareError { hresult: p.hresult };
    }
    if problem(BITLOCKER).is_some() {
        return Verdict::BitLockerHold;
    }
    if let Some(p) = problem(UPDATE_ERROR) {
        return Verdict::RetryAfterError { hresult: p.hresult };
    }
    if status == Some(UpdateStatus::InProgress) {
        if let Some(since) = stalled(raw, now) {
            return Verdict::Stalled { since };
        }
    }
    if newest_state_event(events).map(|event| event.id) == Some(RESTART) {
        return Verdict::RestartNeeded;
    }
    if status == Some(UpdateStatus::InProgress) {
        return Verdict::InProgress;
    }
    // Only NotStarted or no status is left; without a status a readable log decides, since
    // an unreadable one was handled above.
    match bucket {
        Some(Bucket::UnderObservation) => Verdict::WaitingForData,
        Some(Bucket::NoData) => Verdict::NoData,
        _ if status.is_none() && events.is_empty() => Verdict::NoRecordYet,
        _ => Verdict::NotStarted,
    }
}

// ───────────────────────────── Texts ─────────────────────────────

/// What the check is about; every detail starts with it.
const DETAIL: &str = "Secure Boot decides what may start before Windows by checking Microsoft \
    certificates stored in the PC's firmware. The original certificates from 2011 are valid \
    only until 2026, so Windows Update replaces them with certificates from 2023. Without the \
    new ones the PC still starts and updates as usual, but it gets no new protections for its \
    startup.";

const SUMMARY_UPDATED: &str = "The 2023 certificates are installed";
const SUMMARY_UPDATED_TASK_OFF: &str =
    "Installed, but the task for Secure Boot updates is turned off";
const SUMMARY_UPDATED_TASK_MISSING: &str =
    "Installed, but the task for Secure Boot updates is missing";
const SUMMARY_TASK_OFF: &str = "The task that installs the 2023 certificates is turned off";
const SUMMARY_TASK_MISSING: &str = "The task that installs the 2023 certificates is missing";
const SUMMARY_OPTED_OUT: &str = "Automatic installation is turned off on this PC";
const SUMMARY_NEEDS_FIRMWARE: &str = "Needs a firmware update or help from the PC's manufacturer";
const SUMMARY_PAUSED: &str = "Paused by Microsoft for PCs like this one";
const SUMMARY_FIRMWARE_ERROR: &str = "The firmware did not accept the update";
const SUMMARY_BITLOCKER: &str = "Held back because of the BitLocker setup";
const SUMMARY_RETRY: &str = "The update stopped with an error; Windows tries again";
const SUMMARY_STALLED: &str =
    "Windows started installing the 2023 certificates but has not finished";
const SUMMARY_RESTART: &str =
    "Windows needs a restart to continue installing the 2023 certificates";
const SUMMARY_IN_PROGRESS: &str = "Installing the 2023 certificates…";
const SUMMARY_WAITING: &str = "Not installed yet: Microsoft is still checking PCs like this one";
const SUMMARY_NO_DATA: &str = "Not installed yet: Microsoft has no data on PCs like this one";
const SUMMARY_NOT_STARTED: &str = "Windows has not installed the 2023 certificates yet";
const SUMMARY_NO_RECORD_YET: &str = "Windows has no record of the certificate update yet";
const SUMMARY_OFF_NOT_INSTALLED: &str = "Not installed while Secure Boot is off";
const SUMMARY_OFF_INSTALLED: &str = "Installed; not in use while Secure Boot is off";
const SUMMARY_LEGACY_BIOS: &str = "Not used: Windows starts in legacy BIOS mode";
const SUMMARY_NO_RECORD: &str = "No record of the certificate update was found";

const TASK_LATER: &str = "Windows uses the scheduled task \
    \\Microsoft\\Windows\\PI\\Secure-Boot-Update for later Secure Boot updates too.";
const TASK_INSTALLS: &str = "The scheduled task \\Microsoft\\Windows\\PI\\Secure-Boot-Update \
    installs the certificates.";
const TASK_OFF_BLOCKS: &str = "While it is turned off, Windows cannot finish the update.";
const TASK_MISSING_BLOCKS: &str = "While it is missing, Windows cannot finish the update.";
const TASK_TURN_ON: &str = "Cairn does not change the task; it can be turned back on in Task \
    Scheduler, and Microsoft's troubleshooting guide shows how.";
const TASK_RESTORE: &str =
    "Cairn does not change the task; Microsoft's troubleshooting guide covers a missing task.";
const OPTED_OUT: &str = "The setting HighConfidenceOptOut in the Secure Boot registry key \
    keeps Windows Update from installing them automatically.";
const OPTED_OUT_ORGANIZATIONS: &str = "Organizations use it to roll out the update themselves.";
const OPTED_OUT_CAIRN: &str = "Cairn does not change it.";
const NOT_SUPPORTED: &str = "Microsoft says PCs like this one cannot take the automatic update \
    because of hardware or firmware limitations.";
const KNOWN_FIRMWARE_ISSUE: &str =
    "Windows held the update back because of a known problem with this firmware.";
const NO_MANUFACTURER_KEY: &str = "The firmware lacks a key from its manufacturer that Windows \
    needs to update the certificate list.";
const NEWER_FIRMWARE: &str = "A newer firmware (BIOS/UEFI) version from the manufacturer may \
    fix this; Microsoft lists the Secure Boot pages of many manufacturers.";
const OLDER_MODELS: &str = "Older models may not get one.";
const KEEP_SECURE_BOOT: &str =
    "Do not turn off Secure Boot to work around it: that removes its protection.";
const PAUSED: &str =
    "Microsoft has paused the update for this group of PCs because of a known problem.";
const PAUSED_FIRMWARE: &str = "A firmware update from the manufacturer may be needed; Windows \
    continues by itself once the problem is solved.";
const FIRMWARE_REFUSED: &str =
    "When Windows tried to add the certificates, the firmware returned an error.";
const FIRMWARE_RETRY: &str = "Windows tries again after restarts, and a firmware update from \
    the manufacturer may fix it.";
const BITLOCKER_HOLD: &str = "Windows did not install the update because, with this PC's \
    BitLocker setup, the next start would ask for the recovery key.";
const BITLOCKER_STEPS: &str = "Microsoft's steps explain how to pause BitLocker for two \
    restarts so the update can finish.";
const RETRY: &str = "Windows ran into an unexpected error and tries again after the next restart.";
const STALLED: &str = "Microsoft estimates about 48 hours and one or more restarts, so the \
    update seems stuck.";
const STALLED_GUIDE: &str = "Microsoft's troubleshooting guide lists what can stop it.";
const IN_PROGRESS: &str = "Microsoft estimates about 48 hours and one or more restarts; nothing \
    needs to be done.";
const RESTART_CONTINUES: &str = "Windows continues after the next restart; Microsoft estimates \
    about 48 hours and one or more restarts in all.";
const BITLOCKER_ONCE: &str = "On some PCs BitLocker asks for the recovery key once, at the \
    first start after the update; enter it and Windows starts normally.";
const RECOVERY_KEY_PLACE: &str = "For device encryption with a Microsoft account, the recovery \
    key is at aka.ms/myrecoverykey.";
const RESTART_NOT_SHUT_DOWN: &str = "Use Restart, not Shut down.";
const WAITING: &str = "Windows Update installs them by itself once Microsoft has enough data \
    to show that PCs like this one take the update safely.";
const WAITING_OPTIONS: &str =
    "Keep Windows Update turned on; Microsoft's guidance explains the options.";
const NO_DATA: &str = "Windows may not install them automatically.";
const NO_DATA_OPTIONS: &str = "Microsoft's guidance explains the options; a firmware update \
    from the manufacturer can also add them.";
const BY_ITSELF: &str = "Windows Update installs them by itself.";
const KEEP_UPDATING: &str = "Keep Windows Update turned on, install the latest updates and \
    restart when Windows asks.";
const EVENT_NOT_YET: &str = "Event 1801 in the System log means the same: not finished yet.";
const AFTER_EXPIRY: &str =
    "Windows can still install them after the 2011 certificates have expired.";
const RED_BADGE: &str = "If Windows Security shows a red badge, Microsoft has released a \
    startup fix that this PC cannot receive until the update is done.";
const NO_RECORD_YET: &str = "Windows writes this record when its Secure Boot task runs, and a \
    PC that came with the 2023 certificates may not have one.";
const SECOND_OPINION: &str =
    "Windows Security's Device security page shows the status Windows itself sees.";
const SECURE_BOOT_OFF: &str = "Windows installs these certificates only while Secure Boot is \
    on; see the Secure Boot check.";
const LEGACY_BIOS: &str =
    "Secure Boot and its certificates exist only when Windows starts in UEFI mode.";

const LATEST_EVENT_LABEL: &str = "Latest Secure Boot update event";
const ERROR_LABEL: &str = "Error recorded by Windows";
const FIRMWARE_LABEL: &str = "Firmware";
const TASK_LABEL: &str = "Update task";
const SETTING_LABEL: &str = "Setting";
const MANAGED_LINE: &str = "Managed by your organization through Group Policy or Intune.";
const ADMIN_HINT: &str = "Run Cairn as administrator to also check the update task.";

/// What a fix button opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixSpec {
    /// A settings or Windows Security page.
    Page(&'static str, &'static str),
    /// A Microsoft web page.
    Link(&'static str, &'static str),
    /// A built-in Windows tool by id.
    Tool(&'static str, &'static str),
}

const DEVICE_SECURITY: FixSpec =
    FixSpec::Page("Open Device security", "windowsdefender://devicesecurity/");
const WINDOWS_UPDATE: FixSpec = FixSpec::Page("Open Windows Update", WINDOWS_UPDATE_PAGE);
const TASK_SCHEDULER: FixSpec = FixSpec::Tool("Open Task Scheduler", "task_scheduler");
const GUIDANCE: FixSpec = FixSpec::Link("Microsoft's guidance", SECURE_BOOT_GUIDANCE);
const PREVENTED_GUIDANCE: FixSpec = FixSpec::Link("Microsoft's guidance", SECURE_BOOT_PREVENTED);
const MANUFACTURER_PAGES: FixSpec = FixSpec::Link("Manufacturer pages", SECURE_BOOT_MANUFACTURERS);
const TROUBLESHOOTING_GUIDE: FixSpec =
    FixSpec::Link("Troubleshooting guide", SECURE_BOOT_TROUBLESHOOTING);
const BITLOCKER_STEPS_PAGE: FixSpec = FixSpec::Link("Microsoft's steps", SECURE_BOOT_EVENTS);
const RECOVERY_KEY_PAGE: FixSpec = FixSpec::Link("Find the recovery key", RECOVERY_KEY);

/// The state a verdict gives the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Good,
    Attention(Severity),
    Unknown,
    NotApplicable,
}

/// How a verdict reads: its outcome, its summary, the sentences after [`DETAIL`] and its
/// fix buttons, the first one the main action.
#[derive(Debug)]
struct Shown {
    outcome: Outcome,
    summary: String,
    sentences: Vec<&'static str>,
    fixes: Vec<FixSpec>,
}

impl Shown {
    fn new(
        outcome: Outcome,
        summary: impl Into<String>,
        sentences: &[&'static str],
        fixes: &[FixSpec],
    ) -> Shown {
        Shown {
            outcome,
            summary: summary.into(),
            sentences: sentences.to_vec(),
            fixes: fixes.to_vec(),
        }
    }
}

/// The summary of an install that stopped moving.
fn stalled_summary(since: Option<DateTime<Utc>>) -> String {
    match since {
        Some(at) => format!(
            "Windows started installing the 2023 certificates but has not finished since {}",
            local_date(at)
        ),
        None => SUMMARY_STALLED.to_string(),
    }
}

/// The texts and fixes of `verdict`; `fast_startup` adds the advice to restart.
fn shown(verdict: &Verdict, fast_startup: bool) -> Shown {
    use Severity::{Info, Low, Medium};
    let attention = Outcome::Attention;
    match verdict {
        Verdict::LegacyBios => Shown::new(
            Outcome::NotApplicable,
            SUMMARY_LEGACY_BIOS,
            &[LEGACY_BIOS],
            &[],
        ),
        Verdict::SecureBootOff { installed } => Shown::new(
            Outcome::NotApplicable,
            if *installed {
                SUMMARY_OFF_INSTALLED
            } else {
                SUMMARY_OFF_NOT_INSTALLED
            },
            &[SECURE_BOOT_OFF],
            &[DEVICE_SECURITY],
        ),
        Verdict::Unknown(reason) => Shown::new(
            Outcome::Unknown,
            match reason {
                UnknownReason::Unreadable(error) => {
                    format!("Could not read the certificate update status: {error}")
                }
                UnknownReason::UnknownStatus(text) => {
                    format!("Windows reports a status Cairn does not know: \"{text}\"")
                }
                UnknownReason::NoRecord => SUMMARY_NO_RECORD.to_string(),
            },
            &[],
            &[DEVICE_SECURITY, GUIDANCE],
        ),
        Verdict::Updated => Shown::new(Outcome::Good, SUMMARY_UPDATED, &[], &[DEVICE_SECURITY]),
        Verdict::UpdatedTaskOff { missing } => Shown::new(
            attention(Low),
            if *missing {
                SUMMARY_UPDATED_TASK_MISSING
            } else {
                SUMMARY_UPDATED_TASK_OFF
            },
            &[
                TASK_LATER,
                if *missing { TASK_RESTORE } else { TASK_TURN_ON },
            ],
            &[TASK_SCHEDULER, TROUBLESHOOTING_GUIDE],
        ),
        Verdict::TaskOff { missing } => Shown::new(
            attention(Medium),
            if *missing {
                SUMMARY_TASK_MISSING
            } else {
                SUMMARY_TASK_OFF
            },
            &[
                TASK_INSTALLS,
                if *missing {
                    TASK_MISSING_BLOCKS
                } else {
                    TASK_OFF_BLOCKS
                },
                if *missing { TASK_RESTORE } else { TASK_TURN_ON },
            ],
            &[TASK_SCHEDULER, TROUBLESHOOTING_GUIDE],
        ),
        Verdict::OptedOut => Shown::new(
            attention(Medium),
            SUMMARY_OPTED_OUT,
            &[OPTED_OUT, OPTED_OUT_ORGANIZATIONS, OPTED_OUT_CAIRN],
            &[GUIDANCE],
        ),
        Verdict::NeedsFirmware(reason) => Shown::new(
            attention(Medium),
            SUMMARY_NEEDS_FIRMWARE,
            &[
                match reason {
                    FirmwareReason::NotSupported => NOT_SUPPORTED,
                    FirmwareReason::KnownIssue => KNOWN_FIRMWARE_ISSUE,
                    FirmwareReason::NoPkSignedKek => NO_MANUFACTURER_KEY,
                },
                NEWER_FIRMWARE,
                OLDER_MODELS,
                KEEP_SECURE_BOOT,
            ],
            &[MANUFACTURER_PAGES, PREVENTED_GUIDANCE, DEVICE_SECURITY],
        ),
        Verdict::Paused => Shown::new(
            attention(Info),
            SUMMARY_PAUSED,
            &[PAUSED, PAUSED_FIRMWARE],
            &[MANUFACTURER_PAGES, PREVENTED_GUIDANCE, DEVICE_SECURITY],
        ),
        Verdict::FirmwareError { .. } => Shown::new(
            attention(Medium),
            SUMMARY_FIRMWARE_ERROR,
            &[FIRMWARE_REFUSED, FIRMWARE_RETRY, KEEP_SECURE_BOOT],
            &[MANUFACTURER_PAGES, TROUBLESHOOTING_GUIDE],
        ),
        Verdict::BitLockerHold => Shown::new(
            attention(Medium),
            SUMMARY_BITLOCKER,
            &[BITLOCKER_HOLD, BITLOCKER_STEPS],
            &[BITLOCKER_STEPS_PAGE, TROUBLESHOOTING_GUIDE],
        ),
        Verdict::RetryAfterError { .. } => Shown::new(
            attention(Low),
            SUMMARY_RETRY,
            &[RETRY],
            &[WINDOWS_UPDATE, TROUBLESHOOTING_GUIDE],
        ),
        Verdict::Stalled { since } => Shown::new(
            attention(Low),
            stalled_summary(*since),
            &[STALLED, STALLED_GUIDE],
            &[TROUBLESHOOTING_GUIDE, WINDOWS_UPDATE],
        ),
        Verdict::RestartNeeded | Verdict::InProgress => {
            let (summary, first) = if *verdict == Verdict::RestartNeeded {
                (SUMMARY_RESTART, RESTART_CONTINUES)
            } else {
                (SUMMARY_IN_PROGRESS, IN_PROGRESS)
            };
            let mut shown = Shown::new(
                attention(Info),
                summary,
                &[first, BITLOCKER_ONCE, RECOVERY_KEY_PLACE],
                &[WINDOWS_UPDATE, DEVICE_SECURITY, RECOVERY_KEY_PAGE],
            );
            if fast_startup {
                shown.sentences.push(RESTART_NOT_SHUT_DOWN);
            }
            shown
        }
        Verdict::WaitingForData => Shown::new(
            attention(Low),
            SUMMARY_WAITING,
            &[WAITING, WAITING_OPTIONS, AFTER_EXPIRY, RED_BADGE],
            &[GUIDANCE, WINDOWS_UPDATE],
        ),
        Verdict::NoData => Shown::new(
            attention(Low),
            SUMMARY_NO_DATA,
            &[NO_DATA, NO_DATA_OPTIONS, AFTER_EXPIRY, RED_BADGE],
            &[GUIDANCE, MANUFACTURER_PAGES],
        ),
        Verdict::NotStarted => Shown::new(
            attention(Low),
            SUMMARY_NOT_STARTED,
            &[
                BY_ITSELF,
                KEEP_UPDATING,
                EVENT_NOT_YET,
                AFTER_EXPIRY,
                RED_BADGE,
            ],
            &[WINDOWS_UPDATE, DEVICE_SECURITY, GUIDANCE],
        ),
        Verdict::NoRecordYet => Shown::new(
            attention(Low),
            SUMMARY_NO_RECORD_YET,
            &[NO_RECORD_YET, SECOND_OPINION, KEEP_UPDATING],
            &[DEVICE_SECURITY, WINDOWS_UPDATE, GUIDANCE],
        ),
    }
}

/// [`DETAIL`] followed by `sentences`, joined with one space.
fn detail(sentences: &[&str]) -> String {
    let mut text = DETAIL.to_string();
    for sentence in sentences {
        text.push(' ');
        text.push_str(sentence);
    }
    text
}

/// The phrase of the latest-event fact; `None` for an id the check does not read.
pub(crate) fn event_phrase(event: &ServicingEvent) -> Option<String> {
    let phrase = match event.id {
        UPDATED => "all certificates updated",
        NOT_YET => "not finished yet",
        RESTART => "a restart is needed to continue",
        BOOT_MANAGER => "boot manager signed with the 2023 certificate installed",
        DB_UPDATE => "a Secure Boot database update was applied",
        UEFI_CA => "Microsoft UEFI CA 2023 added to the firmware",
        OPTION_ROM_CA => "Option ROM UEFI CA 2023 added to the firmware",
        KEK => "KEK 2K CA 2023 added to the firmware",
        FIRMWARE_ERROR => {
            return Some(match event.hresult {
                Some(code) => format!("the firmware returned error 0x{code:08X}"),
                None => "the firmware returned an error".to_string(),
            })
        }
        UPDATE_ERROR => {
            return Some(match event.hresult {
                Some(code) => format!("the update failed with error 0x{code:08X}"),
                None => "the update failed".to_string(),
            })
        }
        KNOWN_ISSUE => "held back because of a known firmware problem",
        NO_PK_SIGNED_KEK => "the firmware lacks a key from its manufacturer",
        BITLOCKER => "held back because of the BitLocker setup",
        _ => return None,
    };
    Some(phrase.to_string())
}

/// "2026-09-26  ·  all certificates updated (event 1808)" for the newest event.
fn latest_event_text(events: &[ServicingEvent]) -> Option<String> {
    let event = events
        .iter()
        .filter(|event| EVENT_IDS.contains(&event.id))
        .max_by_key(|event| event.at)?;
    let phrase = event_phrase(event)?;
    Some(format!(
        "{}  ·  {phrase} (event {})",
        local_date(event.at),
        event.id
    ))
}

/// "Contoso 1.0.7, released 2025-01-15"; a missing part is left out.
pub(crate) fn firmware_text(firmware: &FirmwareId) -> Option<String> {
    let head = [firmware.maker.as_deref(), firmware.version.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    match (&firmware.released, head.is_empty()) {
        (Some(date), true) => Some(format!("released {date}")),
        (Some(date), false) => Some(format!("{head}, released {date}")),
        (None, false) => Some(head),
        (None, true) => None,
    }
}

/// Check 15: the 2023 Secure Boot certificates are in the firmware.
pub(crate) fn secure_boot_certificates(
    raw: &CheckupRaw,
    ctx: &Context,
    now: DateTime<Utc>,
) -> Check {
    let certificates = match &raw.certificates {
        Ok(certificates) => certificates,
        Err(e) => {
            return add_fix(
                unreadable(
                    CheckId::SecureBootCertificates,
                    DETAIL,
                    "the certificate update status",
                    e,
                ),
                DEVICE_SECURITY,
            )
        }
    };
    let secure_boot = raw
        .device
        .as_ref()
        .map(|device| device.secure_boot)
        .unwrap_or(SecureBoot::Unknown);
    let verdict = verdict(certificates, secure_boot, now);
    let fast_startup = raw.os.as_ref().ok().and_then(|os| os.fast_startup) == Some(true);
    let mut shown = shown(&verdict, fast_startup);
    let applicable = shown.outcome != Outcome::NotApplicable;
    let attention = matches!(shown.outcome, Outcome::Attention(_));
    let updated = matches!(verdict, Verdict::Updated | Verdict::UpdatedTaskOff { .. });
    let servicing = certificates.servicing.as_ref().ok();
    let events = certificates.events.as_deref().unwrap_or(&[]);

    let mut check = Check::new(CheckId::SecureBootCertificates, detail(&shown.sentences));
    if applicable {
        if let Some(text) = latest_event_text(events) {
            check = check.fact(LATEST_EVENT_LABEL, text);
        }
    }
    // The error Windows recorded in the registry, else the code of the problem event that
    // decided the verdict.
    let recorded = servicing
        .and_then(|s| s.error)
        .filter(|e| *e != 0)
        .map(|error| match servicing.and_then(|s| s.error_event) {
            Some(event) => format!("0x{error:08X} (event {event})"),
            None => format!("0x{error:08X}"),
        });
    let logged = match verdict {
        Verdict::FirmwareError {
            hresult: Some(code),
        } => Some(format!("0x{code:08X} (event {FIRMWARE_ERROR})")),
        Verdict::RetryAfterError {
            hresult: Some(code),
        } => Some(format!("0x{code:08X} (event {UPDATE_ERROR})")),
        _ => None,
    };
    if applicable && !updated {
        if let Some(value) = recorded.or(logged) {
            check = check.fact(ERROR_LABEL, value);
        }
    }
    let manufacturer = matches!(
        verdict,
        Verdict::NeedsFirmware(_)
            | Verdict::FirmwareError { .. }
            | Verdict::Paused
            | Verdict::NoData
    );
    if manufacturer {
        if let Some(text) = servicing.and_then(|s| firmware_text(&s.firmware)) {
            check = check.fact(FIRMWARE_LABEL, text);
        }
    }
    match (&verdict, &certificates.task) {
        (Verdict::UpdatedTaskOff { missing } | Verdict::TaskOff { missing }, _) => {
            check = check.fact(TASK_LABEL, if *missing { "missing" } else { "turned off" });
        }
        (_, TaskRead::Failed(error)) if applicable && !updated => {
            check = check.fact(TASK_LABEL, format!("could not be read: {error}"));
        }
        _ => {}
    }
    if verdict == Verdict::OptedOut {
        if let Some(value) = servicing.and_then(|s| s.opt_out) {
            check = check.fact(SETTING_LABEL, format!("HighConfidenceOptOut = {value}"));
        }
    }
    // Microsoft lists an unpaused Windows Update among the update's conditions, so a pause is
    // shown on every applicable result whose status is not Updated.
    let paused = raw
        .windows_update
        .as_ref()
        .ok()
        .and_then(|wu| paused_until(&wu.pause_values, now).0);
    if let Some(until) = paused.filter(|_| applicable && !updated) {
        check = check.line(format!(
            "Windows Update is paused until {}",
            local_date(until)
        ));
        shown.fixes.retain(|fix| *fix != WINDOWS_UPDATE);
        shown.fixes.insert(0, WINDOWS_UPDATE);
    }
    if applicable && servicing.is_some_and(|s| s.policy_managed) {
        check = check.line(MANAGED_LINE);
    }
    if !ctx.elevated && (attention || shown.outcome == Outcome::Unknown) {
        check = check.line(ADMIN_HINT);
    }
    for fix in &shown.fixes {
        check = add_fix(check, *fix);
    }
    match shown.outcome {
        Outcome::Good => check.good(shown.summary),
        Outcome::Attention(severity) => check.attention(severity, shown.summary),
        Outcome::Unknown => check.unknown(shown.summary),
        Outcome::NotApplicable => check.not_applicable(shown.summary),
    }
}

fn add_fix(check: Check, fix: FixSpec) -> Check {
    match fix {
        FixSpec::Page(label, uri) => check.uri(label, uri),
        FixSpec::Link(label, url) => check.link(label, url),
        FixSpec::Tool(label, tool) => check.tool(label, tool),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::checkup::{
        allowed_link, allowed_uri, evaluate, score, CheckState, FixAction, MICROSOFT_LINKS,
    };
    use super::super::probe::fixtures::{self, days_ago, event, now};
    use super::*;
    use crate::debloat::catalog::{self, Action};
    use crate::win::registry::{delete_sandbox_tree, path_within, SANDBOX_ROOT};

    const ACCESS_DENIED: u32 = 0x8007_0005;

    fn coded(id: u16, days: i64, code: u32) -> ServicingEvent {
        ServicingEvent {
            hresult: Some(code),
            ..event(id, days)
        }
    }

    fn raw_with(change: impl FnOnce(&mut CertificatesRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        fixtures::with_certificates(&mut raw, change);
        raw
    }

    fn servicing(c: &mut CertificatesRaw, change: impl FnOnce(&mut ServicingRaw)) {
        if let Ok(s) = &mut c.servicing {
            change(s);
        }
    }

    fn status(c: &mut CertificatesRaw, status: Option<UpdateStatus>) {
        servicing(c, |s| s.status = status);
    }

    /// Not started: the status says so and the periodic 1801 was logged yesterday.
    fn not_started(c: &mut CertificatesRaw) {
        status(c, Some(UpdateStatus::NotStarted));
        c.events = Ok(vec![event(NOT_YET, 1)]);
    }

    fn elevated() -> Context {
        Context {
            elevated: true,
            ..fixtures::ctx()
        }
    }

    fn check_of(raw: &CheckupRaw) -> Check {
        secure_boot_certificates(raw, &fixtures::ctx(), now())
    }

    fn outcome(check: &Check) -> (CheckState, Severity, String) {
        (check.state, check.severity, check.summary.clone())
    }

    fn fact<'a>(check: &'a Check, label: &str) -> Option<&'a str> {
        check
            .facts
            .iter()
            .find(|f| f.label == label)
            .map(|f| f.value.as_str())
    }

    fn lines(check: &Check) -> Vec<&str> {
        check
            .facts
            .iter()
            .filter(|f| f.label.is_empty())
            .map(|f| f.value.as_str())
            .collect()
    }

    fn fix_labels(check: &Check) -> Vec<&str> {
        check.fixes.iter().map(|f| f.label.as_str()).collect()
    }

    fn link(url: &str) -> FixAction {
        FixAction::Link { url: url.into() }
    }

    fn verdict_of(change: impl FnOnce(&mut CertificatesRaw)) -> Verdict {
        let mut raw = fixtures::certificates();
        change(&mut raw);
        verdict(&raw, SecureBoot::On, now())
    }

    /// Every verdict and its variants.
    fn all_verdicts() -> Vec<Verdict> {
        vec![
            Verdict::LegacyBios,
            Verdict::SecureBootOff { installed: false },
            Verdict::SecureBootOff { installed: true },
            Verdict::Unknown(UnknownReason::Unreadable("access denied".into())),
            Verdict::Unknown(UnknownReason::UnknownStatus("Blocked".into())),
            Verdict::Unknown(UnknownReason::NoRecord),
            Verdict::Updated,
            Verdict::UpdatedTaskOff { missing: false },
            Verdict::UpdatedTaskOff { missing: true },
            Verdict::TaskOff { missing: false },
            Verdict::TaskOff { missing: true },
            Verdict::OptedOut,
            Verdict::NeedsFirmware(FirmwareReason::NotSupported),
            Verdict::NeedsFirmware(FirmwareReason::KnownIssue),
            Verdict::NeedsFirmware(FirmwareReason::NoPkSignedKek),
            Verdict::Paused,
            Verdict::FirmwareError {
                hresult: Some(ACCESS_DENIED),
            },
            Verdict::BitLockerHold,
            Verdict::RetryAfterError { hresult: None },
            Verdict::Stalled {
                since: Some(days_ago(20)),
            },
            Verdict::Stalled { since: None },
            Verdict::RestartNeeded,
            Verdict::InProgress,
            Verdict::WaitingForData,
            Verdict::NoData,
            Verdict::NotStarted,
            Verdict::NoRecordYet,
        ]
    }

    #[test]
    fn status_and_bucket_values_are_parsed() {
        assert_eq!(parse_status("Updated"), UpdateStatus::Updated);
        assert_eq!(parse_status("updated"), UpdateStatus::Updated);
        assert_eq!(parse_status(" InProgress "), UpdateStatus::InProgress);
        assert_eq!(parse_status("NOTSTARTED"), UpdateStatus::NotStarted);
        assert_eq!(
            parse_status("Blocked"),
            UpdateStatus::Other("Blocked".into())
        );
        assert_eq!(
            status_of(RegValue::Sz("Updated".into())),
            Some(UpdateStatus::Updated)
        );
        assert_eq!(status_of(RegValue::ExpandSz("  ".into())), None);
        assert_eq!(
            status_of(RegValue::Dword(1)),
            Some(UpdateStatus::Other("0x00000001 (1)".into()))
        );
        for (text, bucket) in [
            ("High Confidence", Some(Bucket::HighConfidence)),
            ("Temporarily Paused", Some(Bucket::Paused)),
            (
                "Not Supported – Known Limitation",
                Some(Bucket::NotSupported),
            ),
            (
                "Not Supported - Known Limitation",
                Some(Bucket::NotSupported),
            ),
            (
                "Under Observation – More Data Needed",
                Some(Bucket::UnderObservation),
            ),
            ("No Data Observed – Action Required", Some(Bucket::NoData)),
            (" no data observed - action required", Some(Bucket::NoData)),
            ("Something else", None),
            ("", None),
        ] {
            assert_eq!(parse_bucket(text), bucket, "{text}");
        }
        assert_eq!(parse_release_date("01/15/2025"), "2025-01-15");
        assert_eq!(parse_release_date("2025-01-15"), "2025-01-15");
        assert_eq!(parse_release_date(" 13/45/2026 "), "13/45/2026");
    }

    #[test]
    fn newest_event_of_each_id_is_kept() {
        let events = vec![
            event(NOT_YET, 0),
            event(UPDATED, 1),
            event(NOT_YET, 2),
            event(KEK, 3),
            event(UPDATED, 4),
        ];
        assert_eq!(
            newest_per_id(events),
            vec![event(NOT_YET, 0), event(UPDATED, 1), event(KEK, 3)]
        );
        assert!(newest_per_id(Vec::new()).is_empty());
    }

    #[test]
    fn event_phrases_cover_every_event() {
        for id in EVENT_IDS {
            let phrase = event_phrase(&event(id, 0)).expect("a phrase");
            assert!(!phrase.is_empty(), "{id}");
        }
        assert_eq!(
            event_phrase(&coded(FIRMWARE_ERROR, 0, ACCESS_DENIED)).unwrap(),
            "the firmware returned error 0x80070005"
        );
        assert_eq!(
            event_phrase(&event(FIRMWARE_ERROR, 0)).unwrap(),
            "the firmware returned an error"
        );
        assert_eq!(
            event_phrase(&coded(UPDATE_ERROR, 0, ACCESS_DENIED)).unwrap(),
            "the update failed with error 0x80070005"
        );
        assert_eq!(
            event_phrase(&event(DB_UPDATE, 0)).unwrap(),
            "a Secure Boot database update was applied"
        );
        assert_eq!(event_phrase(&event(1038, 0)), None);
        assert_eq!(
            hresult_of(&EvtValue::Int(-2_147_024_891)),
            Some(ACCESS_DENIED)
        );
        assert_eq!(hresult_of(&EvtValue::UInt(5)), Some(5));
        assert_eq!(hresult_of(&EvtValue::Null), None);
    }

    #[test]
    fn updated_is_good() {
        let check = check_of(&fixtures::raw());
        assert_eq!(
            outcome(&check),
            (
                CheckState::Good,
                Severity::Medium,
                "The 2023 certificates are installed".into()
            )
        );
        assert_eq!(check.detail, DETAIL);
        assert_eq!(check.facts.len(), 1, "{:?}", check.facts);
        assert_eq!(
            fact(&check, "Latest Secure Boot update event").unwrap(),
            format!(
                "{}  ·  all certificates updated (event 1808)",
                local_date(days_ago(2))
            )
        );
        assert_eq!(check.fixes.len(), 1);
        assert_eq!(check.fixes[0].label, "Open Device security");
        assert_eq!(
            check.fixes[0].action,
            FixAction::Uri {
                uri: "windowsdefender://devicesecurity/".into()
            }
        );
        assert!(!check.needs_admin && !check.per_user);
        assert_eq!(check.title, "Secure Boot certificates");
    }

    #[test]
    fn updated_ignores_old_problems_and_buckets() {
        let raw = raw_with(|c| {
            servicing(c, |s| {
                s.error = Some(ACCESS_DENIED);
                s.error_event = Some(u32::from(FIRMWARE_ERROR));
                s.bucket = Some(Bucket::NotSupported);
                s.opt_out = None;
            });
            c.events = Ok(vec![
                coded(FIRMWARE_ERROR, 1, ACCESS_DENIED),
                event(UPDATED, 2),
            ]);
        });
        let check = check_of(&raw);
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(fact(&check, "Error recorded by Windows"), None);
        assert_eq!(fact(&check, "Firmware"), None);
    }

    #[test]
    fn in_progress_and_restart_are_info() {
        let raw = raw_with(|c| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(KEK, 1), event(NOT_YET, 3)]);
        });
        let check = check_of(&raw);
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Info,
                "Installing the 2023 certificates…".into()
            )
        );
        assert_eq!(
            fix_labels(&check),
            [
                "Open Windows Update",
                "Open Device security",
                "Find the recovery key"
            ]
        );
        assert_eq!(check.fixes[2].action, link(RECOVERY_KEY));
        assert!(check.detail.contains(BITLOCKER_ONCE));
        assert!(check.detail.contains(RECOVERY_KEY_PLACE));
        // A 1800 as the newest state event: a restart is needed.
        let raw = raw_with(|c| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(RESTART, 0), event(KEK, 1)]);
        });
        let check = check_of(&raw);
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Info,
                "Windows needs a restart to continue installing the 2023 certificates".into()
            )
        );
        assert_eq!(check.fixes[0].label, "Open Windows Update");
        assert_eq!(check.fixes[2].action, link(RECOVERY_KEY));
        // A 1801 after the 1800: Windows ran again, so it is installing.
        let verdict = verdict_of(|c| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(NOT_YET, 0), event(RESTART, 1), event(KEK, 2)]);
        });
        assert_eq!(verdict, Verdict::InProgress);
    }

    #[test]
    fn not_started_is_low() {
        let check = check_of(&raw_with(not_started));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Low,
                "Windows has not installed the 2023 certificates yet".into()
            )
        );
        assert_eq!(
            fix_labels(&check),
            [
                "Open Windows Update",
                "Open Device security",
                "Microsoft's guidance"
            ]
        );
        assert_eq!(check.fixes[2].action, link(SECURE_BOOT_GUIDANCE));
        for sentence in [
            BY_ITSELF,
            KEEP_UPDATING,
            EVENT_NOT_YET,
            AFTER_EXPIRY,
            RED_BADGE,
        ] {
            assert!(check.detail.contains(sentence), "{sentence}");
        }
        assert_eq!(
            fact(&check, "Latest Secure Boot update event").unwrap(),
            format!(
                "{}  ·  not finished yet (event 1801)",
                local_date(days_ago(1))
            )
        );
    }

    #[test]
    fn missing_status_reads_the_events() {
        let from = |events: Vec<ServicingEvent>| effective_status(None, &events);
        assert_eq!(
            from(vec![event(NOT_YET, 1)]),
            Some(UpdateStatus::NotStarted)
        );
        assert_eq!(
            from(vec![event(KEK, 1), event(NOT_YET, 2)]),
            Some(UpdateStatus::InProgress)
        );
        assert_eq!(
            from(vec![event(NOT_YET, 1), event(KEK, 2)]),
            Some(UpdateStatus::NotStarted)
        );
        assert_eq!(
            from(vec![event(UPDATED, 1), event(NOT_YET, 2)]),
            Some(UpdateStatus::Updated)
        );
        // A 1801 after the 1808: not finished again.
        assert_eq!(
            from(vec![event(NOT_YET, 1), event(UPDATED, 2)]),
            Some(UpdateStatus::NotStarted)
        );
        // A later database update does not undo a finished update.
        assert_eq!(
            from(vec![event(DB_UPDATE, 1), event(UPDATED, 2)]),
            Some(UpdateStatus::Updated)
        );
        assert_eq!(from(vec![event(RESTART, 1)]), None);
        assert_eq!(from(Vec::new()), None);
        // The recorded status wins.
        assert_eq!(
            effective_status(Some(&UpdateStatus::NotStarted), &[event(UPDATED, 1)]),
            Some(UpdateStatus::NotStarted)
        );
        let check = check_of(&raw_with(|c| {
            status(c, None);
            c.events = Ok(vec![event(UPDATED, 1)]);
        }));
        assert_eq!(check.state, CheckState::Good);
        let check = check_of(&raw_with(|c| {
            status(c, None);
            c.events = Ok(vec![event(OPTION_ROM_CA, 1), event(NOT_YET, 2)]);
        }));
        assert_eq!(check.summary, "Installing the 2023 certificates…");
    }

    #[test]
    fn buckets() {
        let with_bucket = |bucket: Bucket, events: Vec<ServicingEvent>| {
            raw_with(|c| {
                not_started(c);
                servicing(c, |s| s.bucket = Some(bucket));
                c.events = Ok(events);
            })
        };
        // Paused outranks a recent 1802, which is probably how the pause shows.
        let paused = check_of(&with_bucket(
            Bucket::Paused,
            vec![event(KNOWN_ISSUE, 1), event(NOT_YET, 2)],
        ));
        assert_eq!(
            outcome(&paused),
            (
                CheckState::Attention,
                Severity::Info,
                "Paused by Microsoft for PCs like this one".into()
            )
        );
        let unsupported = check_of(&with_bucket(Bucket::NotSupported, vec![event(NOT_YET, 1)]));
        assert_eq!(
            outcome(&unsupported),
            (
                CheckState::Attention,
                Severity::Medium,
                "Needs a firmware update or help from the PC's manufacturer".into()
            )
        );
        assert!(unsupported.detail.contains(NOT_SUPPORTED));
        assert_eq!(
            fix_labels(&unsupported),
            [
                "Manufacturer pages",
                "Microsoft's guidance",
                "Open Device security"
            ]
        );
        assert_eq!(unsupported.fixes[0].action, link(SECURE_BOOT_MANUFACTURERS));
        assert_eq!(unsupported.fixes[1].action, link(SECURE_BOOT_PREVENTED));
        let waiting = check_of(&with_bucket(
            Bucket::UnderObservation,
            vec![event(NOT_YET, 1)],
        ));
        assert_eq!(
            outcome(&waiting),
            (
                CheckState::Attention,
                Severity::Low,
                "Not installed yet: Microsoft is still checking PCs like this one".into()
            )
        );
        assert_eq!(
            fix_labels(&waiting),
            ["Microsoft's guidance", "Open Windows Update"]
        );
        let no_data = check_of(&with_bucket(Bucket::NoData, vec![event(NOT_YET, 1)]));
        assert_eq!(
            outcome(&no_data),
            (
                CheckState::Attention,
                Severity::Low,
                "Not installed yet: Microsoft has no data on PCs like this one".into()
            )
        );
        let high = check_of(&with_bucket(
            Bucket::HighConfidence,
            vec![event(NOT_YET, 1)],
        ));
        assert_eq!(
            high.summary,
            "Windows has not installed the 2023 certificates yet"
        );
    }

    #[test]
    fn recent_problems() {
        // A 1795 counts although the periodic 1801 came later; its code is a fact.
        let check = check_of(&raw_with(|c| {
            status(c, Some(UpdateStatus::NotStarted));
            c.events = Ok(vec![
                event(NOT_YET, 1),
                coded(FIRMWARE_ERROR, 2, ACCESS_DENIED),
            ]);
        }));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Medium,
                "The firmware did not accept the update".into()
            )
        );
        assert_eq!(
            fact(&check, "Error recorded by Windows"),
            Some("0x80070005 (event 1795)")
        );
        assert_eq!(
            fact(&check, "Firmware"),
            Some("Contoso 1.0.7, released 2025-01-15")
        );
        assert_eq!(
            fix_labels(&check),
            ["Manufacturer pages", "Troubleshooting guide"]
        );
        // A step after the error, or an error older than 30 days, no longer counts.
        assert_eq!(
            verdict_of(|c| {
                status(c, Some(UpdateStatus::InProgress));
                c.events = Ok(vec![event(KEK, 1), coded(FIRMWARE_ERROR, 2, ACCESS_DENIED)]);
            }),
            Verdict::InProgress
        );
        assert_eq!(
            verdict_of(|c| {
                status(c, Some(UpdateStatus::NotStarted));
                c.events = Ok(vec![event(NOT_YET, 1), event(FIRMWARE_ERROR, 31)]);
            }),
            Verdict::NotStarted
        );
        // Precedence among recent problems: 1803 > 1802 > 1795 > 1032 > 1796.
        let mut ids = vec![
            NO_PK_SIGNED_KEK,
            KNOWN_ISSUE,
            FIRMWARE_ERROR,
            BITLOCKER,
            UPDATE_ERROR,
        ];
        let expected = [
            Verdict::NeedsFirmware(FirmwareReason::NoPkSignedKek),
            Verdict::NeedsFirmware(FirmwareReason::KnownIssue),
            Verdict::FirmwareError { hresult: None },
            Verdict::BitLockerHold,
            Verdict::RetryAfterError { hresult: None },
        ];
        for verdict in expected {
            let events: Vec<ServicingEvent> = ids.iter().map(|id| event(*id, 1)).collect();
            assert_eq!(
                verdict_of(|c| {
                    status(c, Some(UpdateStatus::NotStarted));
                    c.events = Ok(events);
                }),
                verdict
            );
            ids.remove(0);
        }
    }

    #[test]
    fn opt_out_and_policy() {
        let opted_out = |value: u32| {
            raw_with(|c| {
                not_started(c);
                servicing(c, |s| s.opt_out = Some(value));
            })
        };
        let check = check_of(&opted_out(1));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Medium,
                "Automatic installation is turned off on this PC".into()
            )
        );
        assert_eq!(fact(&check, "Setting"), Some("HighConfidenceOptOut = 1"));
        assert_eq!(fix_labels(&check), ["Microsoft's guidance"]);
        // Any value but 0 opts out.
        let check = check_of(&opted_out(2));
        assert_eq!(fact(&check, "Setting"), Some("HighConfidenceOptOut = 2"));
        assert_eq!(check.severity, Severity::Medium);
        assert_eq!(
            check_of(&opted_out(0)).summary,
            "Windows has not installed the 2023 certificates yet"
        );
        // A policy is a line on any applicable result.
        let managed = raw_with(|c| servicing(c, |s| s.policy_managed = true));
        assert!(lines(&check_of(&managed)).contains(&MANAGED_LINE));
        let managed = raw_with(|c| {
            not_started(c);
            servicing(c, |s| s.policy_managed = true);
        });
        assert!(lines(&check_of(&managed)).contains(&MANAGED_LINE));
        assert!(!lines(&check_of(&raw_with(not_started))).contains(&MANAGED_LINE));
    }

    #[test]
    fn secure_boot_off_and_legacy_bios_are_not_applicable() {
        let with_secure_boot = |state: SecureBoot, change: fn(&mut CertificatesRaw)| {
            let mut raw = raw_with(change);
            if let Ok(device) = &mut raw.device {
                device.secure_boot = state;
            }
            raw
        };
        let off = check_of(&with_secure_boot(SecureBoot::Off, not_started));
        assert_eq!(off.state, CheckState::NotApplicable);
        assert_eq!(off.summary, "Not installed while Secure Boot is off");
        assert!(off.detail.ends_with(SECURE_BOOT_OFF));
        assert_eq!(fix_labels(&off), ["Open Device security"]);
        let installed = check_of(&with_secure_boot(SecureBoot::Off, |_| {}));
        assert_eq!(
            installed.summary,
            "Installed; not in use while Secure Boot is off"
        );
        let bios = check_of(&with_secure_boot(SecureBoot::Unsupported, not_started));
        assert_eq!(bios.state, CheckState::NotApplicable);
        assert_eq!(bios.summary, "Not used: Windows starts in legacy BIOS mode");
        assert!(bios.fixes.is_empty());
        // Not applicable rows carry no facts, not even the administrator hint.
        for check in [&off, &installed, &bios] {
            assert!(check.facts.is_empty(), "{:?}", check.facts);
        }
        // The Secure Boot check carries the score; this one adds nothing.
        let raw = with_secure_boot(SecureBoot::Off, not_started);
        let checks = evaluate(&raw, &fixtures::ctx(), &fixtures::scan_done(), now());
        let by_id = |id: CheckId| checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(by_id(CheckId::SecureBoot).state, CheckState::Attention);
        assert_eq!(by_id(CheckId::SecureBoot).severity, Severity::High);
        assert_eq!(
            by_id(CheckId::SecureBootCertificates).state,
            CheckState::NotApplicable
        );
        assert_eq!(score(&checks).value, 80);
    }

    #[test]
    fn unknown_cases() {
        let unreadable_record = raw_with(|c| {
            c.servicing = Err("access denied".into());
            c.events = Ok(Vec::new());
        });
        let check = check_of(&unreadable_record);
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read the certificate update status: access denied"
        );
        assert_eq!(
            fix_labels(&check),
            ["Open Device security", "Microsoft's guidance"]
        );
        // An empty record with an unreadable log is never a guessed "not started".
        let unreadable_log = raw_with(|c| {
            status(c, None);
            c.events = Err("the log could not be opened".into());
        });
        assert_eq!(
            check_of(&unreadable_log).summary,
            "Could not read the certificate update status: the log could not be opened"
        );
        // The events can still tell the status.
        let from_events = raw_with(|c| {
            c.servicing = Err("access denied".into());
            c.events = Ok(vec![event(UPDATED, 1)]);
        });
        assert_eq!(check_of(&from_events).state, CheckState::Good);
        let other = raw_with(|c| status(c, Some(UpdateStatus::Other("Blocked".into()))));
        assert_eq!(
            check_of(&other).summary,
            "Windows reports a status Cairn does not know: \"Blocked\""
        );
        assert_eq!(check_of(&other).state, CheckState::Unknown);
        // Secure Boot's state unknown: no record is unknown, a recorded update is good.
        let mut no_record = raw_with(|c| {
            status(c, None);
            c.events = Ok(Vec::new());
        });
        no_record.device = Err("x".into());
        assert_eq!(
            check_of(&no_record).summary,
            "No record of the certificate update was found"
        );
        let mut updated = fixtures::raw();
        updated.device = Err("x".into());
        assert_eq!(check_of(&updated).state, CheckState::Good);
    }

    #[test]
    fn admin_extras() {
        let with_task = |task: TaskRead, change: fn(&mut CertificatesRaw)| {
            let raw = raw_with(|c| {
                change(c);
                c.task = task;
            });
            secure_boot_certificates(&raw, &elevated(), now())
        };
        let off = with_task(TaskRead::Disabled, not_started);
        assert_eq!(
            outcome(&off),
            (
                CheckState::Attention,
                Severity::Medium,
                "The task that installs the 2023 certificates is turned off".into()
            )
        );
        assert_eq!(fact(&off, "Update task"), Some("turned off"));
        assert_eq!(
            off.fixes[0].action,
            FixAction::WindowsTool {
                tool: "task_scheduler".into(),
                requires_admin: true
            }
        );
        assert_eq!(off.fixes[0].label, "Open Task Scheduler");
        assert_eq!(off.fixes[1].action, link(SECURE_BOOT_TROUBLESHOOTING));
        assert!(off.detail.contains(TASK_TURN_ON));
        let missing = with_task(TaskRead::Missing, not_started);
        assert_eq!(
            missing.summary,
            "The task that installs the 2023 certificates is missing"
        );
        assert_eq!(fact(&missing, "Update task"), Some("missing"));
        assert!(missing.detail.contains(TASK_RESTORE));
        let updated_off = with_task(TaskRead::Disabled, |_| {});
        assert_eq!(
            outcome(&updated_off),
            (
                CheckState::Attention,
                Severity::Low,
                "Installed, but the task for Secure Boot updates is turned off".into()
            )
        );
        assert_eq!(updated_off.fixes[0].label, "Open Task Scheduler");
        let updated_missing = with_task(TaskRead::Missing, |_| {});
        assert_eq!(
            updated_missing.summary,
            "Installed, but the task for Secure Boot updates is missing"
        );
        // A failed read is a fact only while the certificates are not installed.
        let failed = with_task(TaskRead::Failed("x".into()), not_started);
        assert_eq!(
            failed.summary,
            "Windows has not installed the 2023 certificates yet"
        );
        assert_eq!(fact(&failed, "Update task"), Some("could not be read: x"));
        let failed_updated = with_task(TaskRead::Failed("x".into()), |_| {});
        assert_eq!(failed_updated.state, CheckState::Good);
        assert_eq!(fact(&failed_updated, "Update task"), None);
        let enabled = with_task(TaskRead::Enabled, not_started);
        assert_eq!(fact(&enabled, "Update task"), None);
        assert_eq!(enabled.severity, Severity::Low);
    }

    #[test]
    fn admin_hint_only_when_not_elevated() {
        let hinted = |raw: &CheckupRaw, ctx: &Context| {
            lines(&secure_boot_certificates(raw, ctx, now())).contains(&ADMIN_HINT)
        };
        let attention = raw_with(not_started);
        assert!(hinted(&attention, &fixtures::ctx()));
        assert!(!hinted(&attention, &elevated()));
        let info = raw_with(|c| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(KEK, 1)]);
        });
        assert!(hinted(&info, &fixtures::ctx()));
        let unknown = raw_with(|c| status(c, Some(UpdateStatus::Other("Blocked".into()))));
        assert!(hinted(&unknown, &fixtures::ctx()));
        assert!(!hinted(&fixtures::raw(), &fixtures::ctx()));
        let mut off = raw_with(not_started);
        if let Ok(device) = &mut off.device {
            device.secure_boot = SecureBoot::Off;
        }
        assert!(!hinted(&off, &fixtures::ctx()));
        // The hint is the last line.
        let check = secure_boot_certificates(&attention, &fixtures::ctx(), now());
        assert_eq!(
            check.facts.last().map(|f| f.value.as_str()),
            Some(ADMIN_HINT)
        );
    }

    #[test]
    fn texts_never_advise_turning_secure_boot_off() {
        for verdict in all_verdicts() {
            for fast_startup in [false, true] {
                let shown = shown(&verdict, fast_startup);
                assert!(!shown.summary.is_empty(), "{verdict:?}");
                let detail = detail(&shown.sentences);
                assert!(detail.starts_with(DETAIL), "{verdict:?}");
                let text = format!("{} {detail}", shown.summary).to_lowercase();
                for word in ["disable", "reset", "vulnerable", "at risk"] {
                    assert!(!text.contains(word), "{verdict:?}: {word}");
                }
                let mut rest = text.as_str();
                while let Some(at) = rest.find("turn off secure boot") {
                    assert!(rest[..at].ends_with("do not "), "{verdict:?}");
                    rest = &rest[at + 1..];
                }
                for sentence in &shown.sentences {
                    assert!(sentence.ends_with('.'), "{verdict:?}: {sentence}");
                }
            }
        }
        // Every built check starts its detail the same way.
        for raw in [fixtures::raw(), raw_with(not_started)] {
            assert!(check_of(&raw).detail.starts_with(DETAIL));
        }
    }

    #[test]
    fn every_microsoft_link_is_used() {
        let mut links = BTreeSet::new();
        for verdict in all_verdicts() {
            for fix in shown(&verdict, false).fixes {
                match fix {
                    FixSpec::Link(_, url) => {
                        assert!(allowed_link(url), "{verdict:?}: {url}");
                        links.insert(url);
                    }
                    FixSpec::Page(_, uri) => assert!(allowed_uri(uri), "{verdict:?}: {uri}"),
                    FixSpec::Tool(_, tool) => assert!(
                        crate::tools::windows_tools::windows_tool(tool).is_some(),
                        "{verdict:?}: {tool}"
                    ),
                }
            }
        }
        let expected: BTreeSet<&str> = MICROSOFT_LINKS.into_iter().collect();
        assert_eq!(links, expected);
    }

    #[test]
    fn a_failed_source_is_unknown() {
        let mut raw = fixtures::raw();
        raw.certificates = Err("x".into());
        let check = check_of(&raw);
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read the certificate update status: x"
        );
        assert_eq!(fix_labels(&check), ["Open Device security"]);
        assert_eq!(check.detail, DETAIL);
    }

    #[test]
    fn no_data_never_promises_an_automatic_install() {
        let check = check_of(&raw_with(|c| {
            not_started(c);
            servicing(c, |s| s.bucket = Some(Bucket::NoData));
        }));
        assert_eq!(
            check.summary,
            "Not installed yet: Microsoft has no data on PCs like this one"
        );
        let text = format!("{} {}", check.summary, check.detail).to_lowercase();
        assert!(!text.contains("by itself"), "{text}");
        assert!(!text.contains("installs them"), "{text}");
        assert!(text.contains("may not install them automatically"));
        assert_eq!(
            fix_labels(&check),
            ["Microsoft's guidance", "Manufacturer pages"]
        );
        assert_eq!(check.fixes[0].action, link(SECURE_BOOT_GUIDANCE));
        assert_eq!(
            fact(&check, "Firmware"),
            Some("Contoso 1.0.7, released 2025-01-15")
        );
        // The group Microsoft is still watching does get it by itself.
        let waiting = shown(&Verdict::WaitingForData, false);
        assert!(detail(&waiting.sentences).contains("by itself"));
    }

    #[test]
    fn paused_names_the_manufacturer() {
        let check = check_of(&raw_with(|c| {
            not_started(c);
            servicing(c, |s| s.bucket = Some(Bucket::Paused));
        }));
        assert_eq!(check.severity, Severity::Info);
        assert!(check.detail.contains(PAUSED));
        assert!(check
            .detail
            .contains("A firmware update from the manufacturer may be needed"));
        assert!(!check.detail.contains("nothing needs to be done"));
        assert_eq!(
            fix_labels(&check),
            [
                "Manufacturer pages",
                "Microsoft's guidance",
                "Open Device security"
            ]
        );
        assert_eq!(check.fixes[1].action, link(SECURE_BOOT_PREVENTED));
        assert_eq!(
            fact(&check, "Firmware"),
            Some("Contoso 1.0.7, released 2025-01-15")
        );
    }

    #[test]
    fn no_record_yet_is_not_not_started() {
        let empty = |c: &mut CertificatesRaw| {
            status(c, None);
            c.events = Ok(Vec::new());
        };
        assert_eq!(verdict_of(empty), Verdict::NoRecordYet);
        let check = check_of(&raw_with(empty));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Low,
                "Windows has no record of the certificate update yet".into()
            )
        );
        assert_ne!(check.summary, SUMMARY_NOT_STARTED);
        assert_eq!(
            fix_labels(&check),
            [
                "Open Device security",
                "Open Windows Update",
                "Microsoft's guidance"
            ]
        );
        // An explicit status is not the missing record.
        assert_eq!(
            verdict_of(|c| {
                status(c, Some(UpdateStatus::NotStarted));
                c.events = Ok(Vec::new());
            }),
            Verdict::NotStarted
        );
        // No record at all, not even the Secure Boot key: the same.
        assert_eq!(
            verdict_of(|c| {
                c.servicing = Ok(ServicingRaw::default());
                c.events = Ok(Vec::new());
            }),
            Verdict::NoRecordYet
        );
        // Events without a status: Windows did log the update, so it is not started yet, also
        // when the only event is a problem too old to count.
        for id in PROBLEMS {
            assert_eq!(
                verdict_of(|c| {
                    status(c, None);
                    c.events = Ok(vec![event(id, PROBLEM_DAYS + 10)]);
                }),
                Verdict::NotStarted,
                "{id}"
            );
        }
        let check = check_of(&raw_with(|c| {
            status(c, None);
            c.events = Ok(vec![event(FIRMWARE_ERROR, 40)]);
        }));
        assert_eq!(check.summary, SUMMARY_NOT_STARTED);
        assert_eq!(
            fact(&check, "Latest Secure Boot update event").unwrap(),
            format!(
                "{}  ·  the firmware returned an error (event 1795)",
                local_date(days_ago(40))
            )
        );
    }

    #[test]
    fn stalled_install_is_low_after_14_days() {
        let in_progress = |events: Vec<ServicingEvent>| {
            raw_with(|c| {
                status(c, Some(UpdateStatus::InProgress));
                c.events = Ok(events);
            })
        };
        let check = check_of(&in_progress(vec![event(NOT_YET, 1), event(KEK, 15)]));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Low,
                format!(
                    "Windows started installing the 2023 certificates but has not finished since {}",
                    local_date(days_ago(15))
                )
            )
        );
        assert_eq!(
            fix_labels(&check),
            ["Troubleshooting guide", "Open Windows Update"]
        );
        assert_eq!(
            check_of(&in_progress(vec![event(KEK, 13)])).summary,
            "Installing the 2023 certificates…"
        );
        // No step left in the log: stalled, without a date.
        assert_eq!(
            check_of(&in_progress(vec![event(NOT_YET, 1)])).summary,
            "Windows started installing the 2023 certificates but has not finished"
        );
        assert_eq!(
            check_of(&in_progress(Vec::new())).summary,
            "Windows started installing the 2023 certificates but has not finished"
        );
        // An unreadable log tells nothing about a stall.
        let unreadable = raw_with(|c| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Err("x".into());
        });
        assert_eq!(
            check_of(&unreadable).summary,
            "Installing the 2023 certificates…"
        );
    }

    #[test]
    fn error_event_value_is_a_fallback_problem() {
        let recorded = |id: u16, error: u32, events: Vec<ServicingEvent>| {
            verdict_of(|c| {
                status(c, Some(UpdateStatus::NotStarted));
                servicing(c, |s| {
                    s.error = Some(error);
                    s.error_event = Some(u32::from(id));
                });
                c.events = Ok(events);
            })
        };
        // The 1795 itself rolled out of the log.
        assert_eq!(
            recorded(FIRMWARE_ERROR, ACCESS_DENIED, vec![event(NOT_YET, 1)]),
            Verdict::FirmwareError {
                hresult: Some(ACCESS_DENIED)
            }
        );
        assert_eq!(
            recorded(NO_PK_SIGNED_KEK, 1, Vec::new()),
            Verdict::NeedsFirmware(FirmwareReason::NoPkSignedKek)
        );
        // A step since then shows the update moved on.
        assert_eq!(
            recorded(FIRMWARE_ERROR, ACCESS_DENIED, vec![event(KEK, 1)]),
            Verdict::NotStarted
        );
        // An old event of that id is still in the log, so the log decides.
        assert_eq!(
            recorded(
                FIRMWARE_ERROR,
                ACCESS_DENIED,
                vec![event(FIRMWARE_ERROR, 40)]
            ),
            Verdict::NotStarted
        );
        // 0 means success.
        assert_eq!(
            recorded(UPDATE_ERROR, 0, vec![event(NOT_YET, 1)]),
            Verdict::NotStarted
        );
        // Without a readable log the record decides.
        assert_eq!(
            verdict_of(|c| {
                status(c, Some(UpdateStatus::NotStarted));
                servicing(c, |s| {
                    s.error = Some(1);
                    s.error_event = Some(u32::from(BITLOCKER));
                });
                c.events = Err("x".into());
            }),
            Verdict::BitLockerHold
        );
        // The fact shows the recorded error and its event.
        let check = check_of(&raw_with(|c| {
            status(c, Some(UpdateStatus::NotStarted));
            servicing(c, |s| {
                s.error = Some(ACCESS_DENIED);
                s.error_event = Some(u32::from(UPDATE_ERROR));
            });
            c.events = Ok(vec![event(NOT_YET, 1)]);
        }));
        assert_eq!(
            check.summary,
            "The update stopped with an error; Windows tries again"
        );
        assert_eq!(
            fact(&check, "Error recorded by Windows"),
            Some("0x80070005 (event 1796)")
        );
    }

    #[test]
    fn restart_needed_with_not_started_status() {
        let check = check_of(&raw_with(|c| {
            status(c, Some(UpdateStatus::NotStarted));
            c.events = Ok(vec![event(RESTART, 0), event(NOT_YET, 1)]);
        }));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Info,
                "Windows needs a restart to continue installing the 2023 certificates".into()
            )
        );
        // Without a status as well.
        assert_eq!(
            verdict_of(|c| {
                status(c, None);
                c.events = Ok(vec![event(RESTART, 0)]);
            }),
            Verdict::RestartNeeded
        );
    }

    #[test]
    fn fast_startup_adds_the_restart_advice() {
        let with = |fast_startup: Option<bool>, change: fn(&mut CertificatesRaw)| {
            let mut raw = raw_with(change);
            if let Ok(os) = &mut raw.os {
                os.fast_startup = fast_startup;
            }
            check_of(&raw)
        };
        let installing = |c: &mut CertificatesRaw| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(KEK, 1)]);
        };
        let restart = |c: &mut CertificatesRaw| {
            status(c, Some(UpdateStatus::InProgress));
            c.events = Ok(vec![event(RESTART, 0)]);
        };
        for change in [installing as fn(&mut CertificatesRaw), restart] {
            assert!(with(Some(true), change)
                .detail
                .ends_with(" Use Restart, not Shut down."));
            assert!(!with(Some(false), change)
                .detail
                .contains(RESTART_NOT_SHUT_DOWN));
            assert!(!with(None, change).detail.contains(RESTART_NOT_SHUT_DOWN));
        }
        assert!(!with(Some(true), not_started)
            .detail
            .contains(RESTART_NOT_SHUT_DOWN));
    }

    #[test]
    fn board_maker_replaces_a_placeholder_oem() {
        let sandbox = Sandbox::new("BoardMaker");
        sandbox.set(
            SERVICING,
            "UEFICA2023Status",
            RegValue::Sz("NotStarted".into()),
        );
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "OEMManufacturerName",
            RegValue::Sz("To Be Filled By O.E.M.".into()),
        );
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "BaseBoardManufacturer",
            RegValue::Sz("Contoso".into()),
        );
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "FirmwareVersion",
            RegValue::Sz("1.0.7".into()),
        );
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "FirmwareReleaseDate",
            RegValue::Sz("01/15/2025".into()),
        );
        let record = sandbox.read().unwrap();
        assert_eq!(record.firmware.maker.as_deref(), Some("Contoso"));
        assert_eq!(
            firmware_text(&record.firmware).as_deref(),
            Some("Contoso 1.0.7, released 2025-01-15")
        );
        // The PC's own maker wins when it is not a placeholder.
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "OEMManufacturerName",
            RegValue::Sz("Fabrikam".into()),
        );
        assert_eq!(
            sandbox.read().unwrap().firmware.maker.as_deref(),
            Some("Fabrikam")
        );
        let text = |maker: Option<&str>, version: Option<&str>, released: Option<&str>| {
            firmware_text(&FirmwareId {
                maker: maker.map(str::to_string),
                version: version.map(str::to_string),
                released: released.map(str::to_string),
            })
        };
        assert_eq!(
            text(Some("Contoso"), None, None).as_deref(),
            Some("Contoso")
        );
        assert_eq!(
            text(None, None, Some("2025-01-15")).as_deref(),
            Some("released 2025-01-15")
        );
        assert_eq!(text(None, None, None), None);
        // The fact on a verdict that points at the manufacturer.
        let check = check_of(&raw_with(|c| {
            not_started(c);
            servicing(c, |s| {
                s.bucket = Some(Bucket::NotSupported);
                s.firmware = record.firmware.clone();
            });
        }));
        assert_eq!(
            fact(&check, "Firmware"),
            Some("Contoso 1.0.7, released 2025-01-15")
        );
        assert!(check.detail.contains(NEWER_FIRMWARE));
        assert!(check.detail.contains(OLDER_MODELS));
        assert!(check.detail.contains(KEEP_SECURE_BOOT));
    }

    #[test]
    fn paused_windows_update_is_a_fact() {
        let paused_in = |ctx: &Context, change: fn(&mut CertificatesRaw), until: DateTime<Utc>| {
            let mut raw = raw_with(change);
            if let Ok(wu) = &mut raw.windows_update {
                wu.pause_values = vec![until.to_rfc3339()];
            }
            secure_boot_certificates(&raw, ctx, now())
        };
        let paused = |change: fn(&mut CertificatesRaw), until: DateTime<Utc>| {
            paused_in(&fixtures::ctx(), change, until)
        };
        let until = now() + Duration::days(5);
        let line = format!("Windows Update is paused until {}", local_date(until));
        let check = paused(not_started, until);
        assert!(lines(&check).contains(&line.as_str()), "{:?}", check.facts);
        assert_eq!(check.fixes[0].label, "Open Windows Update");
        assert_eq!(
            check
                .fixes
                .iter()
                .filter(|f| f.label == "Open Windows Update")
                .count(),
            1
        );
        // Windows Update comes first even where it is not otherwise offered.
        let needs_firmware = |c: &mut CertificatesRaw| {
            not_started(c);
            servicing(c, |s| s.bucket = Some(Bucket::NotSupported));
        };
        let check = paused(needs_firmware, until);
        assert_eq!(
            fix_labels(&check),
            [
                "Open Windows Update",
                "Manufacturer pages",
                "Microsoft's guidance",
                "Open Device security"
            ]
        );
        // Installed certificates, or a pause that ended, add nothing.
        assert!(!lines(&paused(|_| {}, until)).contains(&line.as_str()));
        let ended = paused(not_started, now() - Duration::days(1));
        assert!(!lines(&ended)
            .iter()
            .any(|l| l.starts_with("Windows Update is paused")));
        assert_eq!(ended.fixes[0].label, "Open Windows Update");
        // Installed with the update task turned off: Task Scheduler stays the main action.
        let check = paused_in(&elevated(), |c| c.task = TaskRead::Disabled, until);
        assert_eq!(check.summary, SUMMARY_UPDATED_TASK_OFF);
        assert!(!lines(&check).contains(&line.as_str()), "{:?}", check.facts);
        assert_eq!(
            fix_labels(&check),
            ["Open Task Scheduler", "Troubleshooting guide"]
        );
        // A status Cairn does not know is not Updated either.
        let check = paused(
            |c| status(c, Some(UpdateStatus::Other("Blocked".into()))),
            until,
        );
        assert_eq!(check.state, CheckState::Unknown);
        assert!(lines(&check).contains(&line.as_str()), "{:?}", check.facts);
        assert_eq!(
            fix_labels(&check),
            [
                "Open Windows Update",
                "Open Device security",
                "Microsoft's guidance"
            ]
        );
    }

    #[test]
    fn no_tweak_touches_secure_boot_servicing() {
        const SECURITY_CENTER_POLICY: &str =
            r"SOFTWARE\Policies\Microsoft\Windows Defender Security Center\Device security";
        let protected = |path: &str| {
            path_within(path, SECURE_BOOT_KEY) || path_within(path, SECURITY_CENTER_POLICY)
        };
        for tweak in catalog::TWEAKS {
            for action in tweak.actions {
                match action {
                    Action::Registry(r) => {
                        assert!(!protected(r.path), "{}: {}", tweak.id, r.path);
                    }
                    Action::ScheduledTask(task) => {
                        assert!(
                            !task.path.eq_ignore_ascii_case(UPDATE_TASK),
                            "{}: {}",
                            tweak.id,
                            task.path
                        );
                    }
                    Action::Service(_) | Action::Power(_) => {}
                }
            }
            for former in tweak.former {
                assert!(!protected(former.action.path), "{}", tweak.id);
            }
        }
    }

    /// A Secure Boot key in the HKCU test sandbox, deleted with its subkeys when dropped.
    struct Sandbox(String);

    impl Sandbox {
        fn new(name: &str) -> Sandbox {
            let path = format!(r"{SANDBOX_ROOT}\HealthCertificates{name}");
            delete_sandbox_tree(&path).unwrap();
            Sandbox(path)
        }

        fn root(&self) -> String {
            format!(r"{}\SecureBoot", self.0)
        }

        /// Writes `value` into the subkey `sub` of the sandboxed Secure Boot key ("" for the
        /// key itself).
        fn set(&self, sub: &str, name: &str, value: RegValue) {
            let path = if sub.is_empty() {
                self.root()
            } else {
                format!(r"{}\{sub}", self.root())
            };
            let (key, _) = Key::create(Hive::CurrentUser, &path).unwrap();
            key.set(name, &value).unwrap();
        }

        fn read(&self) -> std::result::Result<ServicingRaw, String> {
            read_servicing_at(Hive::CurrentUser, &self.root())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = delete_sandbox_tree(&self.0);
        }
    }

    #[test]
    fn servicing_record_is_read_from_its_keys() {
        let sandbox = Sandbox::new("Record");
        // No Secure Boot key at all: an empty record.
        assert_eq!(sandbox.read(), Ok(ServicingRaw::default()));
        sandbox.set("", "AvailableUpdatesPolicy", RegValue::Dword(0x5944));
        sandbox.set(
            SERVICING,
            "UEFICA2023Status",
            RegValue::Sz(" InProgress ".into()),
        );
        sandbox.set(SERVICING, "UEFICA2023Error", RegValue::Dword(ACCESS_DENIED));
        sandbox.set(SERVICING, "UEFICA2023ErrorEvent", RegValue::Dword(1796));
        sandbox.set(
            SERVICING,
            "ConfidenceLevel",
            RegValue::Sz("Under Observation - More Data Needed".into()),
        );
        sandbox.set(
            DEVICE_ATTRIBUTES,
            "FirmwareReleaseDate",
            RegValue::Sz("01/15/2025".into()),
        );
        let record = sandbox.read().unwrap();
        assert_eq!(
            record,
            ServicingRaw {
                status: Some(UpdateStatus::InProgress),
                error: Some(ACCESS_DENIED),
                error_event: Some(1796),
                bucket: Some(Bucket::UnderObservation),
                opt_out: None,
                policy_managed: true,
                firmware: FirmwareId {
                    maker: None,
                    version: None,
                    released: Some("2025-01-15".into()),
                },
            }
        );
        // A status of another type is a status Cairn does not know.
        sandbox.set(SERVICING, "UEFICA2023Status", RegValue::Dword(2));
        assert_eq!(
            sandbox.read().unwrap().status,
            Some(UpdateStatus::Other("0x00000002 (2)".into()))
        );
    }

    #[test]
    fn no_servicing_key_still_reads_the_opt_out() {
        let sandbox = Sandbox::new("OptOut");
        sandbox.set("", "HighConfidenceOptOut", RegValue::Dword(1));
        let record = sandbox.read().unwrap();
        assert_eq!(record.status, None);
        assert_eq!(record.opt_out, Some(1));
        assert!(!record.policy_managed);
        let raw = CertificatesRaw {
            servicing: Ok(record),
            events: Ok(vec![event(NOT_YET, 1)]),
            task: TaskRead::NotRead,
        };
        assert_eq!(verdict(&raw, SecureBoot::On, now()), Verdict::OptedOut);
    }

    #[test]
    fn live_servicing_and_events_read() {
        // Read-only: the Secure Boot keys and the System log; the task is not read.
        let raw = read_certificates(false).unwrap();
        assert_eq!(raw.task, TaskRead::NotRead);
        let servicing = raw.servicing.expect("the Secure Boot keys are readable");
        if let Some(UpdateStatus::Other(text)) = &servicing.status {
            println!("an unexpected status: {text:?}");
        }
        let events = raw.events.expect("the System log is readable");
        let ids: HashSet<u16> = events.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), events.len(), "one event per id");
        assert!(events.iter().all(|e| EVENT_IDS.contains(&e.id)));
        assert!(
            events.windows(2).all(|pair| pair[0].at >= pair[1].at),
            "newest first"
        );
    }

    #[test]
    fn live_task_is_read_only_when_elevated() {
        // Read-only: Task Scheduler's GetTask and the enabled flag, which only administrators
        // may read.
        if !crate::is_elevated() {
            return;
        }
        let task = read_task();
        println!("Secure-Boot-Update: {task:?}");
        assert_ne!(task, TaskRead::NotRead);
        if let TaskRead::Failed(error) = &task {
            assert!(!error.is_empty());
        }
    }
}
