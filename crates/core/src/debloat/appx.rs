//! Appx package manager: inventory, removal and re-registration of Store (UWP) packages
//! through headless Windows PowerShell (the Appx cmdlets have no stable native API).
//!
//! Removal is per user (`Remove-AppxPackage` without `-AllUsers`) and never deprovisions,
//! so a package that is still provisioned in the image keeps its files and can be
//! re-registered from its manifest. A package whose files are gone can only be reinstalled
//! from the Microsoft Store; rollback reports those with a Store link. A removed package
//! whose family is installed again (Windows Update and the Store reinstall some apps)
//! needs no restore at all.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{info, warn};

use super::catalog;
use super::engine::package_name_from_family;
use crate::safety::state_log::{AppxRecord, JournalTable, NewAppxRecord};
use crate::safety::{MutationOutcome, Safety};
use crate::win::powershell;
use crate::{Error, Result};

/// One installed package of the current user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppxPackage {
    pub name: String,
    pub full_name: String,
    pub family_name: String,
    pub publisher: String,
    pub version: String,
    pub install_location: String,
    pub is_framework: bool,
    pub non_removable: bool,
    /// Get-AppxPackage SignatureKind: None, Developer, Enterprise, Store or System.
    pub signature_kind: String,
    /// Whether the package is provisioned for new users; `None` when that could not be
    /// determined (the provisioned list requires elevation).
    pub provisioned: Option<bool>,
}

/// Per-package result of [`remove`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalResult {
    pub full_name: String,
    pub outcome: MutationOutcome,
    /// Error text when the removal failed; `outcome` is then `Skipped` with the same text.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RestoreOutcome {
    Reregistered,
    /// A package of the family is registered for the current user again, possibly a newer
    /// version, so the removal no longer holds and nothing was registered.
    AlreadyInstalled {
        package_full_name: String,
    },
    StoreRequired {
        package_family: String,
        store_link: String,
    },
}

/// Current-user package list projected to the fields [`parse_inventory_json`] reads.
/// Enums and `System.Version` are cast to strings because ConvertTo-Json would emit them
/// as integers and objects. `-InputObject @(...)` keeps a one-package result an array.
const INVENTORY_SCRIPT: &str = "$ProgressPreference = 'SilentlyContinue'; \
    $WarningPreference = 'SilentlyContinue'; \
    Microsoft.PowerShell.Utility\\ConvertTo-Json -Depth 3 -Compress -InputObject \
    @(Appx\\Get-AppxPackage | Microsoft.PowerShell.Utility\\Select-Object \
    Name, PackageFullName, PackageFamilyName, Publisher, \
    @{n='Version';e={[string]$_.Version}}, InstallLocation, IsFramework, NonRemovable, \
    @{n='SignatureKind';e={[string]$_.SignatureKind}})";

/// HRESULTs from Add-AppxPackage meaning the package files are no longer on disk:
/// ERROR_INSTALL_PACKAGE_NOT_FOUND, ERROR_INSTALL_RESOLVE_DEPENDENCY_FAILED,
/// ERROR_FILE_NOT_FOUND and ERROR_PATH_NOT_FOUND.
const MISSING_FILES_HRESULTS: [&str; 4] = ["0X80073CF1", "0X80073CF3", "0X80070002", "0X80070003"];

/// Installed packages of the current user. `provisioned` is left `None`: querying the
/// provisioned list is slow and requires elevation, and this runs on every scan.
pub fn inventory() -> Result<Vec<AppxPackage>> {
    let output = run_appx(INVENTORY_SCRIPT)?;
    // Compressed JSON is a single line; anything else on stdout is host noise.
    let json = output
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').trim())
        .rfind(|l| l.starts_with('[') || l.starts_with('{'))
        .unwrap_or("");
    parse_inventory_json(json)
}

/// Parses the JSON that [`inventory`] requests from PowerShell. Accepts a single object
/// or an array, as ConvertTo-Json emits either.
pub fn parse_inventory_json(json: &str) -> Result<Vec<AppxPackage>> {
    let text = json.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let items = match serde_json::from_str::<Value>(text)? {
        Value::Null => Vec::new(),
        Value::Array(items) => items,
        Value::Object(obj) => unwrap_array_object(obj),
        other => {
            return Err(Error::Other(format!(
                "unexpected Appx inventory JSON: expected an object or array, found {other}"
            )))
        }
    };
    Ok(items
        .iter()
        .filter_map(Value::as_object)
        .filter_map(package_from_object)
        .collect())
}

/// False for frameworks, non-removable packages, system-signed packages and names in
/// [`crate::debloat::catalog::PROTECTED_PACKAGES`].
pub fn is_removable(pkg: &AppxPackage) -> bool {
    !(pkg.is_framework
        || pkg.non_removable
        || pkg.signature_kind.trim().eq_ignore_ascii_case("System")
        || catalog::is_protected_package(&pkg.name))
}

/// Journals each package with `safety.record_appx`, then removes the journaled packages
/// for the current user in one PowerShell invocation. Packages that are not removable are
/// skipped without being journaled. Results are in input order.
///
/// Fails before journaling anything when the session is not elevated as required, or when
/// the process runs as another account than the signed-in user, whose packages would
/// otherwise stay installed while that account's are removed.
pub fn remove(safety: &Safety, packages: &[AppxPackage]) -> Result<Vec<RemovalResult>> {
    remove_with(safety, packages, run_appx)
}

/// Restores one removed package for the current user; see [`reregister_all`].
pub fn reregister(rec: &AppxRecord) -> Result<RestoreOutcome> {
    reregister_with(rec, run_appx)
}

/// Restores removed packages for the current user, with one result per record in input
/// order. When a package of a record's family is registered again (for example a newer
/// version that Windows reinstalled), its result is `AlreadyInstalled` and nothing is
/// registered. Otherwise the package is re-registered from
/// `<install_location>\AppxManifest.xml` with `Add-AppxPackage -Register
/// -DisableDevelopmentMode`; the result is `StoreRequired` when the manifest no longer
/// exists or registration fails because the files are gone.
///
/// Records without a manifest share a single PowerShell lookup of the families that are
/// registered again. When that lookup cannot run, they are reported as `StoreRequired`,
/// which keeps their journal records active for a later retry.
pub fn reregister_all(records: &[AppxRecord]) -> Vec<Result<RestoreOutcome>> {
    reregister_all_with(records, run_appx)
}

/// Runs one of this module's scripts in Windows PowerShell with the Appx module imported
/// from System32 first.
fn run_appx(script: &str) -> Result<String> {
    powershell::run(&with_appx_module(script)?)
}

/// `script` preceded by the import of the System32 Appx module; the scripts call its
/// cmdlets as `Appx\<cmdlet>`.
fn with_appx_module(script: &str) -> Result<String> {
    Ok(format!(
        "{}{script}",
        powershell::import_system_module("Appx")?
    ))
}

fn reregister_with<F>(rec: &AppxRecord, run: F) -> Result<RestoreOutcome>
where
    F: FnMut(&str) -> Result<String>,
{
    reregister_all_with(std::slice::from_ref(rec), run)
        .pop()
        .unwrap_or_else(|| Err(Error::Other("no restore result".to_string())))
}

/// A record ready to restore: its validated family, and its manifest while the package
/// files remain.
struct Restore<'a> {
    family: &'a str,
    manifest: Option<PathBuf>,
}

fn restore_plan(rec: &AppxRecord) -> Result<Restore<'_>> {
    let family = rec.package_family.trim();
    if !is_valid_identity(family) {
        return Err(Error::Other(format!(
            "invalid package family name: {family:?}"
        )));
    }
    Ok(Restore {
        family,
        manifest: manifest_path(&rec.install_location),
    })
}

fn reregister_all_with<F>(records: &[AppxRecord], mut run: F) -> Vec<Result<RestoreOutcome>>
where
    F: FnMut(&str) -> Result<String>,
{
    let plans: Vec<Result<Restore<'_>>> = records.iter().map(restore_plan).collect();
    let without_files: Vec<&str> = plans
        .iter()
        .filter_map(|plan| match plan {
            Ok(Restore {
                family,
                manifest: None,
            }) => Some(*family),
            _ => None,
        })
        .collect();
    let reinstalled = reinstalled_families(&without_files, &mut run);
    plans
        .into_iter()
        .map(|plan| {
            let Restore { family, manifest } = plan?;
            match manifest {
                Some(manifest) => register_manifest(family, &manifest, &mut run),
                None => Ok(match reinstalled.get(&family.to_ascii_lowercase()) {
                    Some(full_name) => already_installed(family, full_name.clone()),
                    None => store_required(family),
                }),
            }
        })
        .collect()
}

/// Registers `manifest` for the current user unless a package of `family` is registered
/// already.
fn register_manifest<F>(family: &str, manifest: &Path, run: &mut F) -> Result<RestoreOutcome>
where
    F: FnMut(&str) -> Result<String>,
{
    let name = package_name_from_family(family);
    let output = match run(&reregister_script(manifest, name, family)) {
        Ok(output) => output,
        Err(e) if is_missing_files_error(&e) => {
            warn!(family, error = %e, "package files are gone; Store reinstall required");
            return Ok(store_required(family));
        }
        Err(e) => return Err(e),
    };
    match parse_reregister_output(&output) {
        Some(Reply::Installed(package_full_name)) => {
            Ok(already_installed(family, package_full_name))
        }
        Some(Reply::Registered(count)) if count > 0 => {
            info!(family, "Appx package re-registered");
            Ok(RestoreOutcome::Reregistered)
        }
        Some(Reply::Registered(_)) => Err(Error::Other(format!(
            "Add-AppxPackage reported success but {family} is not registered"
        ))),
        None => Err(Error::Other(format!(
            "unexpected output while restoring {family}: {:?}",
            output.trim()
        ))),
    }
}

fn already_installed(family: &str, package_full_name: String) -> RestoreOutcome {
    info!(family, installed = %package_full_name, "package family is installed again");
    RestoreOutcome::AlreadyInstalled { package_full_name }
}

/// Full name of a registered package of each of `families` that has one, keyed by the
/// lowercase family, from one PowerShell lookup (none when `families` is empty). A lookup
/// that fails finds nothing, so it never hides a package that needs restoring.
fn reinstalled_families<F>(families: &[&str], run: &mut F) -> HashMap<String, String>
where
    F: FnMut(&str) -> Result<String>,
{
    if families.is_empty() {
        return HashMap::new();
    }
    match run(&reinstalled_script(families)) {
        Ok(output) => parse_reinstalled_output(&output),
        Err(e) => {
            warn!(error = %e, "cannot look up reinstalled package families; treating them as not installed");
            HashMap::new()
        }
    }
}

/// Microsoft Store page of a package family.
pub fn store_link(package_family: &str) -> String {
    format!("ms-windows-store://pdp/?PFN={package_family}")
}

// ───────────────────────────── inventory parsing ─────────────────────────────

/// ConvertTo-Json in Windows PowerShell 5.1 can wrap an array that carries extended
/// properties as `{"value":[...],"Count":n}`; that shape is unwrapped, any other object is
/// a single package.
fn unwrap_array_object(mut obj: Map<String, Value>) -> Vec<Value> {
    let wrapped = obj.get("value").is_some_and(Value::is_array)
        && obj.keys().all(|k| k == "value" || k == "Count");
    if wrapped {
        if let Some(Value::Array(items)) = obj.remove("value") {
            return items;
        }
    }
    vec![Value::Object(obj)]
}

fn package_from_object(obj: &Map<String, Value>) -> Option<AppxPackage> {
    let name = text(field(obj, "Name"));
    let full_name = text(field(obj, "PackageFullName"));
    if name.trim().is_empty() || full_name.trim().is_empty() {
        warn!(
            name,
            full_name, "Appx inventory entry without a package identity ignored"
        );
        return None;
    }
    let mut family_name = text(field(obj, "PackageFamilyName"));
    if family_name.trim().is_empty() {
        family_name = family_from_full_name(&full_name).unwrap_or_default();
    }
    Some(AppxPackage {
        name,
        full_name,
        family_name,
        publisher: text(field(obj, "Publisher")),
        version: version_text(field(obj, "Version")),
        install_location: text(field(obj, "InstallLocation")),
        is_framework: flag(field(obj, "IsFramework")),
        non_removable: flag(field(obj, "NonRemovable")),
        signature_kind: signature_kind_text(field(obj, "SignatureKind")),
        provisioned: None,
    })
}

/// Property lookup, exact name first, then case-insensitive.
fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).or_else(|| {
        obj.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// String form of a scalar; null, missing, arrays and objects become empty.
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
    }
}

fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            let s = s.trim();
            s.eq_ignore_ascii_case("true") || s == "1"
        }
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// A version string, or a serialized `System.Version` object (undefined components are -1).
fn version_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(obj)) => {
            let parts: Vec<String> = ["Major", "Minor", "Build", "Revision"]
                .iter()
                .filter_map(|k| field(obj, k).and_then(Value::as_i64))
                .take_while(|n| *n >= 0)
                .map(|n| n.to_string())
                .collect();
            parts.join(".")
        }
        other => text(other),
    }
}

/// `Windows.ApplicationModel.PackageSignatureKind` name; integers are mapped to names.
fn signature_kind_text(v: Option<&Value>) -> String {
    let raw = text(v);
    match raw.trim().parse::<i64>() {
        Ok(0) => "None".to_string(),
        Ok(1) => "Developer".to_string(),
        Ok(2) => "Enterprise".to_string(),
        Ok(3) => "Store".to_string(),
        Ok(4) => "System".to_string(),
        _ => raw,
    }
}

/// `Name_PublisherId` from `Name_Version_Architecture_ResourceId_PublisherId`.
fn family_from_full_name(full_name: &str) -> Option<String> {
    let parts: Vec<&str> = full_name.split('_').collect();
    match parts.as_slice() {
        [name, _, _, _, publisher_id] if !name.is_empty() && !publisher_id.is_empty() => {
            Some(format!("{name}_{publisher_id}"))
        }
        _ => None,
    }
}

// ───────────────────────────── removal ─────────────────────────────

/// Where each input package ends up: skipped before journaling, or an index into the
/// batch of distinct packages handed to PowerShell.
enum Slot {
    Skipped(String),
    Batch(usize),
}

/// Per-package status reported by the removal script.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemovalStatus {
    Removed,
    Failed(String),
    /// No result line: the package may or may not have been removed.
    Unreported,
}

const UNREPORTED: &str = "Remove-AppxPackage reported no result for this package";

fn remove_with<F>(
    safety: &Safety,
    packages: &[AppxPackage],
    mut run: F,
) -> Result<Vec<RemovalResult>>
where
    F: FnMut(&str) -> Result<String>,
{
    safety.ensure_elevated()?;
    safety.ensure_interactive_user()?;

    let mut slots = Vec::with_capacity(packages.len());
    let mut batch: Vec<&AppxPackage> = Vec::new();
    let mut batch_index: HashMap<String, usize> = HashMap::new();
    for pkg in packages {
        match removal_refusal(pkg) {
            Some(reason) => slots.push(Slot::Skipped(reason)),
            None => {
                let i = *batch_index
                    .entry(pkg.full_name.to_ascii_lowercase())
                    .or_insert_with(|| {
                        batch.push(pkg);
                        batch.len() - 1
                    });
                slots.push(Slot::Batch(i));
            }
        }
    }

    // Baseline capture precedes any removal; a journal failure aborts before PowerShell
    // runs and withdraws the records this call already created.
    let mut created = Vec::with_capacity(batch.len());
    for pkg in &batch {
        let rec = NewAppxRecord {
            package_full_name: pkg.full_name.clone(),
            package_family: family_of(pkg),
            install_location: pkg.install_location.clone(),
            all_users: false,
        };
        match safety.record_appx(&rec) {
            Ok(c) => created.push(c),
            Err(e) => {
                let journaled: Vec<&str> = batch
                    .iter()
                    .zip(&created)
                    .filter(|(_, &c)| c)
                    .map(|(p, _)| p.full_name.as_str())
                    .collect();
                unjournal(safety, &journaled);
                return Err(e);
            }
        }
    }

    let statuses: Vec<RemovalStatus> = if batch.is_empty() {
        Vec::new()
    } else {
        let names: Vec<&str> = batch.iter().map(|p| p.full_name.as_str()).collect();
        let output = match run(&removal_script(&names)) {
            Ok(output) => output,
            Err(e) => {
                abandon_batch(safety, &batch, &created, &mut run, &e);
                return Err(e);
            }
        };
        let reported = parse_removal_output(&output);
        batch
            .iter()
            .map(|p| {
                reported
                    .get(&p.full_name.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or(RemovalStatus::Unreported)
            })
            .collect()
    };

    // Records created here for packages that are still installed would make rollback
    // re-register something that was never removed. Unreported packages keep theirs.
    let to_unjournal: Vec<&str> = batch
        .iter()
        .zip(&created)
        .zip(&statuses)
        .filter(|((_, &c), status)| c && matches!(status, RemovalStatus::Failed(_)))
        .map(|((p, _), _)| p.full_name.as_str())
        .collect();
    unjournal(safety, &to_unjournal);

    for (pkg, status) in batch.iter().zip(&statuses) {
        match status {
            RemovalStatus::Removed => {
                info!(package = %pkg.full_name, "Appx package removed");
                log_best_effort(safety, &pkg.full_name, "removed", None);
            }
            RemovalStatus::Failed(msg) => {
                warn!(package = %pkg.full_name, error = %msg, "Appx package removal failed");
                log_best_effort(safety, &pkg.full_name, "failed", Some(msg));
            }
            RemovalStatus::Unreported => {
                warn!(package = %pkg.full_name, "Appx removal reported no result");
                let detail = format!("{UNREPORTED}; journal record kept");
                log_best_effort(safety, &pkg.full_name, "failed", Some(&detail));
            }
        }
    }

    Ok(packages
        .iter()
        .zip(slots)
        .map(|(pkg, slot)| match slot {
            Slot::Skipped(reason) => {
                log_best_effort(safety, &pkg.full_name, "skipped", Some(&reason));
                RemovalResult {
                    full_name: pkg.full_name.clone(),
                    outcome: MutationOutcome::Skipped(reason),
                    error: None,
                }
            }
            Slot::Batch(i) => match &statuses[i] {
                RemovalStatus::Removed => RemovalResult {
                    full_name: pkg.full_name.clone(),
                    outcome: MutationOutcome::Applied,
                    error: None,
                },
                RemovalStatus::Failed(msg) => RemovalResult {
                    full_name: pkg.full_name.clone(),
                    outcome: MutationOutcome::Skipped(msg.clone()),
                    error: Some(msg.clone()),
                },
                RemovalStatus::Unreported => RemovalResult {
                    full_name: pkg.full_name.clone(),
                    outcome: MutationOutcome::Skipped(UNREPORTED.to_string()),
                    error: Some(UNREPORTED.to_string()),
                },
            },
        })
        .collect())
}

/// Why a package must not be handed to Remove-AppxPackage, if it must not.
fn removal_refusal(pkg: &AppxPackage) -> Option<String> {
    if !is_removable(pkg) {
        return Some("protected package".to_string());
    }
    // Only this character set can appear in a package identity, and it cannot break out of
    // a single-quoted PowerShell literal.
    if !is_valid_identity(&pkg.full_name) {
        return Some("invalid package full name".to_string());
    }
    // The protection check reads `name`, so it must be the name the full name carries.
    if !has_name_prefix(&pkg.full_name, &pkg.name) {
        return Some("package name does not match its full name".to_string());
    }
    let family = family_of(pkg);
    if !is_valid_identity(&family) || !has_name_prefix(&family, &pkg.name) {
        return Some("invalid package family name".to_string());
    }
    None
}

/// `^[A-Za-z0-9._~-]+$`
fn is_valid_identity(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

/// True when `identity` is `<name>_...`.
fn has_name_prefix(identity: &str, name: &str) -> bool {
    !name.is_empty()
        && identity.len() > name.len()
        && identity.as_bytes()[name.len()] == b'_'
        && identity.is_char_boundary(name.len())
        && identity[..name.len()].eq_ignore_ascii_case(name)
}

fn family_of(pkg: &AppxPackage) -> String {
    let family = pkg.family_name.trim();
    if family.is_empty() {
        family_from_full_name(&pkg.full_name).unwrap_or_default()
    } else {
        family.to_string()
    }
}

/// One compressed JSON object per package and line: `{"FullName":..,"Ok":..,"Error":..}`.
fn removal_script(full_names: &[&str]) -> String {
    let list = quoted_list(full_names);
    format!(
        "$ProgressPreference = 'SilentlyContinue'; $WarningPreference = 'SilentlyContinue'; \
         foreach ($n in @({list})) {{ \
         $ok = $false; $err = $null; \
         try {{ Appx\\Remove-AppxPackage -Package $n -ErrorAction Stop; $ok = $true }} \
         catch {{ $err = $_.Exception.Message; if (-not $err) {{ $err = $_.ToString() }} }} \
         Microsoft.PowerShell.Utility\\ConvertTo-Json -Compress \
         -InputObject ([ordered]@{{ FullName = $n; Ok = $ok; Error = $err }}) \
         }}"
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemovalLine {
    full_name: String,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Status per lowercase full name. Lines that are not result objects are ignored.
fn parse_removal_output(output: &str) -> HashMap<String, RemovalStatus> {
    output
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').trim())
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<RemovalLine>(l).ok())
        .map(|line| {
            let status = if line.ok {
                RemovalStatus::Removed
            } else {
                RemovalStatus::Failed(
                    line.error
                        .map(|e| e.trim().to_string())
                        .filter(|e| !e.is_empty())
                        .unwrap_or_else(|| "Remove-AppxPackage failed".to_string()),
                )
            };
            (line.full_name.to_ascii_lowercase(), status)
        })
        .collect()
}

/// The removal script failed as a whole, so which packages it removed is unknown. Records
/// this call created are withdrawn for packages that are still installed; when that cannot
/// be determined either, PowerShell most likely never ran and all of them are withdrawn.
fn abandon_batch<F>(
    safety: &Safety,
    batch: &[&AppxPackage],
    created: &[bool],
    run: &mut F,
    err: &Error,
) where
    F: FnMut(&str) -> Result<String>,
{
    let names: Vec<&str> = batch.iter().map(|p| p.full_name.as_str()).collect();
    let still_installed = match run(&installed_check_script(&names)) {
        Ok(output) => Some(
            output
                .lines()
                .map(|l| l.trim_start_matches('\u{feff}').trim().to_ascii_lowercase())
                .filter(|l| !l.is_empty())
                .collect::<HashSet<String>>(),
        ),
        Err(e) => {
            warn!(error = %e, "cannot determine which Appx packages are still installed");
            None
        }
    };
    let withdraw: Vec<&str> = batch
        .iter()
        .zip(created)
        .filter(|(p, &c)| {
            c && match &still_installed {
                Some(set) => set.contains(&p.full_name.to_ascii_lowercase()),
                None => true,
            }
        })
        .map(|(p, _)| p.full_name.as_str())
        .collect();
    unjournal(safety, &withdraw);
    let detail = err.to_string();
    for pkg in batch {
        log_best_effort(safety, &pkg.full_name, "failed", Some(&detail));
    }
}

/// Prints the full name of each listed package that is installed for the current user.
fn installed_check_script(full_names: &[&str]) -> String {
    let list = quoted_list(full_names);
    format!(
        "$ProgressPreference = 'SilentlyContinue'; $WarningPreference = 'SilentlyContinue'; \
         $names = @({list}); \
         Appx\\Get-AppxPackage | \
         Microsoft.PowerShell.Core\\Where-Object {{ $names -contains $_.PackageFullName }} | \
         Microsoft.PowerShell.Core\\ForEach-Object {{ $_.PackageFullName }}"
    )
}

/// Marks the active records of `full_names` created in this session as reverted.
fn unjournal(safety: &Safety, full_names: &[&str]) {
    if full_names.is_empty() {
        return;
    }
    let active = match safety.journal().active_appx() {
        Ok(active) => active,
        Err(e) => {
            warn!(error = %e, "cannot read the Appx journal to withdraw records");
            return;
        }
    };
    for name in full_names {
        let rec = active.iter().find(|r| {
            r.session_id == safety.session_id() && r.package_full_name.eq_ignore_ascii_case(name)
        });
        if let Some(rec) = rec {
            if let Err(e) = safety.journal().mark_reverted(JournalTable::Appx, rec.id) {
                warn!(package = name, error = %e, "cannot withdraw Appx journal record");
            }
        }
    }
}

fn log_best_effort(safety: &Safety, full_name: &str, outcome: &str, detail: Option<&str>) {
    if let Err(e) = safety.log_op("remove_appx", full_name, outcome, detail) {
        warn!(package = full_name, error = %e, "cannot write Appx removal to the audit log");
    }
}

// ───────────────────────────── re-registration ─────────────────────────────

/// `<install_location>\AppxManifest.xml` when the location is absolute and the file exists.
fn manifest_path(install_location: &str) -> Option<PathBuf> {
    let location = install_location.trim();
    if location.is_empty() {
        return None;
    }
    let location = Path::new(location);
    if !location.is_absolute() {
        return None;
    }
    let manifest = location.join("AppxManifest.xml");
    manifest.is_file().then_some(manifest)
}

fn store_required(package_family: &str) -> RestoreOutcome {
    RestoreOutcome::StoreRequired {
        package_family: package_family.to_string(),
        store_link: store_link(package_family),
    }
}

/// A reply line of the restore scripts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    /// `installed <full name>`: a package of the family is registered already.
    Installed(String),
    /// `registered <count>`: packages of the family registered after Add-AppxPackage.
    Registered(u32),
}

/// Prints `installed <full name>` when a package of `family` is registered for the current
/// user; otherwise registers `manifest` and prints `registered <count>`. A failing lookup
/// counts as not installed, so it never hides a package that needs restoring.
fn reregister_script(manifest: &Path, name: &str, family: &str) -> String {
    let lookup = format!(
        "@(Appx\\Get-AppxPackage -Name {name} -ErrorAction SilentlyContinue | \
         Microsoft.PowerShell.Core\\Where-Object {{ $_.PackageFamilyName -eq {family} }})",
        name = ps_quote(name),
        family = ps_quote(family),
    );
    format!(
        "$ProgressPreference = 'SilentlyContinue'; $WarningPreference = 'SilentlyContinue'; \
         $installed = {lookup}; \
         if ($installed.Count -gt 0) {{ 'installed ' + $installed[0].PackageFullName }} \
         else {{ Appx\\Add-AppxPackage -Register {manifest} -DisableDevelopmentMode \
         -ErrorAction Stop; \
         $after = {lookup}; 'registered ' + $after.Count }}",
        manifest = ps_quote(&manifest.to_string_lossy()),
    )
}

/// Prints `installed <full name>` for each package of the current user whose family is one
/// of `families` (compared ignoring case). A failing lookup prints nothing.
fn reinstalled_script(families: &[&str]) -> String {
    let list = quoted_list(families);
    format!(
        "$ProgressPreference = 'SilentlyContinue'; $WarningPreference = 'SilentlyContinue'; \
         $families = @({list}); \
         Appx\\Get-AppxPackage -ErrorAction SilentlyContinue | \
         Microsoft.PowerShell.Core\\Where-Object {{ $families -contains $_.PackageFamilyName }} | \
         Microsoft.PowerShell.Core\\ForEach-Object {{ 'installed ' + $_.PackageFullName }}"
    )
}

/// One reply line; `None` for host noise.
fn parse_reply(line: &str) -> Option<Reply> {
    let line = line.trim_start_matches('\u{feff}').trim();
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match word {
        "installed" if is_valid_identity(rest) => Some(Reply::Installed(rest.to_string())),
        "registered" => rest.parse().ok().map(Reply::Registered),
        _ => None,
    }
}

/// The last line of [`reregister_script`]'s output that carries a reply.
fn parse_reregister_output(output: &str) -> Option<Reply> {
    output.lines().rev().find_map(parse_reply)
}

/// The packages [`reinstalled_script`] printed, keyed by lowercase family; the first
/// package of each family is kept.
fn parse_reinstalled_output(output: &str) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for reply in output.lines().filter_map(parse_reply) {
        if let Reply::Installed(full_name) = reply {
            if let Some(family) = family_from_full_name(&full_name) {
                found
                    .entry(family.to_ascii_lowercase())
                    .or_insert(full_name);
            }
        }
    }
    found
}

fn is_missing_files_error(e: &Error) -> bool {
    let text = e.to_string().to_ascii_uppercase();
    MISSING_FILES_HRESULTS.iter().any(|h| text.contains(h))
}

// ───────────────────────────── PowerShell quoting ─────────────────────────────

/// PowerShell single-quoted literal. PowerShell also treats the typographic single quotes
/// U+2018..U+201B as quote characters, so those are doubled as well.
fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn quoted_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|s| ps_quote(s))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::safety::state_log::Journal;
    use crate::safety::{RestorePointPolicy, SafetyOptions};

    const STORE_JSON: &str = r#"{"Name":"Microsoft.WindowsStore","PackageFullName":"Microsoft.WindowsStore_22608.1401.3.0_x64__8wekyb3d8bbwe","PackageFamilyName":"Microsoft.WindowsStore_8wekyb3d8bbwe","Publisher":"CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US","Version":"22608.1401.3.0","InstallLocation":"C:\\Program Files\\WindowsApps\\Microsoft.WindowsStore_22608.1401.3.0_x64__8wekyb3d8bbwe","IsFramework":false,"NonRemovable":false,"SignatureKind":"Store"}"#;

    const NEWS_JSON: &str = r#"{"Name":"Microsoft.BingNews","PackageFullName":"Microsoft.BingNews_4.55.62231.0_x64__8wekyb3d8bbwe","PackageFamilyName":"Microsoft.BingNews_8wekyb3d8bbwe","Publisher":"CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US","Version":"4.55.62231.0","InstallLocation":"C:\\Program Files\\WindowsApps\\Microsoft.BingNews_4.55.62231.0_x64__8wekyb3d8bbwe","IsFramework":false,"NonRemovable":false,"SignatureKind":"Store"}"#;

    const VCLIBS_JSON: &str = r#"{"Name":"Microsoft.VCLibs.140.00","PackageFullName":"Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe","PackageFamilyName":"Microsoft.VCLibs.140.00_8wekyb3d8bbwe","Publisher":"CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US","Version":"14.0.33519.0","InstallLocation":"C:\\Program Files\\WindowsApps\\Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe","IsFramework":true,"NonRemovable":false,"SignatureKind":"Store"}"#;

    fn pkg(name: &str) -> AppxPackage {
        AppxPackage {
            name: name.to_string(),
            full_name: format!("{name}_1.2.3.0_x64__abcdefghijklm"),
            family_name: format!("{name}_abcdefghijklm"),
            publisher: "CN=Contoso".to_string(),
            version: "1.2.3.0".to_string(),
            install_location: format!(
                r"C:\Program Files\WindowsApps\{name}_1.2.3.0_x64__abcdefghijklm"
            ),
            is_framework: false,
            non_removable: false,
            signature_kind: "Store".to_string(),
            provisioned: None,
        }
    }

    fn test_safety(dir: &Path) -> Safety {
        let journal = Journal::open(dir.join("journal.db")).expect("open journal");
        Safety::begin(
            Arc::new(journal),
            SafetyOptions {
                label: "appx test".to_string(),
                restore_point: RestorePointPolicy::Skip,
                restore_description: String::new(),
                require_elevation: false,
            },
        )
        .expect("begin session")
    }

    fn active_names(safety: &Safety) -> Vec<String> {
        let mut names: Vec<String> = safety
            .journal()
            .active_appx()
            .expect("read journal")
            .into_iter()
            .map(|r| r.package_full_name)
            .collect();
        names.sort();
        names
    }

    // ── inventory parsing ──

    #[test]
    fn parses_single_object() {
        let list = parse_inventory_json(STORE_JSON).expect("parse");
        assert_eq!(list.len(), 1);
        let p = &list[0];
        assert_eq!(p.name, "Microsoft.WindowsStore");
        assert_eq!(
            p.full_name,
            "Microsoft.WindowsStore_22608.1401.3.0_x64__8wekyb3d8bbwe"
        );
        assert_eq!(p.family_name, "Microsoft.WindowsStore_8wekyb3d8bbwe");
        assert!(p.publisher.starts_with("CN=Microsoft Corporation"));
        assert_eq!(p.version, "22608.1401.3.0");
        assert!(p
            .install_location
            .ends_with(r"\Microsoft.WindowsStore_22608.1401.3.0_x64__8wekyb3d8bbwe"));
        assert!(!p.is_framework);
        assert!(!p.non_removable);
        assert_eq!(p.signature_kind, "Store");
        assert_eq!(p.provisioned, None);
    }

    #[test]
    fn parses_array_of_two() {
        let list = parse_inventory_json(&format!("[{STORE_JSON},{NEWS_JSON}]")).expect("parse");
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Microsoft.WindowsStore", "Microsoft.BingNews"]);
        assert_eq!(list[1].family_name, "Microsoft.BingNews_8wekyb3d8bbwe");
    }

    #[test]
    fn parses_pretty_printed_and_bom_prefixed_json() {
        let pretty = format!("\u{feff}[\r\n  {STORE_JSON},\r\n  {NEWS_JSON}\r\n]\r\n");
        assert_eq!(parse_inventory_json(&pretty).expect("parse").len(), 2);
    }

    #[test]
    fn parses_empty_output() {
        for empty in ["[]", "", "  \r\n", "null", "{\"value\":[],\"Count\":0}"] {
            assert!(
                parse_inventory_json(empty).expect("parse").is_empty(),
                "{empty:?}"
            );
        }
    }

    #[test]
    fn unwraps_value_count_wrapper() {
        let wrapped = format!("{{\"value\":[{STORE_JSON},{NEWS_JSON}],\"Count\":2}}");
        assert_eq!(parse_inventory_json(&wrapped).expect("parse").len(), 2);
    }

    #[test]
    fn tolerates_nulls_and_missing_fields() {
        let json = r#"[
            {"Name":"Contoso.App","PackageFullName":"Contoso.App_1.0.0.0_neutral__abcdefghijklm",
             "PackageFamilyName":null,"Publisher":null,"Version":null,"InstallLocation":null,
             "IsFramework":null,"NonRemovable":null,"SignatureKind":null},
            {"Name":"Contoso.Min","PackageFullName":"Contoso.Min_2.0.0.0_x64__abcdefghijklm"}
        ]"#;
        let list = parse_inventory_json(json).expect("parse");
        assert_eq!(list.len(), 2);
        let a = &list[0];
        assert_eq!(
            a.family_name, "Contoso.App_abcdefghijklm",
            "derived from the full name"
        );
        assert_eq!(a.publisher, "");
        assert_eq!(a.version, "");
        assert_eq!(a.install_location, "");
        assert!(!a.is_framework && !a.non_removable);
        assert_eq!(a.signature_kind, "");
        let b = &list[1];
        assert_eq!(b.family_name, "Contoso.Min_abcdefghijklm");
        assert_eq!(b.install_location, "");
    }

    #[test]
    fn skips_entries_without_identity() {
        let json = format!(
            r#"[{{"Name":null,"PackageFullName":"X_1_x64__y"}},{{"Name":"Contoso.NoFull"}},null,5,{NEWS_JSON}]"#
        );
        let list = parse_inventory_json(&json).expect("parse");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Microsoft.BingNews");
    }

    #[test]
    fn parses_framework_package() {
        let list = parse_inventory_json(VCLIBS_JSON).expect("parse");
        assert!(list[0].is_framework);
        assert!(!is_removable(&list[0]));
    }

    #[test]
    fn maps_signature_kinds() {
        let with_kind = |kind: &str| {
            let json = format!(
                r#"{{"Name":"Contoso.App","PackageFullName":"Contoso.App_1.0.0.0_x64__abcdefghijklm","SignatureKind":{kind}}}"#
            );
            parse_inventory_json(&json).expect("parse")[0]
                .signature_kind
                .clone()
        };
        assert_eq!(with_kind(r#""None""#), "None");
        assert_eq!(with_kind(r#""Developer""#), "Developer");
        assert_eq!(with_kind(r#""Enterprise""#), "Enterprise");
        assert_eq!(with_kind(r#""Store""#), "Store");
        assert_eq!(with_kind(r#""System""#), "System");
        assert_eq!(with_kind("0"), "None");
        assert_eq!(with_kind("3"), "Store");
        assert_eq!(with_kind("4"), "System");
        assert_eq!(with_kind(r#""4""#), "System");
        assert_eq!(with_kind("9"), "9");
    }

    #[test]
    fn reads_version_objects_and_loose_flags() {
        let json = r#"{"Name":"Contoso.App","PackageFullName":"Contoso.App_1.2.3.4_x64__abcdefghijklm",
            "Version":{"Major":1,"Minor":2,"Build":3,"Revision":4,"MajorRevision":0,"MinorRevision":4},
            "IsFramework":"True","NonRemovable":0}"#;
        let p = &parse_inventory_json(json).expect("parse")[0];
        assert_eq!(p.version, "1.2.3.4");
        assert!(p.is_framework);
        assert!(!p.non_removable);

        let two = r#"{"Name":"Contoso.App","PackageFullName":"Contoso.App_1.2.0.0_x64__abcdefghijklm",
            "Version":{"Major":1,"Minor":2,"Build":-1,"Revision":-1}}"#;
        assert_eq!(parse_inventory_json(two).expect("parse")[0].version, "1.2");
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_inventory_json("{not json").is_err());
        assert!(parse_inventory_json("\"text\"").is_err());
    }

    #[test]
    fn family_derivation() {
        assert_eq!(
            family_from_full_name("Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe")
                .as_deref(),
            Some("Microsoft.VCLibs.140.00_8wekyb3d8bbwe")
        );
        assert_eq!(family_from_full_name("NoUnderscores"), None);
        assert_eq!(family_from_full_name("a_b_c"), None);
    }

    // ── removability and validation ──

    #[test]
    fn removability() {
        assert!(is_removable(&pkg("Microsoft.BingNews")));
        assert!(is_removable(&pkg("king.com.CandyCrushSaga")));
        assert!(
            is_removable(&pkg("Microsoft.Windows.DevHome")),
            "protected-prefix exception"
        );

        let mut framework = pkg("Contoso.Runtime");
        framework.is_framework = true;
        assert!(!is_removable(&framework));

        let mut non_removable = pkg("Contoso.Shell");
        non_removable.non_removable = true;
        assert!(!is_removable(&non_removable));

        for kind in ["System", "system", " SYSTEM "] {
            let mut system = pkg("Contoso.SystemApp");
            system.signature_kind = kind.to_string();
            assert!(!is_removable(&system), "{kind}");
        }

        for name in [
            "Microsoft.WindowsStore",
            "microsoft.windowsstore",
            "Microsoft.VCLibs.140.00",
            "Microsoft.Windows.Photos",
        ] {
            assert!(!is_removable(&pkg(name)), "{name}");
        }
    }

    #[test]
    fn identity_validation() {
        assert!(is_valid_identity(
            "Microsoft.VCLibs.140.00_14.0.33519.0_x64__8wekyb3d8bbwe"
        ));
        assert!(is_valid_identity(
            "Microsoft.LanguageExperiencePackde-DE_26100.1.2.0_neutral__8wekyb3d8bbwe"
        ));
        assert!(is_valid_identity(
            "Contoso.App_1.0.0.0_neutral_~_abcdefghijklm"
        ));
        for bad in [
            "",
            "Contoso.App'_1.0",
            "Contoso App_1.0",
            "Contoso.App_1.0;x",
            "Contoso.App_$x",
            "Contoso.*",
            "Contoso.App\u{2019}x",
            "Contoso.Äpp",
            "Contoso.App\n",
        ] {
            assert!(!is_valid_identity(bad), "{bad:?}");
        }
    }

    #[test]
    fn refusal_reasons() {
        assert_eq!(removal_refusal(&pkg("Contoso.App")), None);

        let mut framework = pkg("Contoso.Runtime");
        framework.is_framework = true;
        assert_eq!(
            removal_refusal(&framework).as_deref(),
            Some("protected package")
        );

        let mut quoted = pkg("Contoso.App");
        quoted.full_name = "Contoso.App_1.0.0.0_x64__abc'x".to_string();
        assert_eq!(
            removal_refusal(&quoted).as_deref(),
            Some("invalid package full name")
        );

        let mut mismatched = pkg("Contoso.App");
        mismatched.full_name = "Microsoft.WindowsStore_1.0.0.0_x64__8wekyb3d8bbwe".to_string();
        assert_eq!(
            removal_refusal(&mismatched).as_deref(),
            Some("package name does not match its full name")
        );

        let mut bad_family = pkg("Contoso.App");
        bad_family.family_name = "Contoso.App_abc def".to_string();
        assert_eq!(
            removal_refusal(&bad_family).as_deref(),
            Some("invalid package family name")
        );

        let mut no_family = pkg("Contoso.App");
        no_family.family_name.clear();
        assert_eq!(
            removal_refusal(&no_family),
            None,
            "family derived from the full name"
        );
    }

    #[test]
    fn name_prefix_check() {
        assert!(has_name_prefix(
            "Contoso.App_1.0.0.0_x64__abc",
            "Contoso.App"
        ));
        assert!(has_name_prefix("contoso.app_abc", "Contoso.App"));
        assert!(!has_name_prefix(
            "Contoso.AppX_1.0.0.0_x64__abc",
            "Contoso.App"
        ));
        assert!(!has_name_prefix("Contoso.App", "Contoso.App"));
        assert!(!has_name_prefix("Contoso.App_1", ""));
    }

    // ── PowerShell text ──

    #[test]
    fn single_quoting() {
        assert_eq!(ps_quote("plain"), "'plain'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(
            ps_quote(r"C:\Program Files\A $b"),
            r"'C:\Program Files\A $b'"
        );
        assert_eq!(quoted_list(&["a", "b"]), "'a', 'b'");
    }

    #[test]
    fn removal_script_quotes_every_name() {
        let script = removal_script(&["Contoso.A_1.0.0.0_x64__abc", "Contoso.B_1.0.0.0_x64__abc"]);
        assert!(script.contains("@('Contoso.A_1.0.0.0_x64__abc', 'Contoso.B_1.0.0.0_x64__abc')"));
        assert!(script.contains("Remove-AppxPackage -Package $n -ErrorAction Stop"));
        assert!(!script.contains("-AllUsers"));
    }

    #[test]
    fn reregister_script_quotes_path() {
        let script = reregister_script(
            Path::new(r"C:\Apps\O'Brien\AppxManifest.xml"),
            "Contoso.App",
            "Contoso.App_abc",
        );
        assert!(script.contains(r"Add-AppxPackage -Register 'C:\Apps\O''Brien\AppxManifest.xml' -DisableDevelopmentMode -ErrorAction Stop"));
        assert!(script.contains("Get-AppxPackage -Name 'Contoso.App'"));
        assert!(script.contains("$_.PackageFamilyName -eq 'Contoso.App_abc'"));
        // The installed check comes first, so a reinstalled family is never registered.
        let check = script.find("$installed =").expect("installed check");
        let register = script.find("Add-AppxPackage").expect("register");
        assert!(check < register);
    }

    #[test]
    fn appx_scripts_import_the_module_and_qualify_every_cmdlet() {
        let import = powershell::import_system_module("Appx").unwrap();
        let scripts = [
            INVENTORY_SCRIPT.to_string(),
            removal_script(&["Contoso.A_1.0.0.0_x64__abc"]),
            installed_check_script(&["Contoso.A_1.0.0.0_x64__abc"]),
            reregister_script(
                Path::new(r"C:\Apps\Contoso\AppxManifest.xml"),
                "Contoso.App",
                "Contoso.App_abc",
            ),
            reinstalled_script(&["Contoso.App_abc"]),
        ];
        let qualified = [
            ("Get-AppxPackage", "Appx\\"),
            ("Add-AppxPackage", "Appx\\"),
            ("Remove-AppxPackage", "Appx\\"),
            ("ConvertTo-Json", "Microsoft.PowerShell.Utility\\"),
            ("Select-Object", "Microsoft.PowerShell.Utility\\"),
            ("Where-Object", "Microsoft.PowerShell.Core\\"),
            ("ForEach-Object", "Microsoft.PowerShell.Core\\"),
        ];
        for script in &scripts {
            let full = with_appx_module(script).unwrap();
            assert!(full.starts_with(&import), "{full}");
            assert!(import.contains(r"\Modules\Appx\Appx.psd1'"), "{import}");
            for (cmdlet, module) in qualified {
                for (at, _) in script.match_indices(cmdlet) {
                    assert!(
                        script[..at].ends_with(module),
                        "{cmdlet} is not called as {module}{cmdlet} in {script}"
                    );
                }
            }
        }
    }

    #[test]
    fn reinstalled_script_looks_up_every_family_without_registering() {
        let script = reinstalled_script(&["Contoso.A_abc", "O'Brien.B_abc"]);
        assert!(script.contains("$families = @('Contoso.A_abc', 'O''Brien.B_abc')"));
        assert!(script.contains("$families -contains $_.PackageFamilyName"));
        assert!(script.contains("'installed ' + $_.PackageFullName"));
        assert!(!script.contains("Add-AppxPackage"));
    }

    #[test]
    fn parses_reinstalled_families() {
        let found = parse_reinstalled_output(
            "\u{feff}noise\r\n\
             installed Contoso.A_2.0.0.0_x64__abc\r\n\
             installed Contoso.A_3.0.0.0_x64__abc\r\n\
             installed Contoso.B_1.0.0.0_neutral_~_xyz\r\n\
             installed not-a-full-name\r\n\
             registered 1\r\n",
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found["contoso.a_abc"], "Contoso.A_2.0.0.0_x64__abc");
        assert_eq!(found["contoso.b_xyz"], "Contoso.B_1.0.0.0_neutral_~_xyz");
        assert!(parse_reinstalled_output("").is_empty());
    }

    #[test]
    fn parses_reregister_replies() {
        assert_eq!(
            parse_reregister_output("\u{feff}noise\r\ninstalled Contoso.App_2.0.0.0_x64__abc\r\n"),
            Some(Reply::Installed("Contoso.App_2.0.0.0_x64__abc".to_string()))
        );
        assert_eq!(
            parse_reregister_output("registered 1\r\n"),
            Some(Reply::Registered(1))
        );
        assert_eq!(
            parse_reregister_output("registered 0"),
            Some(Reply::Registered(0))
        );
        assert_eq!(parse_reregister_output("absent\n"), None);
        assert_eq!(parse_reregister_output("installed "), None);
        assert_eq!(parse_reregister_output("installed bad name"), None);
        assert_eq!(parse_reregister_output(""), None);
        assert_eq!(parse_reregister_output("1"), None);
    }

    #[test]
    fn parses_removal_output() {
        let out = "\u{feff}WARNING: noise\r\n\
                   {\"FullName\":\"Contoso.A_1_x64__abc\",\"Ok\":true,\"Error\":null}\r\n\
                   {\"FullName\":\"Contoso.B_1_x64__abc\",\"Ok\":false,\"Error\":\"Deployment failed with HRESULT: 0x80073CFA\\r\\n\"}\r\n\
                   {\"FullName\":\"Contoso.C_1_x64__abc\",\"Ok\":false,\"Error\":\"\"}\r\n\
                   {broken\r\n";
        let map = parse_removal_output(out);
        assert_eq!(map.len(), 3);
        assert_eq!(map["contoso.a_1_x64__abc"], RemovalStatus::Removed);
        assert_eq!(
            map["contoso.b_1_x64__abc"],
            RemovalStatus::Failed("Deployment failed with HRESULT: 0x80073CFA".to_string())
        );
        assert_eq!(
            map["contoso.c_1_x64__abc"],
            RemovalStatus::Failed("Remove-AppxPackage failed".to_string())
        );
    }

    #[test]
    fn missing_files_errors() {
        let ps = |stderr: &str| Error::PowerShell {
            code: 1,
            stderr: stderr.to_string(),
        };
        for hr in ["0x80073CF1", "0x80073cf3", "0x80070002", "0X80070003"] {
            assert!(
                is_missing_files_error(&ps(&format!("Deployment failed with HRESULT: {hr}, x"))),
                "{hr}"
            );
        }
        assert!(!is_missing_files_error(&ps(
            "Deployment failed with HRESULT: 0x80073CFA"
        )));
        assert!(!is_missing_files_error(&Error::Other(
            "access denied".to_string()
        )));
    }

    #[test]
    fn store_links() {
        assert_eq!(
            store_link("Microsoft.BingNews_8wekyb3d8bbwe"),
            "ms-windows-store://pdp/?PFN=Microsoft.BingNews_8wekyb3d8bbwe"
        );
    }

    // ── removal flow against a scripted runner (no PowerShell is started) ──

    #[test]
    fn remove_journals_first_and_withdraws_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());

        let ok = pkg("Contoso.Removable");
        let fails = pkg("Contoso.Stubborn");
        let mut framework = pkg("Contoso.Runtime");
        framework.is_framework = true;
        let mut injected = pkg("Contoso.Injected");
        injected.full_name = "Contoso.Injected_1.0.0.0_x64__abc'; Get-Date; '".to_string();
        let silent = pkg("Contoso.Silent");

        let scripts = RefCell::new(Vec::<String>::new());
        let runner = |script: &str| -> Result<String> {
            assert!(
                script.contains("Remove-AppxPackage"),
                "unexpected script: {script}"
            );
            assert!(
                !script.contains("Contoso.Injected"),
                "refused package reached PowerShell"
            );
            assert!(
                !script.contains("Contoso.Runtime"),
                "protected package reached PowerShell"
            );
            // Every accepted package is journaled before the removal script runs.
            let journaled = active_names(&safety);
            assert_eq!(journaled.len(), 3, "journal before removal: {journaled:?}");
            scripts.borrow_mut().push(script.to_string());
            Ok(format!(
                "{{\"FullName\":\"{}\",\"Ok\":true,\"Error\":null}}\n\
                 {{\"FullName\":\"{}\",\"Ok\":false,\"Error\":\"Deployment failed with HRESULT: 0x80073CFA\"}}\n",
                ok.full_name, fails.full_name
            ))
        };

        let input = [
            framework.clone(),
            ok.clone(),
            injected.clone(),
            fails.clone(),
            ok.clone(),
            silent.clone(),
        ];
        let results = remove_with(&safety, &input, runner).expect("remove");
        assert_eq!(scripts.borrow().len(), 1, "one PowerShell invocation");
        assert_eq!(
            scripts.borrow()[0].matches("Contoso.Removable_").count(),
            1,
            "duplicates are removed once"
        );

        let full: Vec<&str> = results.iter().map(|r| r.full_name.as_str()).collect();
        let expected: Vec<&str> = input.iter().map(|p| p.full_name.as_str()).collect();
        assert_eq!(full, expected, "results are in input order");

        assert_eq!(
            results[0].outcome,
            MutationOutcome::Skipped("protected package".into())
        );
        assert_eq!(results[0].error, None);
        assert_eq!(results[1].outcome, MutationOutcome::Applied);
        assert_eq!(results[1].error, None);
        assert_eq!(
            results[2].outcome,
            MutationOutcome::Skipped("invalid package full name".into())
        );
        assert_eq!(results[2].error, None);
        let msg = "Deployment failed with HRESULT: 0x80073CFA".to_string();
        assert_eq!(results[3].outcome, MutationOutcome::Skipped(msg.clone()));
        assert_eq!(results[3].error, Some(msg));
        assert_eq!(results[4].outcome, MutationOutcome::Applied);
        assert_eq!(
            results[5].outcome,
            MutationOutcome::Skipped(UNREPORTED.into())
        );
        assert_eq!(results[5].error.as_deref(), Some(UNREPORTED));

        // The failed package's record is withdrawn; a package without a reported result
        // keeps its record because it may have been removed.
        assert_eq!(
            active_names(&safety),
            vec![ok.full_name.clone(), silent.full_name.clone()]
        );
        let rec = safety
            .journal()
            .active_appx()
            .expect("read")
            .into_iter()
            .find(|r| r.package_full_name == ok.full_name)
            .expect("record");
        assert_eq!(rec.package_family, ok.family_name);
        assert_eq!(rec.install_location, ok.install_location);
        assert!(!rec.all_users);

        let ops = safety.journal().ops(50).expect("ops");
        let outcome_of = |name: &str| {
            ops.iter()
                .filter(|o| o.op == "remove_appx" && o.target == name)
                .map(|o| o.outcome.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(outcome_of(&ok.full_name), vec!["removed".to_string()]);
        assert_eq!(outcome_of(&fails.full_name), vec!["failed".to_string()]);
        assert_eq!(
            outcome_of(&framework.full_name),
            vec!["skipped".to_string()]
        );
    }

    #[test]
    fn remove_keeps_existing_baseline_on_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());
        let p = pkg("Contoso.Reinstalled");
        let earlier = NewAppxRecord {
            package_full_name: p.full_name.clone(),
            package_family: p.family_name.clone(),
            install_location: p.install_location.clone(),
            all_users: false,
        };
        assert!(safety.record_appx(&earlier).expect("record"));

        let results = remove_with(
            &safety,
            std::slice::from_ref(&p),
            |_: &str| -> Result<String> {
                Ok(format!(
                    "{{\"FullName\":\"{}\",\"Ok\":false,\"Error\":\"boom\"}}",
                    p.full_name
                ))
            },
        )
        .expect("remove");
        assert_eq!(results[0].error.as_deref(), Some("boom"));
        assert_eq!(
            active_names(&safety),
            vec![p.full_name.clone()],
            "baseline not created here"
        );
    }

    #[test]
    fn remove_withdraws_records_of_packages_still_installed_after_script_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());
        let gone = pkg("Contoso.Gone");
        let kept = pkg("Contoso.Kept");

        let calls = RefCell::new(0);
        let runner = |script: &str| -> Result<String> {
            *calls.borrow_mut() += 1;
            if script.contains("Remove-AppxPackage") {
                Err(Error::PowerShell {
                    code: 1,
                    stderr: "terminated".to_string(),
                })
            } else {
                assert!(
                    script.contains("Get-AppxPackage |"),
                    "unexpected script: {script}"
                );
                assert!(!script.contains("Add-AppxPackage"));
                Ok(format!("{}\r\n", kept.full_name))
            }
        };
        let err = remove_with(&safety, &[gone.clone(), kept.clone()], runner).expect_err("fails");
        assert!(matches!(err, Error::PowerShell { .. }));
        assert_eq!(*calls.borrow(), 2);
        assert_eq!(active_names(&safety), vec![gone.full_name.clone()]);
    }

    #[test]
    fn remove_withdraws_all_new_records_when_powershell_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());
        let a = pkg("Contoso.A");
        let b = pkg("Contoso.B");
        let runner = |_: &str| -> Result<String> {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "powershell.exe",
            )))
        };
        assert!(remove_with(&safety, &[a, b], runner).is_err());
        assert!(active_names(&safety).is_empty());
    }

    #[test]
    fn remove_without_candidates_starts_no_powershell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());
        let mut framework = pkg("Contoso.Runtime");
        framework.is_framework = true;
        let runner = |script: &str| -> Result<String> { panic!("PowerShell started: {script}") };
        let results = remove_with(&safety, &[framework, pkg("Microsoft.WindowsStore")], runner)
            .expect("remove");
        assert!(results
            .iter()
            .all(|r| r.outcome == MutationOutcome::Skipped("protected package".into())));
        assert!(remove_with(&safety, &[], runner)
            .expect("remove")
            .is_empty());
        assert!(active_names(&safety).is_empty());
    }

    // ── re-registration without files ──

    fn record(install_location: String) -> AppxRecord {
        AppxRecord {
            id: 1,
            session_id: 1,
            recorded_at: String::new(),
            package_full_name: "Contoso.Gone_1.0.0.0_x64__abcdefghijklm".to_string(),
            package_family: "Contoso.Gone_abcdefghijklm".to_string(),
            install_location,
            all_users: false,
            active: true,
            reverted_at: None,
        }
    }

    fn store_required_for_gone() -> RestoreOutcome {
        RestoreOutcome::StoreRequired {
            package_family: "Contoso.Gone_abcdefghijklm".to_string(),
            store_link: "ms-windows-store://pdp/?PFN=Contoso.Gone_abcdefghijklm".to_string(),
        }
    }

    #[test]
    fn reregister_without_manifest_requires_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("Contoso.Gone_1.0.0.0_x64__abcdefghijklm");
        assert!(!missing.exists());
        let rec = record(missing.to_string_lossy().into_owned());
        assert_eq!(
            reregister(&rec).expect("reregister"),
            store_required_for_gone()
        );
        assert_eq!(
            reregister(&record(String::new())).expect("reregister"),
            store_required_for_gone()
        );
    }

    #[test]
    fn reregister_reports_a_reinstalled_family_without_registering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let location = dir.path().join("Contoso.Gone_1.0.0.0_x64__abcdefghijklm");
        std::fs::create_dir_all(&location).expect("mkdir");
        std::fs::write(location.join("AppxManifest.xml"), "<Package/>").expect("manifest");
        for install_location in [location.to_string_lossy().into_owned(), String::new()] {
            let scripts = RefCell::new(Vec::<String>::new());
            let outcome = reregister_with(&record(install_location), |script: &str| {
                scripts.borrow_mut().push(script.to_string());
                Ok("installed Contoso.Gone_2.0.0.0_x64__abcdefghijklm\r\n".to_string())
            })
            .expect("reregister");
            assert_eq!(
                outcome,
                RestoreOutcome::AlreadyInstalled {
                    package_full_name: "Contoso.Gone_2.0.0.0_x64__abcdefghijklm".to_string()
                }
            );
            assert_eq!(scripts.borrow().len(), 1, "one PowerShell invocation");
        }
    }

    #[test]
    fn reregister_maps_script_replies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let location = dir.path().join("Contoso.Gone_1.0.0.0_x64__abcdefghijklm");
        std::fs::create_dir_all(&location).expect("mkdir");
        std::fs::write(location.join("AppxManifest.xml"), "<Package/>").expect("manifest");
        let with_manifest = record(location.to_string_lossy().into_owned());

        let reply = |text: &'static str| move |_: &str| -> Result<String> { Ok(text.to_string()) };
        assert_eq!(
            reregister_with(&with_manifest, reply("registered 1")).expect("reregister"),
            RestoreOutcome::Reregistered
        );
        assert!(reregister_with(&with_manifest, reply("registered 0")).is_err());
        assert!(reregister_with(&with_manifest, reply("")).is_err());
        assert_eq!(
            reregister_with(&record(String::new()), reply("")).expect("reregister"),
            store_required_for_gone()
        );

        let missing_files = |_: &str| -> Result<String> {
            Err(Error::PowerShell {
                code: 1,
                stderr: "Deployment failed with HRESULT: 0x80073CF3".to_string(),
            })
        };
        assert_eq!(
            reregister_with(&with_manifest, missing_files).expect("reregister"),
            store_required_for_gone()
        );
        let other_failure = |_: &str| -> Result<String> {
            Err(Error::PowerShell {
                code: 1,
                stderr: "Deployment failed with HRESULT: 0x80073CFA".to_string(),
            })
        };
        assert!(reregister_with(&with_manifest, other_failure).is_err());

        let mut bad = record(String::new());
        bad.package_family = "Contoso.Gone_abc'; Get-Date; '".to_string();
        let never = |script: &str| -> Result<String> { panic!("PowerShell started: {script}") };
        assert!(reregister_with(&bad, never).is_err());
    }

    #[test]
    fn packages_without_files_require_the_store_when_the_lookup_cannot_run() {
        let unavailable = |_: &str| -> Result<String> {
            Err(Error::PowerShell {
                code: -1,
                stderr: "This program is blocked by group policy.".to_string(),
            })
        };
        assert_eq!(
            reregister_with(&record(String::new()), unavailable).expect("reregister"),
            store_required_for_gone()
        );
    }

    #[test]
    fn packages_without_files_share_one_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let location = dir.path().join("Contoso.Kept_1.0.0.0_x64__abcdefghijklm");
        std::fs::create_dir_all(&location).expect("mkdir");
        std::fs::write(location.join("AppxManifest.xml"), "<Package/>").expect("manifest");

        let gone = record(String::new());
        let mut back = record(String::new());
        back.package_full_name = "Contoso.Back_1.0.0.0_x64__abcdefghijklm".to_string();
        back.package_family = "Contoso.Back_abcdefghijklm".to_string();
        let mut kept = record(location.to_string_lossy().into_owned());
        kept.package_full_name = "Contoso.Kept_1.0.0.0_x64__abcdefghijklm".to_string();
        kept.package_family = "Contoso.Kept_abcdefghijklm".to_string();
        let mut bad = record(String::new());
        bad.package_family = "Bad family".to_string();

        let scripts = RefCell::new(Vec::<String>::new());
        let results =
            reregister_all_with(&[gone, back, bad, kept], |script: &str| -> Result<String> {
                scripts.borrow_mut().push(script.to_string());
                Ok(if script.contains("Add-AppxPackage") {
                    "registered 1".to_string()
                } else {
                    "installed Contoso.Back_2.0.0.0_x64__abcdefghijklm\r\n".to_string()
                })
            });

        let scripts = scripts.into_inner();
        assert_eq!(
            scripts.len(),
            2,
            "one lookup and one registration: {scripts:?}"
        );
        assert!(
            scripts[0].contains("@('Contoso.Gone_abcdefghijklm', 'Contoso.Back_abcdefghijklm')")
        );
        assert!(scripts[1].contains("Contoso.Kept_abcdefghijklm"));
        assert_eq!(results.len(), 4);
        assert_eq!(
            results[0].as_ref().expect("gone"),
            &store_required_for_gone()
        );
        assert_eq!(
            results[1].as_ref().expect("back"),
            &RestoreOutcome::AlreadyInstalled {
                package_full_name: "Contoso.Back_2.0.0.0_x64__abcdefghijklm".to_string()
            }
        );
        assert!(results[2].is_err());
        assert_eq!(
            results[3].as_ref().expect("kept"),
            &RestoreOutcome::Reregistered
        );
    }

    #[test]
    fn remove_refuses_when_running_as_another_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let safety = test_safety(dir.path());
        safety.assume_other_user();
        let never = |script: &str| -> Result<String> { panic!("PowerShell started: {script}") };
        let err = remove_with(&safety, &[pkg("Contoso.App")], never).expect_err("refused");
        assert!(err.to_string().contains("different account"), "{err}");
        assert!(active_names(&safety).is_empty(), "nothing journaled");
    }

    #[test]
    fn reregister_finds_an_installed_family() {
        // Read-only: the Store is registered for every interactive user, and the recorded
        // location does not exist, so nothing can be registered either way.
        let installed = inventory().expect("inventory");
        let Some(store) = installed
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case("Microsoft.WindowsStore"))
        else {
            return;
        };
        let mut rec = record(String::new());
        rec.package_full_name = "Microsoft.WindowsStore_1.0.0.0_x64__8wekyb3d8bbwe".to_string();
        rec.package_family = store.family_name.clone();
        match reregister(&rec).expect("reregister") {
            RestoreOutcome::AlreadyInstalled { package_full_name } => assert!(
                installed.iter().any(|p| p.full_name == package_full_name
                    && p.family_name.eq_ignore_ascii_case(&store.family_name)),
                "{package_full_name}"
            ),
            other => panic!("expected AlreadyInstalled, got {other:?}"),
        }
    }

    // ── live, read-only ──

    #[test]
    fn inventory_lists_protected_packages() {
        let list = inventory().expect("inventory");
        assert!(!list.is_empty());
        assert!(
            list.iter()
                .any(|p| p.name.eq_ignore_ascii_case("Microsoft.WindowsStore") || p.is_framework),
            "neither the Store nor a framework package is listed"
        );
        for p in list.iter().filter(|p| p.is_framework) {
            assert!(
                !is_removable(p),
                "{} is a framework but removable",
                p.full_name
            );
        }
        assert!(list.iter().all(|p| p.provisioned.is_none()));
    }

    #[test]
    #[ignore = "timing report; run with --ignored --nocapture"]
    fn inventory_timing() {
        let started = Instant::now();
        let list = inventory().expect("inventory");
        let elapsed = started.elapsed();
        let frameworks = list.iter().filter(|p| p.is_framework).count();
        let removable = list.iter().filter(|p| is_removable(p)).count();
        let bloat: Vec<&AppxPackage> = list
            .iter()
            .filter(|p| catalog::bloat_entry_for(&p.name).is_some() && is_removable(p))
            .collect();
        println!(
            "inventory: {} packages ({frameworks} frameworks, {removable} removable) in {} ms",
            list.len(),
            elapsed.as_millis()
        );
        println!("bloat catalog matches: {}", bloat.len());
        for p in &bloat {
            println!("  {} ({})", p.name, p.full_name);
        }
    }
}
