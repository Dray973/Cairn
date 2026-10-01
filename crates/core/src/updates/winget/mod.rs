//! winget: app update checks, upgrades and installs run as background jobs.
//!
//! Cairn drives the winget command line of the App Installer package, started by its path
//! in the package folder ([`locate`]), never through the per-user alias. Every job runs on
//! the `winget` lane ([`lane`]), a [`JobHost`] polled by the UI:
//!
//! - `winget_scan` (the update check) runs `winget --version`, `winget export` (the
//!   installed apps as JSON, the same in every language) and `winget upgrade` (the table of
//!   available updates, read by [`table`]). It is read-only, writes no audit rows and needs
//!   no elevation.
//! - `winget_upgrade` and `winget_install` update or install the chosen apps one after
//!   another. They need an elevated Cairn run by the signed-in user. Each app gets an
//!   ops_log row "started" ([`OP_UPGRADE`], [`OP_INSTALL`]) before its winget starts and
//!   exactly one final row; nothing is journaled, because updates and installs cannot be
//!   undone. A stop request ends the batch after the current app.
//!
//! Arguments are always separate argv elements; the flags winget uses are the fixed ones of
//! [`item_args`], and [`FORBIDDEN_FLAGS`] are never passed. Test and dev runs refuse
//! upgrades and installs through [`app_installs_forbidden`].

pub mod codes;
pub mod export;
pub mod locate;
pub mod progress;
pub mod table;
mod work;

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use codes::{item_outcome, retry_may_help, scan_outcome, ItemState, ScanCode};
pub use export::{parse_export, InstalledPackage, Inventory, MAX_EXPORT_BYTES};
pub use locate::{locate, windows_apps_root, WingetLocation, APP_INSTALLER_FAMILY};
pub use progress::{step_fraction, step_sizes};
pub use table::{parse_upgrade_table, valid_package_id, valid_source, TableRow, UpgradeTable};

use crate::jobs::{HostConfig, HostJobSnapshot, JobHost, JobSpec};
use crate::safety::state_log::{data_dir, Journal};
use crate::tools::launch::{Launcher, ProgramRoot, SystemLauncher};
use crate::tools::ToolId;
use crate::win::process::{hardened_command, CREATE_NO_WINDOW};
use crate::{Error, Result};

/// Oldest winget with `--no-upgrade` and `--disable-interactivity`.
pub const MIN_VERSION: WingetVersion = WingetVersion {
    major: 1,
    minor: 6,
    patch: 0,
};
/// App Installer in the Microsoft Store.
pub const APP_INSTALLER_STORE_URI: &str = "ms-windows-store://pdp/?ProductId=9NBLGGH4NNS1";
/// The Microsoft Store's updates page.
pub const STORE_UPDATES_URI: &str = "ms-windows-store://downloadsandupdates";
/// ops_log op of an app update.
pub const OP_UPGRADE: &str = "app_upgrade";
/// ops_log op of an app install.
pub const OP_INSTALL: &str = "app_install";
/// Most apps one batch updates or installs.
pub const MAX_BATCH_ITEMS: usize = 200;
/// How long `winget --version` may take.
pub const VERSION_DEADLINE: Duration = Duration::from_secs(30);
/// How long the export and the upgrade listing may each take.
pub const LIST_DEADLINE: Duration = Duration::from_secs(300);
/// How long one app's update or install is waited for.
pub const ITEM_DEADLINE: Duration = Duration::from_secs(3600);
/// The variable that turns upgrades and installs off in test and dev runs.
pub const FORBID_ENV: &str = "OPTIMIZER_FORBID_APP_INSTALLS";
/// The lane's name.
pub const LANE: &str = "winget";
/// winget's own package id; the Microsoft Store updates it.
pub const APP_INSTALLER_ID: &str = "Microsoft.AppInstaller";

/// Flags Cairn never passes to winget.
pub const FORBIDDEN_FLAGS: &[&str] = &[
    "--force",
    "--ignore-security-hash",
    "--allow-reboot",
    "--include-unknown",
    "--include-pinned",
    "--all",
    "-r",
    "--recurse",
    "--interactive",
    "-i",
    "--override",
    "--custom",
    "--location",
    "-l",
    "--purge",
    "--uninstall-previous",
    "--skip-dependencies",
    "--ignore-local-archive-malware-scan",
    "--scope",
    "--version",
    "-v",
];

/// The first of [`FORBIDDEN_FLAGS`] in `args`. `--version` alone (winget's own version, the
/// whole command) is allowed; with a subcommand it would pick a package version.
pub fn forbidden_flag(args: &[String]) -> Option<&str> {
    if args.len() == 1 && args[0] == "--version" {
        return None;
    }
    args.iter().map(String::as_str).find(|arg| {
        FORBIDDEN_FLAGS.contains(arg)
            || arg.starts_with("--override=")
            || arg.starts_with("--custom=")
    })
}

pub const WINGET_MISSING_TEXT: &str = "winget (App Installer) isn't set up for this account. \
     Windows installs it shortly after your first sign-in; you can also get App Installer from \
     the Microsoft Store. Then choose Check again.";
pub const OTHER_USER_TEXT: &str = "Cairn is running as a different account than the one signed \
     in, so app updates and installs would go to that account. They are turned off; start \
     Cairn from your own account to use them.";
pub const USER_UNKNOWN_TEXT: &str = "Cairn can't tell which account is signed in, so app \
     updates and installs are turned off to be safe.";
pub const INSTALLS_FORBIDDEN_TEXT: &str =
    "App installs are turned off in this environment (OPTIMIZER_FORBID_APP_INSTALLS).";
pub const CLOSING_TEXT: &str = "Cairn is closing.";
pub const BATTERY_NOTE: &str =
    "Your PC is on battery power. Updates can take a while; plug it in if you can.";
pub const MSI_BUSY_NOTE: &str = "Another installation is running; these may wait for it or fail.";
pub const WINGET_RUNNING_NOTE: &str =
    "winget is already running in another window; these may wait for it.";
pub const ELEVATED_NOTE: &str = "Apps installed only for your account are installed from an \
     administrator app; a few refuse that and are reported as failed.";
pub const APP_INSTALLER_NOTE: &str = "Updated by the Microsoft Store.";
pub const EXPLICIT_NOTE: &str = "winget updates this app only when it is picked by name; it is \
     not included in Update all.";
pub const STORE_NOTE: &str =
    "Microsoft Store app  ·  if this fails, update it in the Microsoft Store.";
pub const TRUNCATED_NOTE: &str = "winget cut this app's id short; update it from the app itself.";

/// "winget {version} is too old; …".
pub fn outdated_text(version: &str) -> String {
    format!(
        "winget {version} is too old; Cairn needs {MIN_VERSION} or newer. Update App Installer \
         from the Microsoft Store, then choose Check again."
    )
}

/// True in unit tests and whenever `OPTIMIZER_FORBID_APP_INSTALLS` is "1": upgrades and
/// installs are then refused before anything starts. The update check is not affected.
pub fn app_installs_forbidden() -> bool {
    cfg!(test) || std::env::var(FORBID_ENV).is_ok_and(|v| v == "1")
}

// ───────────────────────────── Versions and status ─────────────────────────────

/// winget's version, as `winget --version` prints it ("v1.29.380").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct WingetVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl WingetVersion {
    /// "v1.29.380", "1.30.0-preview" and "1.6" parse; anything else is `None`.
    pub fn parse(text: &str) -> Option<WingetVersion> {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        let mut parts = text.split('.');
        let number = |part: Option<&str>| -> Option<u32> {
            let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        let major = number(parts.next())?;
        let minor = number(parts.next())?;
        let patch = match parts.next() {
            Some(part) => number(Some(part))?,
            None => 0,
        };
        Some(WingetVersion {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for WingetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether Cairn can use winget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ready,
    Missing,
    OtherUser,
    UserUnknown,
}

/// winget as Cairn sees it, without starting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WingetStatus {
    pub availability: Availability,
    /// Why it cannot be used; `None` when ready.
    pub message: Option<String>,
    pub location: Option<WingetLocation>,
    pub elevated: bool,
    pub min_version: String,
    pub store_uri: &'static str,
}

/// The system facts a plan and a job read; tests substitute their own.
#[derive(Clone, Copy)]
pub struct WingetEnv {
    pub elevated: fn() -> bool,
    /// [`crate::win::session::elevated_as_other_user`].
    pub other_user: fn() -> Result<bool>,
    pub locate: fn() -> Result<Option<WingetLocation>>,
    /// Lowercase names of running programs.
    pub processes: fn() -> Option<HashSet<String>>,
    /// Another Windows Installer install runs (`Global\_MSIExecute` exists).
    pub msi_busy: fn() -> Option<bool>,
    pub on_battery: fn() -> Option<bool>,
    /// Title of a running System File Checker or DISM repair job.
    pub servicing_tool: fn() -> Option<String>,
    pub now: fn() -> DateTime<Utc>,
    pub installs_forbidden: fn() -> bool,
}

impl fmt::Debug for WingetEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WingetEnv").finish_non_exhaustive()
    }
}

fn system_servicing_tool() -> Option<String> {
    let job = crate::tools::runner().running()?;
    matches!(job.tool, ToolId::SfcScan | ToolId::DismRestore).then_some(job.title)
}

impl WingetEnv {
    /// This PC.
    pub const SYSTEM: WingetEnv = WingetEnv {
        elevated: crate::is_elevated,
        other_user: crate::win::session::elevated_as_other_user,
        locate: locate::locate,
        processes: crate::win::process::running_process_names,
        msi_busy: || Some(crate::win::mutex::exists(r"Global\_MSIExecute")),
        on_battery: || crate::win::power::power_source().on_battery,
        servicing_tool: system_servicing_tool,
        now: Utc::now,
        installs_forbidden: app_installs_forbidden,
    };
}

/// winget's status on this PC for this account. Starts no process and uses no network.
pub fn winget_status() -> WingetStatus {
    winget_status_with(&WingetEnv::SYSTEM)
}

pub(crate) fn winget_status_with(env: &WingetEnv) -> WingetStatus {
    let mut status = WingetStatus {
        availability: Availability::Ready,
        message: None,
        location: None,
        elevated: (env.elevated)(),
        min_version: MIN_VERSION.to_string(),
        store_uri: APP_INSTALLER_STORE_URI,
    };
    match (env.other_user)() {
        Ok(false) => {}
        Ok(true) => {
            status.availability = Availability::OtherUser;
            status.message = Some(OTHER_USER_TEXT.to_string());
            return status;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot compare this process's account with the signed-in user");
            status.availability = Availability::UserUnknown;
            status.message = Some(USER_UNKNOWN_TEXT.to_string());
            return status;
        }
    }
    match (env.locate)() {
        Ok(Some(location)) => status.location = Some(location),
        Ok(None) => {
            status.availability = Availability::Missing;
            status.message = Some(WINGET_MISSING_TEXT.to_string());
        }
        Err(e) => {
            status.availability = Availability::Missing;
            status.message = Some(e.to_string());
        }
    }
    status
}

// ───────────────────────────── Requests ─────────────────────────────

/// What a winget job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatesKind {
    Scan,
    Upgrade,
    Install,
}

impl UpdatesKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdatesKind::Scan => "scan",
            UpdatesKind::Upgrade => "upgrade",
            UpdatesKind::Install => "install",
        }
    }

    pub fn parse(text: &str) -> Option<UpdatesKind> {
        match text.trim().to_ascii_lowercase().as_str() {
            "scan" => Some(UpdatesKind::Scan),
            "upgrade" => Some(UpdatesKind::Upgrade),
            "install" => Some(UpdatesKind::Install),
            _ => None,
        }
    }

    /// The job kind of the lane.
    pub fn job_kind(self) -> &'static str {
        match self {
            UpdatesKind::Scan => "winget_scan",
            UpdatesKind::Upgrade => "winget_upgrade",
            UpdatesKind::Install => "winget_install",
        }
    }

    fn from_job_kind(kind: &str) -> Option<UpdatesKind> {
        [
            UpdatesKind::Scan,
            UpdatesKind::Upgrade,
            UpdatesKind::Install,
        ]
        .into_iter()
        .find(|k| k.job_kind() == kind)
    }

    fn already_running_text(self) -> &'static str {
        match self {
            UpdatesKind::Scan => "Cairn is already checking for app updates.",
            UpdatesKind::Upgrade => "Cairn is already updating apps.",
            UpdatesKind::Install => "Cairn is already installing apps.",
        }
    }
}

fn winget_source() -> String {
    "winget".to_string()
}

/// One app to update or install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateItem {
    pub id: String,
    #[serde(default = "winget_source")]
    pub source: String,
    #[serde(default)]
    pub name: String,
    /// Installed version (updates), for the record.
    #[serde(default)]
    pub from: Option<String>,
    /// Version offered (updates), for the record.
    #[serde(default)]
    pub to: Option<String>,
}

/// Longest name or version text kept from a request.
const MAX_TEXT: usize = 200;

fn bounded(text: &str) -> String {
    text.trim().chars().take(MAX_TEXT).collect()
}

/// A validated winget job request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatesRequest {
    kind: UpdatesKind,
    items: Vec<UpdateItem>,
}

impl UpdatesRequest {
    pub fn scan() -> UpdatesRequest {
        UpdatesRequest {
            kind: UpdatesKind::Scan,
            items: Vec::new(),
        }
    }

    /// Validates ids and sources, requires 1 to [`MAX_BATCH_ITEMS`] items for an update or
    /// install (none for a check), keeps the first item of each id (ignoring ASCII case) and
    /// names an item without a name after its id.
    pub fn new(kind: UpdatesKind, items: Vec<UpdateItem>) -> Result<UpdatesRequest> {
        if kind == UpdatesKind::Scan {
            if !items.is_empty() {
                return Err(Error::Other("a check for updates takes no apps".into()));
            }
            return Ok(UpdatesRequest::scan());
        }
        if items.is_empty() {
            return Err(Error::Other("choose at least one app".into()));
        }
        if items.len() > MAX_BATCH_ITEMS {
            return Err(Error::Other(format!(
                "at most {MAX_BATCH_ITEMS} apps can be updated or installed at once"
            )));
        }
        let mut kept: Vec<UpdateItem> = Vec::new();
        for item in items {
            let id = item.id.trim().to_string();
            let source = item.source.trim().to_string();
            if !valid_source(&source) {
                return Err(Error::Other(format!(
                    "{source:?} isn't a winget source name"
                )));
            }
            if !valid_package_id(&id, &source) {
                return Err(Error::Other(format!("{id:?} isn't a winget package id")));
            }
            if kept.iter().any(|k| k.id.eq_ignore_ascii_case(&id)) {
                continue;
            }
            let name = bounded(&item.name);
            kept.push(UpdateItem {
                name: if name.is_empty() { id.clone() } else { name },
                id,
                source,
                from: item.from.as_deref().map(bounded).filter(|v| !v.is_empty()),
                to: item.to.as_deref().map(bounded).filter(|v| !v.is_empty()),
            });
        }
        Ok(UpdatesRequest { kind, items: kept })
    }

    pub fn kind(&self) -> UpdatesKind {
        self.kind
    }

    pub fn items(&self) -> &[UpdateItem] {
        &self.items
    }

    /// winget's arguments for one app of this request.
    pub fn item_args(&self, item: &UpdateItem) -> Vec<String> {
        item_args(self.kind, item)
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// winget's arguments for updating or installing one app; empty for a check.
pub fn item_args(kind: UpdatesKind, item: &UpdateItem) -> Vec<String> {
    let verb = match kind {
        UpdatesKind::Scan => return Vec::new(),
        UpdatesKind::Upgrade => "upgrade",
        UpdatesKind::Install => "install",
    };
    let mut args = strings(&[verb, "--id"]);
    args.push(item.id.clone());
    args.push("--exact".into());
    args.push("--source".into());
    args.push(item.source.clone());
    args.push("--silent".into());
    if kind == UpdatesKind::Install {
        args.push("--no-upgrade".into());
    }
    args.extend(strings(&[
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]));
    args
}

/// `winget --version`.
pub fn version_args() -> Vec<String> {
    strings(&["--version"])
}

/// `winget export` of the installed apps with their versions into `output`.
pub fn export_args(output: &Path) -> Vec<String> {
    let mut args = strings(&["export", "--output"]);
    args.push(output.display().to_string());
    args.extend(strings(&[
        "--include-versions",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]));
    args
}

/// `winget upgrade` without a package: the table of available updates.
pub fn upgrade_list_args() -> Vec<String> {
    strings(&[
        "upgrade",
        "--accept-source-agreements",
        "--disable-interactivity",
    ])
}

// ───────────────────────────── Plans and results ─────────────────────────────

/// What starting a request would do, and why it cannot start now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatesPlan {
    pub kind: UpdatesKind,
    pub title: String,
    pub items: Vec<UpdateItem>,
    /// winget.exe, when found.
    pub program: Option<String>,
    /// The commands it runs, for display: at most 20, then "… and N more".
    pub command_lines: Vec<String>,
    pub requires_admin: bool,
    pub irreversible: bool,
    pub cancellable: bool,
    pub blocked_reason: Option<String>,
    pub notes: Vec<String>,
}

/// An app with an update, as the check publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRow {
    pub id: String,
    pub name: String,
    pub installed: String,
    pub available: String,
    pub source: String,
    pub explicit_only: bool,
    /// It can be picked for an update.
    pub selectable: bool,
    pub note: Option<String>,
}

/// Why a check failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanError {
    /// winget's exit code in hex, when it ended with one.
    pub code: Option<String>,
    pub message: String,
    pub availability: Option<Availability>,
    /// winget is older than [`MIN_VERSION`].
    pub outdated: bool,
}

/// The published result of a check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanResult {
    pub kind: UpdatesKind,
    pub winget_version: Option<String>,
    /// RFC 3339, UTC.
    pub checked_at: Option<String>,
    pub upgrades: Vec<UpgradeRow>,
    /// Ids of the installed apps winget knows.
    pub installed: Vec<String>,
    pub inventory_complete: bool,
    pub unparsed_rows: u32,
    pub warnings: Vec<String>,
    pub error: Option<ScanError>,
}

impl ScanResult {
    fn empty() -> ScanResult {
        ScanResult {
            kind: UpdatesKind::Scan,
            winget_version: None,
            checked_at: None,
            upgrades: Vec::new(),
            installed: Vec::new(),
            inventory_complete: true,
            unparsed_rows: 0,
            warnings: Vec::new(),
            error: None,
        }
    }
}

/// Why Update all leaves out an app the check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftOut {
    /// Cairn can't pick it ([`UpgradeRow::selectable`] is false).
    NotSelectable,
    /// winget updates it only when it is named.
    NamedOnly,
    /// A Microsoft Store app, taken only when asked for.
    Store,
}

/// Why Update all leaves out `row`, or `None` when it takes it. Microsoft Store apps are taken
/// only with `include_store`.
pub fn left_out_of_update_all(row: &UpgradeRow, include_store: bool) -> Option<LeftOut> {
    if !row.selectable {
        Some(LeftOut::NotSelectable)
    } else if row.explicit_only {
        Some(LeftOut::NamedOnly)
    } else if !include_store && row.source.eq_ignore_ascii_case("msstore") {
        Some(LeftOut::Store)
    } else {
        None
    }
}

/// What `optctl updates upgrade --all` prints when it takes none of `rows`, the updates a
/// check found: that all apps are up to date when there are none; otherwise each update with
/// why `--all` leaves it out and how to update it, and that nothing was started.
pub fn update_all_takes_none(rows: &[UpgradeRow], include_store: bool) -> Vec<String> {
    if rows.is_empty() {
        return vec!["All apps are up to date.".to_string()];
    }
    let mut lines = vec![if rows.len() == 1 {
        "The check found 1 update, but --all leaves it out:".to_string()
    } else {
        format!(
            "The check found {} updates, but --all leaves them out:",
            rows.len()
        )
    }];
    for row in rows {
        let why = match left_out_of_update_all(row, include_store) {
            Some(LeftOut::NotSelectable) => format!(
                "Cairn can't update it: {}",
                row.note.as_deref().unwrap_or("winget can't target it")
            ),
            Some(LeftOut::NamedOnly) => format!(
                "winget updates it only when it is named, as in: optctl updates upgrade {}",
                row.id
            ),
            Some(LeftOut::Store) => {
                "a Microsoft Store app; add --include-store to update it with --all".to_string()
            }
            None => continue,
        };
        lines.push(format!("  {} ({}): {why}", row.name, row.id));
    }
    lines.push("Nothing was started.".to_string());
    lines
}

/// One app of a published batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: String,
    pub name: String,
    pub source: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub state: ItemState,
    pub exit_code: Option<i32>,
    pub exit_code_hex: Option<String>,
    pub message: Option<String>,
    pub elapsed_ms: u64,
    /// Its winget runs outside Cairn's job object and outlives Cairn.
    pub detached: bool,
    /// Trying again can end differently. False when winget's answer can't change on a retry
    /// ([`retry_may_help`] of its exit code), such as an app installed in a way winget can't
    /// update.
    #[serde(default = "retry_by_default")]
    pub retry: bool,
}

fn retry_by_default() -> bool {
    true
}

impl ItemResult {
    fn queued(item: &UpdateItem) -> ItemResult {
        ItemResult {
            id: item.id.clone(),
            name: item.name.clone(),
            source: item.source.clone(),
            from: item.from.clone(),
            to: item.to.clone(),
            state: ItemState::Queued,
            exit_code: None,
            exit_code_hex: None,
            message: None,
            elapsed_ms: 0,
            detached: false,
            retry: true,
        }
    }
}

/// The published result of an update or install batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResult {
    pub kind: UpdatesKind,
    pub items: Vec<ItemResult>,
    /// Index of the app that runs now.
    pub current: Option<usize>,
    /// Apps that have ended.
    pub done: usize,
    pub total: usize,
    /// A stop was requested: no app starts after the current one.
    pub stopping: bool,
    pub restart_required: bool,
}

/// A plan and, unless it was a dry run, the job that started.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StartOutcome {
    pub plan: UpdatesPlan,
    pub job: Option<HostJobSnapshot>,
}

// ───────────────────────────── The lane ─────────────────────────────

/// Deadlines of the steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub version: Duration,
    pub list: Duration,
    pub item: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            version: VERSION_DEADLINE,
            list: LIST_DEADLINE,
            item: ITEM_DEADLINE,
        }
    }
}

/// What planning and starting a job read besides the request.
#[derive(Clone)]
pub(crate) struct WingetDeps {
    pub env: WingetEnv,
    /// Starts winget; an error when winget's folder cannot be found.
    pub launcher: std::result::Result<Arc<dyn Launcher>, String>,
    pub limits: Limits,
}

impl fmt::Debug for WingetDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WingetDeps")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// `<data dir>\jobs\winget`.
pub fn default_dir() -> PathBuf {
    data_dir().join("jobs").join(LANE)
}

pub(crate) fn host_config(log_dir: PathBuf) -> HostConfig {
    HostConfig {
        lane: LANE,
        log_dir,
        keep_logs: 20,
        tick: Duration::from_millis(100),
        settle_wait: Duration::from_secs(3),
        stop_wait: Duration::from_secs(3),
        keep_finished: 10,
        keep_results_per_kind: None,
    }
}

/// The process-wide `winget` lane; its transcripts go to [`default_dir`].
pub fn lane() -> &'static JobHost {
    static LANE_HOST: OnceLock<JobHost> = OnceLock::new();
    LANE_HOST.get_or_init(|| JobHost::new(host_config(default_dir())))
}

fn system_deps() -> WingetDeps {
    static LAUNCHER: OnceLock<std::result::Result<Arc<dyn Launcher>, String>> = OnceLock::new();
    let launcher = LAUNCHER
        .get_or_init(|| {
            windows_apps_root()
                .map(|root| {
                    Arc::new(SystemLauncher::within(ProgramRoot::Dir(root))) as Arc<dyn Launcher>
                })
                .map_err(|e| format!("winget's folder could not be found: {e}"))
        })
        .clone();
    WingetDeps {
        env: WingetEnv::SYSTEM,
        launcher,
        limits: Limits::default(),
    }
}

/// What starting `request` on this PC would do. Read-only: no process, no file, no journal.
pub fn plan(request: &UpdatesRequest) -> UpdatesPlan {
    plan_with(lane(), &system_deps(), request).0
}

/// Plans `request` and, unless `dry_run`, starts it on [`lane`]. A start refuses with
/// [`Error::NotElevated`] (updates and installs) or the plan's blocked reason before any
/// file, row or process. `open_journal` is called only to start an update or install.
pub fn plan_or_start(
    request: &UpdatesRequest,
    dry_run: bool,
    open_journal: impl FnOnce() -> Result<Arc<Journal>>,
) -> Result<StartOutcome> {
    plan_or_start_with(lane(), &system_deps(), request, dry_run, open_journal)
}

const MAX_COMMAND_LINES: usize = 20;

fn command_lines(request: &UpdatesRequest) -> Vec<String> {
    let line = |args: Vec<String>| format!("winget {}", args.join(" "));
    let all: Vec<String> = match request.kind() {
        UpdatesKind::Scan => vec![
            line(version_args()),
            line(export_args(Path::new(r"<private folder>\inventory.json"))),
            line(upgrade_list_args()),
        ],
        kind => request
            .items()
            .iter()
            .map(|item| line(item_args(kind, item)))
            .collect(),
    };
    if all.len() <= MAX_COMMAND_LINES {
        return all;
    }
    let more = all.len() - MAX_COMMAND_LINES;
    let mut shown: Vec<String> = all.into_iter().take(MAX_COMMAND_LINES).collect();
    shown.push(format!("… and {more} more"));
    shown
}

fn plan_title(request: &UpdatesRequest) -> String {
    let n = request.items().len();
    let apps = if n == 1 { "app" } else { "apps" };
    match request.kind() {
        UpdatesKind::Scan => "Check for app updates".to_string(),
        UpdatesKind::Upgrade => format!("Update {n} {apps}"),
        UpdatesKind::Install => format!("Install {n} {apps}"),
    }
}

/// The plan and, when winget was found, its location.
pub(crate) fn plan_with(
    host: &JobHost,
    deps: &WingetDeps,
    request: &UpdatesRequest,
) -> (UpdatesPlan, Option<WingetLocation>) {
    let kind = request.kind();
    let batch = kind != UpdatesKind::Scan;
    let env = &deps.env;
    let status = winget_status_with(env);
    let mut blocked: Option<String> = None;
    if batch && (env.installs_forbidden)() {
        blocked = Some(INSTALLS_FORBIDDEN_TEXT.to_string());
    }
    if blocked.is_none() && status.availability != Availability::Ready {
        blocked = status.message.clone();
    }
    if blocked.is_none() {
        if let Err(reason) = &deps.launcher {
            blocked = Some(reason.clone());
        }
    }
    if blocked.is_none() {
        if let Some(running) = host.running() {
            let text = UpdatesKind::from_job_kind(running.kind).map_or(
                "Cairn is already running winget.",
                UpdatesKind::already_running_text,
            );
            blocked = Some(text.to_string());
        }
    }
    if blocked.is_none() && batch {
        if let Some(title) = (env.servicing_tool)() {
            blocked = Some(format!(
                "Wait for {title} to finish; Windows can't install apps safely while it repairs itself."
            ));
        }
    }
    if blocked.is_none() && host.is_closed() {
        blocked = Some(CLOSING_TEXT.to_string());
    }

    let mut notes = Vec::new();
    if batch {
        if (env.on_battery)() == Some(true) {
            notes.push(BATTERY_NOTE.to_string());
        }
        if (env.msi_busy)() == Some(true) {
            notes.push(MSI_BUSY_NOTE.to_string());
        }
        if (env.processes)().is_some_and(|p| p.contains("winget.exe")) {
            notes.push(WINGET_RUNNING_NOTE.to_string());
        }
        if status.elevated {
            notes.push(ELEVATED_NOTE.to_string());
        }
    }
    let plan = UpdatesPlan {
        kind,
        title: plan_title(request),
        items: request.items().to_vec(),
        program: status
            .location
            .as_ref()
            .map(|l| l.path.display().to_string()),
        command_lines: command_lines(request),
        requires_admin: batch,
        irreversible: batch,
        cancellable: true,
        blocked_reason: blocked,
        notes,
    };
    (plan, status.location)
}

/// Longest command line the job host keeps for a job.
const MAX_JOB_COMMAND_LINE: usize = 200;
/// Room a batch's command line keeps after an id for " and 200 more".
const MORE_IDS_ROOM: usize = 14;

/// The job's command line, which heads its transcript: the check's listing, or for a batch
/// the winget run each app gets and the apps' ids, as many as fit in
/// [`MAX_JOB_COMMAND_LINE`] characters ("winget upgrade --id <id> --exact … for 3 apps:
/// Contoso.A, Contoso.B and 1 more").
fn job_command_line(request: &UpdatesRequest) -> String {
    let kind = request.kind();
    if kind == UpdatesKind::Scan {
        return format!("winget {}", upgrade_list_args().join(" "));
    }
    let items = request.items();
    let total = items.len();
    let mut line = format!(
        "winget {} --id <id> --exact … for {total} {}:",
        kind.as_str(),
        if total == 1 { "app" } else { "apps" }
    );
    for (index, item) in items.iter().enumerate() {
        let separator = if index == 0 { " " } else { ", " };
        let room = if index + 1 == total {
            MAX_JOB_COMMAND_LINE
        } else {
            MAX_JOB_COMMAND_LINE - MORE_IDS_ROOM
        };
        if line.chars().count() + separator.len() + item.id.chars().count() > room {
            line.push_str(&format!(" and {} more", total - index));
            break;
        }
        line.push_str(separator);
        line.push_str(&item.id);
    }
    line
}

pub(crate) fn plan_or_start_with(
    host: &JobHost,
    deps: &WingetDeps,
    request: &UpdatesRequest,
    dry_run: bool,
    open_journal: impl FnOnce() -> Result<Arc<Journal>>,
) -> Result<StartOutcome> {
    let (plan, location) = plan_with(host, deps, request);
    if dry_run {
        return Ok(StartOutcome { plan, job: None });
    }
    if plan.requires_admin && !(deps.env.elevated)() {
        return Err(Error::NotElevated);
    }
    if let Some(reason) = &plan.blocked_reason {
        return Err(Error::Other(reason.clone()));
    }
    let (Some(location), Ok(launcher)) = (location, deps.launcher.clone()) else {
        return Err(Error::Other(WINGET_MISSING_TEXT.to_string()));
    };
    let spec = JobSpec {
        kind: request.kind().job_kind(),
        title: plan.title.clone(),
        command_line: job_command_line(request),
        cancellable: true,
        audit: None,
        needs_journal: request.kind() != UpdatesKind::Scan,
        log: true,
    };
    let work = work::work(
        work::WorkDeps {
            launcher,
            program: location.path,
            env: deps.env,
            limits: deps.limits,
            private_steps: crate::is_elevated(),
        },
        request.clone(),
    );
    let job = host.start(spec, open_journal, work)?;
    Ok(StartOutcome {
        plan,
        job: Some(job),
    })
}

/// Runs `winget --version` from the located package (for `optctl updates status`). Starts a
/// process that changes nothing; `Ok(None)` when winget is not installed for this account.
pub fn probe_version() -> Result<Option<String>> {
    let Some(location) = locate()? else {
        return Ok(None);
    };
    let mut command = hardened_command(&location.path)?;
    command
        .args(version_args())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("winget's output could not be read".into()))?;
    let reader = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let deadline = Instant::now() + VERSION_DEADLINE;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(Error::Other("winget didn't answer within 30 s".into()));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let text = reader
        .join()
        .map_err(|_| Error::Other("winget's output could not be read".into()))?;
    let text = String::from_utf8_lossy(&text);
    Ok(text
        .split_whitespace()
        .find_map(WingetVersion::parse)
        .map(|v| v.to_string()))
}

#[cfg(test)]
mod tests;
