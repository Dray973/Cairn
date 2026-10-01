//! The profile file: a strict, versioned JSON document naming Cairn settings.
//!
//! Reading happens in three stages. The header stage parses any JSON and checks `format` and
//! `schema`, so a file of another program or a newer Cairn gets a clear message. The typed
//! stage refuses unknown and duplicate fields and wrong types at every level. The semantic
//! stage checks every string against a hand-written grammar, applies the per-section limits,
//! trims and removes duplicates. A profile's strings are only ever compared with what Cairn
//! knows; none of them names a path, command or address.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::Win32::Foundation::{
    ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE, ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT,
};

use super::step::{MaintenanceChoice, WindowsUpdateChoice};
use crate::{Error, Result};

/// Value of the `format` field of every profile.
pub const FORMAT: &str = "cairn.profile";
/// The profile format this version reads and writes.
pub const SCHEMA: u32 = 1;
/// Largest profile file accepted, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Longest profile name, in characters.
pub(crate) const MAX_NAME: usize = 60;
pub(crate) const MAX_DESCRIPTION: usize = 300;
pub(crate) const MAX_CREATED_WITH: usize = 40;
pub(crate) const MAX_TWEAKS: usize = 256;
pub(crate) const MAX_APPS: usize = 128;
pub(crate) const MAX_STARTUP: usize = 256;
pub(crate) const MAX_STARTUP_KEY: usize = 260;
pub(crate) const MAX_STARTUP_NAME: usize = 120;
pub(crate) const MAX_CLEAN: usize = 16;
/// Longest tweak id; the name after the category dot has at most 60 characters.
const MAX_TWEAK_ID: usize = 64;
const MAX_TWEAK_NAME: usize = 60;
/// Longest Store package Name, without a trailing `*`.
const MAX_APP_NAME: usize = 50;
const MAX_CHOICE_ID: usize = 32;
/// Characters of a refused string quoted in an error message.
const QUOTED_CHARS: usize = 60;
/// Longest span of active hours Windows Update accepts.
const MAX_ACTIVE_HOURS: u32 = 18;

/// Why a path is refused as a profile file.
pub const PROFILE_PATH_TEXT: &str = "Profiles are saved as .json files.";

const ONEDRIVE_HINT: &str =
    " If it is in OneDrive, make it available offline or reconnect, then try again.";

/// Why a text is not a usable profile; the message is shown to the user as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

impl From<Invalid> for Error {
    fn from(invalid: Invalid) -> Error {
        Error::Other(invalid.0)
    }
}

fn invalid(message: impl Into<String>) -> Invalid {
    Invalid(message.into())
}

/// A profile as its file holds it. After [`parse`] or [`validate`] every string is trimmed,
/// checked and free of duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub format: String,
    pub schema: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `YYYY-MM-DD`; informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// The Cairn version that wrote the file; informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_with: Option<String>,
    /// Catalog tweak ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tweaks: Vec<String>,
    /// Store package Names, or catalog patterns ending in `*`, to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// Startup entries to turn off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup: Vec<StartupChoice>,
    /// A DNS preset per adapter kind and address family.
    #[serde(default, skip_serializing_if = "DnsChoices::is_empty")]
    pub dns: DnsChoices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_update: Option<WindowsUpdateChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceChoice>,
}

/// A startup entry to turn off: its id as `startup::list` reports it, and the name shown
/// when the entry is not on this PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupChoice {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// DNS presets for the Ethernet and the Wi-Fi adapters of a PC.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsChoices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ethernet: Option<DnsFamilies>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi: Option<DnsFamilies>,
}

impl DnsChoices {
    pub fn is_empty(&self) -> bool {
        self.ethernet.is_none() && self.wifi.is_none()
    }

    /// Number of adapter kinds with a choice.
    pub fn kinds(&self) -> usize {
        usize::from(self.ethernet.is_some()) + usize::from(self.wifi.is_some())
    }
}

/// Preset ids per address family; a missing family is left as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsFamilies {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
}

/// How many settings each section of a profile holds: `dns` counts adapter kinds,
/// `windows_update` the fields set and `maintenance` is 0 or 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionCounts {
    pub tweaks: usize,
    pub apps: usize,
    pub startup: usize,
    pub dns: usize,
    pub windows_update: usize,
    pub maintenance: usize,
}

impl SectionCounts {
    pub fn of(profile: &Profile) -> SectionCounts {
        SectionCounts {
            tweaks: profile.tweaks.len(),
            apps: profile.apps.len(),
            startup: profile.startup.len(),
            dns: profile.dns.kinds(),
            windows_update: profile
                .windows_update
                .as_ref()
                .map_or(0, windows_update_fields),
            maintenance: usize::from(profile.maintenance.is_some()),
        }
    }

    pub fn total(&self) -> usize {
        self.tweaks + self.apps + self.startup + self.dns + self.windows_update + self.maintenance
    }
}

/// Number of fields a Windows Update choice sets.
pub(crate) fn windows_update_fields(wu: &WindowsUpdateChoice) -> usize {
    usize::from(wu.active_hours.is_some())
        + usize::from(wu.restart_notify.is_some())
        + usize::from(wu.exclude_drivers)
        + usize::from(wu.defer_feature_days.is_some())
}

/// What a validated profile holds, with its canonical text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub name: String,
    pub description: String,
    pub created: Option<String>,
    pub created_with: Option<String>,
    pub counts: SectionCounts,
    /// The profile as [`to_text`] writes it.
    pub text: String,
}

// ───────────────────────────── reading ─────────────────────────────

/// Reads a profile from bytes: refuses UTF-16, other non-UTF-8 text and oversized input,
/// strips a UTF-8 byte order mark, then [`parse`]s.
pub fn parse_bytes(bytes: &[u8]) -> std::result::Result<Profile, Invalid> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(invalid(
            "The file is saved as UTF-16; save it as UTF-8 and open it again.",
        ));
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(oversize());
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("The file is not UTF-8 text."))?;
    parse(text)
}

/// Reads a profile from text: the header, the typed fields, then the semantic checks.
/// Returns the normalized profile.
pub fn parse(text: &str) -> std::result::Result<Profile, Invalid> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.len() > MAX_FILE_BYTES {
        return Err(oversize());
    }
    check_header(text)?;
    let profile: Profile = serde_json::from_str(text)
        .map_err(|e| invalid(format!("The profile is not valid: {e}.")))?;
    validate(&profile)
}

fn oversize() -> Invalid {
    invalid("The file is larger than 256 KB, so it is not a Cairn profile.")
}

fn not_a_profile() -> Invalid {
    invalid("This file is not a Cairn profile.")
}

/// Stage 1: any JSON value, which must be an object with `format` and a supported `schema`.
fn check_header(text: &str) -> std::result::Result<(), Invalid> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| invalid(format!("The file is not valid JSON: {e}.")))?;
    let Value::Object(map) = &value else {
        return Err(not_a_profile());
    };
    match map.get("format") {
        Some(Value::String(format)) if format == FORMAT => {}
        _ => return Err(not_a_profile()),
    }
    let bad_schema =
        || invalid("The profile is not valid: \"schema\" must be a whole number of at least 1.");
    match map.get("schema") {
        None => Err(invalid(
            "The profile is not valid: it has no \"schema\" number.",
        )),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(n) if n == u64::from(SCHEMA) => Ok(()),
            Some(0) | None => Err(bad_schema()),
            Some(n) => Err(invalid(format!(
                "This profile was made by a newer version of Cairn (profile format {n}). Update \
                 Cairn to open it."
            ))),
        },
        Some(_) => Err(bad_schema()),
    }
}

/// Whether `text` starts like a Cairn profile of any schema (an object whose `format` is
/// [`FORMAT`]).
pub(crate) fn looks_like_profile(text: &str) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => {
            matches!(map.get("format"), Some(Value::String(f)) if f == FORMAT)
        }
        _ => false,
    }
}

// ───────────────────────────── validation ─────────────────────────────

/// A refused string for an error message: at most [`QUOTED_CHARS`] characters, control
/// characters escaped, in double quotes.
fn quoted(s: &str) -> String {
    let mut shown: String = s.chars().take(QUOTED_CHARS).collect();
    if s.chars().count() > QUOTED_CHARS {
        shown.push('…');
    }
    format!("\"{}\"", shown.escape_debug())
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn is_lower_or_digit(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// `category.name`: a lowercase letter then lowercase letters or digits, a dot, then 1 to 60
/// lowercase letters, digits or underscores; at most 64 characters.
pub(crate) fn is_tweak_id(s: &str) -> bool {
    if s.len() > MAX_TWEAK_ID {
        return false;
    }
    let Some((category, name)) = s.split_once('.') else {
        return false;
    };
    let mut chars = category.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(is_lower_or_digit)
        && (1..=MAX_TWEAK_NAME).contains(&name.len())
        && name.chars().all(|c| is_lower_or_digit(c) || c == '_')
}

/// A Store package Name (a letter or digit, then letters, digits, dots or dashes; 2 to 50
/// characters), optionally followed by `*`.
pub(crate) fn is_app_name(s: &str) -> bool {
    let body = s.strip_suffix('*').unwrap_or(s);
    let mut chars = body.chars();
    (2..=MAX_APP_NAME).contains(&body.len())
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// `source:key`: a source of a lowercase letter then up to 31 lowercase letters, digits or
/// underscores, and a key of 1 to 260 characters without control characters.
pub(crate) fn is_startup_id(s: &str) -> bool {
    let Some((source, key)) = s.split_once(':') else {
        return false;
    };
    let mut chars = source.chars();
    source.len() <= MAX_CHOICE_ID
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| is_lower_or_digit(c) || c == '_')
        && (1..=MAX_STARTUP_KEY).contains(&key.chars().count())
        && !has_control(key)
}

/// A DNS preset id: 1 to 32 lowercase letters, digits or underscores.
fn is_choice_id(s: &str) -> bool {
    (1..=MAX_CHOICE_ID).contains(&s.len()) && s.chars().all(|c| is_lower_or_digit(c) || c == '_')
}

/// A cleanup target id: 1 to 32 lowercase letters or underscores.
fn is_clean_id(s: &str) -> bool {
    (1..=MAX_CHOICE_ID).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// `HH:MM` from 00:00 to 23:59.
pub(crate) fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    let digits = [b[0], b[1], b[3], b[4]];
    if !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let hours = (b[0] - b'0') * 10 + (b[1] - b'0');
    let minutes = (b[3] - b'0') * 10 + (b[4] - b'0');
    hours < 24 && minutes < 60
}

/// Checks a text field: trimmed, at most `max` characters, no control characters.
fn text_field(value: &str, field: &str, max: usize) -> std::result::Result<String, Invalid> {
    let value = value.trim();
    if value.chars().count() > max {
        return Err(invalid(format!("{field} is longer than {max} characters.")));
    }
    if has_control(value) {
        return Err(invalid(format!("{field} contains control characters.")));
    }
    Ok(value.to_string())
}

/// Stage 3: checks every field of `p` and returns it trimmed and without duplicates.
pub(crate) fn validate(p: &Profile) -> std::result::Result<Profile, Invalid> {
    if p.format != FORMAT {
        return Err(not_a_profile());
    }
    if p.schema != SCHEMA {
        return Err(invalid(format!(
            "The profile is not valid: this version of Cairn reads profile format {SCHEMA}, \
             not {}.",
            p.schema
        )));
    }
    let name = p.name.trim();
    if name.is_empty() {
        return Err(invalid("The profile needs a name."));
    }
    let name = text_field(name, "The profile name", MAX_NAME)?;
    let description = text_field(&p.description, "The description", MAX_DESCRIPTION)?;
    let created = match &p.created {
        None => None,
        Some(date) => {
            let date = date.trim();
            if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() || date.len() != 10 {
                return Err(invalid(format!(
                    "created {} must be a date written as YYYY-MM-DD.",
                    quoted(date)
                )));
            }
            Some(date.to_string())
        }
    };
    let created_with = match &p.created_with {
        None => None,
        Some(v) => Some(text_field(v, "created_with", MAX_CREATED_WITH)?),
    };

    if p.tweaks.len() > MAX_TWEAKS {
        return Err(invalid(format!("There are more than {MAX_TWEAKS} tweaks.")));
    }
    let mut tweaks: Vec<String> = Vec::new();
    for (i, id) in p.tweaks.iter().enumerate() {
        if !is_tweak_id(id) {
            return Err(invalid(format!(
                "tweaks[{i}] {} is not a valid setting id.",
                quoted(id)
            )));
        }
        if !tweaks.contains(id) {
            tweaks.push(id.clone());
        }
    }

    if p.apps.len() > MAX_APPS {
        return Err(invalid(format!("There are more than {MAX_APPS} apps.")));
    }
    let mut apps: Vec<String> = Vec::new();
    for (i, app) in p.apps.iter().enumerate() {
        if !is_app_name(app) {
            return Err(invalid(format!(
                "apps[{i}] {} is not a Store app name.",
                quoted(app)
            )));
        }
        if !apps.iter().any(|a| a.eq_ignore_ascii_case(app)) {
            apps.push(app.clone());
        }
    }

    if p.startup.len() > MAX_STARTUP {
        return Err(invalid(format!(
            "There are more than {MAX_STARTUP} startup entries."
        )));
    }
    let mut startup: Vec<StartupChoice> = Vec::new();
    for (i, choice) in p.startup.iter().enumerate() {
        if !is_startup_id(&choice.id) {
            return Err(invalid(format!(
                "startup[{i}].id must look like source:name."
            )));
        }
        let entry_name = text_field(
            &choice.name,
            &format!("startup[{i}].name"),
            MAX_STARTUP_NAME,
        )?;
        if !startup
            .iter()
            .any(|s| s.id.eq_ignore_ascii_case(&choice.id))
        {
            startup.push(StartupChoice {
                id: choice.id.clone(),
                name: entry_name,
            });
        }
    }

    let dns = DnsChoices {
        ethernet: validate_dns(p.dns.ethernet.as_ref(), "ethernet")?,
        wifi: validate_dns(p.dns.wifi.as_ref(), "wifi")?,
    };
    let windows_update = match &p.windows_update {
        None => None,
        Some(wu) => Some(validate_windows_update(wu)?),
    };
    let maintenance = match &p.maintenance {
        None => None,
        Some(m) => Some(validate_maintenance(m)?),
    };

    let profile = Profile {
        format: FORMAT.to_string(),
        schema: SCHEMA,
        name,
        description,
        created,
        created_with,
        tweaks,
        apps,
        startup,
        dns,
        windows_update,
        maintenance,
    };
    if SectionCounts::of(&profile).total() == 0 {
        return Err(invalid("This profile contains no settings."));
    }
    Ok(profile)
}

fn validate_dns(
    families: Option<&DnsFamilies>,
    kind: &str,
) -> std::result::Result<Option<DnsFamilies>, Invalid> {
    let Some(families) = families else {
        return Ok(None);
    };
    if families.ipv4.is_none() && families.ipv6.is_none() {
        return Err(invalid(format!("dns.{kind} names no address family.")));
    }
    for (family, value) in [("ipv4", &families.ipv4), ("ipv6", &families.ipv6)] {
        if let Some(value) = value {
            if !is_choice_id(value) {
                return Err(invalid(format!(
                    "dns.{kind}.{family} {} is not a DNS choice id.",
                    quoted(value)
                )));
            }
        }
    }
    Ok(Some(families.clone()))
}

fn validate_windows_update(
    wu: &WindowsUpdateChoice,
) -> std::result::Result<WindowsUpdateChoice, Invalid> {
    if windows_update_fields(wu) == 0 {
        return Err(invalid("windows_update sets nothing."));
    }
    if let Some(days) = wu.defer_feature_days {
        if !(1..=365).contains(&days) {
            return Err(invalid(
                "windows_update.defer_feature_days must be from 1 to 365.",
            ));
        }
    }
    if let Some(hours) = &wu.active_hours {
        if hours.automatic {
            if hours.start.is_some() || hours.end.is_some() {
                return Err(invalid(
                    "windows_update.active_hours can't set start or end when automatic is true.",
                ));
            }
        } else {
            let (Some(start), Some(end)) = (hours.start, hours.end) else {
                return Err(invalid(
                    "windows_update.active_hours needs start and end, or automatic.",
                ));
            };
            if start > 23 || end > 23 {
                return Err(invalid(
                    "windows_update.active_hours start and end must be hours from 0 to 23.",
                ));
            }
            if start == end {
                return Err(invalid(
                    "windows_update.active_hours start and end must differ.",
                ));
            }
            let span = (u32::from(end) + 24 - u32::from(start)) % 24;
            if span > MAX_ACTIVE_HOURS {
                return Err(invalid(format!(
                    "windows_update.active_hours can span at most {MAX_ACTIVE_HOURS} hours."
                )));
            }
        }
    }
    Ok(wu.clone())
}

fn validate_maintenance(m: &MaintenanceChoice) -> std::result::Result<MaintenanceChoice, Invalid> {
    if !m.enabled {
        if m.day.is_some()
            || m.time.is_some()
            || !m.clean.is_empty()
            || m.sfc_verify
            || m.dism_check
        {
            return Err(invalid(
                "maintenance turns scheduled maintenance off, so it can't set anything else.",
            ));
        }
        return Ok(m.clone());
    }
    if m.day.is_none() {
        return Err(invalid("maintenance.day is required."));
    }
    let time = match &m.time {
        None => return Err(invalid("maintenance.time is required.")),
        Some(time) => time.trim(),
    };
    if !is_time(time) {
        return Err(invalid("maintenance.time must be HH:MM."));
    }
    if m.clean.len() > MAX_CLEAN {
        return Err(invalid(format!(
            "maintenance.clean has more than {MAX_CLEAN} targets."
        )));
    }
    let mut clean: Vec<String> = Vec::new();
    for (i, id) in m.clean.iter().enumerate() {
        if !is_clean_id(id) {
            return Err(invalid(format!(
                "maintenance.clean[{i}] {} is not a cleanup target id.",
                quoted(id)
            )));
        }
        if !clean.contains(id) {
            clean.push(id.clone());
        }
    }
    if clean.is_empty() && !m.sfc_verify && !m.dism_check {
        return Err(invalid(
            "maintenance does nothing: name cleanup targets or a check.",
        ));
    }
    Ok(MaintenanceChoice {
        enabled: true,
        day: m.day,
        time: Some(time.to_string()),
        clean,
        sfc_verify: m.sfc_verify,
        dism_check: m.dism_check,
    })
}

// ───────────────────────────── writing ─────────────────────────────

/// The profile as pretty JSON (two-space indent, fields in declaration order) ending with a
/// line feed.
pub fn to_text(p: &Profile) -> String {
    let mut text = serde_json::to_string_pretty(p).unwrap_or_default();
    text.push('\n');
    text
}

/// The file name of `path` for messages, never the folder.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Whether reading `path` failed because a cloud placeholder could not be downloaded.
fn cloud_placeholder(path: &Path, error: &std::io::Error) -> bool {
    let code = error.raw_os_error().map(|c| c as u32);
    if code == Some(ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING.0)
        || code == Some(ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE.0)
    {
        return true;
    }
    let cloud = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
        | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
        | FILE_ATTRIBUTE_OFFLINE.0;
    fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & cloud != 0)
}

fn read_error(path: &Path, error: &std::io::Error) -> Error {
    let hint = if cloud_placeholder(path, error) {
        ONEDRIVE_HINT
    } else {
        ""
    };
    Error::Other(format!(
        "{} could not be read: {error}.{hint}",
        display_name(path)
    ))
}

/// Reads at most [`MAX_FILE_BYTES`] + 1 bytes of `path`, so [`parse_bytes`] can refuse a
/// larger file. Folders are refused; a read that fails on a cloud placeholder says how to
/// make the file available.
pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path).map_err(|e| read_error(path, &e))?;
    if meta.is_dir() {
        return Err(Error::Other(format!("{} is a folder.", display_name(path))));
    }
    let file = File::open(path).map_err(|e| read_error(path, &e))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| read_error(path, &e))?;
    Ok(bytes)
}

fn has_json_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("json"))
}

/// Whether [`write_file`] accepts `path` by its form: absolute, with the `.json` extension.
pub fn is_profile_path(path: &Path) -> bool {
    path.is_absolute() && has_json_extension(path)
}

/// Checks the name and description an export gives a profile, with the messages
/// [`validate`] would give.
pub fn check_export_fields(name: &str, description: &str) -> std::result::Result<(), Invalid> {
    if name.trim().is_empty() {
        return Err(invalid("The profile needs a name."));
    }
    text_field(name, "The profile name", MAX_NAME)?;
    text_field(description, "The description", MAX_DESCRIPTION)?;
    Ok(())
}

/// Writes `text` to `path` through a temporary file in the same folder that replaces the
/// target once it is complete. Refuses a relative path or another extension than `.json`,
/// an existing folder or link, and an existing file that is not a Cairn profile, so the
/// elevated process never replaces a file the user picked by mistake.
pub fn write_file(path: &Path, text: &str) -> Result<()> {
    let name = display_name(path);
    if !is_profile_path(path) {
        return Err(Error::Other(PROFILE_PATH_TEXT.into()));
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(Error::Other(format!(
                    "{name} is a folder or a link; choose another file name."
                )));
            }
            let profile = read_file(path)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .is_some_and(|existing| looks_like_profile(&existing));
            if !profile {
                return Err(Error::Other(format!(
                    "A file named {name} already exists and is not a Cairn profile; choose \
                     another name."
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Other(format!("{name} could not be checked: {e}.")));
        }
    }
    let folder = path
        .parent()
        .ok_or_else(|| Error::Other(PROFILE_PATH_TEXT.into()))?;
    let temp = folder.join(format!("{name}.{}.tmp", std::process::id()));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        created = true;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if let Err(e) = result {
        if created {
            let _ = fs::remove_file(&temp);
        }
        return Err(Error::Other(format!("{name} could not be saved: {e}.")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::config::ScheduleDay;
    use crate::profiles::step::ActiveHoursChoice;

    const MINIMAL: &str = r#"{"format": "cairn.profile", "schema": 1, "name": "Test", "tweaks": ["gaming.game_mode"]}"#;

    fn with(fields: &str) -> String {
        format!(r#"{{"format": "cairn.profile", "schema": 1, "name": "Test", {fields}}}"#)
    }

    fn err(text: &str) -> String {
        parse(text).unwrap_err().0
    }

    fn profile() -> Profile {
        parse(MINIMAL).unwrap()
    }

    #[test]
    fn minimal_profile_parses_and_round_trips() {
        let p = profile();
        assert_eq!(p.name, "Test");
        assert_eq!(p.tweaks, vec!["gaming.game_mode"]);
        let text = to_text(&p);
        assert!(text.ends_with("}\n"));
        assert!(text.contains("\n  \"format\": \"cairn.profile\""));
        assert_eq!(parse(&text).unwrap(), p);
        assert_eq!(to_text(&parse(&text).unwrap()), text);

        let full = with(
            r#""description": "Desk PC", "created": "2026-09-28", "created_with": "Cairn 0.2.0",
               "tweaks": ["gaming.game_mode", "privacy.advertising_id"],
               "apps": ["Microsoft.BingNews", "king.com.*"],
               "startup": [{"id": "user_run:Discord", "name": "Discord"}],
               "dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "cloudflare"}, "wifi": {"ipv4": "quad9"}},
               "windows_update": {"active_hours": {"start": 8, "end": 23}, "exclude_drivers": true},
               "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "clean": ["user_temp"],
                               "sfc_verify": true, "dism_check": true}"#,
        );
        let p = parse(&full).unwrap();
        let text = to_text(&p);
        assert_eq!(parse(&text).unwrap(), p);
        let order: Vec<usize> = [
            "\"format\"",
            "\"schema\"",
            "\"name\"",
            "\"description\"",
            "\"created\"",
            "\"created_with\"",
            "\"tweaks\"",
            "\"apps\"",
            "\"startup\"",
            "\"dns\"",
            "\"windows_update\"",
            "\"maintenance\"",
        ]
        .iter()
        .map(|k| text.find(k).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
        let counts = SectionCounts::of(&p);
        assert_eq!(
            counts,
            SectionCounts {
                tweaks: 2,
                apps: 2,
                startup: 1,
                dns: 2,
                windows_update: 2,
                maintenance: 1
            }
        );
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(MINIMAL.as_bytes());
        assert_eq!(parse_bytes(&bytes).unwrap(), profile());
        assert_eq!(parse(&format!("\u{feff}{MINIMAL}")).unwrap(), profile());
    }

    #[test]
    fn utf16_is_refused_with_a_hint() {
        for bom in [[0xFF, 0xFE], [0xFE, 0xFF]] {
            let mut bytes = bom.to_vec();
            bytes.extend(MINIMAL.encode_utf16().flat_map(u16::to_le_bytes));
            let e = parse_bytes(&bytes).unwrap_err().0;
            assert!(e.contains("UTF-16") && e.contains("UTF-8"), "{e}");
        }
    }

    #[test]
    fn non_utf8_is_refused() {
        let bytes = [b'{', 0xC3, 0x28, b'}'];
        assert_eq!(
            parse_bytes(&bytes).unwrap_err().0,
            "The file is not UTF-8 text."
        );
    }

    #[test]
    fn oversize_input_is_refused() {
        let pad = " ".repeat(MAX_FILE_BYTES);
        let text = format!("{MINIMAL}{pad}");
        let e = parse(&text).unwrap_err().0;
        assert!(e.contains("larger than 256 KB"), "{e}");
        assert_eq!(parse_bytes(text.as_bytes()).unwrap_err().0, e);
        // Exactly at the limit is still read.
        let at_limit = format!("{MINIMAL}{}", " ".repeat(MAX_FILE_BYTES - MINIMAL.len()));
        assert!(parse(&at_limit).is_ok());
    }

    #[test]
    fn not_json_reports_line_and_column() {
        let e = err("{\n  \"format\": \"cairn.profile\",\n  oops\n}");
        assert!(e.starts_with("The file is not valid JSON: "), "{e}");
        assert!(e.contains("line 3 column"), "{e}");
    }

    #[test]
    fn array_or_wrong_format_is_not_a_profile() {
        for text in [
            "[1, 2]",
            "\"cairn.profile\"",
            "42",
            r#"{"schema": 1, "name": "x"}"#,
            r#"{"format": "other.profile", "schema": 1}"#,
            r#"{"format": 1, "schema": 1}"#,
        ] {
            assert_eq!(err(text), "This file is not a Cairn profile.", "{text}");
        }
    }

    #[test]
    fn missing_schema() {
        let e = err(r#"{"format": "cairn.profile", "name": "x", "tweaks": ["gaming.game_mode"]}"#);
        assert!(e.contains("no \"schema\""), "{e}");
    }

    #[test]
    fn schema_zero_string_and_fraction_are_invalid() {
        for schema in ["0", "\"1\"", "1.5", "1.0", "-1", "null", "true"] {
            let text = format!(
                r#"{{"format": "cairn.profile", "schema": {schema}, "name": "x", "tweaks": ["gaming.game_mode"]}}"#
            );
            let e = err(&text);
            assert!(e.contains("whole number"), "{schema}: {e}");
        }
    }

    #[test]
    fn newer_schema_asks_to_update() {
        let e = err(r#"{"format": "cairn.profile", "schema": 2, "name": "x", "future": {}}"#);
        assert_eq!(
            e,
            "This profile was made by a newer version of Cairn (profile format 2). Update Cairn \
             to open it."
        );
    }

    #[test]
    fn unknown_top_level_field_is_refused() {
        let e = err(&with(
            r#""tweaks": ["gaming.game_mode"], "registry": ["HKLM\\x"]"#,
        ));
        assert!(
            e.starts_with("The profile is not valid: unknown field `registry`"),
            "{e}"
        );
    }

    #[test]
    fn unknown_nested_field_is_refused() {
        let e = err(&with(r#""dns": {"lan": {"ipv4": "cloudflare"}}"#));
        assert!(e.contains("unknown field `lan`"), "{e}");
        let e = err(&with(
            r#""startup": [{"id": "user_run:Discord", "path": "C:\\Users\\Test\\x.exe"}]"#,
        ));
        assert!(e.contains("unknown field `path`"), "{e}");
        let e = err(&with(r#""windows_update": {"pause_days": 7}"#));
        assert!(e.contains("unknown field `pause_days`"), "{e}");
        let e = err(&with(
            r#""maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "command": "x"}"#,
        ));
        assert!(e.contains("unknown field `command`"), "{e}");
    }

    #[test]
    fn duplicate_field_is_refused() {
        let e = err(&with(r#""tweaks": ["gaming.game_mode"], "name": "Again""#));
        assert!(e.contains("duplicate field `name`"), "{e}");
        let e = err(&with(
            r#""dns": {"wifi": {"ipv4": "quad9", "ipv4": "google"}}"#,
        ));
        assert!(e.contains("duplicate field `ipv4`"), "{e}");
    }

    #[test]
    fn wrong_types_are_refused() {
        let e = err(&with(r#""tweaks": "gaming.game_mode""#));
        assert!(
            e.starts_with("The profile is not valid: invalid type"),
            "{e}"
        );
        let e = err(&with(r#""tweaks": ["gaming.game_mode"], "description": 5"#));
        assert!(e.contains("invalid type"), "{e}");
    }

    #[test]
    fn name_rules() {
        let named = |name: &str| {
            parse(
                &serde_json::json!({"format": FORMAT, "schema": 1, "name": name,
                    "tweaks": ["gaming.game_mode"]})
                .to_string(),
            )
        };
        assert_eq!(named("").unwrap_err().0, "The profile needs a name.");
        assert_eq!(named("   ").unwrap_err().0, "The profile needs a name.");
        assert!(named(&"x".repeat(61))
            .unwrap_err()
            .0
            .contains("longer than 60"));
        assert!(named("a\u{7}b")
            .unwrap_err()
            .0
            .contains("control characters"));
        assert!(named("a\nb").unwrap_err().0.contains("control characters"));
        assert_eq!(named(&"x".repeat(60)).unwrap().name.len(), 60);
        assert_eq!(
            named("  Spiel-PC für Ärger 🎮  ").unwrap().name,
            "Spiel-PC für Ärger 🎮"
        );
        assert_eq!(named("ゲーム").unwrap().name, "ゲーム");
    }

    #[test]
    fn description_and_created_rules() {
        let desc = |d: &str| {
            parse(
                &serde_json::json!({"format": FORMAT, "schema": 1, "name": "x", "description": d,
                    "tweaks": ["gaming.game_mode"]})
                .to_string(),
            )
        };
        assert!(desc(&"d".repeat(301))
            .unwrap_err()
            .0
            .contains("longer than 300"));
        assert!(desc("tab\there")
            .unwrap_err()
            .0
            .contains("control characters"));
        assert_eq!(desc("  kept  ").unwrap().description, "kept");
        assert_eq!(desc(&"d".repeat(300)).unwrap().description.len(), 300);

        let created = |c: &str| {
            parse(
                &serde_json::json!({"format": FORMAT, "schema": 1, "name": "x", "created": c,
                    "tweaks": ["gaming.game_mode"]})
                .to_string(),
            )
        };
        assert_eq!(
            created("2026-09-28").unwrap().created.as_deref(),
            Some("2026-09-28")
        );
        for bad in [
            "2026-13-01",
            "28.09.2026",
            "2026-9-28",
            "yesterday",
            "2026-02-30",
        ] {
            assert!(created(bad).unwrap_err().0.contains("YYYY-MM-DD"), "{bad}");
        }
        let with_version = |v: &str| {
            parse(
                &serde_json::json!({"format": FORMAT, "schema": 1, "name": "x", "created_with": v,
                    "tweaks": ["gaming.game_mode"]})
                .to_string(),
            )
        };
        assert!(with_version(&"v".repeat(41))
            .unwrap_err()
            .0
            .contains("longer than 40"));
        assert_eq!(
            with_version("Cairn 9.9.9").unwrap().created_with.as_deref(),
            Some("Cairn 9.9.9")
        );
    }

    #[test]
    fn tweak_id_grammar() {
        for bad in [
            "Privacy.location",
            "privacy.Location",
            r"HKLM\SOFTWARE\x",
            "privacy.a b",
            "privacy..x",
            "privacy.",
            ".x",
            "privacy",
            "1privacy.x",
            "privacy.x.y",
            "privacy.x-y",
            " privacy.x",
        ] {
            assert!(!is_tweak_id(bad), "{bad}");
        }
        assert!(!is_tweak_id(&format!("privacy.{}", "a".repeat(61))));
        assert!(!is_tweak_id(&format!("privacyprivacy.{}", "a".repeat(50))));
        assert!(is_tweak_id("privacy.new_future_id"));
        assert!(is_tweak_id("gaming.game_mode"));
        assert!(is_tweak_id(&format!("privacy.{}", "a".repeat(56))));
        let e = err(&with(
            r#""tweaks": ["gaming.game_mode", "privacy.ok", "x.y", "Privacy.Location"]"#,
        ));
        assert_eq!(
            e,
            "tweaks[3] \"Privacy.Location\" is not a valid setting id."
        );
    }

    #[test]
    fn app_name_grammar() {
        for bad in [
            r"C:\x",
            r"..\x",
            "*",
            "a*",
            "a",
            "cmd /c",
            ".Microsoft",
            "-x",
            "Micro*soft",
            "Microsoft.Bing_News",
            "Microsoft.BingNews**",
        ] {
            assert!(!is_app_name(bad), "{bad}");
        }
        assert!(!is_app_name(&"A".repeat(51)));
        assert!(is_app_name(&"A".repeat(50)));
        for good in [
            "king.com.*",
            "Microsoft.*",
            "Foo.Bar",
            "ab",
            "ab*",
            "Microsoft.549981C3F5F10",
        ] {
            assert!(is_app_name(good), "{good}");
        }
        let e = err(&with(r#""apps": ["Microsoft.BingNews", "cmd /c del"]"#));
        assert_eq!(e, "apps[1] \"cmd /c del\" is not a Store app name.");
    }

    #[test]
    fn startup_id_grammar() {
        for bad in [
            "Discord",
            "user_run:",
            ":Discord",
            "User_Run:Discord",
            "user-run:Discord",
            "user_run:Dis\u{1}cord",
            "user_run:a\nb",
        ] {
            assert!(!is_startup_id(bad), "{bad:?}");
        }
        assert!(!is_startup_id(&format!("user_run:{}", "k".repeat(261))));
        assert!(!is_startup_id(&format!("{}:x", "s".repeat(33))));
        assert!(is_startup_id("user_run:Discord"));
        assert!(is_startup_id("future_source:Some Name"));
        assert!(is_startup_id(
            r"packaged_task:Microsoft.WindowsTerminal_8wekyb3d8bbwe\StartTerminalOnLoginTask"
        ));
        assert!(is_startup_id("user_run:a:b"));
        let e = err(&with(r#""startup": [{"id": "Discord"}]"#));
        assert_eq!(e, "startup[0].id must look like source:name.");
        let e = err(&with(&format!(
            r#""startup": [{{"id": "user_run:x", "name": "{}"}}]"#,
            "n".repeat(121)
        )));
        assert!(e.contains("startup[0].name is longer than 120"), "{e}");
    }

    #[test]
    fn dns_rules() {
        let e = err(&with(r#""dns": {"wifi": {}}"#));
        assert_eq!(e, "dns.wifi names no address family.");
        let e = err(&with(r#""dns": {"ethernet": {"ipv4": "1.1.1.1"}}"#));
        assert_eq!(e, "dns.ethernet.ipv4 \"1.1.1.1\" is not a DNS choice id.");
        let e = err(&with(r#""dns": {"ethernet": {"ipv6": "Cloudflare"}}"#));
        assert!(e.contains("is not a DNS choice id"), "{e}");
        let p = parse(&with(r#""dns": {"wifi": {"ipv6": "future_preset"}}"#)).unwrap();
        assert_eq!(p.dns.wifi.unwrap().ipv6.as_deref(), Some("future_preset"));
        // An empty dns object sets nothing.
        assert_eq!(
            err(&with(r#""dns": {}"#)),
            "This profile contains no settings."
        );
    }

    #[test]
    fn windows_update_ranges() {
        let wu = |json: &str| parse(&with(&format!(r#""windows_update": {json}"#)));
        assert_eq!(wu("{}").unwrap_err().0, "windows_update sets nothing.");
        assert_eq!(
            wu(r#"{"exclude_drivers": false}"#).unwrap_err().0,
            "windows_update sets nothing."
        );
        for days in ["0", "366"] {
            assert!(wu(&format!(r#"{{"defer_feature_days": {days}}}"#))
                .unwrap_err()
                .0
                .contains("from 1 to 365"));
        }
        assert!(wu(r#"{"defer_feature_days": -1}"#)
            .unwrap_err()
            .0
            .contains("invalid value"));
        assert_eq!(
            wu(r#"{"defer_feature_days": 365}"#)
                .unwrap()
                .windows_update
                .unwrap()
                .defer_feature_days,
            Some(365)
        );
        assert!(wu(r#"{"active_hours": {"automatic": true, "start": 8}}"#)
            .unwrap_err()
            .0
            .contains("automatic"));
        assert!(wu(r#"{"active_hours": {"start": 8}}"#)
            .unwrap_err()
            .0
            .contains("needs start and end"));
        assert!(wu(r#"{"active_hours": {"start": 8, "end": 24}}"#)
            .unwrap_err()
            .0
            .contains("0 to 23"));
        assert!(wu(r#"{"active_hours": {"start": 8, "end": 8}}"#)
            .unwrap_err()
            .0
            .contains("differ"));
        assert!(wu(r#"{"active_hours": {"start": 1, "end": 20}}"#)
            .unwrap_err()
            .0
            .contains("18 hours"));
        // The span wraps around midnight: 20:00 to 12:00 is 16 hours.
        let p = wu(r#"{"active_hours": {"start": 20, "end": 12}}"#).unwrap();
        assert_eq!(
            p.windows_update.unwrap().active_hours,
            Some(ActiveHoursChoice {
                automatic: false,
                start: Some(20),
                end: Some(12)
            })
        );
        assert!(wu(r#"{"active_hours": {"start": 5, "end": 0}}"#)
            .unwrap_err()
            .0
            .contains("18 hours"));
        assert!(wu(r#"{"active_hours": {"start": 6, "end": 0}}"#).is_ok());
        assert!(wu(r#"{"active_hours": {"automatic": true}}"#).is_ok());
        assert!(wu(r#"{"restart_notify": false}"#).is_ok());
        assert!(wu(r#"{"pause_days": 7}"#)
            .unwrap_err()
            .0
            .contains("unknown field"));
    }

    #[test]
    fn maintenance_rules() {
        let m = |json: &str| parse(&with(&format!(r#""maintenance": {json}"#)));
        assert!(m(r#"{"enabled": false}"#).is_ok());
        assert!(m(r#"{"enabled": false, "sfc_verify": true}"#)
            .unwrap_err()
            .0
            .contains("can't set anything else"));
        assert!(m(r#"{"enabled": false, "day": "monday"}"#).is_err());
        assert_eq!(
            m(r#"{"enabled": true, "time": "12:00", "sfc_verify": true}"#)
                .unwrap_err()
                .0,
            "maintenance.day is required."
        );
        assert_eq!(
            m(r#"{"enabled": true, "day": "sunday", "sfc_verify": true}"#)
                .unwrap_err()
                .0,
            "maintenance.time is required."
        );
        for time in ["24:00", "12:60", "7:00", "12.00", "noon", "12:00:00"] {
            assert_eq!(
                m(&format!(
                    r#"{{"enabled": true, "day": "sunday", "time": "{time}", "sfc_verify": true}}"#
                ))
                .unwrap_err()
                .0,
                "maintenance.time must be HH:MM.",
                "{time}"
            );
        }
        assert!(
            m(r#"{"enabled": true, "day": "funday", "time": "12:00", "sfc_verify": true}"#)
                .unwrap_err()
                .0
                .contains("unknown variant")
        );
        assert!(m(r#"{"enabled": true, "day": "sunday", "time": "12:00"}"#)
            .unwrap_err()
            .0
            .contains("does nothing"));
        assert!(
            m(r#"{"enabled": true, "day": "sunday", "time": "12:00", "clean": ["C:\\Temp"]}"#)
                .unwrap_err()
                .0
                .contains("is not a cleanup target id")
        );
        let many: Vec<String> = (0..17)
            .map(|i| format!("\"t{}\"", "x".repeat(i + 1)))
            .collect();
        assert!(m(&format!(
            r#"{{"enabled": true, "day": "sunday", "time": "12:00", "clean": [{}]}}"#,
            many.join(",")
        ))
        .unwrap_err()
        .0
        .contains("more than 16"));
        let p = m(r#"{"enabled": true, "day": "sunday", "time": "00:00", "clean": ["user_temp", "user_temp", "windows_temp"], "dism_check": true}"#)
            .unwrap();
        let choice = p.maintenance.unwrap();
        assert_eq!(choice.day, Some(ScheduleDay::Sunday));
        assert_eq!(choice.clean, vec!["user_temp", "windows_temp"]);
        assert!(choice.dism_check && !choice.sfc_verify);
    }

    #[test]
    fn empty_profile_has_no_settings() {
        assert_eq!(
            err(r#"{"format": "cairn.profile", "schema": 1, "name": "Empty"}"#),
            "This profile contains no settings."
        );
        assert_eq!(
            err(&with(r#""tweaks": [], "apps": [], "startup": []"#)),
            "This profile contains no settings."
        );
    }

    #[test]
    fn limits_per_section() {
        let ids: Vec<String> = (0..257).map(|i| format!("\"privacy.x{i}\"")).collect();
        assert_eq!(
            err(&with(&format!(r#""tweaks": [{}]"#, ids.join(",")))),
            "There are more than 256 tweaks."
        );
        let ids: Vec<String> = (0..256).map(|i| format!("\"privacy.x{i}\"")).collect();
        assert_eq!(
            parse(&with(&format!(r#""tweaks": [{}]"#, ids.join(","))))
                .unwrap()
                .tweaks
                .len(),
            256
        );
        let apps: Vec<String> = (0..129).map(|i| format!("\"App.N{i}\"")).collect();
        assert_eq!(
            err(&with(&format!(r#""apps": [{}]"#, apps.join(",")))),
            "There are more than 128 apps."
        );
        let entries: Vec<String> = (0..257)
            .map(|i| format!(r#"{{"id": "user_run:e{i}"}}"#))
            .collect();
        assert_eq!(
            err(&with(&format!(r#""startup": [{}]"#, entries.join(",")))),
            "There are more than 256 startup entries."
        );
    }

    #[test]
    fn dedupe_keeps_first_spelling() {
        let p = parse(&with(
            r#""tweaks": ["gaming.game_mode", "privacy.cortana", "gaming.game_mode"],
               "apps": ["Microsoft.BingNews", "microsoft.bingnews", "KING.COM.*", "king.com.*"],
               "startup": [{"id": "user_run:Discord", "name": "Discord"}, {"id": "user_run:DISCORD", "name": "Other"}]"#,
        ))
        .unwrap();
        assert_eq!(p.tweaks, vec!["gaming.game_mode", "privacy.cortana"]);
        assert_eq!(p.apps, vec!["Microsoft.BingNews", "KING.COM.*"]);
        assert_eq!(p.startup.len(), 1);
        assert_eq!(p.startup[0].name, "Discord");
    }

    #[test]
    fn hostile_strings_never_parse_into_paths() {
        let hostile = [
            r"C:\Windows\System32\cmd.exe",
            r"\\server\share\x",
            r"..\..\Windows",
            "HKLM\\SOFTWARE\\Microsoft",
            "cmd /c del /q C:\\",
            "powershell -enc AAAA",
            "https://example.com/x",
            "198.51.100.53",
            "%SystemRoot%\\x",
            "$(Get-Item)",
            "a;b",
            "a|b",
            "\u{0}",
        ];
        let plain = |s: &str| {
            !s.chars()
                .any(|c| matches!(c, '\\' | '/' | ':' | '%' | '$' | ';' | '|') || c.is_whitespace())
        };
        for value in hostile {
            let tweaks = serde_json::json!({"format": FORMAT, "schema": 1, "name": "x",
                "tweaks": [value]})
            .to_string();
            assert!(parse(&tweaks).is_err(), "tweaks: {value}");
            // A well-formed package Name such as "198.51.100.53" is kept as a plain string
            // that is only ever compared with the catalog.
            let apps = serde_json::json!({"format": FORMAT, "schema": 1, "name": "x",
                "apps": [value]})
            .to_string();
            match parse(&apps) {
                Ok(p) => assert!(plain(&p.apps[0]), "apps: {value}"),
                Err(e) => assert!(e.0.starts_with("apps[0]"), "{e}"),
            }
            let dns = serde_json::json!({"format": FORMAT, "schema": 1, "name": "x",
                "dns": {"wifi": {"ipv4": value}}})
            .to_string();
            assert!(parse(&dns).is_err(), "dns: {value}");
            let clean = serde_json::json!({"format": FORMAT, "schema": 1, "name": "x",
                "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "clean": [value]}})
            .to_string();
            assert!(parse(&clean).is_err(), "clean: {value}");
            // A startup id may carry any key text, but it stays a plain string compared with
            // listed entries; control characters are refused.
            let startup = serde_json::json!({"format": FORMAT, "schema": 1, "name": "x",
                "startup": [{"id": format!("user_run:{value}")}]})
            .to_string();
            match parse(&startup) {
                Ok(p) => assert_eq!(p.startup[0].id, format!("user_run:{value}")),
                Err(e) => assert!(e.0.contains("startup[0].id"), "{e}"),
            }
        }
    }

    #[test]
    fn looks_like_profile_accepts_any_schema() {
        assert!(looks_like_profile(MINIMAL));
        assert!(looks_like_profile(
            r#"{"format": "cairn.profile", "schema": 7}"#
        ));
        assert!(!looks_like_profile("[]"));
        assert!(!looks_like_profile("not json"));
        assert!(!looks_like_profile(r#"{"format": "x"}"#));
    }

    // ───────────────────────────── files ─────────────────────────────

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_file_replaces_a_profile_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Gaming PC.json");
        let first = to_text(&profile());
        write_file(&path, &first).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        let mut second = profile();
        second.name = "Second".into();
        let second = to_text(&second);
        write_file(&path, &second).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), second);
        assert!(temp_files(dir.path()).is_empty());
        // A profile of a newer schema is still a Cairn profile and may be replaced.
        fs::write(&path, r#"{"format": "cairn.profile", "schema": 9}"#).unwrap();
        write_file(&path, &first).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        // The extension is compared ignoring case.
        write_file(&dir.path().join("UPPER.JSON"), &first).unwrap();
    }

    #[test]
    fn write_file_leaves_no_temp_on_error() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.json");
        fs::write(&path, to_text(&profile())).unwrap();
        // Held open without delete sharing, the file can be read but not replaced.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        let mut other = profile();
        other.name = "Other".into();
        let e = write_file(&path, &to_text(&other)).unwrap_err().to_string();
        assert!(e.starts_with("held.json could not be saved: "), "{e}");
        assert!(temp_files(dir.path()).is_empty());
        drop(held);
        assert_eq!(fs::read_to_string(&path).unwrap(), to_text(&profile()));

        // A missing folder fails before any file is made.
        let missing = dir.path().join("no such folder").join("x.json");
        assert!(write_file(&missing, "{}").is_err());
        assert!(temp_files(dir.path()).is_empty());
    }

    #[test]
    fn write_file_requires_json_extension() {
        let dir = tempfile::tempdir().unwrap();
        let text = to_text(&profile());
        for name in ["profile.txt", "profile", "profile.json.exe"] {
            let e = write_file(&dir.path().join(name), &text)
                .unwrap_err()
                .to_string();
            assert_eq!(e, "Profiles are saved as .json files.", "{name}");
        }
        let e = write_file(Path::new("relative.json"), &text)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "Profiles are saved as .json files.");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn write_file_refuses_non_profile_file_folder_and_junction() {
        let dir = tempfile::tempdir().unwrap();
        let text = to_text(&profile());

        let other = dir.path().join("settings.json");
        fs::write(&other, r#"{"editor.fontSize": 14}"#).unwrap();
        let e = write_file(&other, &text).unwrap_err().to_string();
        assert!(
            e.contains("already exists and is not a Cairn profile"),
            "{e}"
        );
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            r#"{"editor.fontSize": 14}"#
        );

        let folder = dir.path().join("folder.json");
        fs::create_dir(&folder).unwrap();
        let e = write_file(&folder, &text).unwrap_err().to_string();
        assert_eq!(
            e,
            "folder.json is a folder or a link; choose another file name."
        );

        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let junction = dir.path().join("junction.json");
        let cmd = crate::win::paths::system_dir().unwrap().join("cmd.exe");
        let status = std::process::Command::new(cmd)
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&junction)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let e = write_file(&junction, &text).unwrap_err().to_string();
        assert_eq!(
            e,
            "junction.json is a folder or a link; choose another file name."
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        fs::remove_dir(&junction).unwrap();
        assert!(temp_files(dir.path()).is_empty());
    }

    #[test]
    fn read_file_caps_size_and_refuses_folders() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.json");
        fs::write(&big, vec![b' '; MAX_FILE_BYTES + 5000]).unwrap();
        let bytes = read_file(&big).unwrap();
        assert_eq!(bytes.len(), MAX_FILE_BYTES + 1);
        assert!(parse_bytes(&bytes)
            .unwrap_err()
            .0
            .contains("larger than 256 KB"));

        let small = dir.path().join("small.json");
        fs::write(&small, MINIMAL).unwrap();
        assert_eq!(read_file(&small).unwrap(), MINIMAL.as_bytes());

        let e = read_file(dir.path()).unwrap_err().to_string();
        assert!(e.ends_with(" is a folder."), "{e}");
        let e = read_file(&dir.path().join("missing.json"))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("missing.json could not be read: "), "{e}");
        assert!(!e.contains("OneDrive"), "{e}");
    }

    #[test]
    fn export_fields_and_paths_are_checked_like_the_file() {
        assert!(check_export_fields("Desk PC", "").is_ok());
        assert_eq!(
            check_export_fields("  ", "").unwrap_err().0,
            "The profile needs a name."
        );
        assert!(check_export_fields(&"n".repeat(61), "")
            .unwrap_err()
            .0
            .contains("60"));
        assert!(check_export_fields("a\u{1b}b", "")
            .unwrap_err()
            .0
            .contains("control"));
        assert!(check_export_fields("x", &"d".repeat(301))
            .unwrap_err()
            .0
            .contains("300"));
        assert!(is_profile_path(Path::new(
            r"C:\Users\Test\Documents\Gaming.JSON"
        )));
        assert!(!is_profile_path(Path::new("Gaming.json")));
        assert!(!is_profile_path(Path::new(
            r"C:\Users\Test\Documents\Gaming.txt"
        )));
    }

    #[test]
    fn invalid_converts_to_an_engine_error_with_its_message() {
        let e: Error = Invalid("The profile needs a name.".into()).into();
        assert_eq!(e.to_string(), "The profile needs a name.");
        assert_eq!(Invalid("x".into()).to_string(), "x");
    }
}
