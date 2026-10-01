//! Windows Update settings: pause, active hours, drivers, feature update delay and restart
//! notifications, each journaled as registry values.
//!
//! The values live in `HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings`, which is what the
//! Settings app writes and every edition reads, and, for drivers and the feature update
//! delay, in the Windows Update policy key, which Microsoft documents for Pro and higher
//! only. Every write goes through [`Safety::set_registry_value`] or
//! [`Safety::delete_registry_value`], which record the baseline before writing, so History,
//! Revert All and `revert_targets` undo them. The settings are machine-wide, so running as
//! another account than the signed-in user is not refused. Reading needs no elevation.

use std::fmt;

use chrono::{DateTime, Duration as ChronoDuration, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::profiles::step::{
    ActiveHoursChoice, SettingStep, StepOutcome, StepReason, StepResult, StepStatus,
    WindowsUpdateChoice,
};
use crate::safety::rollback::{RegistryTarget, RollbackFilter};
use crate::safety::state_log::{Journal, RegistryRecord};
use crate::safety::{MutationOutcome, Safety};
use crate::win::registry::{self, Hive, Key, RawValue, RegValue};
use crate::win::scm::{Scm, StartType, READ_ACCESS};
use crate::{Error, Result};

pub const UX_SETTINGS: &str = r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings";
pub const WU_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
pub const WU_POLICY_AU: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU";
pub const REBOOT_REQUIRED_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired";
/// Longest pause, in days (five weeks, as in Settings).
pub const MAX_PAUSE_DAYS: u32 = 35;
/// Longest feature update delay, in days.
pub const MAX_DEFER_DAYS: u32 = 365;
/// Longest span of active hours.
pub const MAX_ACTIVE_SPAN: u32 = 18;
pub const HOME_DEFER_TEXT: &str = "Windows 11 Home ignores this setting. On Home, a new Windows \
     version installs only when you choose it in Settings, until your current version nears \
     the end of its support.";
pub const HOME_DRIVERS_CAVEAT: &str =
    "Microsoft documents this setting for Pro and higher; Windows 11 Home may ignore it.";
pub const POLICY_CAVEAT: &str =
    "This is a policy, so Windows Settings will say some settings are managed by your organization.";
pub const PAUSE_POLICY_TEXT: &str = "Your organization turned off pausing updates on this PC.";
pub const PAUSE_LIMIT_TEXT: &str =
    "Updates are already paused for the 5 weeks Windows allows from the start of this pause.";
pub const ACTIVE_HOURS_POLICY_TEXT: &str = "Active hours are set by a policy on this PC.";
pub const WSUS_NOTE: &str = "Windows Update uses an update server set by your organization, so \
     Cairn's settings may have no effect.";
pub const NO_AUTO_UPDATE_NOTE: &str = "Automatic updates are turned off by a policy on this PC.";
pub const NO_ACCESS_NOTE: &str = "A policy blocks access to Windows Update on this PC.";
pub const JOURNAL_UNREADABLE: &str =
    "History could not be read, so Cairn can't tell which settings it changed.";
pub const SERVICE_DISABLED_WARNING: &str = "The Windows Update service is disabled, so Windows \
     doesn't check for updates and these settings have no effect.";
pub const EDITION_UNKNOWN_WARNING: &str =
    "Windows' edition could not be read, so Cairn treats this PC as Pro or higher.";

const SERVICE_NAME: &str = "wuauserv";
const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

const PAUSE_FEATURE_START: &str = "PauseFeatureUpdatesStartTime";
const PAUSE_QUALITY_START: &str = "PauseQualityUpdatesStartTime";
const PAUSE_START: &str = "PauseUpdatesStartTime";
const PAUSE_FEATURE_END: &str = "PauseFeatureUpdatesEndTime";
const PAUSE_QUALITY_END: &str = "PauseQualityUpdatesEndTime";
const PAUSE_EXPIRY: &str = "PauseUpdatesExpiryTime";
const ACTIVE_START: &str = "ActiveHoursStart";
const ACTIVE_END: &str = "ActiveHoursEnd";
const SMART_ACTIVE: &str = "SmartActiveHoursState";
const EXCLUDE_DRIVERS: &str = "ExcludeWUDriversInQualityUpdate";
const DEFER_DAYS: &str = "DeferFeatureUpdatesPeriodInDays";
const DEFER_ON: &str = "DeferFeatureUpdates";
const RESTART_NOTIFY: &str = "RestartNotificationsAllowed2";
const POLICY_NO_PAUSE: &str = "SetDisablePauseUXAccess";
const POLICY_SET_ACTIVE: &str = "SetActiveHours";
const POLICY_WU_SERVER: &str = "WUServer";
const POLICY_NO_ACCESS: &str = "DisableWindowsUpdateAccess";
const AU_USE_WU_SERVER: &str = "UseWUServer";
const AU_NO_AUTO_UPDATE: &str = "NoAutoUpdate";

const KEY_ACTIVE_HOURS: &str = "windows_update:active_hours";
const KEY_RESTART_NOTIFY: &str = "windows_update:restart_notify";
const KEY_EXCLUDE_DRIVERS: &str = "windows_update:exclude_drivers";
const KEY_DEFER_FEATURE: &str = "windows_update:defer_feature";
const TITLE_ACTIVE_HOURS: &str = "Active hours";
const TITLE_RESTART_NOTIFY: &str = "Restart notifications";
const TITLE_EXCLUDE_DRIVERS: &str = "Exclude drivers from quality updates";
const TITLE_DEFER_FEATURE: &str = "Defer feature updates";
const AUTOMATIC_HOURS_TEXT: &str = "Windows adjusts them automatically";
const DRIVERS_OFF_TEXT: &str = "Drivers are left out of quality updates";
const NOT_IN_PROFILE: &str = "This profile doesn't set this Windows Update setting.";
const UNKNOWN_KEY: &str = "Cairn doesn't know this Windows Update setting.";

// ───────────────────────────── Settings ─────────────────────────────

/// One Windows Update setting Cairn offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WuSettingId {
    Pause,
    ActiveHours,
    ExcludeDrivers,
    DeferFeature,
    RestartNotify,
}

impl WuSettingId {
    /// Every setting, in display order.
    pub const ALL: [WuSettingId; 5] = [
        WuSettingId::Pause,
        WuSettingId::ActiveHours,
        WuSettingId::ExcludeDrivers,
        WuSettingId::DeferFeature,
        WuSettingId::RestartNotify,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WuSettingId::Pause => "pause",
            WuSettingId::ActiveHours => "active_hours",
            WuSettingId::ExcludeDrivers => "exclude_drivers",
            WuSettingId::DeferFeature => "defer_feature",
            WuSettingId::RestartNotify => "restart_notify",
        }
    }

    /// Ignores ASCII case and accepts '-' for '_'.
    pub fn parse(text: &str) -> Option<WuSettingId> {
        let text = text.trim().replace('-', "_");
        WuSettingId::ALL
            .into_iter()
            .find(|id| id.as_str().eq_ignore_ascii_case(&text))
    }

    /// The id History and the catalog know it by ("wu.pause", …).
    pub fn catalog_id(self) -> &'static str {
        match self {
            WuSettingId::Pause => "wu.pause",
            WuSettingId::ActiveHours => "wu.active_hours",
            WuSettingId::ExcludeDrivers => "wu.exclude_drivers",
            WuSettingId::DeferFeature => "wu.defer_feature",
            WuSettingId::RestartNotify => "wu.restart_notify",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            WuSettingId::Pause => "Pause updates",
            WuSettingId::ActiveHours => "Active hours",
            WuSettingId::ExcludeDrivers => "Skip drivers in Windows Update",
            WuSettingId::DeferFeature => "Delay feature updates",
            WuSettingId::RestartNotify => "Notify me before restarting",
        }
    }

    /// The title of the setting's group in History.
    pub fn history_title(self) -> &'static str {
        match self {
            WuSettingId::Pause => "Windows Update: pause",
            WuSettingId::ActiveHours => "Windows Update: active hours",
            WuSettingId::ExcludeDrivers => "Windows Update: skip drivers",
            WuSettingId::DeferFeature => "Windows Update: delay feature updates",
            WuSettingId::RestartNotify => "Windows Update: restart notification",
        }
    }

    /// The values the setting consists of, in target order.
    fn values(self) -> &'static [(Place, &'static str)] {
        match self {
            WuSettingId::Pause => &[
                (Place::Ux, PAUSE_FEATURE_START),
                (Place::Ux, PAUSE_QUALITY_START),
                (Place::Ux, PAUSE_START),
                (Place::Ux, PAUSE_FEATURE_END),
                (Place::Ux, PAUSE_QUALITY_END),
                (Place::Ux, PAUSE_EXPIRY),
            ],
            WuSettingId::ActiveHours => &[
                (Place::Ux, ACTIVE_START),
                (Place::Ux, ACTIVE_END),
                (Place::Ux, SMART_ACTIVE),
            ],
            WuSettingId::ExcludeDrivers => &[(Place::Policy, EXCLUDE_DRIVERS)],
            WuSettingId::DeferFeature => &[(Place::Policy, DEFER_DAYS), (Place::Policy, DEFER_ON)],
            WuSettingId::RestartNotify => &[(Place::Ux, RESTART_NOTIFY)],
        }
    }
}

/// Which of the layout's keys a value lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Ux,
    Policy,
    PolicyAu,
}

/// Where the values live. [`WuLayout::system`] is HKLM with the constants above; tests point
/// it at `HKCU\Software\PCOptimizer\SelfTest\Updates\<test>\{UX,Policy,PolicyAU,Reboot}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WuLayout {
    pub hive: Hive,
    pub ux: String,
    pub policy: String,
    pub policy_au: String,
    pub reboot_required: String,
}

impl WuLayout {
    pub fn system() -> WuLayout {
        WuLayout {
            hive: Hive::LocalMachine,
            ux: UX_SETTINGS.to_string(),
            policy: WU_POLICY.to_string(),
            policy_au: WU_POLICY_AU.to_string(),
            reboot_required: REBOOT_REQUIRED_KEY.to_string(),
        }
    }

    /// The registry values of setting `id`, which undo together.
    pub fn targets(&self, id: WuSettingId) -> Vec<RegistryTarget> {
        id.values()
            .iter()
            .map(|&(place, name)| self.target(place, name))
            .collect()
    }

    fn key(&self, place: Place) -> &str {
        match place {
            Place::Ux => &self.ux,
            Place::Policy => &self.policy,
            Place::PolicyAu => &self.policy_au,
        }
    }

    fn target(&self, place: Place, name: &str) -> RegistryTarget {
        RegistryTarget {
            hive: self.hive,
            key_path: self.key(place).to_string(),
            value_name: name.to_string(),
        }
    }
}

/// "HKLM\<key>\<value>", exactly as [`RegistryRecord::target`] words it.
fn target_text(target: &RegistryTarget) -> String {
    let name = if target.value_name.is_empty() {
        "(Default)"
    } else {
        &target.value_name
    };
    format!("{}\\{}\\{}", target.hive.short(), target.key_path, name)
}

// ───────────────────────────── Environment ─────────────────────────────

/// Windows' edition and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Edition {
    /// `EditionID` ("Core", "Professional", …); empty when it cannot be read.
    pub id: String,
    /// Product name for display ("Windows 11 Home").
    pub name: String,
    /// A Home edition (the id starts with "Core").
    pub home: bool,
    /// `DisplayVersion` ("25H2").
    pub version: Option<String>,
    /// "{CurrentBuildNumber}.{UBR}".
    pub build: String,
}

impl Edition {
    pub(crate) fn from_parts(
        id: Option<&str>,
        product: Option<&str>,
        version: Option<&str>,
        build: Option<&str>,
        ubr: Option<u32>,
    ) -> Edition {
        let id = id.map(str::trim).unwrap_or_default().to_string();
        let build_text = build.map(str::trim).unwrap_or_default();
        let build_number = build_text.parse::<u32>().unwrap_or(0);
        let build = match (build_text.is_empty(), ubr) {
            (true, _) => String::new(),
            (false, Some(ubr)) => format!("{build_text}.{ubr}"),
            (false, None) => build_text.to_string(),
        };
        Edition {
            home: id.starts_with("Core"),
            name: crate::sysinfo::os::product_name(product, Some(&id), build_number),
            version: version
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            build,
            id,
        }
    }
}

fn reg_text(value: Option<RegValue>) -> Option<String> {
    match value? {
        RegValue::Sz(s) | RegValue::ExpandSz(s) => Some(s),
        _ => None,
    }
}

fn reg_dword(value: Option<RegValue>) -> Option<u32> {
    match value? {
        RegValue::Dword(v) => Some(v),
        _ => None,
    }
}

fn system_edition() -> Edition {
    let read = |name: &str| {
        registry::read_value(Hive::LocalMachine, CURRENT_VERSION, name)
            .ok()
            .flatten()
    };
    let build = reg_text(read("CurrentBuildNumber")).or_else(|| reg_text(read("CurrentBuild")));
    Edition::from_parts(
        reg_text(read("EditionID")).as_deref(),
        reg_text(read("ProductName")).as_deref(),
        reg_text(read("DisplayVersion")).as_deref(),
        build.as_deref(),
        reg_dword(read("UBR")),
    )
}

fn system_service_start() -> Result<Option<StartType>> {
    let scm = Scm::connect()?;
    match scm.open(SERVICE_NAME, READ_ACCESS)? {
        Some(service) => Ok(Some(service.config()?.start_type)),
        None => Ok(None),
    }
}

/// The system facts the settings read; tests substitute their own.
#[derive(Clone, Copy)]
pub struct WuEnv {
    pub edition: fn() -> Edition,
    /// Start type of the Windows Update service (`wuauserv`); `None` when it is missing.
    pub service_start: fn() -> Result<Option<StartType>>,
    pub now: fn() -> DateTime<Utc>,
}

impl fmt::Debug for WuEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WuEnv").finish_non_exhaustive()
    }
}

impl WuEnv {
    /// This PC.
    pub const SYSTEM: WuEnv = WuEnv {
        edition: system_edition,
        service_start: system_service_start,
        now: Utc::now,
    };
}

/// How the Windows Update service starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Automatic,
    Manual,
    Disabled,
    Missing,
    Unknown,
}

impl ServiceState {
    fn from_start(start: &Result<Option<StartType>>) -> ServiceState {
        match start {
            Ok(Some(StartType::Automatic | StartType::Boot | StartType::System)) => {
                ServiceState::Automatic
            }
            Ok(Some(StartType::Manual)) => ServiceState::Manual,
            Ok(Some(StartType::Disabled)) => ServiceState::Disabled,
            Ok(None) => ServiceState::Missing,
            Err(_) => ServiceState::Unknown,
        }
    }
}

// ───────────────────────────── State ─────────────────────────────

/// A setting's current value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WuValue {
    /// `until` and `started` are RFC 3339 UTC. `expired`: pause values exist, but their end
    /// has passed.
    Pause {
        paused: bool,
        until: Option<String>,
        started: Option<String>,
        expired: bool,
    },
    /// `policy`: a policy sets the hours, then `start` and `end` are the policy's.
    ActiveHours {
        automatic: bool,
        start: Option<u32>,
        end: Option<u32>,
        policy: bool,
    },
    Switch {
        on: bool,
    },
    Defer {
        days: Option<u32>,
    },
}

/// One setting as the Windows Update view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WuSetting {
    pub id: WuSettingId,
    pub title: &'static str,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub caveat: Option<String>,
    pub value: WuValue,
    /// An active journal record exists for one of `targets`.
    pub by_cairn: bool,
    /// The current value of some journaled target differs from its recorded original (Undo
    /// changes something).
    pub differs: bool,
    pub targets: Vec<RegistryTarget>,
}

/// The Windows Update view's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WuState {
    pub edition: Edition,
    pub service: ServiceState,
    /// Windows waits for a restart to finish installing updates.
    pub restart_pending: bool,
    /// Organization policies that limit what these settings do.
    pub managed: Vec<String>,
    pub settings: Vec<WuSetting>,
    pub warnings: Vec<String>,
}

/// The layout's three keys, opened for reading once.
struct Keys {
    ux: Option<Key>,
    policy: Option<Key>,
    policy_au: Option<Key>,
}

impl Keys {
    fn open(layout: &WuLayout) -> Result<Keys> {
        Ok(Keys {
            ux: Key::open(layout.hive, &layout.ux, false)?,
            policy: Key::open(layout.hive, &layout.policy, false)?,
            policy_au: Key::open(layout.hive, &layout.policy_au, false)?,
        })
    }

    fn raw(&self, place: Place, name: &str) -> Result<Option<RawValue>> {
        let key = match place {
            Place::Ux => &self.ux,
            Place::Policy => &self.policy,
            Place::PolicyAu => &self.policy_au,
        };
        match key {
            Some(key) => key.query_raw(name),
            None => Ok(None),
        }
    }

    fn dword(&self, place: Place, name: &str) -> Result<Option<u32>> {
        Ok(reg_dword(self.raw(place, name)?.map(|r| r.decode())))
    }

    fn text(&self, place: Place, name: &str) -> Result<Option<String>> {
        Ok(reg_text(self.raw(place, name)?.map(|r| r.decode())))
    }

    fn time(&self, name: &str) -> Result<Option<DateTime<Utc>>> {
        Ok(self.text(Place::Ux, name)?.as_deref().and_then(parse_time))
    }

    /// The pause values that are set.
    fn pause_times(&self) -> Result<PauseTimes> {
        let until = [self.time(PAUSE_EXPIRY)?, self.time(PAUSE_QUALITY_END)?]
            .into_iter()
            .flatten()
            .max();
        let started = match self.time(PAUSE_START)? {
            Some(t) => Some(t),
            None => self.time(PAUSE_QUALITY_START)?,
        };
        Ok(PauseTimes { until, started })
    }

    /// The pause in effect at `now`, if any.
    fn running_pause(&self, now: DateTime<Utc>) -> Result<Option<RunningPause>> {
        let PauseTimes { until, started } = self.pause_times()?;
        Ok(until
            .filter(|until| *until > now)
            .map(|until| RunningPause { until, started }))
    }

    fn pause_blocked(&self) -> Result<bool> {
        Ok(self.dword(Place::Policy, POLICY_NO_PAUSE)? == Some(1))
    }

    fn active_hours_policy(&self) -> Result<bool> {
        Ok(self.dword(Place::Policy, POLICY_SET_ACTIVE)? == Some(1))
    }
}

/// "2026-10-05T10:00:00Z" (the form Settings writes); any RFC 3339 time is read.
fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// UTC to the second with a "Z", as Settings writes it.
fn format_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// An RFC 3339 time in local time for display ("Mon 12 Oct 2026, 10:00"); text that is not
/// a time is returned as it is.
pub fn local_time_text(rfc3339: &str) -> String {
    match parse_time(rfc3339) {
        Some(time) => time
            .with_timezone(&Local)
            .format("%a %d %b %Y, %H:%M")
            .to_string(),
        None => rfc3339.to_string(),
    }
}

/// A setting's value as `optctl updates` prints it after the setting's title. Pause updates
/// reads "updates are on" or "paused until …", never a bare "on", which would read as the
/// pause being on.
pub fn value_text(value: &WuValue) -> String {
    match value {
        WuValue::Pause {
            paused: true,
            until,
            ..
        } => format!(
            "paused until {}",
            until.as_deref().map_or_else(String::new, local_time_text)
        ),
        WuValue::Pause {
            expired: true,
            until: Some(until),
            ..
        } => format!(
            "updates are on (the last pause ended on {})",
            local_time_text(until)
        ),
        WuValue::Pause { .. } => "updates are on".to_string(),
        WuValue::ActiveHours {
            start: Some(s),
            end: Some(e),
            policy: true,
            ..
        } => format!("{}–{} (set by a policy)", hour_text(*s), hour_text(*e)),
        WuValue::ActiveHours {
            automatic: true, ..
        } => "adjusted automatically".to_string(),
        WuValue::ActiveHours {
            start: Some(s),
            end: Some(e),
            ..
        } => format!("{}–{}", hour_text(*s), hour_text(*e)),
        WuValue::ActiveHours { .. } => "unknown".to_string(),
        WuValue::Switch { on } => on_off(*on).to_string(),
        WuValue::Defer { days: Some(days) } => format!("{days} days"),
        WuValue::Defer { days: None } => "no delay".to_string(),
    }
}

/// The pause values as far as they are set: the later of the two end values Windows reads,
/// and the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PauseTimes {
    until: Option<DateTime<Utc>>,
    started: Option<DateTime<Utc>>,
}

/// A pause in effect: its end and, when it can be read, its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunningPause {
    until: DateTime<Utc>,
    started: Option<DateTime<Utc>>,
}

/// `time` without its fraction of a second.
fn whole_seconds(time: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(time.timestamp(), 0).unwrap_or(time)
}

/// The end `pause` gets when it is extended by `days`: `days` after its end, but at most
/// [`MAX_PAUSE_DAYS`] after it began (after `now` when its start can't be read or lies
/// ahead). `None` when that end is not later than the current one.
fn extended_end(pause: RunningPause, days: u32, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let began = pause.started.map_or(now, |started| started.min(now));
    let limit = whole_seconds(began) + ChronoDuration::days(i64::from(MAX_PAUSE_DAYS));
    let end = (pause.until + ChronoDuration::days(i64::from(days))).min(limit);
    (end > pause.until).then_some(end)
}

fn pause_value(keys: &Keys, now: DateTime<Utc>) -> Result<WuValue> {
    let PauseTimes { until, started } = keys.pause_times()?;
    Ok(WuValue::Pause {
        paused: until.is_some_and(|u| u > now),
        until: until.map(format_time),
        started: started.map(format_time),
        expired: until.is_some_and(|u| u <= now),
    })
}

fn active_hours_value(keys: &Keys) -> Result<WuValue> {
    if keys.active_hours_policy()? {
        return Ok(WuValue::ActiveHours {
            automatic: false,
            start: keys.dword(Place::Policy, ACTIVE_START)?,
            end: keys.dword(Place::Policy, ACTIVE_END)?,
            policy: true,
        });
    }
    let start = keys.dword(Place::Ux, ACTIVE_START)?;
    let end = keys.dword(Place::Ux, ACTIVE_END)?;
    let smart = keys.dword(Place::Ux, SMART_ACTIVE)?;
    Ok(WuValue::ActiveHours {
        automatic: smart == Some(1) || start.is_none() || end.is_none(),
        start,
        end,
        policy: false,
    })
}

fn defer_value(keys: &Keys) -> Result<WuValue> {
    let on = keys.dword(Place::Policy, DEFER_ON)? == Some(1);
    let days = keys.dword(Place::Policy, DEFER_DAYS)?;
    Ok(WuValue::Defer {
        days: if on { days } else { None },
    })
}

/// "08:00".
fn hour_text(hour: u32) -> String {
    format!("{hour:02}:00")
}

/// Whether Cairn changed one of `id`'s values (an active record), and whether one of those
/// values now differs from its record's original.
fn journal_marks(
    records: &[RegistryRecord],
    layout: &WuLayout,
    keys: &Keys,
    id: WuSettingId,
) -> Result<(bool, bool)> {
    let (mut by_cairn, mut differs) = (false, false);
    for &(place, name) in id.values() {
        let key_path = layout.key(place);
        let Some(record) = records.iter().find(|r| {
            r.hive == layout.hive
                && r.key_path.eq_ignore_ascii_case(key_path)
                && r.value_name.eq_ignore_ascii_case(name)
        }) else {
            continue;
        };
        by_cairn = true;
        if keys.raw(place, name)? != record.original {
            differs = true;
        }
    }
    Ok((by_cairn, differs))
}

/// The state of every setting on this PC. Registry reads only; `journal` tells which
/// settings Cairn changed (without it, none, with a warning).
pub fn wu_state(journal: Option<&Journal>) -> Result<WuState> {
    wu_state_with(&WuLayout::system(), WuEnv::SYSTEM, journal)
}

pub(crate) fn wu_state_with(
    layout: &WuLayout,
    env: WuEnv,
    journal: Option<&Journal>,
) -> Result<WuState> {
    let keys = Keys::open(layout)?;
    let edition = (env.edition)();
    let mut warnings = Vec::new();
    if edition.id.is_empty() {
        warnings.push(EDITION_UNKNOWN_WARNING.to_string());
    }
    let records = match journal.map(Journal::active_registry) {
        Some(Ok(records)) => records,
        Some(Err(e)) => {
            tracing::warn!(error = %e, "cannot read the journal's registry records");
            warnings.push(JOURNAL_UNREADABLE.to_string());
            Vec::new()
        }
        None => {
            warnings.push(JOURNAL_UNREADABLE.to_string());
            Vec::new()
        }
    };

    let mut managed = Vec::new();
    let wsus = keys
        .text(Place::Policy, POLICY_WU_SERVER)?
        .is_some_and(|s| !s.trim().is_empty());
    if wsus || keys.dword(Place::PolicyAu, AU_USE_WU_SERVER)? == Some(1) {
        managed.push(WSUS_NOTE.to_string());
    }
    if keys.dword(Place::PolicyAu, AU_NO_AUTO_UPDATE)? == Some(1) {
        managed.push(NO_AUTO_UPDATE_NOTE.to_string());
    }
    if keys.dword(Place::Policy, POLICY_NO_ACCESS)? == Some(1) {
        managed.push(NO_ACCESS_NOTE.to_string());
    }

    let now = (env.now)();
    let mut settings = Vec::with_capacity(WuSettingId::ALL.len());
    for id in WuSettingId::ALL {
        let (by_cairn, differs) = journal_marks(&records, layout, &keys, id)?;
        let mut setting = WuSetting {
            id,
            title: id.title(),
            available: true,
            unavailable_reason: None,
            caveat: None,
            value: WuValue::Switch { on: false },
            by_cairn,
            differs,
            targets: layout.targets(id),
        };
        match id {
            WuSettingId::Pause => {
                setting.value = pause_value(&keys, now)?;
                if keys.pause_blocked()? {
                    setting.available = false;
                    setting.unavailable_reason = Some(PAUSE_POLICY_TEXT.to_string());
                }
            }
            WuSettingId::ActiveHours => {
                let value = active_hours_value(&keys)?;
                if let WuValue::ActiveHours {
                    policy: true,
                    start,
                    end,
                    ..
                } = value
                {
                    setting.available = false;
                    setting.unavailable_reason = Some(match (start, end) {
                        (Some(s), Some(e)) => format!(
                            "Active hours are set by a policy on this PC ({}–{}).",
                            hour_text(s),
                            hour_text(e)
                        ),
                        _ => ACTIVE_HOURS_POLICY_TEXT.to_string(),
                    });
                }
                setting.value = value;
            }
            WuSettingId::ExcludeDrivers => {
                setting.value = WuValue::Switch {
                    on: keys.dword(Place::Policy, EXCLUDE_DRIVERS)? == Some(1),
                };
                setting.caveat = Some(
                    if edition.home {
                        HOME_DRIVERS_CAVEAT
                    } else {
                        POLICY_CAVEAT
                    }
                    .to_string(),
                );
            }
            WuSettingId::DeferFeature => {
                setting.value = defer_value(&keys)?;
                if edition.home {
                    setting.available = false;
                    setting.unavailable_reason = Some(HOME_DEFER_TEXT.to_string());
                } else {
                    setting.caveat = Some(POLICY_CAVEAT.to_string());
                }
            }
            WuSettingId::RestartNotify => {
                setting.value = WuValue::Switch {
                    on: keys.dword(Place::Ux, RESTART_NOTIFY)? == Some(1),
                };
            }
        }
        settings.push(setting);
    }

    Ok(WuState {
        service: ServiceState::from_start(&(env.service_start)()),
        restart_pending: registry::exists(layout.hive, &layout.reboot_required).unwrap_or(false),
        edition,
        managed,
        settings,
        warnings,
    })
}

// ───────────────────────────── Changes ─────────────────────────────

/// A change to one setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WuChange {
    /// Pauses updates for `days` from now. A running pause is extended instead: its end moves
    /// `days` later, to at most [`MAX_PAUSE_DAYS`] after it began, and its start stays.
    Pause {
        days: u32,
    },
    Resume,
    ActiveHours {
        start: u32,
        end: u32,
    },
    AutomaticActiveHours,
    ExcludeDrivers(bool),
    /// `None`: don't delay.
    DeferFeature(Option<u32>),
    RestartNotify(bool),
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

fn parse_on_off(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Hours of active hours from `start` to `end`, across midnight when `end` is earlier.
pub fn active_span(start: u32, end: u32) -> u32 {
    (end + 24 - start) % 24
}

impl WuChange {
    pub fn setting(&self) -> WuSettingId {
        match self {
            WuChange::Pause { .. } | WuChange::Resume => WuSettingId::Pause,
            WuChange::ActiveHours { .. } | WuChange::AutomaticActiveHours => {
                WuSettingId::ActiveHours
            }
            WuChange::ExcludeDrivers(_) => WuSettingId::ExcludeDrivers,
            WuChange::DeferFeature(_) => WuSettingId::DeferFeature,
            WuChange::RestartNotify(_) => WuSettingId::RestartNotify,
        }
    }

    /// Checks the ranges; the error is the user text.
    pub fn validate(&self) -> Result<()> {
        let invalid = |text: String| -> Result<()> { Err(Error::Other(text)) };
        match *self {
            WuChange::Pause { days } if !(1..=MAX_PAUSE_DAYS).contains(&days) => invalid(format!(
                "Updates can be paused for 1 to {MAX_PAUSE_DAYS} days."
            )),
            WuChange::ActiveHours { start, end } if start > 23 || end > 23 => {
                invalid("Active hours start and end must be hours from 0 to 23.".into())
            }
            WuChange::ActiveHours { start, end } if start == end => {
                invalid("Start and end must differ.".into())
            }
            WuChange::ActiveHours { start, end } if active_span(start, end) > MAX_ACTIVE_SPAN => {
                invalid(format!(
                    "Active hours can span at most {MAX_ACTIVE_SPAN} hours."
                ))
            }
            WuChange::DeferFeature(Some(days)) if !(1..=MAX_DEFER_DAYS).contains(&days) => invalid(
                format!("Feature updates can be delayed by 1 to {MAX_DEFER_DAYS} days."),
            ),
            _ => Ok(()),
        }
    }

    /// The journal session's label, such as "windows update: pause 14 days".
    pub fn label(&self) -> String {
        let what = match *self {
            WuChange::Pause { days } => format!("pause {days} days"),
            WuChange::Resume => "resume".to_string(),
            WuChange::ActiveHours { start, end } => {
                format!("active hours {}-{}", hour_text(start), hour_text(end))
            }
            WuChange::AutomaticActiveHours => "automatic active hours".to_string(),
            WuChange::ExcludeDrivers(on) => format!("skip drivers {}", on_off(on)),
            WuChange::DeferFeature(Some(days)) => format!("delay feature updates {days} days"),
            WuChange::DeferFeature(None) => "delay feature updates off".to_string(),
            WuChange::RestartNotify(on) => format!("restart notification {}", on_off(on)),
        };
        format!("windows update: {what}")
    }

    /// The command-line form: "pause 14|off", "active-hours 8-17|auto",
    /// "exclude-drivers on|off", "defer-feature 90|off", "restart-notify on|off".
    pub fn parse_cli(setting: &str, value: &str) -> Result<WuChange> {
        let id = WuSettingId::parse(setting).ok_or_else(|| {
            let valid: Vec<String> = WuSettingId::ALL
                .iter()
                .map(|id| id.as_str().replace('_', "-"))
                .collect();
            Error::Other(format!(
                "unknown Windows Update setting {setting:?}; expected one of: {}",
                valid.join(", ")
            ))
        })?;
        let value = value.trim();
        let off = value.eq_ignore_ascii_case("off");
        let number = |what: &str| -> Result<u32> {
            value.parse::<u32>().map_err(|_| {
                Error::Other(format!(
                    "{what}: expected a number or \"off\", got {value:?}"
                ))
            })
        };
        let change = match id {
            WuSettingId::Pause if off => WuChange::Resume,
            WuSettingId::Pause => WuChange::Pause {
                days: number("pause")?,
            },
            WuSettingId::ActiveHours
                if value.eq_ignore_ascii_case("auto")
                    || value.eq_ignore_ascii_case("automatic") =>
            {
                WuChange::AutomaticActiveHours
            }
            WuSettingId::ActiveHours => {
                let (start, end) = value
                    .split_once('-')
                    .and_then(|(s, e)| Some((s.trim().parse().ok()?, e.trim().parse().ok()?)))
                    .ok_or_else(|| {
                        Error::Other(format!(
                            "active-hours: expected START-END in hours, such as 8-17, or \"auto\", got {value:?}"
                        ))
                    })?;
                WuChange::ActiveHours { start, end }
            }
            WuSettingId::DeferFeature if off => WuChange::DeferFeature(None),
            WuSettingId::DeferFeature => WuChange::DeferFeature(Some(number("defer-feature")?)),
            WuSettingId::ExcludeDrivers | WuSettingId::RestartNotify => {
                let on = parse_on_off(value).ok_or_else(|| {
                    Error::Other(format!(
                        "{}: expected \"on\" or \"off\", got {value:?}",
                        id.as_str().replace('_', "-")
                    ))
                })?;
                if id == WuSettingId::ExcludeDrivers {
                    WuChange::ExcludeDrivers(on)
                } else {
                    WuChange::RestartNotify(on)
                }
            }
        };
        change.validate()?;
        Ok(change)
    }
}

/// One registry write of a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WuWrite {
    pub target: String,
    /// The value before, `None` when absent.
    pub before: Option<String>,
    /// The value written, `None` for a delete.
    pub after: Option<String>,
    /// `None` in a dry run.
    pub outcome: Option<MutationOutcome>,
}

/// What a change did, or would do in a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WuReport {
    pub dry_run: bool,
    pub setting: WuSettingId,
    /// The journal session; `None` in a dry run.
    pub session_id: Option<i64>,
    pub writes: Vec<WuWrite>,
    pub warnings: Vec<String>,
}

/// A setting as History titles it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WuCatalogEntry {
    pub id: &'static str,
    pub title: &'static str,
    /// Target strings as the journal words them ("HKLM\…\PauseUpdatesExpiryTime").
    pub targets: Vec<String>,
}

/// One setting per entry, with the journal targets of this PC's layout. Static; no I/O.
pub fn wu_catalog() -> Vec<WuCatalogEntry> {
    let layout = WuLayout::system();
    WuSettingId::ALL
        .into_iter()
        .map(|id| WuCatalogEntry {
            id: id.catalog_id(),
            title: id.history_title(),
            targets: layout.targets(id).iter().map(target_text).collect(),
        })
        .collect()
}

/// One registry operation: write `value`, or delete when `None`.
struct Op {
    place: Place,
    name: &'static str,
    value: Option<RegValue>,
}

fn set(place: Place, name: &'static str, value: RegValue) -> Op {
    Op {
        place,
        name,
        value: Some(value),
    }
}

fn delete(place: Place, name: &'static str) -> Op {
    Op {
        place,
        name,
        value: None,
    }
}

/// The writes of `change`, in order: the value Windows reads as effective goes last. A pause
/// with `extend_to` moves only the end values of the running pause there; any other pause
/// starts now.
fn ops(change: &WuChange, now: DateTime<Utc>, extend_to: Option<DateTime<Utc>>) -> Vec<Op> {
    match *change {
        WuChange::Pause { days } => {
            let ends = |end: DateTime<Utc>| {
                let end = format_time(end);
                [PAUSE_FEATURE_END, PAUSE_QUALITY_END, PAUSE_EXPIRY]
                    .into_iter()
                    .map(move |name| set(Place::Ux, name, RegValue::Sz(end.clone())))
            };
            if let Some(end) = extend_to {
                return ends(end).collect();
            }
            let start = whole_seconds(now);
            let text = format_time(start);
            let mut ops: Vec<Op> = [PAUSE_FEATURE_START, PAUSE_QUALITY_START, PAUSE_START]
                .into_iter()
                .map(|name| set(Place::Ux, name, RegValue::Sz(text.clone())))
                .collect();
            ops.extend(ends(start + ChronoDuration::days(i64::from(days))));
            ops
        }
        WuChange::Resume => [
            PAUSE_EXPIRY,
            PAUSE_QUALITY_END,
            PAUSE_FEATURE_END,
            PAUSE_START,
            PAUSE_QUALITY_START,
            PAUSE_FEATURE_START,
        ]
        .into_iter()
        .map(|name| delete(Place::Ux, name))
        .collect(),
        WuChange::ActiveHours { start, end } => vec![
            set(Place::Ux, ACTIVE_START, RegValue::Dword(start)),
            set(Place::Ux, ACTIVE_END, RegValue::Dword(end)),
            set(Place::Ux, SMART_ACTIVE, RegValue::Dword(0)),
        ],
        WuChange::AutomaticActiveHours => vec![set(Place::Ux, SMART_ACTIVE, RegValue::Dword(1))],
        WuChange::ExcludeDrivers(true) => {
            vec![set(Place::Policy, EXCLUDE_DRIVERS, RegValue::Dword(1))]
        }
        WuChange::ExcludeDrivers(false) => vec![delete(Place::Policy, EXCLUDE_DRIVERS)],
        WuChange::DeferFeature(Some(days)) => vec![
            set(Place::Policy, DEFER_DAYS, RegValue::Dword(days)),
            set(Place::Policy, DEFER_ON, RegValue::Dword(1)),
        ],
        WuChange::DeferFeature(None) => vec![
            delete(Place::Policy, DEFER_ON),
            delete(Place::Policy, DEFER_DAYS),
        ],
        WuChange::RestartNotify(true) => vec![set(Place::Ux, RESTART_NOTIFY, RegValue::Dword(1))],
        WuChange::RestartNotify(false) => vec![delete(Place::Ux, RESTART_NOTIFY)],
    }
}

/// Why `change` is refused on this PC, before anything is written.
fn refusal(keys: &Keys, edition: &Edition, change: &WuChange) -> Result<Option<&'static str>> {
    Ok(match change {
        WuChange::DeferFeature(Some(_)) if edition.home => Some(HOME_DEFER_TEXT),
        WuChange::Pause { .. } if keys.pause_blocked()? => Some(PAUSE_POLICY_TEXT),
        WuChange::ActiveHours { .. } | WuChange::AutomaticActiveHours
            if keys.active_hours_policy()? =>
        {
            Some(ACTIVE_HOURS_POLICY_TEXT)
        }
        _ => None,
    })
}

fn change_warnings(env: WuEnv, edition: &Edition, change: &WuChange) -> Vec<String> {
    let mut warnings = Vec::new();
    if ServiceState::from_start(&(env.service_start)()) == ServiceState::Disabled {
        warnings.push(SERVICE_DISABLED_WARNING.to_string());
    }
    if edition.home && *change == WuChange::ExcludeDrivers(true) {
        warnings.push(HOME_DRIVERS_CAVEAT.to_string());
    }
    warnings
}

fn planned_write(layout: &WuLayout, keys: &Keys, op: &Op) -> Result<WuWrite> {
    Ok(WuWrite {
        target: target_text(&layout.target(op.place, op.name)),
        before: keys
            .raw(op.place, op.name)?
            .map(|raw| raw.decode().display()),
        after: op.value.as_ref().map(RegValue::display),
        outcome: None,
    })
}

/// Performs `ops` in order under `safety`. The writes done before a failure are returned
/// with it; they stay journaled.
fn apply_ops(safety: &Safety, layout: &WuLayout, ops: &[Op]) -> (Vec<WuWrite>, Option<Error>) {
    let mut writes = Vec::with_capacity(ops.len());
    for op in ops {
        let key_path = layout.key(op.place);
        let before = match registry::read_value(layout.hive, key_path, op.name) {
            Ok(value) => value.map(|v| v.display()),
            Err(e) => return (writes, Some(e)),
        };
        let outcome = match &op.value {
            Some(value) => safety.set_registry_value(layout.hive, key_path, op.name, value),
            None => safety.delete_registry_value(layout.hive, key_path, op.name),
        };
        match outcome {
            Ok(outcome) => writes.push(WuWrite {
                target: target_text(&layout.target(op.place, op.name)),
                before,
                after: op.value.as_ref().map(RegValue::display),
                outcome: Some(outcome),
            }),
            Err(e) => return (writes, Some(e)),
        }
    }
    (writes, None)
}

/// Plans `change` and, unless `dry_run`, applies it in a journal session from `begin`
/// (production: [`Safety::begin`] requiring elevation, so a standard user gets
/// [`Error::NotElevated`] before any session). Validation and the edition and policy
/// refusals come first, then, for a pause that runs already, the refusal when its end can't
/// move later within [`MAX_PAUSE_DAYS`] of its start ([`PAUSE_LIMIT_TEXT`]); a dry run never
/// calls `begin`. On a failed write the earlier writes stay journaled and can be undone.
pub fn plan_or_apply_wu(
    change: &WuChange,
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<WuReport> {
    plan_or_apply_wu_with(&WuLayout::system(), WuEnv::SYSTEM, change, dry_run, begin)
}

pub(crate) fn plan_or_apply_wu_with(
    layout: &WuLayout,
    env: WuEnv,
    change: &WuChange,
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<WuReport> {
    change.validate()?;
    let keys = Keys::open(layout)?;
    let edition = (env.edition)();
    if let Some(text) = refusal(&keys, &edition, change)? {
        return Err(Error::Other(text.to_string()));
    }
    let now = (env.now)();
    let extend_to = match *change {
        WuChange::Pause { days } => match keys.running_pause(now)? {
            Some(pause) => Some(
                extended_end(pause, days, now)
                    .ok_or_else(|| Error::Other(PAUSE_LIMIT_TEXT.to_string()))?,
            ),
            None => None,
        },
        _ => None,
    };
    let ops = ops(change, now, extend_to);
    let warnings = change_warnings(env, &edition, change);
    if dry_run {
        let writes = ops
            .iter()
            .map(|op| planned_write(layout, &keys, op))
            .collect::<Result<Vec<_>>>()?;
        return Ok(WuReport {
            dry_run: true,
            setting: change.setting(),
            session_id: None,
            writes,
            warnings,
        });
    }
    drop(keys);
    let safety = begin()?;
    let (writes, error) = apply_ops(&safety, layout, &ops);
    if let Some(e) = error {
        return Err(e);
    }
    Ok(WuReport {
        dry_run: false,
        setting: change.setting(),
        session_id: Some(safety.session_id()),
        writes,
        warnings,
    })
}

/// Runs `optctl updates wu-set` for `change` and returns the lines it prints. With `dry_run`,
/// or without `yes`, it is a dry run under the heading "dry run: nothing was changed" and
/// `begin` is not called. Otherwise the change is made at once, without a dry run before it,
/// so the lines list only the writes made, with their outcomes and the journal session.
pub fn cli_set_lines(
    change: &WuChange,
    dry_run: bool,
    yes: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<Vec<String>> {
    cli_set_lines_with(
        &WuLayout::system(),
        WuEnv::SYSTEM,
        change,
        dry_run,
        yes,
        begin,
    )
}

pub(crate) fn cli_set_lines_with(
    layout: &WuLayout,
    env: WuEnv,
    change: &WuChange,
    dry_run: bool,
    yes: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<Vec<String>> {
    let report = plan_or_apply_wu_with(layout, env, change, dry_run || !yes, begin)?;
    Ok(report_lines(&report))
}

/// `report` as `optctl updates wu-set` prints it: the heading of a dry run, each write with
/// its value before and after and, once made, its outcome, then the journal session and the
/// warnings.
fn report_lines(report: &WuReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(report.writes.len() + report.warnings.len() + 2);
    if report.dry_run {
        lines.push("dry run: nothing was changed".to_string());
    }
    for write in &report.writes {
        let before = write.before.as_deref().unwrap_or("(absent)");
        let after = write.after.as_deref().unwrap_or("(deleted)");
        let outcome = match &write.outcome {
            None => String::new(),
            Some(MutationOutcome::Applied) => "  [applied]".to_string(),
            Some(MutationOutcome::AlreadyInDesiredState) => {
                "  [already_in_desired_state]".to_string()
            }
            Some(MutationOutcome::Skipped(reason)) => format!("  [skipped: {reason}]"),
        };
        lines.push(format!("  {}: {before} → {after}{outcome}", write.target));
    }
    if let Some(session) = report.session_id {
        lines.push(format!("journal session     {session}"));
    }
    lines.extend(report.warnings.iter().map(|w| format!("warning: {w}")));
    lines
}

// ───────────────────────────── Profiles ─────────────────────────────

/// The Windows Update settings Cairn set that still differ from their baseline, as a profile
/// section. Pause is never part of a profile.
pub fn profile_current(journal: &Journal) -> Result<WindowsUpdateChoice> {
    let state = wu_state_with(&WuLayout::system(), WuEnv::SYSTEM, Some(journal))?;
    Ok(choice_from_state(&state))
}

pub(crate) fn choice_from_state(state: &WuState) -> WindowsUpdateChoice {
    let mut choice = WindowsUpdateChoice::default();
    for setting in state.settings.iter().filter(|s| s.by_cairn && s.differs) {
        match (setting.id, &setting.value) {
            (
                WuSettingId::ActiveHours,
                WuValue::ActiveHours {
                    automatic,
                    start,
                    end,
                    policy: false,
                },
            ) => {
                if *automatic {
                    choice.active_hours = Some(ActiveHoursChoice {
                        automatic: true,
                        start: None,
                        end: None,
                    });
                } else if let (Some(s), Some(e)) = (start, end) {
                    if let (Ok(s), Ok(e)) = (u8::try_from(*s), u8::try_from(*e)) {
                        if s < 24 && e < 24 {
                            choice.active_hours = Some(ActiveHoursChoice {
                                automatic: false,
                                start: Some(s),
                                end: Some(e),
                            });
                        }
                    }
                }
            }
            (WuSettingId::RestartNotify, WuValue::Switch { on }) => {
                choice.restart_notify = Some(*on);
            }
            (WuSettingId::ExcludeDrivers, WuValue::Switch { on: true }) => {
                choice.exclude_drivers = true;
            }
            (WuSettingId::DeferFeature, WuValue::Defer { days: Some(days) })
                if (1..=MAX_DEFER_DAYS).contains(days) =>
            {
                choice.defer_feature_days = Some(*days);
            }
            _ => {}
        }
    }
    choice
}

/// The caution a profile row that delays feature updates carries, so it starts unselected:
/// the confirmation the Windows Update view asks for.
pub fn defer_caution(days: u32) -> String {
    format!(
        "Windows Update waits {days} days after each new Windows version is released before \
         offering it. {POLICY_CAVEAT}"
    )
}

fn days_text(days: u32) -> String {
    format!("{days} day{}", if days == 1 { "" } else { "s" })
}

/// The changes a profile section asks for, one per set field, keyed as profile steps.
fn profile_changes(want: &WindowsUpdateChoice) -> Vec<(&'static str, &'static str, WuChange)> {
    let mut changes = Vec::new();
    if let Some(hours) = want.active_hours {
        let change = match (hours.automatic, hours.start, hours.end) {
            (false, Some(start), Some(end)) => WuChange::ActiveHours {
                start: u32::from(start),
                end: u32::from(end),
            },
            _ => WuChange::AutomaticActiveHours,
        };
        changes.push((KEY_ACTIVE_HOURS, TITLE_ACTIVE_HOURS, change));
    }
    if let Some(on) = want.restart_notify {
        changes.push((
            KEY_RESTART_NOTIFY,
            TITLE_RESTART_NOTIFY,
            WuChange::RestartNotify(on),
        ));
    }
    if want.exclude_drivers {
        changes.push((
            KEY_EXCLUDE_DRIVERS,
            TITLE_EXCLUDE_DRIVERS,
            WuChange::ExcludeDrivers(true),
        ));
    }
    if let Some(days) = want.defer_feature_days {
        changes.push((
            KEY_DEFER_FEATURE,
            TITLE_DEFER_FEATURE,
            WuChange::DeferFeature(Some(days)),
        ));
    }
    changes
}

/// What a profile row says the change sets.
fn change_detail(change: &WuChange, edition: &Edition) -> String {
    match *change {
        WuChange::ActiveHours { start, end } => {
            format!("{} to {}", hour_text(start), hour_text(end))
        }
        WuChange::AutomaticActiveHours => AUTOMATIC_HOURS_TEXT.to_string(),
        WuChange::RestartNotify(on) => if on { "On" } else { "Off" }.to_string(),
        WuChange::ExcludeDrivers(_) if edition.home => {
            format!("{DRIVERS_OFF_TEXT}  ·  {HOME_DRIVERS_CAVEAT}")
        }
        WuChange::ExcludeDrivers(_) => DRIVERS_OFF_TEXT.to_string(),
        WuChange::DeferFeature(Some(days)) => days_text(days),
        WuChange::DeferFeature(None) => "Don't delay".to_string(),
        WuChange::Pause { days } => days_text(days),
        WuChange::Resume => "Resume updates".to_string(),
    }
}

/// Whether the current value already is what `change` sets.
fn already_set(state: &WuState, change: &WuChange) -> bool {
    let value = state
        .settings
        .iter()
        .find(|s| s.id == change.setting())
        .map(|s| &s.value);
    match (change, value) {
        (
            WuChange::ActiveHours { start, end },
            Some(WuValue::ActiveHours {
                automatic: false,
                start: Some(s),
                end: Some(e),
                policy: false,
            }),
        ) => s == start && e == end,
        (
            WuChange::AutomaticActiveHours,
            Some(WuValue::ActiveHours {
                automatic,
                policy: false,
                ..
            }),
        ) => *automatic,
        (WuChange::RestartNotify(want), Some(WuValue::Switch { on })) => want == on,
        (WuChange::ExcludeDrivers(want), Some(WuValue::Switch { on })) => want == on,
        (WuChange::DeferFeature(want), Some(WuValue::Defer { days })) => want == days,
        _ => false,
    }
}

fn skipped_step(key: &str, title: &str, reason: StepReason, detail: String) -> SettingStep {
    SettingStep {
        key: key.to_string(),
        title: title.to_string(),
        status: StepStatus::Skipped,
        detail,
        reason: Some(reason),
        caution: None,
    }
}

/// What applying `want` would change, one step per set field. Reads only.
pub fn profile_plan(journal: &Journal, want: &WindowsUpdateChoice) -> Result<Vec<SettingStep>> {
    profile_plan_with(&WuLayout::system(), WuEnv::SYSTEM, journal, want)
}

pub(crate) fn profile_plan_with(
    layout: &WuLayout,
    env: WuEnv,
    journal: &Journal,
    want: &WindowsUpdateChoice,
) -> Result<Vec<SettingStep>> {
    let state = wu_state_with(layout, env, Some(journal))?;
    let keys = Keys::open(layout)?;
    let mut steps = Vec::new();
    for (key, title, change) in profile_changes(want) {
        if let Err(e) = change.validate() {
            steps.push(skipped_step(
                key,
                title,
                StepReason::CannotChange,
                e.to_string(),
            ));
            continue;
        }
        if let Some(text) = refusal(&keys, &state.edition, &change)? {
            let reason = if text == HOME_DEFER_TEXT {
                StepReason::Edition
            } else {
                StepReason::CannotChange
            };
            steps.push(skipped_step(key, title, reason, text.to_string()));
            continue;
        }
        let detail = change_detail(&change, &state.edition);
        if already_set(&state, &change) {
            steps.push(SettingStep {
                key: key.to_string(),
                title: title.to_string(),
                status: StepStatus::Already,
                detail,
                reason: None,
                caution: None,
            });
            continue;
        }
        let caution = match change {
            WuChange::DeferFeature(Some(days)) => Some(defer_caution(days)),
            _ => None,
        };
        steps.push(SettingStep {
            key: key.to_string(),
            title: title.to_string(),
            status: StepStatus::Change,
            detail,
            reason: None,
            caution,
        });
    }
    Ok(steps)
}

/// Applies the fields of `want` named by `keys` under the caller's session, each with the
/// same writes and refusals as the Windows Update view, and returns the step results and
/// the rollback filter of the values written.
pub fn profile_apply_in(
    safety: &Safety,
    want: &WindowsUpdateChoice,
    keys: &[String],
) -> Result<(Vec<StepResult>, RollbackFilter)> {
    profile_apply_in_with(&WuLayout::system(), WuEnv::SYSTEM, safety, want, keys)
}

pub(crate) fn profile_apply_in_with(
    layout: &WuLayout,
    env: WuEnv,
    safety: &Safety,
    want: &WindowsUpdateChoice,
    keys: &[String],
) -> Result<(Vec<StepResult>, RollbackFilter)> {
    let changes = profile_changes(want);
    let edition = (env.edition)();
    let mut results = Vec::with_capacity(keys.len());
    let mut filter = RollbackFilter::default();
    for key in keys {
        let result = |outcome: StepOutcome, details: Vec<String>| StepResult {
            key: key.clone(),
            outcome,
            details,
        };
        let Some((_, _, change)) = changes.iter().find(|(k, _, _)| *k == key.as_str()) else {
            let known = [
                KEY_ACTIVE_HOURS,
                KEY_RESTART_NOTIFY,
                KEY_EXCLUDE_DRIVERS,
                KEY_DEFER_FEATURE,
            ]
            .contains(&key.as_str());
            let text = if known { NOT_IN_PROFILE } else { UNKNOWN_KEY };
            results.push(result(StepOutcome::Skipped, vec![text.to_string()]));
            continue;
        };
        if let Err(e) = change.validate() {
            results.push(result(StepOutcome::Skipped, vec![e.to_string()]));
            continue;
        }
        let refused = Keys::open(layout).and_then(|k| refusal(&k, &edition, change));
        match refused {
            Ok(Some(text)) => {
                results.push(result(StepOutcome::Skipped, vec![text.to_string()]));
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                results.push(result(StepOutcome::Failed, vec![e.to_string()]));
                continue;
            }
        }
        // A profile never pauses updates, so there is no pause to extend.
        let ops = ops(change, (env.now)(), None);
        let (writes, error) = apply_ops(safety, layout, &ops);
        let mut details = Vec::new();
        let mut applied = false;
        for (write, op) in writes.iter().zip(&ops) {
            if write.outcome == Some(MutationOutcome::Applied) {
                applied = true;
                filter.registry.push(layout.target(op.place, op.name));
                details.push(match &write.after {
                    Some(after) => format!("{} = {after}", write.target),
                    None => format!("{} deleted", write.target),
                });
            }
        }
        let outcome = match error {
            Some(e) => {
                details.push(e.to_string());
                StepOutcome::Failed
            }
            None if applied => StepOutcome::Applied,
            None => StepOutcome::AlreadySet,
        };
        results.push(result(outcome, details));
    }
    Ok((results, filter))
}

#[cfg(test)]
mod tests;
