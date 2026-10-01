//! The weekly schedule of the maintenance task and what each run does.
//!
//! A schedule is a day, a time of day, cleanup targets from a fixed allow-list and two
//! read-only checks. It is kept only in the task's command line ([`task_arguments`]), which
//! only administrators can change, and read back with the exact grammar of
//! [`parse_task_arguments`]. The Recycle Bin is never schedulable, and there is no field that
//! could carry a repair.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Days, NaiveDateTime, NaiveTime, Weekday};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::run::{RunOrigin, RunRequest};
use crate::cleanup;
use crate::{Error, Result};

/// Day of the week the maintenance task runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleDay {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl ScheduleDay {
    pub const ALL: [ScheduleDay; 7] = [
        ScheduleDay::Monday,
        ScheduleDay::Tuesday,
        ScheduleDay::Wednesday,
        ScheduleDay::Thursday,
        ScheduleDay::Friday,
        ScheduleDay::Saturday,
        ScheduleDay::Sunday,
    ];

    /// The full English name ("sunday") or its first three letters ("Sun"), ASCII case and
    /// surrounding spaces ignored.
    pub fn parse(s: &str) -> Option<ScheduleDay> {
        let s = s.trim();
        ScheduleDay::ALL.into_iter().find(|day| {
            let label = day.label();
            s.eq_ignore_ascii_case(label) || s.eq_ignore_ascii_case(&label[..3])
        })
    }

    /// "Sunday".
    pub fn label(self) -> &'static str {
        match self {
            ScheduleDay::Monday => "Monday",
            ScheduleDay::Tuesday => "Tuesday",
            ScheduleDay::Wednesday => "Wednesday",
            ScheduleDay::Thursday => "Thursday",
            ScheduleDay::Friday => "Friday",
            ScheduleDay::Saturday => "Saturday",
            ScheduleDay::Sunday => "Sunday",
        }
    }

    /// The day's bit in Task Scheduler's `DaysOfWeek` mask: Sunday 1, Monday 2, … Saturday 64.
    pub fn days_of_week_mask(self) -> i16 {
        match self {
            ScheduleDay::Sunday => 1,
            ScheduleDay::Monday => 2,
            ScheduleDay::Tuesday => 4,
            ScheduleDay::Wednesday => 8,
            ScheduleDay::Thursday => 16,
            ScheduleDay::Friday => 32,
            ScheduleDay::Saturday => 64,
        }
    }

    /// The day whose bit is the only one set in `mask`; None for no day or several days.
    pub fn from_mask(mask: i16) -> Option<ScheduleDay> {
        ScheduleDay::ALL
            .into_iter()
            .find(|day| day.days_of_week_mask() == mask)
    }

    /// Element name of the day inside `<DaysOfWeek>` in a task's XML (`<Sunday />`).
    pub(crate) fn xml_element(self) -> &'static str {
        self.label()
    }
}

/// The chrono weekday of `day`.
fn weekday(day: ScheduleDay) -> Weekday {
    match day {
        ScheduleDay::Monday => Weekday::Mon,
        ScheduleDay::Tuesday => Weekday::Tue,
        ScheduleDay::Wednesday => Weekday::Wed,
        ScheduleDay::Thursday => Weekday::Thu,
        ScheduleDay::Friday => Weekday::Fri,
        ScheduleDay::Saturday => Weekday::Sat,
        ScheduleDay::Sunday => Weekday::Sun,
    }
}

/// Time of day of the weekly run, to the minute. Serialized as "HH:MM" (24-hour clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleTime {
    pub hour: u8,
    pub minute: u8,
}

impl ScheduleTime {
    /// 12:00, the default.
    pub const NOON: ScheduleTime = ScheduleTime {
        hour: 12,
        minute: 0,
    };

    /// "HH:MM" or "H:MM" on the 24-hour clock: an hour 0-23 and a minute of exactly two
    /// digits 00-59. Surrounding spaces are ignored.
    pub fn parse(s: &str) -> Result<ScheduleTime> {
        let bad = || {
            Error::Other(format!(
                "not a time of day: {s:?}; expected HH:MM on the 24-hour clock, such as 12:00"
            ))
        };
        let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        if !digits(h) || h.len() > 2 || !digits(m) || m.len() != 2 {
            return Err(bad());
        }
        let hour: u8 = h.parse().map_err(|_| bad())?;
        let minute: u8 = m.parse().map_err(|_| bad())?;
        if hour > 23 || minute > 59 {
            return Err(bad());
        }
        Ok(ScheduleTime { hour, minute })
    }

    /// "HH:MM", for example "09:30".
    pub fn text(self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    fn naive(self) -> NaiveTime {
        NaiveTime::from_hms_opt(
            u32::from(self.hour.min(23)),
            u32::from(self.minute.min(59)),
            0,
        )
        .unwrap_or(NaiveTime::MIN)
    }
}

impl fmt::Display for ScheduleTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.text())
    }
}

impl Serialize for ScheduleTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for ScheduleTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        ScheduleTime::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// What the weekly task does and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    pub day: ScheduleDay,
    pub time: ScheduleTime,
    /// Schedulable cleanup target ids ([`schedulable_targets`]).
    #[serde(default)]
    pub targets: Vec<String>,
    /// System File Checker, read-only (`sfc /verifyonly`).
    #[serde(default)]
    pub system_file_check: bool,
    /// DISM CheckHealth, read-only.
    #[serde(default)]
    pub component_store_check: bool,
}

/// Why a target can never be scheduled.
pub const RECYCLE_BIN_REFUSED: &str = "the Recycle Bin is never emptied by scheduled maintenance";
const NOTHING_TO_DO: &str =
    "scheduled maintenance needs at least one cleanup location or one check";

impl ScheduleConfig {
    /// Sunday at 12:00, both checks, and the cleanup targets that are on by default.
    pub fn default_config() -> ScheduleConfig {
        ScheduleConfig {
            day: ScheduleDay::Sunday,
            time: ScheduleTime::NOON,
            targets: schedulable_targets()
                .into_iter()
                .filter(|t| t.default_on)
                .map(|t| t.id)
                .collect(),
            system_file_check: true,
            component_store_check: true,
        }
    }

    /// The same schedule with its targets deduplicated and in cleanup catalog order. Refuses
    /// the Recycle Bin, unknown and unschedulable ids, and a schedule with nothing to do.
    pub fn validated(self) -> Result<ScheduleConfig> {
        for id in &self.targets {
            if id == "recycle_bin" {
                return Err(Error::Other(RECYCLE_BIN_REFUSED.to_string()));
            }
            if allowed(id).is_none() {
                return Err(Error::Other(format!(
                    "{id:?} is not a cleanup location scheduled maintenance can clean"
                )));
            }
        }
        let targets: Vec<String> = cleanup::catalog()
            .into_iter()
            .map(|t| t.id)
            .filter(|id| self.targets.iter().any(|t| t == id))
            .collect();
        if targets.is_empty() && !self.system_file_check && !self.component_store_check {
            return Err(Error::Other(NOTHING_TO_DO.to_string()));
        }
        Ok(ScheduleConfig { targets, ..self })
    }

    /// For logs: "every Sunday at 12:00; cleans user_temp, windows_temp; checks sfc_verify,
    /// dism_check".
    pub fn describe(&self) -> String {
        let cleans = if self.targets.is_empty() {
            "nothing".to_string()
        } else {
            self.targets.join(", ")
        };
        let mut checks = Vec::new();
        if self.system_file_check {
            checks.push("sfc_verify");
        }
        if self.component_store_check {
            checks.push("dism_check");
        }
        let checks = if checks.is_empty() {
            "nothing".to_string()
        } else {
            checks.join(", ")
        };
        format!(
            "every {} at {}; cleans {cleans}; checks {checks}",
            self.day.label(),
            self.time
        )
    }

    /// What a run of this schedule does.
    pub fn request(&self, origin: RunOrigin) -> RunRequest {
        RunRequest {
            targets: self.targets.clone(),
            system_file_check: self.system_file_check,
            component_store_check: self.component_store_check,
            origin,
        }
    }

    /// The part of the schedule the task's command line carries.
    #[cfg(test)]
    pub(crate) fn selection(&self) -> RunSelection {
        RunSelection {
            targets: self.targets.clone(),
            system_file_check: self.system_file_check,
            component_store_check: self.component_store_check,
        }
    }
}

/// A cleanup target scheduled maintenance may clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedulableTarget {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Cleans the signed-in account's own files.
    pub per_user: bool,
    /// Selected when a schedule is created.
    pub default_on: bool,
    /// Skipped while Windows Update is working or waiting for a restart.
    pub servicing_guard: bool,
    pub requires_admin: bool,
}

/// One entry of the allow-list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Allowed {
    pub(crate) id: &'static str,
    pub(crate) per_user: bool,
    pub(crate) default_on: bool,
    pub(crate) servicing_guard: bool,
}

const fn allow(id: &'static str, per_user: bool, default_on: bool, guard: bool) -> Allowed {
    Allowed {
        id,
        per_user,
        default_on,
        servicing_guard: guard,
    }
}

/// Every cleanup target a schedule may name; the Recycle Bin is deliberately missing.
const SCHEDULABLE: [Allowed; 11] = [
    allow("user_temp", true, true, false),
    allow("windows_temp", false, true, false),
    allow("update_cache", false, true, true),
    allow("delivery_optimization", false, true, true),
    allow("crash_dumps", true, false, false),
    allow("error_reports", true, true, false),
    allow("thumbnail_cache", true, false, false),
    allow("shader_cache", true, false, false),
    allow("browser_chrome", true, false, false),
    allow("browser_edge", true, false, false),
    allow("browser_firefox", true, false, false),
];

/// The allow-list entry of `id`.
pub(crate) fn allowed(id: &str) -> Option<&'static Allowed> {
    SCHEDULABLE.iter().find(|a| a.id == id)
}

/// The schedulable cleanup targets in cleanup catalog order, with their catalog texts.
pub fn schedulable_targets() -> Vec<SchedulableTarget> {
    cleanup::catalog()
        .into_iter()
        .filter_map(|t| {
            let a = allowed(&t.id)?;
            Some(SchedulableTarget {
                per_user: a.per_user,
                default_on: a.default_on,
                servicing_guard: a.servicing_guard,
                requires_admin: t.requires_admin,
                id: t.id,
                title: t.title,
                description: t.description,
            })
        })
        .collect()
}

/// What one run does, as the task's command line carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunSelection {
    pub(crate) targets: Vec<String>,
    pub(crate) system_file_check: bool,
    pub(crate) component_store_check: bool,
}

impl RunSelection {
    pub(crate) fn request(&self, origin: RunOrigin) -> RunRequest {
        RunRequest {
            targets: self.targets.clone(),
            system_file_check: self.system_file_check,
            component_store_check: self.component_store_check,
            origin,
        }
    }
}

/// A journal path that can be passed on a task's command line: absolute, without quotes,
/// control characters, a trailing backslash or `$(` (Task Scheduler would substitute it).
fn check_journal_path(journal: &str) -> Result<()> {
    let fits = Path::new(journal).is_absolute()
        && !journal.contains('"')
        && !journal.contains("$(")
        && !journal.ends_with('\\')
        && !journal.chars().any(char::is_control);
    if fits {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "the journal path {journal} can't be passed to a scheduled task"
        )))
    }
}

/// The task's arguments: `--journal "<journal>"[ --targets id,id][ --sfc][ --dism]`.
/// `config` must be validated.
pub(crate) fn task_arguments(journal: &Path, config: &ScheduleConfig) -> Result<String> {
    let text = journal.to_str().ok_or_else(|| {
        Error::Other(format!(
            "the journal path {} is not valid Unicode",
            journal.display()
        ))
    })?;
    check_journal_path(text)?;
    let mut args = format!("--journal \"{text}\"");
    if !config.targets.is_empty() {
        args.push_str(" --targets ");
        args.push_str(&config.targets.join(","));
    }
    if config.system_file_check {
        args.push_str(" --sfc");
    }
    if config.component_store_check {
        args.push_str(" --dism");
    }
    Ok(args)
}

/// Splits a command line at spaces and tabs outside double quotes; the quotes are removed.
/// None for an unbalanced quote.
fn split_arguments(args: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        tokens.push(current);
    }
    Some(tokens)
}

/// Reads the arguments [`task_arguments`] writes, already split: `--journal <path>`, then
/// optionally `--targets <id,id>`, `--sfc` and `--dism` in this order, each at most once.
/// Every target must be schedulable and appear once, and the run must do something. None for
/// anything else.
pub(crate) fn parse_task_tokens(tokens: &[String]) -> Option<(PathBuf, RunSelection)> {
    let mut it = tokens.iter().map(String::as_str).peekable();
    if it.next()? != "--journal" {
        return None;
    }
    let journal = it.next()?;
    check_journal_path(journal).ok()?;
    let mut targets: Vec<String> = Vec::new();
    if it.peek() == Some(&"--targets") {
        it.next();
        for id in it.next()?.split(',') {
            if allowed(id).is_none() || targets.iter().any(|t| t == id) {
                return None;
            }
            targets.push(id.to_string());
        }
    }
    let system_file_check = it.peek() == Some(&"--sfc");
    if system_file_check {
        it.next();
    }
    let component_store_check = it.peek() == Some(&"--dism");
    if component_store_check {
        it.next();
    }
    if it.next().is_some() || (targets.is_empty() && !system_file_check && !component_store_check) {
        return None;
    }
    Some((
        PathBuf::from(journal),
        RunSelection {
            targets,
            system_file_check,
            component_store_check,
        },
    ))
}

/// [`parse_task_tokens`] of a whole command line as Task Scheduler stores it.
pub(crate) fn parse_task_arguments(args: &str) -> Option<(PathBuf, RunSelection)> {
    parse_task_tokens(&split_arguments(args)?)
}

/// The first `day` at `time` strictly after `now` (local wall-clock time).
pub(crate) fn next_start(
    now: NaiveDateTime,
    day: ScheduleDay,
    time: ScheduleTime,
) -> NaiveDateTime {
    let today = now.date();
    let ahead =
        (7 + weekday(day).num_days_from_monday() - today.weekday().num_days_from_monday()) % 7;
    let candidate = today
        .checked_add_days(Days::new(u64::from(ahead)))
        .unwrap_or(today)
        .and_time(time.naive());
    if candidate > now {
        candidate
    } else {
        candidate
            .checked_add_days(Days::new(7))
            .unwrap_or(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_through_mask_label_and_serde() {
        let mut seen = 0i16;
        for day in ScheduleDay::ALL {
            let mask = day.days_of_week_mask();
            assert_eq!(mask.count_ones(), 1, "{day:?}");
            assert_eq!(seen & mask, 0, "{day:?} shares a bit");
            seen |= mask;
            assert_eq!(ScheduleDay::from_mask(mask), Some(day));
            assert_eq!(ScheduleDay::parse(day.label()), Some(day));
            assert_eq!(ScheduleDay::parse(&day.label().to_uppercase()), Some(day));
            assert_eq!(ScheduleDay::parse(&day.label()[..3]), Some(day));
            assert_eq!(day.xml_element(), day.label());
            let json = serde_json::to_value(day).unwrap();
            assert_eq!(json, day.label().to_ascii_lowercase());
            assert_eq!(serde_json::from_value::<ScheduleDay>(json).unwrap(), day);
        }
        assert_eq!(seen, 127);
        assert_eq!(ScheduleDay::Sunday.days_of_week_mask(), 1);
        assert_eq!(ScheduleDay::Saturday.days_of_week_mask(), 64);
    }

    #[test]
    fn parse_and_from_mask_reject_everything_else() {
        assert_eq!(ScheduleDay::parse(" sun "), Some(ScheduleDay::Sunday));
        assert_eq!(ScheduleDay::parse("TUE"), Some(ScheduleDay::Tuesday));
        for bad in ["", "su", "sunda", "sundays", "tues", "weekday", "7"] {
            assert_eq!(ScheduleDay::parse(bad), None, "{bad:?}");
        }
        for mask in [0i16, 3, 127, 128, -1] {
            assert_eq!(ScheduleDay::from_mask(mask), None, "{mask}");
        }
        assert!(serde_json::from_str::<ScheduleDay>(r#""Sunday""#).is_err());
    }

    #[test]
    fn times_parse_serialize_and_refuse_bad_text() {
        assert_eq!(ScheduleTime::parse("12:00").unwrap(), ScheduleTime::NOON);
        assert_eq!(
            ScheduleTime::parse(" 7:05 ").unwrap(),
            ScheduleTime { hour: 7, minute: 5 }
        );
        assert_eq!(ScheduleTime::parse("23:59").unwrap().text(), "23:59");
        assert_eq!(ScheduleTime::parse("00:00").unwrap().text(), "00:00");
        assert_eq!(
            ScheduleTime {
                hour: 9,
                minute: 30
            }
            .to_string(),
            "09:30"
        );
        for bad in [
            "24:00", "7:5", "", "12", "12:60", "123:00", "-1:00", "12:0a", "12:000", "1 2:00",
        ] {
            assert!(ScheduleTime::parse(bad).is_err(), "{bad:?}");
        }
        let json = serde_json::to_value(ScheduleTime {
            hour: 6,
            minute: 45,
        })
        .unwrap();
        assert_eq!(json, "06:45");
        assert_eq!(
            serde_json::from_value::<ScheduleTime>(json).unwrap(),
            ScheduleTime {
                hour: 6,
                minute: 45
            }
        );
        assert!(serde_json::from_str::<ScheduleTime>(r#""25:00""#).is_err());
    }

    #[test]
    fn every_schedulable_id_is_a_cleanup_target_and_the_recycle_bin_is_not() {
        let catalog: Vec<String> = cleanup::catalog().into_iter().map(|t| t.id).collect();
        for a in SCHEDULABLE {
            assert!(catalog.iter().any(|id| id == a.id), "{}", a.id);
        }
        assert!(allowed("recycle_bin").is_none());
        let targets = schedulable_targets();
        assert_eq!(targets.len(), SCHEDULABLE.len());
        assert!(targets
            .iter()
            .all(|t| !t.title.is_empty() && !t.description.is_empty()));
        let defaults: Vec<&str> = targets
            .iter()
            .filter(|t| t.default_on)
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(
            defaults,
            [
                "user_temp",
                "windows_temp",
                "update_cache",
                "delivery_optimization",
                "error_reports"
            ]
        );
        let guarded: Vec<&str> = targets
            .iter()
            .filter(|t| t.servicing_guard)
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(guarded, ["update_cache", "delivery_optimization"]);
        let user = targets.iter().find(|t| t.id == "user_temp").unwrap();
        assert!(user.per_user && !user.requires_admin);
    }

    fn config(targets: &[&str], sfc: bool, dism: bool) -> ScheduleConfig {
        ScheduleConfig {
            day: ScheduleDay::Sunday,
            time: ScheduleTime::NOON,
            targets: targets.iter().map(|t| t.to_string()).collect(),
            system_file_check: sfc,
            component_store_check: dism,
        }
    }

    #[test]
    fn validated_dedupes_orders_and_refuses() {
        let v = config(
            &["error_reports", "user_temp", "error_reports"],
            false,
            false,
        )
        .validated()
        .unwrap();
        assert_eq!(v.targets, ["user_temp", "error_reports"]);
        let err = config(&["user_temp", "recycle_bin"], true, true)
            .validated()
            .unwrap_err();
        assert_eq!(err.to_string(), RECYCLE_BIN_REFUSED);
        assert!(config(&["no_such"], true, true).validated().is_err());
        assert!(config(&[], false, false).validated().is_err());
        assert!(config(&[], true, false).validated().is_ok());
        assert!(config(&[], false, true).validated().is_ok());
        let d = ScheduleConfig::default_config();
        assert_eq!(d.clone().validated().unwrap(), d);
        assert!(d.system_file_check && d.component_store_check);
        assert_eq!(d.day, ScheduleDay::Sunday);
        assert_eq!(d.time, ScheduleTime::NOON);
    }

    #[test]
    fn describe_and_serde_shape() {
        assert_eq!(
            config(&["user_temp", "windows_temp"], true, true).describe(),
            "every Sunday at 12:00; cleans user_temp, windows_temp; checks sfc_verify, dism_check"
        );
        assert_eq!(
            config(&[], true, false).describe(),
            "every Sunday at 12:00; cleans nothing; checks sfc_verify"
        );
        let json = serde_json::to_value(config(&["user_temp"], true, false)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "day": "sunday", "time": "12:00", "targets": ["user_temp"],
                "system_file_check": true, "component_store_check": false
            })
        );
        let parsed: ScheduleConfig =
            serde_json::from_str(r#"{"day": "monday", "time": "08:15"}"#).unwrap();
        assert_eq!(parsed.day, ScheduleDay::Monday);
        assert!(parsed.targets.is_empty() && !parsed.system_file_check);
        assert!(serde_json::from_str::<ScheduleConfig>(
            r#"{"day": "monday", "time": "08:15", "sfc_scan": true}"#
        )
        .is_err());
        let request = config(&["user_temp"], true, false).request(RunOrigin::Cli);
        assert_eq!(request.targets, ["user_temp"]);
        assert!(request.system_file_check && !request.component_store_check);
        assert_eq!(request.origin, RunOrigin::Cli);
    }

    #[test]
    fn task_arguments_round_trip() {
        let journal = Path::new(r"C:\Users\Test\AppData\Local\Contoso & Co\journal.db");
        for c in [
            config(&["user_temp", "windows_temp"], true, true),
            config(&[], true, false),
            config(&[], false, true),
            config(&["browser_edge"], false, false),
        ] {
            let args = task_arguments(journal, &c).unwrap();
            assert!(!args.contains("$("), "{args}");
            assert!(args.starts_with(&format!("--journal \"{}\"", journal.display())));
            let (path, selection) = parse_task_arguments(&args).unwrap();
            assert_eq!(path, journal);
            assert_eq!(selection, c.selection());
        }
        assert_eq!(
            task_arguments(journal, &config(&["user_temp"], true, true)).unwrap(),
            format!(
                "--journal \"{}\" --targets user_temp --sfc --dism",
                journal.display()
            )
        );
    }

    #[test]
    fn task_arguments_refuse_paths_the_command_line_cannot_carry() {
        let c = config(&[], true, false);
        for bad in [
            r"journal.db",
            r"C:\Users\$(Arg0)\journal.db",
            "C:\\Users\\Test\\jour\"nal.db",
            r"C:\Users\Test\",
            "C:\\Users\\Test\u{7}\\journal.db",
        ] {
            assert!(task_arguments(Path::new(bad), &c).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parse_task_arguments_accepts_only_the_exact_grammar() {
        let ok = r#"--journal "C:\Data\journal.db" --targets user_temp,error_reports --sfc --dism"#;
        let (path, selection) = parse_task_arguments(ok).unwrap();
        assert_eq!(path, Path::new(r"C:\Data\journal.db"));
        assert_eq!(selection.targets, ["user_temp", "error_reports"]);
        assert!(selection.system_file_check && selection.component_store_check);
        assert!(parse_task_arguments(r#"--journal C:\Data\journal.db --sfc"#).is_some());
        for bad in [
            "",
            "--sfc",
            r#"--journal "C:\Data\journal.db""#,
            r#"--journal "C:\Data\journal.db" --dism --sfc"#,
            r#"--journal "C:\Data\journal.db" --sfc --sfc"#,
            r#"--journal "C:\Data\journal.db" --targets recycle_bin"#,
            r#"--journal "C:\Data\journal.db" --targets user_temp,user_temp"#,
            r#"--journal "C:\Data\journal.db" --targets"#,
            r#"--journal "C:\Data\journal.db" --targets user_temp --scheduled"#,
            r#"--journal "C:\Data\journal.db" --sfc --scan"#,
            r#"--journal "C:\Data\journal.db --sfc"#,
            r#"--journal "relative\journal.db" --sfc"#,
            r#"--journal "C:\$(Arg0)\journal.db" --sfc"#,
            r#"--targets user_temp --journal "C:\Data\journal.db""#,
            r#"--Journal "C:\Data\journal.db" --sfc"#,
            r#"--journal "C:\Data\journal.db" --SFC"#,
            r#"maintenance run --journal "C:\Data\journal.db" --sfc"#,
        ] {
            assert!(parse_task_arguments(bad).is_none(), "{bad:?}");
        }
        let tokens: Vec<String> = ["--journal", r"C:\a b\journal.db", "--dism"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (path, selection) = parse_task_tokens(&tokens).unwrap();
        assert_eq!(path, Path::new(r"C:\a b\journal.db"));
        assert!(selection.targets.is_empty() && selection.component_store_check);
    }

    #[test]
    fn next_start_is_strictly_in_the_future_on_the_right_day() {
        use chrono::NaiveDate;
        // 2026-10-04 is a Sunday.
        let base = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        let noon = ScheduleTime::NOON;
        for offset in 0..7u64 {
            let date = base.checked_add_days(Days::new(offset)).unwrap();
            for (h, m) in [(11, 59), (12, 0), (12, 1), (0, 0), (23, 59)] {
                let now = date.and_hms_opt(h, m, 0).unwrap();
                for day in ScheduleDay::ALL {
                    let next = next_start(now, day, noon);
                    assert!(next > now, "{now} {day:?} -> {next}");
                    assert!(
                        next - now <= chrono::TimeDelta::days(7),
                        "{now} {day:?} -> {next}"
                    );
                    assert_eq!(next.weekday(), weekday(day), "{now} {day:?}");
                    assert_eq!(next.time(), noon.naive());
                }
            }
        }
        let sunday_noon = base.and_hms_opt(12, 0, 0).unwrap();
        assert_eq!(
            next_start(sunday_noon, ScheduleDay::Sunday, noon),
            base.checked_add_days(Days::new(7))
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
        );
        let sunday_morning = base.and_hms_opt(11, 0, 0).unwrap();
        assert_eq!(
            next_start(sunday_morning, ScheduleDay::Sunday, noon),
            sunday_noon
        );
    }
}
