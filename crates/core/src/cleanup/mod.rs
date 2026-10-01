//! Disk cleanup: sizes and deletes disposable files in well-known locations.
//!
//! Deleting files cannot be journaled or undone. Every target except the Recycle Bin is a
//! fixed, documented location of regenerable data (temporary files, caches, crash dumps,
//! error reports), resolved from the shell's known folders and `GetWindowsDirectoryW`
//! rather than from environment variables. The one location an environment variable
//! chooses, the process temp folder (`%TMP%` / `%TEMP%`), is cleaned only when it resolves
//! to the standard `AppData\Local\Temp` folder of the user's profile or to a numbered
//! per-session folder directly inside it; anywhere else, off a fixed local disk, or when
//! the standard folder is itself a link, the target is blocked and the scan says why. The
//! folder is then validated as `%TMP%` names it, so a link there is refused as well, and the
//! folder actually opened must still resolve to the standard one, so a link above it that
//! is changed after the check cannot move the clean elsewhere.
//!
//! Folder targets are walked without following reparse points (junctions, symbolic links,
//! mount points), never leave the target's root, skip files that are in use, and in the two
//! temporary folders also keep anything created, written or changed in the last 24 hours
//! and any folder created in that time (see `walk` for the exact rules and the checks
//! every root must pass).
//!
//! The Recycle Bin target is the one exception to the regenerable-data rule: emptying it
//! permanently deletes files the user already deleted, which cannot be regenerated or
//! restored. It is off by default, its description says so, and it is emptied through the
//! shell instead of the walk.
//!
//! The Windows Update target stops the Windows Update service while it deletes the
//! downloaded updates. Like any other service change, the stop is journaled first; the
//! service is started again afterwards and its journal record closed, and if it cannot be
//! started the record stays active so the service can be started again from History. Each
//! run is recorded in the journal's audit log.
//!
//! A browser cache is skipped while that browser runs. The Windows Update and Delivery
//! Optimization targets are skipped while DISM runs, because DISM can use Windows Update as
//! its repair source. When the process list cannot be read, those targets are skipped too.

mod system;
mod walk;

use std::collections::HashSet;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::System::Services::{SERVICE_START, SERVICE_STOP};
use windows::Win32::UI::Shell::{FOLDERID_Profile, FOLDERID_ProgramData};

use crate::safety::state_log::{Journal, JournalTable, NewServiceRecord};
use crate::win::scm::{self, Scm, Service, ServiceState};
use crate::{Error, Result};

use walk::{Filter, Guards, Measure, Purge, Resolved, SafeRoot, MAX_ERRORS};

/// A cleanup location. `id` values are stable and used by the UI and CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Deleting here needs an elevated process.
    pub requires_admin: bool,
    /// Selected by default in the UI.
    pub default_on: bool,
    /// Files created, written or changed in the last 24 hours (and folders created in that
    /// time) are left in place.
    #[serde(default)]
    pub recent_files_kept: bool,
}

/// Size of one target as found by [`scan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetScan {
    pub id: String,
    pub title: String,
    pub description: String,
    pub requires_admin: bool,
    pub default_on: bool,
    /// Bytes that a clean would try to delete (eligible files only).
    pub bytes: u64,
    pub files: u64,
    /// Why the target cannot be cleaned right now (for example "Chrome is running",
    /// "needs administrator rights", DISM repairing Windows, a temp folder outside the
    /// standard location, or every location refused by a safety check); `None` when it can.
    /// [`clean`] skips a target for missing rights, a running blocking program or a blocked
    /// location by the same rule.
    pub blocked_reason: Option<String>,
    /// Files created, written or changed in the last 24 hours (and folders created in that
    /// time) are left in place.
    #[serde(default)]
    pub recent_files_kept: bool,
    /// The folders and single files that were measured and that a clean works on, as
    /// display paths (`C:\Users\Test\AppData\Local\Temp`). Empty when none exists, the target
    /// is blocked by its location, or it is not file based (the Recycle Bin).
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupScan {
    pub targets: Vec<TargetScan>,
    pub total_bytes: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetResult {
    pub id: String,
    pub freed_bytes: u64,
    pub deleted_files: u64,
    /// Files left in place because they were in use, too recent or not deletable.
    pub skipped_files: u64,
    /// Set when the whole target was skipped (blocked, unknown id, missing rights).
    pub skipped_reason: Option<String>,
    /// Up to a handful of representative error messages; the most important one (a
    /// service left stopped, a result missing from the audit log) comes first.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupReport {
    pub results: Vec<TargetResult>,
    pub freed_bytes: u64,
    pub duration_ms: u64,
}

// ───────────────────────────── Catalog ─────────────────────────────

const NEEDS_ADMIN: &str = "needs administrator rights";
const UNKNOWN_TARGET: &str = "unknown target";
const SESSION_LABEL: &str = "cleanup";
const OP: &str = "cleanup";

/// Files in temporary folders must be at least this old.
const TEMP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long to wait for the Windows Update service to stop.
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const WINDOWS_UPDATE_SERVICE: &str = "wuauserv";
const DELIVERY_OPTIMIZATION_CACHE: &str =
    r"ServiceProfiles\NetworkService\AppData\Local\Microsoft\Windows\DeliveryOptimization\Cache";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Delete the eligible files of the target's folders.
    Files,
    /// Delete the downloaded updates with the Windows Update service stopped.
    UpdateCache,
    /// Run the Delivery Optimization cleanup cmdlet.
    DeliveryOptimization,
    /// Empty the Recycle Bin through the shell.
    RecycleBin,
}

/// Programs that must not run while a target is cleaned: a browser using its cache, or
/// DISM using Windows Update as its repair source.
#[derive(Debug)]
struct Blocker {
    /// Lowercase executable names; any one of them running blocks the target.
    exes: &'static [&'static str],
    name: &'static str,
    /// The reason shown while one of them runs.
    running: &'static str,
}

const DISM_BLOCKER: Blocker = Blocker {
    exes: &["dism.exe", "dismhost.exe"],
    name: "DISM",
    running: "DISM is repairing Windows; clean this after it finishes",
};

#[derive(Debug)]
struct Def {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    requires_admin: bool,
    default_on: bool,
    /// Its folders use the [`TEMP_MIN_AGE`] rule.
    keeps_recent: bool,
    action: Action,
    blocker: Option<Blocker>,
}

const DEFS: &[Def] = &[
    Def {
        id: "user_temp",
        title: "Temporary files",
        description: "Temporary files that apps left in your temp folder.",
        requires_admin: false,
        default_on: true,
        keeps_recent: true,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "windows_temp",
        title: "Windows temp folder",
        description: "Temporary files left by Windows, services and installers in the system \
                      temp folder.",
        requires_admin: true,
        default_on: true,
        keeps_recent: true,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "update_cache",
        title: "Windows Update downloads",
        description: "Update files Windows Update already downloaded; the Windows Update \
                      service is paused while they are removed and anything still needed is \
                      downloaded again.",
        requires_admin: true,
        default_on: true,
        keeps_recent: false,
        action: Action::UpdateCache,
        blocker: Some(DISM_BLOCKER),
    },
    Def {
        id: "delivery_optimization",
        title: "Delivery Optimization cache",
        description: "Update pieces Windows keeps to share with other PCs, removed with the \
                      built-in Delivery Optimization cleanup.",
        requires_admin: true,
        default_on: true,
        keeps_recent: false,
        action: Action::DeliveryOptimization,
        blocker: Some(DISM_BLOCKER),
    },
    Def {
        id: "crash_dumps",
        title: "Crash dumps",
        description: "Memory dumps written when apps or Windows crashed, only useful when \
                      sending a crash to a developer for analysis.",
        requires_admin: true,
        default_on: true,
        keeps_recent: false,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "error_reports",
        title: "Error reports",
        description: "Queued and archived Windows Error Reporting files describing past \
                      crashes and problems.",
        requires_admin: true,
        default_on: true,
        keeps_recent: false,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "thumbnail_cache",
        title: "Thumbnail cache",
        description: "Picture and video previews saved by File Explorer, which rebuilds them \
                      slowly the next time you open those folders.",
        requires_admin: false,
        default_on: false,
        keeps_recent: false,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "recycle_bin",
        title: "Recycle Bin",
        description: "Files you deleted that are still in the Recycle Bin; emptying it means \
                      they can no longer be restored.",
        requires_admin: false,
        default_on: false,
        keeps_recent: false,
        action: Action::RecycleBin,
        blocker: None,
    },
    Def {
        id: "shader_cache",
        title: "GPU shader cache",
        description: "Compiled graphics shaders saved by DirectX and the graphics driver; \
                      games rebuild them on next launch, which can cause brief stutter.",
        requires_admin: false,
        default_on: false,
        keeps_recent: false,
        action: Action::Files,
        blocker: None,
    },
    Def {
        id: "browser_chrome",
        title: "Chrome cache",
        description: "Cached web pages, scripts and graphics data from Google Chrome; \
                      logins, history and settings are kept.",
        requires_admin: false,
        default_on: true,
        keeps_recent: false,
        action: Action::Files,
        blocker: Some(Blocker {
            exes: &["chrome.exe"],
            name: "Chrome",
            running: "Chrome is running",
        }),
    },
    Def {
        id: "browser_edge",
        title: "Edge cache",
        description: "Cached web pages, scripts and graphics data from Microsoft Edge; \
                      logins, history and settings are kept.",
        requires_admin: false,
        default_on: true,
        keeps_recent: false,
        action: Action::Files,
        blocker: Some(Blocker {
            exes: &["msedge.exe"],
            name: "Edge",
            running: "Edge is running (check the system tray)",
        }),
    },
    Def {
        id: "browser_firefox",
        title: "Firefox cache",
        description: "Cached web pages and startup data from Mozilla Firefox; logins, \
                      history and settings are kept.",
        requires_admin: false,
        default_on: true,
        keeps_recent: false,
        action: Action::Files,
        blocker: Some(Blocker {
            exes: &["firefox.exe"],
            name: "Firefox",
            running: "Firefox is running",
        }),
    },
];

fn find(id: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.id == id)
}

/// Every cleanup target, in display order.
pub fn catalog() -> Vec<CleanupTarget> {
    DEFS.iter()
        .map(|d| CleanupTarget {
            id: d.id.to_string(),
            title: d.title.to_string(),
            description: d.description.to_string(),
            requires_admin: d.requires_admin,
            default_on: d.default_on,
            recent_files_kept: d.keeps_recent,
        })
        .collect()
}

// ───────────────────────────── Locations ─────────────────────────────

/// One place a target's files live.
#[derive(Debug)]
enum Spot {
    /// The contents of a folder, limited by a filter; the folder itself is kept.
    Folder(PathBuf, Filter),
    /// The process temp folder, cleaned like `Folder` only while the folder opened resolves
    /// to the standard temp folder (whose comparison key is held here) or to a numbered
    /// folder directly inside it.
    Temp(PathBuf, Filter, String),
    /// A single file.
    File(PathBuf),
    /// The location cannot be determined (its system folder is unavailable).
    Unknown(String),
    /// The location is not one this target may clean; the whole target is blocked.
    Blocked(String),
}

/// A system folder, or why it is unavailable.
type Base = std::result::Result<PathBuf, String>;

/// The system folders the targets live in.
struct Places {
    /// `GetWindowsDirectoryW`.
    windows: Base,
    /// `FOLDERID_Profile`.
    profile: Base,
    /// The profile's `AppData\Local`, from [`local_app_data`] rather than
    /// `FOLDERID_LocalAppData`.
    local: Base,
    /// `FOLDERID_ProgramData`.
    program_data: Base,
}

impl Places {
    fn current() -> Places {
        let profile = walk::known_folder(&FOLDERID_Profile);
        Places {
            windows: walk::windows_dir(),
            local: local_app_data(&profile),
            profile,
            program_data: walk::known_folder(&FOLDERID_ProgramData),
        }
    }
}

/// The user's `AppData\Local` folder, taken from the profile folder (which the user's
/// shell-folder settings cannot redirect) rather than from `FOLDERID_LocalAppData`. That
/// known folder is backed by the per-user `HKCU\…\User Shell Folders\"Local AppData"` value,
/// which an unelevated process of the same user can rewrite without elevation: it could
/// otherwise point the folder at another account's profile (or at a base whose fixed
/// per-target suffix lands on administrator-owned files) and have the elevated, unattended
/// maintenance run delete files the process itself could not. The per-user targets
/// (`crash_dumps`, `error_reports`, `thumbnail_cache`, `shader_cache`, the browser caches)
/// resolve from this base; on a machine with no redirect it is the same folder as
/// `FOLDERID_LocalAppData`.
fn local_app_data(profile: &Base) -> Base {
    match profile {
        Ok(profile) => Ok(profile.join(r"AppData\Local")),
        Err(reason) => Err(reason.clone()),
    }
}

fn folder(base: &Base, rel: &str, filter: Filter) -> Spot {
    match base {
        Ok(base) => Spot::Folder(base.join(rel), filter),
        Err(reason) => Spot::Unknown(reason.clone()),
    }
}

fn file(base: &Base, rel: &str) -> Spot {
    match base {
        Ok(base) => Spot::File(base.join(rel)),
        Err(reason) => Spot::Unknown(reason.clone()),
    }
}

fn temp_filter() -> Filter {
    Filter::everything().older_than(TEMP_MIN_AGE)
}

/// Chromium layout: per-profile caches plus the shared shader caches under `User Data`.
fn chromium(local: &Base, user_data: &str) -> Vec<Spot> {
    let base = match local {
        Ok(base) => base.join(user_data),
        Err(reason) => return vec![Spot::Unknown(reason.clone())],
    };
    let mut spots = Vec::new();
    for profile in walk::plain_subfolders(&base, |n| n == "Default" || n.starts_with("Profile ")) {
        for rel in [r"Cache\Cache_Data", "Code Cache", "GPUCache"] {
            spots.push(Spot::Folder(profile.join(rel), Filter::everything()));
        }
    }
    for rel in ["ShaderCache", "GrShaderCache"] {
        spots.push(Spot::Folder(base.join(rel), Filter::everything()));
    }
    spots
}

fn firefox(local: &Base) -> Vec<Spot> {
    let profiles = match local {
        Ok(base) => base.join(r"Mozilla\Firefox\Profiles"),
        Err(reason) => return vec![Spot::Unknown(reason.clone())],
    };
    walk::plain_subfolders(&profiles, |_| true)
        .into_iter()
        .flat_map(|profile| {
            ["cache2", "startupCache"]
                .map(|rel| Spot::Folder(profile.join(rel), Filter::everything()))
        })
        .collect()
}

/// The process temp folder (`GetTempPath2W`: `%TMP%`, then `%TEMP%`), accepted only when it
/// is the standard `AppData\Local\Temp` folder of the user's profile. The profile folder
/// comes from the machine's profile list, which the user's shell-folder settings cannot
/// redirect the way they can redirect the Local AppData folder.
fn user_temp(profile: &Base) -> Spot {
    match profile {
        Ok(profile) => standard_temp(&std::env::temp_dir(), &profile.join(r"AppData\Local\Temp")),
        Err(reason) => Spot::Unknown(reason.clone()),
    }
}

/// `actual` as a temp cleanup folder when it resolves to `standard` or to a numbered
/// per-session folder directly inside it (Remote Desktop sessions use `Temp\<n>`);
/// otherwise the target is blocked with the folder `actual` resolves to.
///
/// Only the comparison uses resolved paths. The folder is handed on as `actual` spells it,
/// so root validation still refuses it when that folder is itself a link or mount point,
/// and [`resolve_folder`] repeats the comparison on the folder it opens. A path that is not
/// on a fixed local disk is blocked from its spelling alone, without being opened, and a
/// `standard` folder that is a link blocks the target: the folder it points to is not the
/// standard one, whichever spelling names it.
fn standard_temp(actual: &Path, standard: &Path) -> Spot {
    let not_standard = |shown: &Path| {
        Spot::Blocked(format!(
            "the temp folder is set to {}, not the standard {}, so it is not cleaned",
            walk::display(shown),
            walk::display(standard)
        ))
    };
    if !walk::on_local_disk(actual) || !walk::on_local_disk(standard) {
        return not_standard(actual);
    }
    if walk::is_link(standard) {
        return Spot::Blocked(format!(
            "the standard temp folder {} is a link or mount point, so it is not cleaned",
            walk::display(standard)
        ));
    }
    let resolve = |p: &Path| walk::canonical_local(p).unwrap_or_else(|| p.to_path_buf());
    let resolved = resolve(actual);
    let expected = walk::key(&resolve(standard));
    if is_standard_temp(&resolved, &expected) {
        Spot::Temp(actual.to_path_buf(), temp_filter(), expected)
    } else {
        not_standard(&resolved)
    }
}

/// True when `resolved` is the folder whose comparison key is `standard`, or a numbered
/// per-session folder directly inside it.
fn is_standard_temp(resolved: &Path, standard: &str) -> bool {
    let per_session = resolved.parent().is_some_and(|p| walk::key(p) == standard)
        && resolved.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
        });
    walk::key(resolved) == standard || per_session
}

/// Validates the folder of a `Folder` or `Temp` location. A temp folder must also resolve,
/// as opened, to the standard temp folder or a numbered folder inside it: a link above the
/// configured path may point elsewhere by the time the folder is opened, and the opened
/// folder's final path is the one the walk then uses.
fn resolve_folder(spot: &Spot, guards: &Guards) -> Resolved<SafeRoot> {
    match spot {
        Spot::Folder(path, _) => walk::resolve_root(path, guards),
        Spot::Temp(path, _, standard) => match walk::resolve_root(path, guards) {
            Resolved::Ready(root) if !is_standard_temp(root.path(), standard) => {
                Resolved::Refused(format!(
                    "{} resolves to {}, not the standard temp folder",
                    walk::display(path),
                    walk::display(root.path())
                ))
            }
            other => other,
        },
        Spot::File(_) | Spot::Unknown(_) | Spot::Blocked(_) => {
            Resolved::Refused(format!("{spot:?} is not a folder location"))
        }
    }
}

/// Where the target's files live, resolved from the shell's known folders and the Windows
/// folder.
fn spots(id: &str) -> Vec<Spot> {
    let places = Places::current();
    let (windows, local) = (&places.windows, &places.local);
    let everything = Filter::everything;
    match id {
        "user_temp" => vec![user_temp(&places.profile)],
        "windows_temp" => vec![folder(windows, "Temp", temp_filter())],
        "update_cache" => vec![folder(
            windows,
            r"SoftwareDistribution\Download",
            everything(),
        )],
        "delivery_optimization" => vec![folder(windows, DELIVERY_OPTIMIZATION_CACHE, everything())],
        "crash_dumps" => {
            let dumps = || everything().named("", ".dmp");
            vec![
                folder(local, "CrashDumps", dumps()),
                folder(windows, "Minidump", dumps()),
                file(windows, "MEMORY.DMP"),
                folder(windows, "LiveKernelReports", dumps()),
            ]
        }
        "error_reports" => {
            let mut spots = Vec::new();
            for base in [&places.program_data, local] {
                for queue in ["ReportArchive", "ReportQueue"] {
                    let rel = format!(r"Microsoft\Windows\WER\{queue}");
                    spots.push(folder(base, &rel, everything()));
                }
            }
            spots
        }
        "thumbnail_cache" => vec![folder(
            local,
            r"Microsoft\Windows\Explorer",
            everything().named("thumbcache_", ".db").top_level(),
        )],
        "shader_cache" => [
            "D3DSCache",
            r"NVIDIA\DXCache",
            r"NVIDIA\GLCache",
            r"AMD\DxCache",
            r"AMD\GLCache",
        ]
        .into_iter()
        .map(|rel| folder(local, rel, everything()))
        .collect(),
        "browser_chrome" => chromium(local, r"Google\Chrome\User Data"),
        "browser_edge" => chromium(local, r"Microsoft\Edge\User Data"),
        "browser_firefox" => firefox(local),
        _ => Vec::new(),
    }
}

/// The reason of the first blocked location, if any.
fn blocked_spot(spots: &[Spot]) -> Option<String> {
    spots.iter().find_map(|spot| match spot {
        Spot::Blocked(reason) => Some(reason.clone()),
        _ => None,
    })
}

/// Why a target with a blocker cannot run now, if it cannot: one of the blocking programs
/// runs, or the process list could not be read (the target is skipped then as well).
fn process_block(blocker: &Blocker, processes: Option<&HashSet<String>>) -> Option<String> {
    match processes {
        None => Some(format!(
            "could not check whether {} is running",
            blocker.name
        )),
        Some(names) if blocker.exes.iter().any(|exe| names.contains(*exe)) => {
            Some(blocker.running.to_string())
        }
        Some(_) => None,
    }
}

/// Why this process cannot clean a target right now, before its files are looked at:
/// missing administrator rights, or one of its blocking programs running (a browser using
/// its cache, DISM using Windows Update), including when that cannot be checked. The scan
/// and the clean use this same rule.
fn block_reason(def: &Def, processes: Option<&HashSet<String>>, elevated: bool) -> Option<String> {
    if def.requires_admin && !elevated {
        return Some(NEEDS_ADMIN.to_string());
    }
    def.blocker
        .as_ref()
        .and_then(|b| process_block(b, processes))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ───────────────────────────── Scan ─────────────────────────────

/// Read-only: measures every target. Never deletes anything.
pub fn scan() -> Result<CleanupScan> {
    let started = Instant::now();
    let elevated = crate::is_elevated();
    let processes = system::running_process_names();
    let guards = Guards::current();

    let targets = std::thread::scope(|s| {
        let workers: Vec<_> = DEFS
            .iter()
            .map(|def| {
                let (guards, processes) = (&guards, processes.as_ref());
                s.spawn(move || scan_target(def, guards, processes, elevated))
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .map_err(|_| Error::Other("cleanup scan worker panicked".into()))
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let total_bytes = targets.iter().map(|t| t.bytes).sum();
    Ok(CleanupScan {
        targets,
        total_bytes,
        duration_ms: elapsed_ms(started),
    })
}

/// What measuring a target's locations found.
#[derive(Debug, Default)]
struct Survey {
    measure: Measure,
    /// Display paths of the locations that passed validation.
    paths: Vec<String>,
    /// The first safety refusal, if any location was refused.
    refused: Option<String>,
}

impl Survey {
    /// The refusal, when no location could be used at all.
    fn all_refused(&self) -> Option<String> {
        self.refused.clone().filter(|_| self.paths.is_empty())
    }
}

fn survey(spots: &[Spot], guards: &Guards) -> Survey {
    let mut out = Survey::default();
    for spot in spots {
        let refused = match spot {
            Spot::Folder(_, filter) | Spot::Temp(_, filter, _) => {
                match resolve_folder(spot, guards) {
                    Resolved::Ready(root) => {
                        out.measure.add(walk::measure(&root, filter));
                        out.paths.push(walk::display(root.path()));
                        None
                    }
                    Resolved::Missing => None,
                    Resolved::Denied => {
                        out.measure.denied = true;
                        None
                    }
                    Resolved::Refused(reason) => Some(reason),
                }
            }
            Spot::File(path) => match walk::resolve_file(path) {
                Resolved::Ready(file) => {
                    out.measure.add(Measure {
                        bytes: file.size(),
                        files: 1,
                        denied: false,
                    });
                    out.paths.push(walk::display(file.path()));
                    None
                }
                Resolved::Missing => None,
                Resolved::Denied => {
                    out.measure.denied = true;
                    None
                }
                Resolved::Refused(reason) => Some(reason),
            },
            Spot::Unknown(reason) => {
                tracing::warn!(%reason, "cleanup location unknown");
                None
            }
            Spot::Blocked(_) => None,
        };
        if let Some(reason) = refused {
            tracing::warn!(%reason, "cleanup location refused");
            out.refused.get_or_insert(reason);
        }
    }
    out
}

fn scan_target(
    def: &Def,
    guards: &Guards,
    processes: Option<&HashSet<String>>,
    elevated: bool,
) -> TargetScan {
    let (found, blocked_location) = match def.action {
        Action::RecycleBin => {
            let measure = match system::query_recycle_bin() {
                Ok(bin) => Measure {
                    bytes: bin.bytes,
                    files: bin.items,
                    denied: false,
                },
                Err(e) => {
                    tracing::warn!(error = %e, "Recycle Bin query failed");
                    Measure::default()
                }
            };
            (
                Survey {
                    measure,
                    ..Survey::default()
                },
                None,
            )
        }
        _ => {
            let spots = spots(def.id);
            (survey(&spots, guards), blocked_spot(&spots))
        }
    };
    let blocked_reason = block_reason(def, processes, elevated)
        .or(blocked_location)
        .or_else(|| found.all_refused());
    TargetScan {
        id: def.id.to_string(),
        title: def.title.to_string(),
        description: def.description.to_string(),
        requires_admin: def.requires_admin,
        default_on: def.default_on,
        bytes: found.measure.bytes,
        files: found.measure.files,
        blocked_reason,
        recent_files_kept: def.keeps_recent,
        paths: found.paths,
    }
}

// ───────────────────────────── Clean ─────────────────────────────

/// What the targets of one cleanup run share.
struct Ctx<'a> {
    guards: &'a Guards,
    journal: &'a Journal,
    session: i64,
}

/// Deletes the eligible files of the given targets and records the run in `journal`'s
/// audit log. Targets that are blocked, unknown or need elevation the process lacks are
/// reported as skipped rather than failing the whole run.
///
/// The run opens one journal session labelled `cleanup` and logs one entry per target
/// (`cleaned`, `skipped` or `failed`). Deleted files are not journaled for rollback: they
/// cannot be restored. Repeated ids are cleaned once. Only opening the session can fail the
/// call; once targets run, a failed audit entry is reported on that target's result (and
/// the remaining targets still run), and a failure to close the session is logged.
pub fn clean(journal: &Journal, ids: &[String]) -> Result<CleanupReport> {
    let started = Instant::now();
    let session = journal.begin_session(SESSION_LABEL, crate::VERSION)?;
    let guards = Guards::current();
    let ctx = Ctx {
        guards: &guards,
        journal,
        session,
    };
    let results = run_targets(&ctx, ids);
    if let Err(e) = journal.end_session(session) {
        tracing::error!(error = %e, "cannot close the cleanup session in the journal");
    }
    let freed_bytes = results.iter().map(|r| r.freed_bytes).sum();
    Ok(CleanupReport {
        results,
        freed_bytes,
        duration_ms: elapsed_ms(started),
    })
}

fn run_targets(ctx: &Ctx<'_>, ids: &[String]) -> Vec<TargetResult> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let mut run = run_target(id, ctx);
        tracing::info!(target_id = %id, outcome = run.outcome(), detail = %run.detail(), "cleanup");
        let logged = ctx.journal.log_op(
            Some(ctx.session),
            OP,
            id,
            run.outcome(),
            Some(&run.detail()),
        );
        if let Err(e) = logged {
            tracing::error!(target_id = %id, error = %e, "cannot write the cleanup result to the audit log");
            run.note_first(format!("not recorded in the audit log: {e}"));
        }
        results.push(run.result);
    }
    results
}

/// Result of one target, the empty folders it removed, how many errors occurred and whether
/// a fatal error stopped it.
#[derive(Debug)]
struct Run {
    result: TargetResult,
    removed_dirs: u64,
    /// Every error, including those beyond the messages kept in `result.errors`.
    error_count: u64,
    failed: bool,
}

impl Run {
    fn new(id: &str) -> Run {
        Run {
            result: TargetResult {
                id: id.to_string(),
                freed_bytes: 0,
                deleted_files: 0,
                skipped_files: 0,
                skipped_reason: None,
                errors: Vec::new(),
            },
            removed_dirs: 0,
            error_count: 0,
            failed: false,
        }
    }

    fn skipped(id: &str, reason: impl Into<String>) -> Run {
        let mut run = Run::new(id);
        run.result.skipped_reason = Some(reason.into());
        run
    }

    /// Stores a message while fewer than [`MAX_ERRORS`] are kept; does not count it.
    fn keep(&mut self, message: String) {
        if self.result.errors.len() < MAX_ERRORS {
            self.result.errors.push(message);
        }
    }

    fn note(&mut self, message: impl Into<String>) {
        self.error_count += 1;
        self.keep(message.into());
    }

    /// Records an error that must not be lost to the message cap: it is stored first.
    fn note_first(&mut self, message: impl Into<String>) {
        self.error_count += 1;
        self.result.errors.insert(0, message.into());
        self.result.errors.truncate(MAX_ERRORS);
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.failed = true;
        self.note(message);
    }

    fn absorb(&mut self, purge: Purge) {
        self.result.freed_bytes += purge.freed_bytes;
        self.result.deleted_files += purge.deleted_files;
        self.result.skipped_files += purge.skipped_files;
        self.removed_dirs += purge.removed_dirs;
        self.error_count += purge.error_count.max(purge.errors.len() as u64);
        for e in purge.errors {
            self.keep(e);
        }
    }

    fn outcome(&self) -> &'static str {
        if self.result.skipped_reason.is_some() {
            "skipped"
        } else if self.failed {
            "failed"
        } else {
            "cleaned"
        }
    }

    fn detail(&self) -> String {
        let r = &self.result;
        if let Some(reason) = &r.skipped_reason {
            return reason.clone();
        }
        let mut detail = format!(
            "freed {} bytes, deleted {} files, skipped {} files",
            r.freed_bytes, r.deleted_files, r.skipped_files
        );
        if self.removed_dirs > 0 {
            detail.push_str(&format!(", removed {} empty folders", self.removed_dirs));
        }
        if let Some(first) = r.errors.first() {
            detail.push_str(&format!("; {} error(s), first: {first}", self.error_count));
        }
        detail
    }
}

fn run_target(id: &str, ctx: &Ctx<'_>) -> Run {
    let Some(def) = find(id) else {
        return Run::skipped(id, UNKNOWN_TARGET);
    };
    let processes = def
        .blocker
        .as_ref()
        .and_then(|_| system::running_process_names());
    if let Some(reason) = block_reason(def, processes.as_ref(), crate::is_elevated()) {
        return Run::skipped(id, reason);
    }
    if def.action == Action::RecycleBin {
        return clean_recycle_bin(id);
    }
    let spots = spots(id);
    if let Some(reason) = blocked_spot(&spots) {
        return Run::skipped(id, reason);
    }
    match def.action {
        Action::Files => clean_files(id, &spots, ctx.guards),
        Action::UpdateCache => clean_update_cache(id, &spots, ctx),
        Action::DeliveryOptimization => clean_delivery_optimization(id, &spots, ctx.guards),
        Action::RecycleBin => clean_recycle_bin(id),
    }
}

fn clean_files(id: &str, spots: &[Spot], guards: &Guards) -> Run {
    let mut run = Run::new(id);
    let (mut processed, mut blocked) = (0usize, 0usize);
    for spot in spots {
        let purge = match spot {
            Spot::Folder(path, filter) | Spot::Temp(path, filter, _) => {
                match resolve_folder(spot, guards) {
                    Resolved::Ready(root) => walk::purge(&root, filter, guards),
                    Resolved::Missing => continue,
                    Resolved::Denied => {
                        blocked += 1;
                        run.note(format!(
                            "cannot open {}: access denied",
                            walk::display(path)
                        ));
                        continue;
                    }
                    Resolved::Refused(reason) => {
                        blocked += 1;
                        run.note(format!("refused: {reason}"));
                        continue;
                    }
                }
            }
            Spot::File(path) => match walk::resolve_file(path) {
                Resolved::Ready(file) => walk::purge_file(&file, guards),
                Resolved::Missing => continue,
                Resolved::Denied => {
                    blocked += 1;
                    run.note(format!(
                        "cannot open {}: access denied",
                        walk::display(path)
                    ));
                    continue;
                }
                Resolved::Refused(reason) => {
                    blocked += 1;
                    run.note(format!("refused: {reason}"));
                    continue;
                }
            },
            Spot::Unknown(reason) | Spot::Blocked(reason) => {
                blocked += 1;
                run.note(reason.clone());
                continue;
            }
        };
        if purge.denied {
            blocked += 1;
        } else {
            processed += 1;
        }
        run.absorb(purge);
    }
    if processed == 0 && blocked > 0 {
        run.failed = true;
    }
    run
}

/// The validated folder of a one-location target. `None` when it does not exist or cannot
/// be used; the latter is recorded on `run` as a failure.
fn single_root(spots: &[Spot], guards: &Guards, run: &mut Run) -> Option<(SafeRoot, Filter)> {
    match spots.first() {
        Some(spot @ (Spot::Folder(path, filter) | Spot::Temp(path, filter, _))) => {
            match resolve_folder(spot, guards) {
                Resolved::Ready(root) => Some((root, filter.clone())),
                Resolved::Missing => None,
                Resolved::Denied => {
                    run.fail(format!(
                        "cannot open {}: access denied",
                        walk::display(path)
                    ));
                    None
                }
                Resolved::Refused(reason) => {
                    run.fail(format!("refused: {reason}"));
                    None
                }
            }
        }
        Some(Spot::Unknown(reason) | Spot::Blocked(reason)) => {
            run.fail(reason.clone());
            None
        }
        Some(Spot::File(_)) | None => None,
    }
}

/// The service operations a restart needs. Implemented by [`Service`]; tests substitute a
/// scripted service so the restart logic never touches a real one.
trait ServiceControl {
    fn name(&self) -> &str;
    fn state(&self) -> Result<ServiceState>;
    /// Requests a start. Like [`Service::start`], "already running" may be reported as
    /// success even while a stop is still pending.
    fn start(&self) -> Result<()>;
}

impl ServiceControl for Service {
    fn name(&self) -> &str {
        Service::name(self)
    }

    fn state(&self) -> Result<ServiceState> {
        Ok(self.status()?.state)
    }

    fn start(&self) -> Result<()> {
        Service::start(self)
    }
}

/// Timing of [`start_again`].
#[derive(Debug, Clone, Copy)]
struct RestartTiming {
    /// How long a pending stop may take before the restart is given up.
    wait: Duration,
    /// Pause between state checks.
    poll: Duration,
    /// Pause after a start request that failed.
    retry: Duration,
}

const RESTART_TIMING: RestartTiming = RestartTiming {
    wait: Duration::from_secs(60),
    poll: Duration::from_millis(250),
    retry: Duration::from_secs(1),
};

/// Most start requests sent while restarting a service.
const START_ATTEMPTS: u32 = 5;

/// Starts a service this cleanup stopped. A stop that is still pending is waited out first:
/// the service manager only starts a stopped service, and a start requested during the stop
/// can come back as "already running" although the service then stays stopped. Success is
/// judged only from the state the service reports afterwards (running, starting or paused),
/// never from the start request. Fails after `timing.wait` or [`START_ATTEMPTS`] start
/// requests, with the service's last state or start error.
fn start_again(service: &dyn ServiceControl, timing: RestartTiming) -> Result<()> {
    let deadline = Instant::now() + timing.wait;
    let mut attempts = 0u32;
    let mut last_error: Option<Error> = None;
    loop {
        let state = service.state()?;
        if state.is_active() {
            return Ok(());
        }
        if Instant::now() >= deadline || attempts >= START_ATTEMPTS {
            return Err(match (state, last_error) {
                (ServiceState::StopPending, _) => Error::Other(format!(
                    "{} was still stopping after {} s and was not started again",
                    service.name(),
                    timing.wait.as_secs()
                )),
                (_, Some(e)) => e,
                (_, None) => Error::Other(format!("{} did not start", service.name())),
            });
        }
        let pause = if state == ServiceState::Stopped {
            attempts += 1;
            match service.start() {
                Ok(()) => timing.poll,
                Err(e) => {
                    last_error = Some(e);
                    timing.retry
                }
            }
        } else {
            timing.poll
        };
        std::thread::sleep(pause);
    }
}

/// Starts the service it holds when dropped, unless [`RestartGuard::finish`] ran first, so
/// the service comes back even if the cleanup in between panics.
struct RestartGuard<'a> {
    service: Option<&'a Service>,
}

impl RestartGuard<'_> {
    /// Starts the held service again. `Ok(true)` when one was held and runs again.
    fn finish(mut self) -> Result<bool> {
        match self.service.take() {
            Some(service) => start_again(service, RESTART_TIMING).map(|()| true),
            None => Ok(false),
        }
    }
}

impl Drop for RestartGuard<'_> {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            if let Err(e) = start_again(service, RESTART_TIMING) {
                tracing::error!(error = %e, "could not restart {}", service.name());
            }
        }
    }
}

/// Journals a running service before this cleanup stops it, so an interrupted run can be
/// undone from History. Returns whether a new record was written; an active record from an
/// earlier change already holds the state to return to and is left as it is.
fn journal_service(ctx: &Ctx<'_>, service: &Service) -> Result<bool> {
    let config = service.config()?;
    ctx.journal.record_service(
        ctx.session,
        &NewServiceRecord {
            name: config.name,
            display_name: config.display_name,
            start_type: config.start_type,
            delayed_auto_start: config.delayed_auto_start,
            was_running: true,
        },
    )
}

/// Closes this session's journal record of a service once it runs again, so History no
/// longer lists it as a pending change.
fn close_service_record(journal: &Journal, session: i64, name: &str) -> Result<()> {
    for rec in journal.active_services()? {
        if rec.session_id == session && rec.name.eq_ignore_ascii_case(name) {
            journal.mark_reverted(JournalTable::Service, rec.id)?;
        }
    }
    Ok(())
}

/// Deletes `SoftwareDistribution\Download`. A running Windows Update service is journaled
/// and stopped first and started again afterwards, whatever happened in between (waiting
/// out a stop that is still pending). Its journal record is closed once it runs again; if
/// it cannot be started, the target fails with that error first and the record stays
/// active.
fn clean_update_cache(id: &str, spots: &[Spot], ctx: &Ctx<'_>) -> Run {
    let mut run = Run::new(id);
    let Some((root, filter)) = single_root(spots, ctx.guards, &mut run) else {
        return run;
    };
    let before = walk::measure(&root, &filter);
    if before.files == 0 {
        return run;
    }

    let scm = match Scm::connect() {
        Ok(scm) => scm,
        Err(e) => {
            run.fail(format!("cannot connect to the service manager: {e}"));
            return run;
        }
    };
    let access = scm::READ_ACCESS | SERVICE_START | SERVICE_STOP;
    let service = match scm.open(WINDOWS_UPDATE_SERVICE, access) {
        Ok(service) => service,
        Err(e) => {
            run.fail(format!("cannot open the Windows Update service: {e}"));
            return run;
        }
    };
    let was_running = match service.as_ref().map(Service::status).transpose() {
        Ok(status) => status.is_some_and(|s| s.state.is_active()),
        Err(e) => {
            run.fail(format!("cannot query the Windows Update service: {e}"));
            return run;
        }
    };

    let running = service.as_ref().filter(|_| was_running);
    let mut journaled = false;
    if let Some(service) = running {
        match journal_service(ctx, service) {
            Ok(inserted) => journaled = inserted,
            Err(e) => {
                run.fail(format!(
                    "the Windows Update service could not be recorded in the journal, so it \
                     was not stopped: {e}"
                ));
                return run;
            }
        }
    }
    let restart = RestartGuard { service: running };
    let stopped = match running {
        None => true,
        Some(service) => match service.stop(SERVICE_STOP_TIMEOUT) {
            Ok(ServiceState::Stopped) => true,
            Ok(state) => {
                run.fail(format!(
                    "the Windows Update service did not stop in time (state: {state:?})"
                ));
                false
            }
            Err(e) => {
                run.fail(format!("cannot stop the Windows Update service: {e}"));
                false
            }
        },
    };
    if stopped {
        run.absorb(walk::purge(&root, &filter, ctx.guards));
    }
    match restart.finish() {
        Ok(true) if journaled => {
            if let Err(e) = close_service_record(ctx.journal, ctx.session, WINDOWS_UPDATE_SERVICE) {
                tracing::warn!(error = %e, "cannot close the Windows Update service journal record");
            }
        }
        Ok(_) => {}
        Err(e) => {
            run.failed = true;
            run.note_first(format!(
                "the Windows Update service could not be started again ({e}); restart the PC \
                 or undo its entry in History to start it"
            ));
        }
    }
    run
}

/// The script that clears the Delivery Optimization cache: the module imported from
/// System32 and its cmdlet called module-qualified.
fn delivery_optimization_script() -> Result<String> {
    Ok(format!(
        "{}DeliveryOptimization\\Delete-DeliveryOptimizationCache -Force",
        crate::win::powershell::import_system_module("DeliveryOptimization")?
    ))
}

/// Runs `Delete-DeliveryOptimizationCache` and reports the difference it made to the cache
/// folder.
fn clean_delivery_optimization(id: &str, spots: &[Spot], guards: &Guards) -> Run {
    let mut run = Run::new(id);
    let Some((root, filter)) = single_root(spots, guards, &mut run) else {
        return run;
    };
    let before = walk::measure(&root, &filter);
    if before.denied {
        run.fail(format!(
            "cannot open {}: access denied",
            walk::display(root.path())
        ));
        return run;
    }
    if before.files == 0 {
        return run;
    }
    let cleared = delivery_optimization_script().and_then(|s| crate::win::powershell::run(&s));
    if let Err(e) = cleared {
        let text = e.to_string();
        let short: String = text.chars().take(300).collect();
        run.fail(format!("Delete-DeliveryOptimizationCache failed: {short}"));
        return run;
    }
    // The cmdlet may remove the cache folder itself; a missing folder measures as empty.
    let after = match walk::resolve_root(root.path(), guards) {
        Resolved::Ready(root) => walk::measure(&root, &filter),
        _ => Measure::default(),
    };
    run.result.freed_bytes = before.bytes.saturating_sub(after.bytes);
    run.result.deleted_files = before.files.saturating_sub(after.files);
    run.result.skipped_files = after.files;
    run
}

/// Empties the Recycle Bin on every drive and reports the difference it made.
fn clean_recycle_bin(id: &str) -> Run {
    let mut run = Run::new(id);
    let before = match system::query_recycle_bin() {
        Ok(bin) => bin,
        Err(e) => {
            run.fail(format!("cannot read the Recycle Bin: {e}"));
            return run;
        }
    };
    if before.items == 0 && before.bytes == 0 {
        return run;
    }
    let emptied = system::empty_recycle_bin();
    let after = match system::query_recycle_bin() {
        Ok(bin) => bin,
        Err(_) if emptied.is_ok() => system::RecycleBin::default(),
        Err(_) => before,
    };
    run.result.freed_bytes = before.bytes.saturating_sub(after.bytes);
    run.result.deleted_files = before.items.saturating_sub(after.items);
    run.result.skipped_files = after.items;
    if let Err(e) = emptied {
        let message = format!("emptying the Recycle Bin failed: {e}");
        if run.result.deleted_files == 0 {
            run.fail(message);
        } else {
            run.note(message);
        }
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    use windows::Win32::UI::Shell::FOLDERID_LocalAppData;

    use crate::win::scm::StartType;

    fn temp_journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().expect("temp dir");
        let journal = Journal::open(dir.path().join("journal.db")).expect("open journal");
        (dir, journal)
    }

    #[test]
    fn cleanup_catalog_ids_are_unique_and_every_target_has_locations() {
        let ids: Vec<&str> = DEFS.iter().map(|d| d.id).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
        for def in DEFS {
            assert!(
                !def.description.contains("  "),
                "{} description spacing",
                def.id
            );
            if def.action != Action::RecycleBin && !def.id.starts_with("browser_") {
                assert!(!spots(def.id).is_empty(), "{} has no locations", def.id);
            }
        }
    }

    #[test]
    fn cleanup_recent_files_flag_matches_the_filters() {
        for def in DEFS {
            for spot in spots(def.id) {
                if let Spot::Folder(path, filter) | Spot::Temp(path, filter, _) = spot {
                    assert_eq!(
                        filter.keeps_recent(),
                        def.keeps_recent,
                        "{}: {}",
                        def.id,
                        path.display()
                    );
                }
            }
        }
        let flagged: Vec<&str> = DEFS
            .iter()
            .filter(|d| d.keeps_recent)
            .map(|d| d.id)
            .collect();
        assert_eq!(flagged, ["user_temp", "windows_temp"]);
    }

    #[test]
    fn cleanup_locations_come_from_system_folders() {
        let windows = walk::windows_dir().unwrap();
        let local = walk::known_folder(&FOLDERID_LocalAppData).unwrap();
        match &spots("windows_temp")[..] {
            [Spot::Folder(path, _)] => assert_eq!(path, &windows.join("Temp")),
            other => panic!("windows_temp: {other:?}"),
        }
        match &spots("thumbnail_cache")[..] {
            [Spot::Folder(path, _)] => assert!(path.starts_with(&local)),
            other => panic!("thumbnail_cache: {other:?}"),
        }
        // The temp folder is handed on as configured, not in resolved form.
        match &spots("user_temp")[..] {
            [Spot::Temp(path, _, _)] => assert_eq!(path, &std::env::temp_dir()),
            [Spot::Blocked(_)] => {}
            other => panic!("user_temp: {other:?}"),
        }
    }

    #[test]
    fn cleanup_local_base_is_the_profile_not_the_redirectable_known_folder() {
        // The per-user base is derived from the profile folder, which the user's shell-folder
        // settings cannot redirect, not from FOLDERID_LocalAppData, which an unelevated
        // process of the same user can repoint through HKCU User Shell Folders. Pointing it
        // at another account's profile would otherwise let the elevated, unattended
        // maintenance run delete files the process itself could not. The base is a pure
        // function of the profile argument, so a base built from this stand-in profile is
        // under it, never under the running account's real (known-folder) Local AppData.
        let profile = PathBuf::from(r"C:\Users\Test");
        assert_eq!(
            local_app_data(&Ok(profile.clone())),
            Ok(profile.join(r"AppData\Local"))
        );
        // An unavailable profile is carried through, so the per-user targets report it
        // rather than falling back to the redirectable known folder.
        assert_eq!(
            local_app_data(&Err("profile unavailable".into())),
            Err("profile unavailable".into())
        );
        // Places anchors `local` on the profile, never on FOLDERID_LocalAppData; on a machine
        // with no redirect that is the same folder, so the per-user targets still resolve.
        let places = Places::current();
        if let (Ok(local), Ok(profile)) = (&places.local, &places.profile) {
            assert_eq!(local, &profile.join(r"AppData\Local"));
        }
    }

    fn standard_temp_path(spot: &Spot) -> Option<&Path> {
        match spot {
            Spot::Temp(path, filter, _) => {
                assert!(filter.keeps_recent());
                Some(path.as_path())
            }
            _ => None,
        }
    }

    #[test]
    fn cleanup_user_temp_accepts_only_the_standard_folder() {
        let dir = tempfile::tempdir().unwrap();
        let standard = dir.path().join(r"Local\Temp");
        let session = standard.join("2");
        let named = standard.join("build");
        let elsewhere = dir.path().join("Documents");
        for d in [&session, &named, &elsewhere] {
            fs::create_dir_all(d).unwrap();
        }

        let trailing = PathBuf::from(format!("{}\\", standard.display()).to_uppercase());
        for accepted in [&standard, &trailing, &session] {
            assert_eq!(
                standard_temp_path(&standard_temp(accepted, &standard)),
                Some(accepted.as_path()),
                "{} is not handed on as configured",
                accepted.display()
            );
        }
        assert!(matches!(standard_temp(&named, &standard), Spot::Blocked(_)));
        match standard_temp(&elsewhere, &standard) {
            Spot::Blocked(reason) => {
                let shown = walk::display(&fs::canonicalize(&elsewhere).unwrap());
                assert!(reason.contains(&shown), "{reason}");
            }
            other => panic!("non-standard temp folder accepted: {other:?}"),
        }
        // Shares and device paths are blocked from their spelling, never opened.
        for remote in [
            r"\\192.0.2.1\home\Temp",
            r"\\?\UNC\192.0.2.1\home\Temp",
            r"\\.\C:\Temp",
        ] {
            assert!(
                matches!(
                    standard_temp(Path::new(remote), &standard),
                    Spot::Blocked(_)
                ),
                "{remote}"
            );
        }
    }

    #[test]
    fn cleanup_user_temp_pointing_elsewhere_is_blocked() {
        // A TMP value naming any other folder (here a sandbox that merely lies inside the
        // real temp folder) blocks the target instead of cleaning that folder.
        let dir = tempfile::tempdir().unwrap();
        let profile = walk::known_folder(&FOLDERID_Profile).unwrap();
        let standard = profile.join(r"AppData\Local\Temp");
        let spot = standard_temp(dir.path(), &standard);
        assert!(matches!(&spot, Spot::Blocked(reason) if reason.contains("not the standard")));
        assert!(
            blocked_spot(&[spot]).is_some(),
            "a blocked location blocks the target"
        );
        let documents = walk::known_folder(&windows::Win32::UI::Shell::FOLDERID_Documents).unwrap();
        assert!(matches!(
            standard_temp(&documents, &standard),
            Spot::Blocked(_)
        ));
    }

    /// How a refusal or block names a folder that is a link.
    const LINK: &str = "is a link or mount point";

    /// The spot `standard_temp` gives for `actual` must be blocked or refused as a root, and
    /// cleaning it must leave `canary` in place.
    fn assert_temp_not_cleaned(actual: &Path, standard: &Path, guards: &Guards, canary: &Path) {
        let spot = standard_temp(actual, standard);
        if let Some(path) = standard_temp_path(&spot) {
            let resolved = walk::resolve_root(path, guards);
            assert!(
                matches!(&resolved, Resolved::Refused(reason) if reason.contains(LINK)),
                "{} accepted as the temp folder: {resolved:?}",
                actual.display()
            );
        }
        let run = clean_files("user_temp", std::slice::from_ref(&spot), guards);
        assert!(run.failed, "{}: {:?}", actual.display(), run.result);
        assert_eq!(run.result.deleted_files, 0, "{}", actual.display());
        assert!(canary.exists(), "{} was cleaned", actual.display());
    }

    #[test]
    fn cleanup_user_temp_behind_a_junction_is_never_cleaned() {
        const OLD: Duration = Duration::from_secs(3 * 24 * 60 * 60);
        let dir = tempfile::tempdir().unwrap();
        let guards = Guards::current();
        let photos = dir.path().join("Photos");
        let canary = photos.join("holiday.jpg");
        fs::create_dir_all(&photos).unwrap();
        fs::write(&canary, b"keep").unwrap();
        walk::testing::age(&canary, OLD);

        // The standard folder itself is a junction standing in for the temp folder.
        let linked = dir.path().join(r"Linked\AppData\Local\Temp");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        walk::testing::junction(&linked, &photos);
        for actual in [&linked, &photos] {
            let spot = standard_temp(actual, &linked);
            assert!(
                matches!(&spot, Spot::Blocked(reason) if reason.contains(LINK)),
                "{}: {spot:?}",
                actual.display()
            );
            assert_temp_not_cleaned(actual, &linked, &guards, &canary);
        }

        // An ordinary standard folder, reached through links.
        let standard = dir.path().join(r"Plain\AppData\Local\Temp");
        let other_session = standard.join("7");
        fs::create_dir_all(&other_session).unwrap();
        let session_link = standard.join("2");
        walk::testing::junction(&session_link, &other_session);
        let session_away = standard.join("3");
        walk::testing::junction(&session_away, &photos);
        let temp_link = dir.path().join("TempLink");
        walk::testing::junction(&temp_link, &standard);
        let kept = other_session.join("old.tmp");
        fs::write(&kept, b"x").unwrap();
        walk::testing::age(&kept, OLD);
        for actual in [&session_link, &session_away, &temp_link] {
            assert_temp_not_cleaned(actual, &standard, &guards, &canary);
        }
        assert!(
            kept.exists(),
            "a per-session folder was cleaned through a link"
        );

        // The same layout without links is cleaned, so the refusals above are not an
        // artefact of the sandbox or the age rule.
        let stale = standard.join("stale.tmp");
        fs::write(&stale, b"x").unwrap();
        walk::testing::age(&stale, OLD);
        let spot = standard_temp(&standard, &standard);
        let run = clean_files("user_temp", std::slice::from_ref(&spot), &guards);
        assert!(!run.failed, "{:?}", run.result);
        assert!(!stale.exists(), "the plain standard folder was not cleaned");
        assert!(kept.exists() && canary.exists());

        for link in [&linked, &session_link, &session_away, &temp_link] {
            fs::remove_dir(link).unwrap();
        }
    }

    #[test]
    fn cleanup_user_temp_is_checked_again_as_opened() {
        const OLD: Duration = Duration::from_secs(3 * 24 * 60 * 60);
        let dir = tempfile::tempdir().unwrap();
        let guards = Guards::current();
        let local = dir.path().join(r"Profile\AppData\Local");
        let standard = local.join("Temp");
        let office = dir.path().join("Office16");
        for d in [
            standard.join("1033"),
            office.join("1033"),
            office.join("Temp"),
        ] {
            fs::create_dir_all(d).unwrap();
        }

        // A link above the configured folder names the standard folder while the location
        // is checked and another folder by the time it is opened.
        let above = dir.path().join("above");
        let session_above = dir.path().join("session_above");
        let cases = [
            (&session_above, &standard, session_above.join("1033")),
            (&above, &local, above.join("Temp")),
        ];
        for (link, before, actual) in cases {
            walk::testing::junction(link, before);
            let spot = standard_temp(&actual, &standard);
            assert!(
                matches!(&spot, Spot::Temp(..)),
                "{}: {spot:?}",
                actual.display()
            );
            fs::remove_dir(link).unwrap();
            walk::testing::junction(link, &office);

            let moved = office.join(actual.file_name().unwrap());
            let canary = moved.join("setup.dll");
            fs::write(&canary, b"keep").unwrap();
            walk::testing::age(&canary, OLD);

            let found = survey(std::slice::from_ref(&spot), &guards);
            assert!(found.paths.is_empty(), "{:?}", found.paths);
            assert!(
                found
                    .refused
                    .as_deref()
                    .is_some_and(|r| r.contains("not the standard")),
                "{:?}",
                found.refused
            );
            let run = clean_files("user_temp", std::slice::from_ref(&spot), &guards);
            assert!(run.failed, "{}: {:?}", actual.display(), run.result);
            assert_eq!(run.result.deleted_files, 0);
            assert!(canary.exists(), "{} was cleaned", moved.display());
            fs::remove_dir(link).unwrap();
        }
    }

    #[test]
    fn cleanup_process_block_reasons() {
        let chrome = find("browser_chrome")
            .and_then(|d| d.blocker.as_ref())
            .unwrap();
        let running: HashSet<String> = ["chrome.exe".to_string()].into();
        assert_eq!(
            process_block(chrome, Some(&running)).as_deref(),
            Some("Chrome is running")
        );
        assert_eq!(process_block(chrome, Some(&HashSet::new())), None);
        assert_eq!(
            process_block(chrome, None).as_deref(),
            Some("could not check whether Chrome is running")
        );
        let edge = find("browser_edge")
            .and_then(|d| d.blocker.as_ref())
            .unwrap();
        let running: HashSet<String> = ["msedge.exe".to_string()].into();
        assert_eq!(
            process_block(edge, Some(&running)).as_deref(),
            Some("Edge is running (check the system tray)")
        );
        assert_eq!(
            process_block(edge, Some(&["chrome.exe".into()].into())),
            None
        );
        for def in DEFS {
            let expected = matches!(
                def.id,
                "browser_chrome"
                    | "browser_edge"
                    | "browser_firefox"
                    | "update_cache"
                    | "delivery_optimization"
            );
            assert_eq!(def.blocker.is_some(), expected, "{}", def.id);
        }
    }

    #[test]
    fn update_targets_are_blocked_while_dism_or_dismhost_runs() {
        let dism_running = "DISM is repairing Windows; clean this after it finishes";
        for id in ["update_cache", "delivery_optimization"] {
            let def = find(id).unwrap();
            for exe in ["dism.exe", "dismhost.exe"] {
                let running: HashSet<String> = [exe.to_string(), "chrome.exe".to_string()].into();
                assert_eq!(
                    block_reason(def, Some(&running), true).as_deref(),
                    Some(dism_running),
                    "{id} with {exe}"
                );
            }
            // An unreadable process list fails closed.
            assert_eq!(
                block_reason(def, None, true).as_deref(),
                Some("could not check whether DISM is running"),
                "{id}"
            );
            let others: HashSet<String> =
                ["sfc.exe".to_string(), "tiworker.exe".to_string()].into();
            assert_eq!(block_reason(def, Some(&others), true), None, "{id}");
            // Missing rights are reported first.
            assert_eq!(
                block_reason(def, Some(&["dism.exe".to_string()].into()), false).as_deref(),
                Some(NEEDS_ADMIN)
            );
        }
        // Other targets ignore DISM.
        let dism: HashSet<String> = ["dism.exe".to_string(), "dismhost.exe".to_string()].into();
        for id in ["windows_temp", "crash_dumps", "browser_chrome"] {
            assert_eq!(
                block_reason(find(id).unwrap(), Some(&dism), true),
                None,
                "{id}"
            );
        }
    }

    #[test]
    fn cleanup_admin_targets_are_blocked_without_elevation() {
        let none = HashSet::new();
        for def in DEFS {
            let unelevated = block_reason(def, Some(&none), false);
            let elevated = block_reason(def, Some(&none), true);
            if def.requires_admin {
                assert_eq!(unelevated.as_deref(), Some(NEEDS_ADMIN), "{}", def.id);
            } else {
                assert_eq!(unelevated, None, "{}", def.id);
            }
            assert_eq!(elevated, None, "{}", def.id);
        }
        let chrome = find("browser_chrome").unwrap();
        let running: HashSet<String> = ["chrome.exe".to_string()].into();
        assert_eq!(
            block_reason(chrome, Some(&running), false).as_deref(),
            Some("Chrome is running")
        );
    }

    #[test]
    fn cleanup_unknown_id_is_skipped() {
        let (_dir, journal) = temp_journal();
        let session = journal
            .begin_session(SESSION_LABEL, crate::VERSION)
            .unwrap();
        let guards = Guards::default();
        let ctx = Ctx {
            guards: &guards,
            journal: &journal,
            session,
        };
        let run = run_target("does_not_exist", &ctx);
        assert_eq!(run.outcome(), "skipped");
        assert_eq!(run.result.skipped_reason.as_deref(), Some(UNKNOWN_TARGET));
    }

    #[test]
    fn cleanup_run_outcome_and_detail() {
        let mut run = Run::new("x");
        assert_eq!(run.outcome(), "cleaned");
        run.absorb(Purge {
            freed_bytes: 10,
            deleted_files: 2,
            skipped_files: 1,
            errors: (0..MAX_ERRORS).map(|i| format!("e{i}")).collect(),
            error_count: 8,
            ..Purge::default()
        });
        assert_eq!(run.result.errors.len(), MAX_ERRORS);
        assert_eq!(run.error_count, 8);
        let detail = run.detail();
        assert!(detail.starts_with("freed 10 bytes, deleted 2 files, skipped 1 files"));
        assert!(detail.contains("; 8 error(s), first: e0"), "{detail}");
        run.fail("boom");
        assert_eq!(run.outcome(), "failed");
        assert_eq!(run.error_count, 9);
        assert_eq!(run.result.errors.len(), MAX_ERRORS);
        assert_eq!(Run::skipped("x", NEEDS_ADMIN).outcome(), "skipped");

        // A hand-built purge without a count still counts its messages.
        let mut run = Run::new("y");
        run.absorb(Purge {
            errors: vec!["a".into(), "b".into()],
            ..Purge::default()
        });
        assert_eq!(run.error_count, 2);
    }

    #[test]
    fn cleanup_important_errors_survive_the_cap() {
        let mut run = Run::new("x");
        for i in 0..MAX_ERRORS + 3 {
            run.note(format!("e{i}"));
        }
        run.note_first("service left stopped");
        assert_eq!(run.result.errors.len(), MAX_ERRORS);
        assert_eq!(run.result.errors[0], "service left stopped");
        assert_eq!(run.error_count, MAX_ERRORS as u64 + 4);
        assert!(run
            .detail()
            .contains("; 9 error(s), first: service left stopped"));
    }

    /// A service whose state changes as a script says: it reports `StopPending` for the
    /// first `stopping` state queries, then `Stopped`; the first `failing` start requests
    /// fail, the next `ignored` ones report success without starting it (as a start sent
    /// during a stop does), and any later one starts it.
    struct ScriptedService {
        stopping: Cell<u32>,
        failing: Cell<u32>,
        ignored: Cell<u32>,
        state: Cell<ServiceState>,
        starts: Cell<u32>,
        log: RefCell<Vec<ServiceState>>,
    }

    impl ScriptedService {
        fn new(state: ServiceState, stopping: u32, failing: u32, ignored: u32) -> ScriptedService {
            ScriptedService {
                stopping: Cell::new(stopping),
                failing: Cell::new(failing),
                ignored: Cell::new(ignored),
                state: Cell::new(state),
                starts: Cell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServiceControl for ScriptedService {
        fn name(&self) -> &str {
            "scripted"
        }

        fn state(&self) -> Result<ServiceState> {
            if self.state.get() == ServiceState::StopPending {
                match self.stopping.get() {
                    0 => self.state.set(ServiceState::Stopped),
                    n => self.stopping.set(n - 1),
                }
            }
            self.log.borrow_mut().push(self.state.get());
            Ok(self.state.get())
        }

        fn start(&self) -> Result<()> {
            self.starts.set(self.starts.get() + 1);
            if self.state.get() != ServiceState::Stopped {
                // What the service manager reports while a stop is pending.
                return Ok(());
            }
            if self.failing.get() > 0 {
                self.failing.set(self.failing.get() - 1);
                return Err(Error::Other("start refused".into()));
            }
            if self.ignored.get() > 0 {
                self.ignored.set(self.ignored.get() - 1);
                return Ok(());
            }
            self.state.set(ServiceState::StartPending);
            Ok(())
        }
    }

    const FAST: RestartTiming = RestartTiming {
        wait: Duration::from_secs(5),
        poll: Duration::from_millis(1),
        retry: Duration::from_millis(1),
    };

    #[test]
    fn cleanup_restart_waits_out_a_pending_stop() {
        let service = ScriptedService::new(ServiceState::StopPending, 3, 0, 0);
        start_again(&service, FAST).unwrap();
        assert_eq!(service.starts.get(), 1, "no start is sent while stopping");
        assert_eq!(service.state.get(), ServiceState::StartPending);
        let log = service.log.borrow();
        assert_eq!(&log[..3], [ServiceState::StopPending; 3]);
    }

    #[test]
    fn cleanup_restart_confirms_the_state_after_starting() {
        // A start reported as successful that leaves the service stopped is retried.
        let service = ScriptedService::new(ServiceState::Stopped, 0, 0, 2);
        start_again(&service, FAST).unwrap();
        assert_eq!(service.starts.get(), 3);
        assert!(service.state.get().is_active());
    }

    #[test]
    fn cleanup_restart_retries_failed_starts() {
        let service = ScriptedService::new(ServiceState::Stopped, 0, 2, 0);
        start_again(&service, FAST).unwrap();
        assert_eq!(service.starts.get(), 3);

        let service = ScriptedService::new(ServiceState::Stopped, 0, u32::MAX, 0);
        let err = start_again(&service, FAST).unwrap_err();
        assert_eq!(service.starts.get(), START_ATTEMPTS);
        assert!(err.to_string().contains("start refused"), "{err}");
    }

    #[test]
    fn cleanup_restart_reports_a_stop_that_never_finishes() {
        let service = ScriptedService::new(ServiceState::StopPending, u32::MAX, 0, 0);
        let timing = RestartTiming {
            wait: Duration::from_millis(30),
            ..FAST
        };
        let err = start_again(&service, timing).unwrap_err();
        assert_eq!(service.starts.get(), 0);
        assert!(err.to_string().contains("still stopping"), "{err}");
    }

    #[test]
    fn cleanup_restart_leaves_a_running_service_alone() {
        let service = ScriptedService::new(ServiceState::Running, 0, 0, 0);
        start_again(&service, FAST).unwrap();
        assert_eq!(service.starts.get(), 0);
    }

    #[test]
    fn cleanup_service_record_is_closed_only_for_this_session() {
        let (_dir, journal) = temp_journal();
        let earlier = journal.begin_session("tweaks", crate::VERSION).unwrap();
        let session = journal
            .begin_session(SESSION_LABEL, crate::VERSION)
            .unwrap();
        let record = |name: &str| NewServiceRecord {
            name: name.to_string(),
            display_name: name.to_string(),
            start_type: StartType::Manual,
            delayed_auto_start: false,
            was_running: true,
        };
        assert!(journal
            .record_service(earlier, &record("OtherSvc"))
            .unwrap());
        assert!(journal
            .record_service(session, &record("SelfTestSvc"))
            .unwrap());

        close_service_record(&journal, session, "selftestsvc").unwrap();
        let active: Vec<String> = journal
            .active_services()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(active, ["OtherSvc"]);
    }

    #[test]
    fn delivery_optimization_script_imports_its_module_and_qualifies_the_cmdlet() {
        let script = delivery_optimization_script().unwrap();
        let import = crate::win::powershell::import_system_module("DeliveryOptimization").unwrap();
        assert!(script.starts_with(&import), "{script}");
        assert_eq!(
            &script[import.len()..],
            r"DeliveryOptimization\Delete-DeliveryOptimizationCache -Force"
        );
    }
}
