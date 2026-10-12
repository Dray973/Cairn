//! Whether this PC has what a tweak's policies are for (Microsoft Office, Microsoft Edge, a
//! graphics driver with hardware-accelerated GPU scheduling, the Recall component, Notepad,
//! Paint, Windows' text and image generation for apps), and whether its Windows build and
//! edition use the tweak's settings. Read-only; no probe needs elevation.
//!
//! A tweak that is retired, outside its Windows range or whose requirement is known to be
//! missing scans as unavailable and is skipped on apply before anything is recorded; a check
//! that fails leaves the tweak available and adds a warning. Each probe runs at most once per
//! scan or apply.
//!
//! Windows 11 versions are named by build number; builds released as enablement packages
//! share the cumulative updates, and so the revision numbers, of an older build (their
//! servicing family). A range's floor is met by a PC of the floor's servicing family whose
//! build number and revision both reach the floor's, so a floor on an enablement build (25H2,
//! build 26200) is never met by 24H2 (build 26100); a PC of a newer family than every floor
//! meets the range, an older one does not. Revisions compare as numbers, which orders the
//! monthly cumulative updates; Enterprise hotpatch revisions are not ordered with them, so a
//! hotpatched PC can be judged wrongly against a floor.
//!
//! The Recall component is read from its Component Based Servicing record: no record, or a
//! record marked removed, counts as missing. Notepad and Paint are the packages installed for
//! the account this process runs as; the consent store entry of text and image generation is
//! read from the same account.

use std::cell::OnceCell;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::catalog::{Build, Editions, Requirement, Tweak, WindowsRange, ANY_WINDOWS};
use crate::sysinfo::os::product_name;
use crate::win::gpu::{self, AdapterCaps, HWSCH_ENABLED, HWSCH_SUPPORTED};
use crate::win::package::installed_packages;
use crate::win::registry::{exists, read_value, Hive, Key, RegValue};
use crate::{Error, Result};

/// Package family of Notepad.
pub(crate) const NOTEPAD_FAMILY: &str = "Microsoft.WindowsNotepad_8wekyb3d8bbwe";
/// The first Notepad version with its AI features.
pub(crate) const NOTEPAD_AI_MIN: [u16; 4] = [11, 2503, 16, 0];
/// Package family of Paint.
pub(crate) const PAINT_FAMILY: &str = "Microsoft.Paint_8wekyb3d8bbwe";

/// Build, version and edition of the running Windows.
const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
/// Component Based Servicing's record of the Recall optional component; users may read it.
const RECALL_RECORD: &str = concat!(
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing",
    r"\Notifications\OptionalFeatures\Recall"
);
/// Consent store entry of Windows' text and image generation for apps, per user.
const SYSTEM_AI_MODELS_CONSENT: &str = concat!(
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager",
    r"\ConsentStore\systemAIModels"
);
/// Click-to-Run configuration of Office; `ProductReleaseIds` lists the installed products.
const C2R_CONFIGURATION: &str = r"SOFTWARE\Microsoft\Office\ClickToRun\Configuration";
/// Windows Installer Office 2016 or later, 64-bit and 32-bit registration.
const MSI_INSTALL_ROOT: &str = r"SOFTWARE\Microsoft\Office\16.0\Common\InstallRoot";
const MSI_INSTALL_ROOT_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Office\16.0\Common\InstallRoot";
/// Where the shell finds msedge.exe (default value: the program path).
const EDGE_APP_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe";
/// EdgeUpdate client of stable Edge; `pv` is the installed version.
const EDGE_UPDATE_CLIENT: &str =
    r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{56EB18F8-B008-4CBD-B6D2-8C97FE7E9062}";

/// A floor this PC's build does not reach. `{floors}`: the range's floors in words; `{pc}`:
/// the running Windows with its version and build.
const NEEDS_NEWER_BUILD: &str = "Needs Windows 11 {floors}; this PC runs {pc}.";
/// A range that ends before this PC's servicing family. `{next}`: the first version after it.
const NO_LONGER_USED: &str =
    "Windows 11 {next} and later no longer use this setting; this PC runs {pc}.";
/// An edition below Pro. `{name}`: the product name of the running Windows.
const NEEDS_PRO: &str = "Needs Windows 11 Pro, Enterprise or Education; this PC runs {name}.";
/// An edition below Enterprise and Education.
const NEEDS_ENTERPRISE: &str = "Needs Windows 11 Enterprise or Education; this PC runs {name}.";
/// Shown after the item id when Cairn does not know the edition.
const EDITION_UNKNOWN: &str = "cannot check whether this edition of Windows uses this setting: \
     Cairn does not know the edition \"{edition_id}\"";
/// Shown once by a scan whose Windows facts could not be read.
const FACTS_UNREADABLE: &str = "Cannot read the Windows version ({error}); tweaks meant for \
     some Windows versions or editions are shown as available.";
/// Shown for an Office-policy tweak when no Office 2016 or later is installed: the policies
/// apply to version 16.0, the only version the probe reads, so an older Office counts as none.
/// A consumer edition is told apart with [`Requirement::OfficeBusiness`]'s own text.
pub(crate) const OFFICE_NOT_INSTALLED: &str = "Microsoft Office (2016 or later) is not installed.";

/// Hardware-accelerated GPU scheduling as the graphics drivers report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuScheduling {
    /// At least one adapter's driver supports it.
    pub supported: bool,
    /// At least one adapter runs with it now.
    pub enabled: bool,
}

/// Edition family of the running Windows, from its `EditionID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditionClass {
    Home,
    Pro,
    Enterprise,
    Education,
    Unknown,
}

/// What the engine knows about the running Windows; part of every scan report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsFacts {
    pub build: Build,
    /// Servicing family of `build.number`: 22621 for 23H2, 26100 for 25H2 and 26H2.
    pub family: u32,
    /// `DisplayVersion` ("26H2"); `None` when Windows does not report one.
    pub version: Option<String>,
    /// `EditionID` ("Core", "Professional", ...).
    pub edition_id: String,
    pub edition: EditionClass,
    /// Product name for display ("Windows 11 Home").
    pub name: String,
}

/// The Recall optional component as Component Based Servicing records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecallState {
    Installed,
    Removed,
    Absent,
}

/// Which Office, if any, the Office-policy tweaks are for: an edition that honours the
/// policies (business or enterprise, or a volume-licensed Office), a consumer edition that
/// ignores them (Microsoft 365 Home, Office Home editions), or no Office at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfficeProduct {
    Business,
    Consumer,
    None,
}

/// Why a tweak is not offered on this PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    /// An app, component or device the tweak is for is missing.
    NotOnThisPc,
    /// This Windows build does not use the tweak's settings.
    WindowsVersion,
    /// This Windows edition does not use the tweak's settings.
    Edition,
    /// The tweak is no longer offered; its journal records still undo.
    Retired,
}

/// A condition this PC does not meet, with the sentence the scan shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unmet {
    pub kind: Unavailable,
    pub text: String,
}

/// Windows 11 versions by build number (Microsoft's "Windows 11 release information").
const VERSIONS: &[(u32, &str)] = &[
    (22000, "21H2"),
    (22621, "22H2"),
    (22631, "23H2"),
    (26100, "24H2"),
    (26200, "25H2"),
    (26300, "26H2"),
    (28000, "26H1"),
];

/// Builds that receive the cumulative updates of an older build (enablement-package releases)
/// and therefore share its revision numbers.
const SERVICING_FAMILIES: &[(u32, u32)] = &[(22631, 22621), (26200, 26100), (26300, 26100)];

/// The build whose cumulative updates `number` receives; `number` itself for a family base
/// and for builds this table does not know.
pub fn servicing_family(number: u32) -> u32 {
    SERVICING_FAMILIES
        .iter()
        .find(|(build, _)| *build == number)
        .map_or(number, |&(_, family)| family)
}

/// Version label of a build number ("24H2" for 26100); `None` for an unknown build.
pub(crate) fn version_label(number: u32) -> Option<&'static str> {
    VERSIONS
        .iter()
        .find(|(build, _)| *build == number)
        .map(|&(_, label)| label)
}

/// Label of the newest version in servicing family `family` ("23H2" for 22621).
pub(crate) fn family_label(family: u32) -> Option<&'static str> {
    VERSIONS
        .iter()
        .filter(|&&(build, _)| servicing_family(build) == family)
        .max_by_key(|&&(build, _)| build)
        .map(|&(_, label)| label)
}

/// Label of the first version of the first servicing family after `family` ("24H2" after
/// 22621); `None` when no newer family is known.
fn next_family_label(family: u32) -> Option<&'static str> {
    VERSIONS
        .iter()
        .find(|&&(build, _)| build > family && servicing_family(build) == build)
        .map(|&(_, label)| label)
}

/// The read-only checks behind the requirements. Production uses [`SYSTEM_PROBES`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Probes {
    pub office: fn() -> Result<bool>,
    /// Which Office the Office-policy tweaks are for (business, consumer or none).
    pub office_product: fn() -> Result<OfficeProduct>,
    pub edge: fn() -> Result<bool>,
    pub gpu: fn() -> Result<GpuScheduling>,
    /// The running Windows: build, version and edition.
    pub windows: fn() -> Result<WindowsFacts>,
    /// The Recall optional component as Component Based Servicing records it.
    pub recall: fn() -> Result<RecallState>,
    /// Versions of the packages of `family` installed for this process's account
    /// (`win::package::installed_packages`, version parsed from the package full name);
    /// Err when the lookup itself fails.
    pub packages: fn(&str) -> Result<Vec<[u16; 4]>>,
    /// HKCU\…\CapabilityAccessManager\ConsentStore\systemAIModels exists (existence only).
    pub system_ai_models: fn() -> Result<bool>,
}

pub(crate) const SYSTEM_PROBES: Probes = Probes {
    office: office_installed,
    office_product,
    edge: edge_installed,
    gpu: gpu_scheduling,
    windows: windows_facts,
    recall: recall_state,
    packages: package_versions,
    system_ai_models: system_ai_models_present,
};

/// Outcome of one check of a tweak's requirement, Windows range or retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Availability {
    Met,
    /// Known to be unmet, with the kind and the sentence the scan shows.
    Missing(Unmet),
    /// The check itself failed; the tweak is treated as available and a warning is shown.
    Unknown(String),
}

/// A probe's answer as a scan keeps it: the error as text, so the answer can be cloned.
type Answer<T> = std::result::Result<T, String>;

/// Runs `probe` the first time `cell` is asked for and keeps its answer.
fn answer<T>(cell: &OnceCell<Answer<T>>, probe: impl FnOnce() -> Result<T>) -> &Answer<T> {
    cell.get_or_init(|| probe().map_err(|e| e.to_string()))
}

/// Requirement answers for one scan or apply; each probe runs at most once.
#[derive(Debug)]
pub(crate) struct Requirements {
    probes: Probes,
    office: OnceCell<Answer<bool>>,
    office_product: OnceCell<Answer<OfficeProduct>>,
    edge: OnceCell<Answer<bool>>,
    gpu: OnceCell<Answer<GpuScheduling>>,
    windows: OnceCell<Answer<WindowsFacts>>,
    recall: OnceCell<Answer<RecallState>>,
    notepad: OnceCell<Answer<Vec<[u16; 4]>>>,
    paint: OnceCell<Answer<Vec<[u16; 4]>>>,
    system_ai_models: OnceCell<Answer<bool>>,
}

impl Requirements {
    pub(crate) fn new(probes: Probes) -> Self {
        Self {
            probes,
            office: OnceCell::new(),
            office_product: OnceCell::new(),
            edge: OnceCell::new(),
            gpu: OnceCell::new(),
            windows: OnceCell::new(),
            recall: OnceCell::new(),
            notepad: OnceCell::new(),
            paint: OnceCell::new(),
            system_ai_models: OnceCell::new(),
        }
    }

    /// Whether this PC has what `req` names. `Unknown` carries the probe's error.
    pub(crate) fn check(&self, req: Requirement) -> Availability {
        if req == Requirement::OfficeBusiness {
            return self.office_business_availability();
        }
        let probes = &self.probes;
        let present = match req {
            Requirement::Office => answer(&self.office, probes.office).clone(),
            // Handled above by its own three-way probe.
            Requirement::OfficeBusiness => unreachable!(),
            Requirement::Edge => answer(&self.edge, probes.edge).clone(),
            Requirement::GpuScheduling => self.gpu_result().clone().map(|g| g.supported),
            Requirement::RecallComponent => answer(&self.recall, probes.recall)
                .clone()
                .map(|state| state == RecallState::Installed),
            Requirement::NotepadAi => answer(&self.notepad, || (probes.packages)(NOTEPAD_FAMILY))
                .as_ref()
                .map(|versions| notepad_has_ai(versions))
                .map_err(Clone::clone),
            Requirement::Paint => answer(&self.paint, || (probes.packages)(PAINT_FAMILY))
                .as_ref()
                .map(|versions| !versions.is_empty())
                .map_err(Clone::clone),
            Requirement::SystemAiModels => {
                answer(&self.system_ai_models, probes.system_ai_models).clone()
            }
        };
        match present {
            Ok(true) => Availability::Met,
            Ok(false) => Availability::Missing(Unmet {
                kind: Unavailable::NotOnThisPc,
                text: req.missing_text().to_string(),
            }),
            Err(e) => Availability::Unknown(e),
        }
    }

    /// Whether an Office that honours the Office policies is installed. A consumer edition
    /// (Microsoft 365 Home, Office Home) and no Office at all are both not met, with their
    /// own texts, so the scan can tell them apart.
    fn office_business_availability(&self) -> Availability {
        match answer(&self.office_product, self.probes.office_product) {
            Ok(OfficeProduct::Business) => Availability::Met,
            Ok(OfficeProduct::Consumer) => Availability::Missing(Unmet {
                kind: Unavailable::NotOnThisPc,
                text: Requirement::OfficeBusiness.missing_text().to_string(),
            }),
            Ok(OfficeProduct::None) => Availability::Missing(Unmet {
                kind: Unavailable::NotOnThisPc,
                text: OFFICE_NOT_INSTALLED.to_string(),
            }),
            Err(e) => Availability::Unknown(e.clone()),
        }
    }

    /// Whether this PC's Windows build and edition use settings meant for `range`. `Met`
    /// for [`ANY_WINDOWS`] without probing, and when the Windows facts cannot be read (the
    /// scan then shows [`Self::windows_warning`] once).
    pub(crate) fn check_windows(&self, range: &WindowsRange) -> Availability {
        if *range == ANY_WINDOWS {
            return Availability::Met;
        }
        match self.windows() {
            Some(pc) => range_availability(range, pc),
            None => Availability::Met,
        }
    }

    /// Retired first, then the Windows range, then the requirement: the first condition this
    /// PC is known not to meet wins. Otherwise a check that failed gives `Unknown` with the
    /// warning to show after the item id ("cannot check whether …").
    pub(crate) fn check_tweak(&self, t: &Tweak) -> Availability {
        if let Some(reason) = t.retired {
            return Availability::Missing(Unmet {
                kind: Unavailable::Retired,
                text: reason.to_string(),
            });
        }
        let windows = self.check_windows(&t.windows);
        if let Availability::Missing(_) = windows {
            return windows;
        }
        let requirement = match t.requires.map(|req| (req, self.check(req))) {
            Some((req, Availability::Unknown(e))) => {
                Availability::Unknown(format!("cannot check whether {}: {e}", req.subject()))
            }
            Some((_, availability)) => availability,
            None => Availability::Met,
        };
        match (windows, requirement) {
            (_, missing @ Availability::Missing(_)) => missing,
            (unknown @ Availability::Unknown(_), _) => unknown,
            (_, requirement) => requirement,
        }
    }

    /// The running Windows, when its facts could be read.
    pub(crate) fn windows(&self) -> Option<&WindowsFacts> {
        answer(&self.windows, self.probes.windows).as_ref().ok()
    }

    /// The warning a scan shows once when a Windows range needed the facts and they could
    /// not be read; `None` while they were not needed or could be read.
    pub(crate) fn windows_warning(&self) -> Option<String> {
        match self.windows.get() {
            Some(Err(e)) => Some(FACTS_UNREADABLE.replace("{error}", e)),
            _ => None,
        }
    }

    /// The GPU probe's answer, when it succeeded.
    pub(crate) fn gpu(&self) -> Option<GpuScheduling> {
        self.gpu_result().as_ref().ok().copied()
    }

    fn gpu_result(&self) -> &Answer<GpuScheduling> {
        answer(&self.gpu, self.probes.gpu)
    }
}

/// Whether the newest of the installed Notepad versions has the AI features.
fn notepad_has_ai(versions: &[[u16; 4]]) -> bool {
    versions
        .iter()
        .max()
        .is_some_and(|newest| *newest >= NOTEPAD_AI_MIN)
}

/// The running Windows: build, servicing family, version, edition and product name, from
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`. Read-only; needs no elevation.
pub fn windows_facts() -> Result<WindowsFacts> {
    let key = Key::open(Hive::LocalMachine, CURRENT_VERSION, false)?
        .ok_or_else(|| Error::Other("the Windows version key is missing".into()))?;
    let value = |name: &str| key.query(name);
    let build = match value("CurrentBuildNumber")? {
        Some(number) if text(Some(&number), false).is_some() => Some(number),
        _ => value("CurrentBuild")?,
    };
    let ubr = match value("UBR")? {
        Some(RegValue::Dword(revision)) => Some(revision),
        _ => None,
    };
    facts_from(
        text(build.as_ref(), false),
        ubr,
        text(value("DisplayVersion")?.as_ref(), false),
        text(value("EditionID")?.as_ref(), false),
        text(value("ProductName")?.as_ref(), false),
    )
}

/// The facts made of the `CurrentVersion` values: `build` is `CurrentBuildNumber`, `ubr` the
/// update build revision (0 when missing). A missing or unparsable build number is an error.
pub(crate) fn facts_from(
    build: Option<&str>,
    ubr: Option<u32>,
    version: Option<&str>,
    edition_id: Option<&str>,
    product: Option<&str>,
) -> Result<WindowsFacts> {
    let build = build
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Error::Other("the build number is missing".into()))?;
    let number: u32 = build
        .parse()
        .map_err(|_| Error::Other(format!("the build number \"{build}\" is not a number")))?;
    let edition_id = edition_id.map(str::trim).unwrap_or_default().to_string();
    Ok(WindowsFacts {
        build: Build::new(number, ubr.unwrap_or(0)),
        family: servicing_family(number),
        version: version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        edition: edition_class(&edition_id),
        name: product_name(product, Some(&edition_id), number),
        edition_id,
    })
}

/// Edition family of an `EditionID`: "Core…" is Home, "Professional…" (Workstation and
/// Education included) is Pro, "Enterprise…", "IoTEnterprise…" and "ServerRdsh" are
/// Enterprise, "Education…" is Education; anything else is unknown. The one classifier of
/// editions; `updates::wu::Edition` keeps its own Home reader, which agrees with it.
pub(crate) fn edition_class(edition_id: &str) -> EditionClass {
    let id = edition_id.trim();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| id.starts_with(p));
    if starts(&["Core"]) {
        EditionClass::Home
    } else if starts(&["Professional"]) {
        EditionClass::Pro
    } else if starts(&["Enterprise", "IoTEnterprise", "ServerRdsh"]) {
        EditionClass::Enterprise
    } else if starts(&["Education"]) {
        EditionClass::Education
    } else {
        EditionClass::Unknown
    }
}

/// The Recall optional component as Component Based Servicing records it.
pub(crate) fn recall_state() -> Result<RecallState> {
    match Key::open(Hive::LocalMachine, RECALL_RECORD, false)? {
        Some(key) => recall_from(true, key.query("Removed")?.as_ref()),
        None => recall_from(false, None),
    }
}

/// The Recall state of a servicing record: `present` is whether the record's key exists,
/// `removed` its `Removed` value. No record is `Absent`; a record without a non-zero
/// `Removed` is `Installed`.
pub(crate) fn recall_from(present: bool, removed: Option<&RegValue>) -> Result<RecallState> {
    if !present {
        return Ok(RecallState::Absent);
    }
    match removed {
        None | Some(RegValue::Dword(0)) => Ok(RecallState::Installed),
        Some(RegValue::Dword(_)) => Ok(RecallState::Removed),
        Some(other) => Err(Error::Other(format!(
            "the Recall servicing record holds an unexpected Removed value ({})",
            other.display()
        ))),
    }
}

/// Versions of the packages of `family` installed for this process's account.
pub(crate) fn package_versions(family: &str) -> Result<Vec<[u16; 4]>> {
    let packages = installed_packages(family).ok_or_else(|| {
        Error::Other(format!(
            "the installed packages of {family} cannot be listed"
        ))
    })?;
    packages
        .iter()
        .map(|p| {
            package_version(&p.full_name).ok_or_else(|| {
                Error::Other(format!("the package name {} holds no version", p.full_name))
            })
        })
        .collect()
}

/// Version of a package full name (`Name_Version_Architecture_ResourceId_PublisherId`);
/// `None` unless it has four numeric parts.
pub(crate) fn package_version(full_name: &str) -> Option<[u16; 4]> {
    let mut parts = full_name.split('_').nth(1)?.split('.');
    let mut version = [0u16; 4];
    for part in &mut version {
        *part = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

/// Whether the consent store has an entry for Windows' text and image generation for apps.
pub(crate) fn system_ai_models_present() -> Result<bool> {
    exists(Hive::CurrentUser, SYSTEM_AI_MODELS_CONSENT)
}

/// What each read-only probe behind the requirements finds on this PC, one line per probe
/// ("recall: Ok(Removed)"), for diagnostics.
pub fn probe_lines() -> Vec<String> {
    vec![
        format!("windows: {:?}", windows_facts()),
        format!("recall: {:?}", recall_state()),
        format!("notepad: {:?}", package_versions(NOTEPAD_FAMILY)),
        format!("paint: {:?}", package_versions(PAINT_FAMILY)),
        format!("system AI models: {:?}", system_ai_models_present()),
        format!("office: {:?}", office_installed()),
        format!("office product: {:?}", office_product()),
        format!("edge: {:?}", edge_installed()),
        format!("gpu scheduling: {:?}", gpu_scheduling()),
    ]
}

/// Whether a PC with `pc`'s Windows build and edition uses settings meant for `range`: the
/// newest family first (`through`), then the floors, then the editions. `Unknown` carries the
/// warning to show after the item id, as in [`Requirements::check_tweak`].
pub(crate) fn range_availability(range: &WindowsRange, pc: &WindowsFacts) -> Availability {
    let family = servicing_family(pc.build.number);
    if let Some(last) = range.through.filter(|&last| family > last) {
        let next =
            next_family_label(last).map_or_else(|| format!("build {family}"), str::to_string);
        return Availability::Missing(Unmet {
            kind: Unavailable::WindowsVersion,
            text: NO_LONGER_USED
                .replace("{next}", &next)
                .replace("{pc}", &pc_text(pc)),
        });
    }
    if !range.from.is_empty() && !floors_met(range.from, pc.build) {
        return Availability::Missing(Unmet {
            kind: Unavailable::WindowsVersion,
            text: NEEDS_NEWER_BUILD
                .replace("{floors}", &floors_text(range.from))
                .replace("{pc}", &pc_text(pc)),
        });
    }
    match (range.editions, pc.edition) {
        (Editions::All, _) => Availability::Met,
        (_, EditionClass::Unknown) => {
            Availability::Unknown(EDITION_UNKNOWN.replace("{edition_id}", &pc.edition_id))
        }
        (Editions::ProOrHigher, EditionClass::Home) => Availability::Missing(Unmet {
            kind: Unavailable::Edition,
            text: NEEDS_PRO.replace("{name}", &pc.name),
        }),
        (Editions::EnterpriseOrEducation, EditionClass::Home | EditionClass::Pro) => {
            Availability::Missing(Unmet {
                kind: Unavailable::Edition,
                text: NEEDS_ENTERPRISE.replace("{name}", &pc.name),
            })
        }
        _ => Availability::Met,
    }
}

/// Whether `build` reaches the floor of its servicing family; with no floor of that family,
/// whether its family is newer than every floor's.
fn floors_met(floors: &[Build], build: Build) -> bool {
    let family = servicing_family(build.number);
    match floors
        .iter()
        .find(|floor| servicing_family(floor.number) == family)
    {
        Some(floor) => build.number >= floor.number && build.revision >= floor.revision,
        None => floors
            .iter()
            .all(|floor| servicing_family(floor.number) < family),
    }
}

/// The floors of a range in words: "24H2 (build 26100.3915) or later" for one floor;
/// "22H2 (build 22621.4870 or later) or 24H2 (build 26100.3360 or later)" for two, and
/// "A, B or C" for more. A build without a version label is named by its number alone.
pub(crate) fn floors_text(floors: &[Build]) -> String {
    let named = |floor: &Build, inner: &str| match version_label(floor.number) {
        Some(label) => format!("{label} (build {floor}{inner})"),
        None => format!("build {floor}{inner}"),
    };
    match floors {
        [floor] => format!("{} or later", named(floor, "")),
        _ => {
            let parts: Vec<String> = floors.iter().map(|f| named(f, " or later")).collect();
            match parts.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => String::new(),
            }
        }
    }
}

/// The running Windows as range texts name it: "Windows 11 Home 26H2 (build 26300.9457)", or
/// "Windows 11 Home (build 26300.9457)" without a version.
fn pc_text(pc: &WindowsFacts) -> String {
    match &pc.version {
        Some(version) => format!("{} {version} (build {})", pc.name, pc.build),
        None => format!("{} (build {})", pc.name, pc.build),
    }
}

/// Reads one HKLM value; `Err` only when the key or value exists but cannot be read.
fn read(path: &str, name: &str) -> Result<Option<RegValue>> {
    read_value(Hive::LocalMachine, path, name)
}

/// Combines readings of which any one is enough to say "installed": a positive answer from
/// the readings that succeeded is certain; otherwise a failed read makes the answer unknown.
fn decide(
    readings: Vec<Result<Option<RegValue>>>,
    installed: impl FnOnce(&[Option<RegValue>]) -> bool,
) -> Result<bool> {
    let mut values = Vec::with_capacity(readings.len());
    let mut failure: Option<Error> = None;
    for reading in readings {
        match reading {
            Ok(value) => values.push(value),
            Err(e) => {
                values.push(None);
                failure.get_or_insert(e);
            }
        }
    }
    if installed(&values) {
        return Ok(true);
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(false),
    }
}

/// Microsoft Office 2016 or later (Click-to-Run, or Windows Installer 64-bit or 32-bit).
pub fn office_installed() -> Result<bool> {
    decide(
        vec![
            read(C2R_CONFIGURATION, "ProductReleaseIds"),
            read(MSI_INSTALL_ROOT, "Path"),
            read(MSI_INSTALL_ROOT_32, "Path"),
        ],
        |v| office_from(v[0].clone(), v[1].clone(), v[2].clone()),
    )
}

/// Which Office the Office-policy tweaks are for. Read-only: the Click-to-Run
/// `ProductReleaseIds` first (the comma-separated list of installed products), then the
/// Windows Installer registration. A read that fails is an error, so the tweak is shown as
/// available with a "cannot check" warning rather than wrongly offered or hidden.
pub(crate) fn office_product() -> Result<OfficeProduct> {
    if let Some(ids) = text(
        read(C2R_CONFIGURATION, "ProductReleaseIds")?.as_ref(),
        false,
    ) {
        return classify_office(ids);
    }
    // No Click-to-Run products: a Windows Installer Office 2016 or later is volume-licensed
    // and honours the policies.
    let msi = match read(MSI_INSTALL_ROOT, "Path")? {
        Some(path) => Some(path),
        None => read(MSI_INSTALL_ROOT_32, "Path")?,
    };
    if text(msi.as_ref(), true).is_some() {
        return Ok(OfficeProduct::Business);
    }
    Ok(OfficeProduct::None)
}

/// Click-to-Run products that are not Office apps the Office policies are for, by the start of
/// their id (lower case): OneNote (free), language packs, proofing tools and the Access
/// runtime.
const NOT_OFFICE_APPS: &[&str] = &[
    "onenotefree",
    "languagepack",
    "proofingtools",
    "accessruntime",
];

/// Classifies a Click-to-Run `ProductReleaseIds` list. A business or enterprise product
/// anywhere in the list wins; otherwise a consumer product makes it consumer; a list of only
/// products that are not Office apps (OneNote free, a language pack) counts as no
/// policy-honouring Office. Any other product, such as a standalone Word, is one Cairn cannot
/// classify: an error, so the tweak is shown with a "cannot check" warning rather than wrongly
/// offered or hidden.
fn classify_office(release_ids: &str) -> Result<OfficeProduct> {
    let mut consumer = false;
    let mut unknown = None;
    for id in release_ids.split(',') {
        let id = id.trim();
        if id.is_empty() || is_not_office_app(id) {
            continue;
        }
        if is_business_release(id) {
            return Ok(OfficeProduct::Business);
        }
        if is_consumer_release(id) {
            consumer = true;
        } else if unknown.is_none() {
            unknown = Some(id);
        }
    }
    match (consumer, unknown) {
        (true, _) => Ok(OfficeProduct::Consumer),
        (false, Some(id)) => Err(Error::Other(format!(
            "Cairn does not recognise the Office product {id}"
        ))),
        (false, None) => Ok(OfficeProduct::None),
    }
}

/// A Click-to-Run product that is not an Office app (see [`NOT_OFFICE_APPS`]).
fn is_not_office_app(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    NOT_OFFICE_APPS.iter().any(|prefix| id.starts_with(prefix))
}

/// A consumer Office product id: Microsoft 365 Home, Family and Personal, and the Office Home
/// editions (Home & Student, Home & Business). These ignore the Office policies.
fn is_consumer_release(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.contains("home") || id.contains("personal") || id.contains("student")
}

/// A business, enterprise or volume-licensed Office product id. An id with a consumer marker
/// (so "HomeBusiness") is never business; "O365BusinessRetail" (no consumer marker) is. Skype
/// for Business (formerly Lync) is not an Office app the policies are for, despite its name.
fn is_business_release(id: &str) -> bool {
    if is_consumer_release(id) {
        return false;
    }
    let id = id.to_ascii_lowercase();
    if ["skypeforbusiness", "lync"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return false;
    }
    [
        "proplus",
        "business",
        "enterprise",
        "standard",
        "professional",
        "volume",
        "project",
        "visio",
        "smallbus",
    ]
    .iter()
    .any(|marker| id.contains(marker))
}

/// Microsoft Edge (the App Paths registration of an existing msedge.exe, or the EdgeUpdate
/// client of stable Edge).
pub fn edge_installed() -> Result<bool> {
    decide(
        vec![read(EDGE_APP_PATH, ""), read(EDGE_UPDATE_CLIENT, "pv")],
        |v| edge_from(v[0].clone(), v[1].clone(), &|p: &Path| p.is_file()),
    )
}

/// Hardware-accelerated GPU scheduling over every adapter the display kernel lists.
pub fn gpu_scheduling() -> Result<GpuScheduling> {
    Ok(gpu_from(&gpu::wddm_2_7_caps()?))
}

/// Text of a string value, trimmed; `None` for other types and blank strings.
fn text(value: Option<&RegValue>, expand_ok: bool) -> Option<&str> {
    let s = match value? {
        RegValue::Sz(s) => s,
        RegValue::ExpandSz(s) if expand_ok => s,
        _ => return None,
    };
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

fn office_from(
    c2r_release_ids: Option<RegValue>,
    msi64_path: Option<RegValue>,
    msi32_path: Option<RegValue>,
) -> bool {
    text(c2r_release_ids.as_ref(), false).is_some()
        || text(msi64_path.as_ref(), true).is_some()
        || text(msi32_path.as_ref(), true).is_some()
}

fn edge_from(
    app_path: Option<RegValue>,
    update_pv: Option<RegValue>,
    exists: &dyn Fn(&Path) -> bool,
) -> bool {
    let program = text(app_path.as_ref(), true)
        .map(|s| s.trim_matches('"').trim())
        .filter(|s| !s.is_empty());
    if program.is_some_and(|p| exists(Path::new(p))) {
        return true;
    }
    text(update_pv.as_ref(), false).is_some_and(|pv| pv != "0.0.0.0")
}

fn gpu_from(adapters: &[AdapterCaps]) -> GpuScheduling {
    let caps = adapters.iter().filter_map(|a| a.wddm_2_7);
    let mut out = GpuScheduling::default();
    for bits in caps {
        out.supported |= bits & HWSCH_SUPPORTED != 0;
        out.enabled |= bits & HWSCH_ENABLED != 0;
    }
    out
}

/// Note for the GPU scheduling tweak: what the GPU runs with now when that differs from the
/// stored setting. `stored` is HwSchMode (2 on, 1 off), `None` when absent.
pub(crate) fn gpu_note(stored: Option<u32>, gpu: GpuScheduling) -> Option<&'static str> {
    match (stored, gpu.enabled) {
        (Some(2), false) => {
            Some("Turned on in the settings; Windows starts using it after the next restart.")
        }
        (Some(1), true) => {
            Some("Turned off in the settings; the GPU keeps using it until the next restart.")
        }
        (None, true) => Some(
            "The GPU already uses it: Windows turned it on for this GPU without storing the \
             setting. Applying stores the setting so it stays on.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::debloat::catalog::Topic;
    use crate::debloat::{Category, RestartNeed, Risk};

    fn sz(s: &str) -> Option<RegValue> {
        Some(RegValue::Sz(s.to_string()))
    }

    #[test]
    fn office_from_click_to_run_or_msi() {
        assert!(office_from(sz("O365HomePremRetail"), None, None));
        assert!(office_from(
            sz(" O365HomePremRetail,ProPlus2021Retail "),
            None,
            None
        ));
        assert!(!office_from(sz(""), None, None));
        assert!(!office_from(sz("   "), None, None));
        assert!(!office_from(Some(RegValue::Dword(1)), None, None));
        assert!(office_from(
            None,
            sz(r"C:\Program Files\Microsoft Office\root\Office16\"),
            None
        ));
        assert!(office_from(
            None,
            Some(RegValue::ExpandSz(
                r"%ProgramFiles%\Microsoft Office\".into()
            )),
            None
        ));
        assert!(office_from(
            None,
            None,
            sz(r"C:\Program Files (x86)\Microsoft Office\")
        ));
        assert!(!office_from(None, sz(""), sz(" ")));
        assert!(!office_from(None, Some(RegValue::Dword(0)), None));
        assert!(!office_from(None, None, None));
    }

    #[test]
    fn edge_from_app_path_or_update_client() {
        let edge = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
        let exists = |p: &Path| p == Path::new(edge);
        let missing = |_: &Path| false;
        assert!(edge_from(sz(&format!("\"{edge}\"")), None, &exists));
        assert!(edge_from(sz(&format!("  {edge} ")), None, &exists));
        assert!(!edge_from(sz(&format!("\"{edge}\"")), None, &missing));
        assert!(!edge_from(sz("\"\""), None, &exists));
        assert!(edge_from(None, sz("154.0.1.2"), &missing));
        assert!(edge_from(sz(edge), sz("154.0.1.2"), &missing));
        assert!(!edge_from(None, sz("0.0.0.0"), &missing));
        assert!(!edge_from(None, sz(""), &missing));
        assert!(!edge_from(None, Some(RegValue::Dword(154)), &missing));
        assert!(!edge_from(None, None, &exists));
    }

    #[test]
    fn gpu_from_ors_supported_and_enabled() {
        let caps = |bits: &[Option<u32>]| -> Vec<AdapterCaps> {
            bits.iter()
                .enumerate()
                .map(|(i, &b)| AdapterCaps {
                    luid: i as i64,
                    wddm_2_7: b,
                })
                .collect()
        };
        assert_eq!(
            gpu_from(&caps(&[Some(0xb), Some(0)])),
            GpuScheduling {
                supported: true,
                enabled: true
            }
        );
        assert_eq!(
            gpu_from(&caps(&[Some(0x1)])),
            GpuScheduling {
                supported: true,
                enabled: false
            }
        );
        assert_eq!(gpu_from(&caps(&[None])), GpuScheduling::default());
        assert_eq!(gpu_from(&[]), GpuScheduling::default());
    }

    #[test]
    fn gpu_note_cases() {
        let on = GpuScheduling {
            supported: true,
            enabled: true,
        };
        let off = GpuScheduling {
            supported: true,
            enabled: false,
        };
        assert_eq!(
            gpu_note(Some(2), off),
            Some("Turned on in the settings; Windows starts using it after the next restart.")
        );
        assert_eq!(
            gpu_note(Some(1), on),
            Some("Turned off in the settings; the GPU keeps using it until the next restart.")
        );
        assert_eq!(
            gpu_note(None, on),
            Some(
                "The GPU already uses it: Windows turned it on for this GPU without storing the \
                 setting. Applying stores the setting so it stays on."
            )
        );
        assert_eq!(gpu_note(Some(2), on), None);
        assert_eq!(gpu_note(Some(1), off), None);
        assert_eq!(gpu_note(None, off), None);
        assert_eq!(gpu_note(Some(7), on), None);
        assert_eq!(gpu_note(Some(7), off), None);
    }

    static OFFICE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static OFFICE_PRODUCT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EDGE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static GPU_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WINDOWS_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RECALL_CALLS: AtomicUsize = AtomicUsize::new(0);
    static NOTEPAD_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PAINT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static AI_MODELS_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Windows 11 Home 26H2 as the test PC reports it.
    fn home_26h2() -> WindowsFacts {
        WindowsFacts {
            build: Build::new(26300, 9457),
            family: 26100,
            version: Some("26H2".into()),
            edition_id: "Core".into(),
            edition: EditionClass::Home,
            name: "Windows 11 Home".into(),
        }
    }

    /// The facts of a PC on `build` ("26300.9457") of edition `edition_id`, with the version
    /// label of its build number and the product name Windows derives from the edition.
    fn pc(build: &str, edition_id: &str) -> WindowsFacts {
        let (number, revision) = build.split_once('.').unwrap();
        let label = version_label(number.parse().unwrap());
        facts_from(
            Some(number),
            Some(revision.parse().unwrap()),
            label,
            Some(edition_id),
            None,
        )
        .unwrap()
    }

    /// Probes that report every requirement as met, on Windows 11 Home 26H2.
    const ALL_MET: Probes = Probes {
        office: || Ok(true),
        office_product: || Ok(OfficeProduct::Business),
        edge: || Ok(true),
        gpu: || {
            Ok(GpuScheduling {
                supported: true,
                enabled: true,
            })
        },
        windows: || Ok(home_26h2()),
        recall: || Ok(RecallState::Installed),
        packages: |_| Ok(vec![[11, 2507, 2, 0]]),
        system_ai_models: || Ok(true),
    };

    /// Unmet with the kind and sentence of a missing requirement.
    fn missing(req: Requirement) -> Availability {
        Availability::Missing(Unmet {
            kind: Unavailable::NotOnThisPc,
            text: req.missing_text().to_string(),
        })
    }

    #[test]
    fn office_product_classifies_the_release_ids() {
        // Consumer editions ignore the policies.
        for consumer in [
            "O365HomePremRetail",
            "O365HomePremRetail,OneNoteFreeRetail",
            "HomeBusinessRetail",
            "HomeStudent2019Retail",
            "PersonalRetail",
            "O365HomePremRetail,SkypeforBusinessEntryRetail",
        ] {
            assert_eq!(
                classify_office(consumer).unwrap(),
                OfficeProduct::Consumer,
                "{consumer}"
            );
        }
        // Business, enterprise and volume editions honour them; a business id in the list wins.
        for business in [
            "O365ProPlusRetail",
            "O365BusinessRetail",
            "ProPlus2024Volume",
            "ProfessionalRetail",
            "Standard2024Volume",
            "O365HomePremRetail,O365ProPlusRetail",
            "VisioProRetail",
            "ProjectProRetail",
            "Word2021Retail,O365ProPlusRetail",
        ] {
            assert_eq!(
                classify_office(business).unwrap(),
                OfficeProduct::Business,
                "{business}"
            );
        }
        // No Office app the policies are for: OneNote free, a language pack, proofing tools or
        // the Access runtime alone, or an empty list.
        for none in [
            "OneNoteFreeRetail",
            "LanguagePack",
            "ProofingTools,AccessRuntimeRetail",
            "",
            "  ",
            ",",
        ] {
            assert_eq!(
                classify_office(none).unwrap(),
                OfficeProduct::None,
                "{none:?}"
            );
        }
        // A product Cairn cannot classify, such as a standalone app or Skype for Business (no
        // business Office despite its name), is an error: the scan then shows the tweak with a
        // "cannot check" warning instead of "Microsoft Office (2016 or later) is not installed."
        // or offering it.
        for (unknown, id) in [
            ("Word2021Retail", "Word2021Retail"),
            ("OneNoteFreeRetail,Excel2024Retail", "Excel2024Retail"),
            ("SkypeforBusinessEntryRetail", "SkypeforBusinessEntryRetail"),
            ("LyncEntryRetail", "LyncEntryRetail"),
        ] {
            assert_eq!(
                classify_office(unknown).unwrap_err().to_string(),
                format!("Cairn does not recognise the Office product {id}"),
                "{unknown}"
            );
        }
    }

    #[test]
    fn office_business_availability_tells_consumer_office_from_none() {
        let reqs = |product: OfficeProduct| {
            Requirements::new(Probes {
                office_product: match product {
                    OfficeProduct::Business => || Ok(OfficeProduct::Business),
                    OfficeProduct::Consumer => || Ok(OfficeProduct::Consumer),
                    OfficeProduct::None => || Ok(OfficeProduct::None),
                },
                ..ALL_MET
            })
        };
        assert_eq!(
            reqs(OfficeProduct::Business).check(Requirement::OfficeBusiness),
            Availability::Met
        );
        assert_eq!(
            reqs(OfficeProduct::Consumer).check(Requirement::OfficeBusiness),
            Availability::Missing(Unmet {
                kind: Unavailable::NotOnThisPc,
                text: "Microsoft 365 Home ignores Office policies.".to_string(),
            })
        );
        // The probe reads only Office 2016 and later (version 16.0), the version the policies
        // are for, so a PC with an older Office is told that no Office 2016 or later is there.
        assert_eq!(
            reqs(OfficeProduct::None).check(Requirement::OfficeBusiness),
            Availability::Missing(Unmet {
                kind: Unavailable::NotOnThisPc,
                text: "Microsoft Office (2016 or later) is not installed.".to_string(),
            })
        );
        assert_eq!(OFFICE_NOT_INSTALLED, Requirement::Office.missing_text());
        let unknown = Requirements::new(Probes {
            office_product: || Err(Error::Other("key locked".into())),
            ..ALL_MET
        });
        assert_eq!(
            unknown.check(Requirement::OfficeBusiness),
            Availability::Unknown("key locked".into())
        );
    }

    /// Unmet because of the Windows version, with its sentence.
    fn too_old_or_new(text: &str) -> Availability {
        Availability::Missing(Unmet {
            kind: Unavailable::WindowsVersion,
            text: text.to_string(),
        })
    }

    const RECALL_FLOOR: WindowsRange = WindowsRange {
        from: &[Build::new(26100, 3915)],
        through: None,
        editions: Editions::All,
    };
    const PAINT_FLOORS: WindowsRange = WindowsRange {
        from: &[Build::new(22621, 4870), Build::new(26100, 3360)],
        through: None,
        editions: Editions::All,
    };
    const LEGACY_COPILOT: WindowsRange = WindowsRange {
        from: &[Build::new(22621, 2361)],
        through: Some(22621),
        editions: Editions::All,
    };

    #[test]
    fn probes_run_at_most_once() {
        let probes = Probes {
            office: || {
                OFFICE_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            },
            office_product: || {
                OFFICE_PRODUCT_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(OfficeProduct::Consumer)
            },
            edge: || {
                EDGE_CALLS.fetch_add(1, Ordering::SeqCst);
                Err(Error::Other("probe failed".into()))
            },
            gpu: || {
                GPU_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(GpuScheduling {
                    supported: true,
                    enabled: false,
                })
            },
            windows: || {
                WINDOWS_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(home_26h2())
            },
            recall: || {
                RECALL_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(RecallState::Removed)
            },
            packages: |family| {
                let calls = if family == NOTEPAD_FAMILY {
                    &NOTEPAD_CALLS
                } else {
                    &PAINT_CALLS
                };
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            },
            system_ai_models: || {
                AI_MODELS_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            },
        };
        let reqs = Requirements::new(probes);
        for _ in 0..2 {
            assert_eq!(
                reqs.check(Requirement::Office),
                missing(Requirement::Office)
            );
            assert_eq!(
                reqs.check(Requirement::OfficeBusiness),
                missing(Requirement::OfficeBusiness),
                "consumer Office ignores the policies"
            );
            assert_eq!(
                reqs.check(Requirement::Edge),
                Availability::Unknown("probe failed".into())
            );
            assert_eq!(reqs.check(Requirement::GpuScheduling), Availability::Met);
            assert_eq!(reqs.windows(), Some(&home_26h2()));
            assert_eq!(
                reqs.check_windows(&RECALL_FLOOR),
                Availability::Met,
                "26300.9457 reaches 26100.3915"
            );
            for req in [
                Requirement::RecallComponent,
                Requirement::NotepadAi,
                Requirement::Paint,
                Requirement::SystemAiModels,
            ] {
                assert_eq!(reqs.check(req), missing(req), "{req:?}");
            }
        }
        assert_eq!(
            reqs.gpu(),
            Some(GpuScheduling {
                supported: true,
                enabled: false
            })
        );
        for (name, calls) in [
            ("office", &OFFICE_CALLS),
            ("office product", &OFFICE_PRODUCT_CALLS),
            ("edge", &EDGE_CALLS),
            ("gpu", &GPU_CALLS),
            ("windows", &WINDOWS_CALLS),
            ("recall", &RECALL_CALLS),
            ("notepad", &NOTEPAD_CALLS),
            ("paint", &PAINT_CALLS),
            ("system AI models", &AI_MODELS_CALLS),
        ] {
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{name} probe");
        }

        let failing = Requirements::new(Probes {
            gpu: || Err(Error::Other("no adapters".into())),
            windows: || Err(Error::Other("no build number".into())),
            ..ALL_MET
        });
        assert_eq!(
            failing.check(Requirement::GpuScheduling),
            Availability::Unknown("no adapters".into())
        );
        assert_eq!(failing.gpu(), None);
        assert_eq!(failing.windows(), None);
        assert_eq!(failing.check(Requirement::Office), Availability::Met);
    }

    /// A tweak with only the fields `check_tweak` reads set apart from defaults.
    fn tweak(
        requires: Option<Requirement>,
        windows: WindowsRange,
        retired: Option<&'static str>,
    ) -> Tweak {
        Tweak {
            id: "selftest.requirements",
            category: Category::Privacy,
            topic: Topic::Telemetry,
            title: "Sandbox tweak",
            description: "Stands in for a catalog tweak.",
            risk: Risk::Low,
            default_on: false,
            restart: RestartNeed::None,
            requires,
            windows,
            retired,
            undo_limit: None,
            actions: &[],
            former: &[],
        }
    }

    #[test]
    fn check_tweak_puts_retired_before_the_range_and_the_requirement() {
        const RETIRED: &str = "Cairn found no way to observe an effect of this setting.";
        let no_office = Requirements::new(Probes {
            office: || Ok(false),
            windows: || Ok(pc("22631.4890", "Core")),
            ..ALL_MET
        });
        assert_eq!(
            no_office.check_tweak(&tweak(
                Some(Requirement::Office),
                RECALL_FLOOR,
                Some(RETIRED)
            )),
            Availability::Missing(Unmet {
                kind: Unavailable::Retired,
                text: RETIRED.to_string(),
            })
        );
        let too_old = "Needs Windows 11 24H2 (build 26100.3915) or later; this PC runs \
                       Windows 11 Home 23H2 (build 22631.4890).";
        assert_eq!(
            no_office.check_tweak(&tweak(Some(Requirement::Office), RECALL_FLOOR, None)),
            too_old_or_new(too_old),
            "the range comes before the requirement"
        );
        assert_eq!(
            no_office.check_tweak(&tweak(Some(Requirement::Office), ANY_WINDOWS, None)),
            missing(Requirement::Office)
        );
        assert_eq!(
            no_office.check_tweak(&tweak(Some(Requirement::Edge), ANY_WINDOWS, None)),
            Availability::Met
        );
        assert_eq!(
            no_office.check_tweak(&tweak(None, ANY_WINDOWS, None)),
            Availability::Met
        );

        // An edition Cairn does not know: a missing requirement still wins, otherwise the
        // edition warning is the answer.
        let pro_only = WindowsRange {
            editions: Editions::ProOrHigher,
            ..ANY_WINDOWS
        };
        let weird = Requirements::new(Probes {
            office: || Ok(false),
            windows: || Ok(pc("26300.9457", "Weird")),
            ..ALL_MET
        });
        assert_eq!(
            weird.check_tweak(&tweak(Some(Requirement::Office), pro_only, None)),
            missing(Requirement::Office)
        );
        assert_eq!(
            weird.check_tweak(&tweak(Some(Requirement::Edge), pro_only, None)),
            Availability::Unknown(
                "cannot check whether this edition of Windows uses this setting: Cairn does \
                 not know the edition \"Weird\""
                    .into()
            )
        );
    }

    #[test]
    fn check_tweak_names_the_requirement_of_a_failed_check() {
        let failed = Requirements::new(Probes {
            edge: || Err(Error::Other("access denied".into())),
            packages: |_| Err(Error::Other("lookup failed".into())),
            ..ALL_MET
        });
        assert_eq!(
            failed.check_tweak(&tweak(Some(Requirement::Edge), ANY_WINDOWS, None)),
            Availability::Unknown(
                "cannot check whether Microsoft Edge is installed: access denied".into()
            )
        );
        assert_eq!(
            failed.check_tweak(&tweak(Some(Requirement::NotepadAi), ANY_WINDOWS, None)),
            Availability::Unknown(
                "cannot check whether Notepad 11.2503.16 or later is installed: lookup failed"
                    .into()
            )
        );
    }

    #[test]
    fn any_windows_is_met_without_reading_the_windows_facts() {
        let reqs = Requirements::new(Probes {
            windows: || panic!("the Windows facts were read"),
            ..ALL_MET
        });
        assert_eq!(reqs.check_windows(&ANY_WINDOWS), Availability::Met);
        assert_eq!(
            reqs.check_tweak(&tweak(None, ANY_WINDOWS, None)),
            Availability::Met
        );
        assert_eq!(reqs.windows_warning(), None);
    }

    #[test]
    fn unreadable_facts_meet_every_range_and_warn_once() {
        let failing = Requirements::new(Probes {
            windows: || Err(Error::Other("no build number".into())),
            ..ALL_MET
        });
        assert_eq!(
            failing.windows_warning(),
            None,
            "nothing needed the facts yet"
        );
        for range in [RECALL_FLOOR, LEGACY_COPILOT, PAINT_FLOORS] {
            assert_eq!(failing.check_windows(&range), Availability::Met);
        }
        assert_eq!(
            failing.windows_warning().as_deref(),
            Some(
                "Cannot read the Windows version (no build number); tweaks meant for some \
                 Windows versions or editions are shown as available."
            )
        );
        let readable = Requirements::new(ALL_MET);
        assert_eq!(readable.check_windows(&RECALL_FLOOR), Availability::Met);
        assert_eq!(readable.windows_warning(), None);
    }

    #[test]
    fn requirement_constants_match_their_texts() {
        let [major, minor, build, _] = NOTEPAD_AI_MIN;
        assert!(Requirement::NotepadAi
            .missing_text()
            .starts_with(&format!("Notepad {major}.{minor}.{build} or later")));
        assert!(Requirement::NotepadAi
            .subject()
            .starts_with(&format!("Notepad {major}.{minor}.{build} or later")));
        for family in [NOTEPAD_FAMILY, PAINT_FAMILY] {
            assert!(family.ends_with("_8wekyb3d8bbwe"), "{family}");
        }
        assert!(RECALL_RECORD.ends_with(r"\OptionalFeatures\Recall"));
        assert!(SYSTEM_AI_MODELS_CONSENT.ends_with(r"\ConsentStore\systemAIModels"));
    }

    #[test]
    fn a_positive_reading_wins_over_a_failed_one() {
        let failed = || Err(Error::Other("access denied".into()));
        assert!(decide(vec![failed(), Ok(sz("x"))], |v| v[1].is_some()).unwrap());
        let err = decide(vec![failed(), Ok(None)], |v| v.iter().any(Option::is_some))
            .unwrap_err()
            .to_string();
        assert!(err.contains("access denied"), "{err}");
        assert!(!decide(vec![Ok(None), Ok(None)], |v| v.iter().any(Option::is_some)).unwrap());
    }

    #[test]
    fn servicing_family_maps_enablement_builds() {
        for (number, family) in [
            (22631, 22621),
            (26200, 26100),
            (26300, 26100),
            (26100, 26100),
            (22621, 22621),
            (28000, 28000),
            (99999, 99999),
        ] {
            assert_eq!(servicing_family(number), family, "build {number}");
        }
    }

    #[test]
    fn version_tables_are_sorted_and_labelled() {
        assert!(VERSIONS.windows(2).all(|w| w[0].0 < w[1].0));
        for &(build, family) in SERVICING_FAMILIES {
            assert!(family < build, "{build} is serviced with an older build");
            assert!(version_label(build).is_some() && version_label(family).is_some());
            assert_eq!(
                servicing_family(family),
                family,
                "{family} is a family base"
            );
        }
        assert_eq!(version_label(26100), Some("24H2"));
        assert_eq!(version_label(26300), Some("26H2"));
        assert_eq!(version_label(26101), None);
        assert_eq!(family_label(22000), Some("21H2"));
        assert_eq!(family_label(22621), Some("23H2"));
        assert_eq!(family_label(26100), Some("26H2"));
        assert_eq!(family_label(28000), Some("26H1"));
        assert_eq!(family_label(26300), None);
        assert_eq!(next_family_label(22000), Some("22H2"));
        assert_eq!(next_family_label(22621), Some("24H2"));
        assert_eq!(next_family_label(26100), Some("26H1"));
        assert_eq!(next_family_label(28000), None);
    }

    #[test]
    fn edition_classes() {
        use EditionClass::*;
        for (id, class) in [
            ("Core", Home),
            ("CoreN", Home),
            ("CoreSingleLanguage", Home),
            ("CoreCountrySpecific", Home),
            ("Professional", Pro),
            ("ProfessionalN", Pro),
            ("ProfessionalWorkstation", Pro),
            ("ProfessionalEducation", Pro),
            ("Enterprise", Enterprise),
            ("EnterpriseS", Enterprise),
            ("EnterpriseN", Enterprise),
            ("IoTEnterprise", Enterprise),
            ("IoTEnterpriseS", Enterprise),
            ("ServerRdsh", Enterprise),
            ("Education", Education),
            ("EducationN", Education),
            (" Core ", Home),
            ("", Unknown),
            ("Weird", Unknown),
            ("core", Unknown),
            ("ServerStandard", Unknown),
        ] {
            assert_eq!(edition_class(id), class, "{id:?}");
        }
    }

    #[test]
    fn edition_readers_agree() {
        for id in [
            "Core",
            "CoreN",
            "CoreSingleLanguage",
            "CoreCountrySpecific",
            "Professional",
            "ProfessionalWorkstation",
            "ProfessionalEducation",
            "Enterprise",
            "EnterpriseS",
            "IoTEnterprise",
            "Education",
            "ServerRdsh",
            "ServerStandard",
            "CloudEdition",
            " Core ",
            "core",
            "",
            "Weird",
        ] {
            let wu = crate::updates::wu::Edition::from_parts(
                Some(id),
                None,
                Some("26H2"),
                Some("26300"),
                Some(9457),
            );
            assert_eq!(
                wu.home,
                edition_class(id) == EditionClass::Home,
                "{id:?} is Home for one reader and not for the other"
            );
            let facts =
                facts_from(Some("26300"), Some(9457), Some("26H2"), Some(id), None).unwrap();
            assert_eq!(facts.name, wu.name, "{id:?}");
        }
    }

    #[test]
    fn facts_from_registry_parts() {
        let facts = facts_from(
            Some("26300"),
            Some(9457),
            Some("26H2"),
            Some("Core"),
            Some("Windows 10 Home"),
        )
        .unwrap();
        assert_eq!(
            facts,
            home_26h2(),
            "Windows 11 still names itself Windows 10"
        );
        assert_eq!(pc("26300.9457", "Core"), home_26h2());

        let no_ubr =
            facts_from(Some(" 22631 "), None, Some(" "), Some("Professional"), None).unwrap();
        assert_eq!(no_ubr.build, Build::new(22631, 0));
        assert_eq!(no_ubr.family, 22621);
        assert_eq!(no_ubr.version, None);
        assert_eq!(no_ubr.edition, EditionClass::Pro);
        assert_eq!(no_ubr.name, "Windows 11 Pro");

        let no_edition = facts_from(Some("26100"), Some(1), None, None, None).unwrap();
        assert_eq!(no_edition.edition_id, "");
        assert_eq!(no_edition.edition, EditionClass::Unknown);
        assert_eq!(no_edition.name, "Windows 11");

        for (build, error) in [
            (None, "the build number is missing"),
            (Some("  "), "the build number is missing"),
            (
                Some("26300a"),
                "the build number \"26300a\" is not a number",
            ),
        ] {
            let e = facts_from(build, Some(1), None, Some("Core"), None).unwrap_err();
            assert_eq!(e.to_string(), error, "{build:?}");
        }
    }

    #[test]
    fn windows_facts_serialize_as_the_scan_report_shows_them() {
        let facts = home_26h2();
        let json = serde_json::to_value(&facts).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "build": {"number": 26300, "revision": 9457}, "family": 26100,
                "version": "26H2", "edition_id": "Core", "edition": "home",
                "name": "Windows 11 Home"
            })
        );
        assert_eq!(serde_json::from_value::<WindowsFacts>(json).unwrap(), facts);
    }

    /// The kind of an unmet range on `pc`, `None` when it is met.
    fn unmet(range: &WindowsRange, pc: &WindowsFacts) -> Option<Unavailable> {
        match range_availability(range, pc) {
            Availability::Met => None,
            Availability::Missing(unmet) => Some(unmet.kind),
            Availability::Unknown(e) => panic!("{e}"),
        }
    }

    #[test]
    fn range_truth_table() {
        let version = Some(Unavailable::WindowsVersion);
        let edition = Some(Unavailable::Edition);
        let home = |build: &str| pc(build, "Core");
        for (build, expected) in [
            ("26300.9457", None),
            ("26100.3915", None),
            ("26100.3000", version),
            ("22631.4890", version),
            ("28000.1", None),
        ] {
            assert_eq!(unmet(&RECALL_FLOOR, &home(build)), expected, "{build}");
        }
        for (build, expected) in [
            ("22631.4900", None),
            ("22621.4870", None),
            ("22631.4317", version),
            ("26200.3000", version),
            ("26200.5000", None),
            ("22000.2000", version),
            ("28000.1", None),
        ] {
            assert_eq!(unmet(&PAINT_FLOORS, &home(build)), expected, "{build}");
        }
        let through = WindowsRange {
            through: Some(22621),
            ..ANY_WINDOWS
        };
        for (build, expected) in [
            ("22000.100", None),
            ("22631.1", None),
            ("26100.1", version),
            ("28000.1", version),
        ] {
            assert_eq!(unmet(&through, &home(build)), expected, "{build}");
        }
        for (build, expected) in [
            ("22621.2000", version),
            ("22631.2361", None),
            ("26300.9457", version),
        ] {
            assert_eq!(unmet(&LEGACY_COPILOT, &home(build)), expected, "{build}");
        }

        let pro_only = WindowsRange {
            editions: Editions::ProOrHigher,
            ..ANY_WINDOWS
        };
        let enterprise_only = WindowsRange {
            editions: Editions::EnterpriseOrEducation,
            ..ANY_WINDOWS
        };
        for (id, pro, enterprise) in [
            ("Core", edition, edition),
            ("Professional", None, edition),
            ("Enterprise", None, None),
            ("Education", None, None),
        ] {
            let facts = pc("26300.9457", id);
            assert_eq!(unmet(&pro_only, &facts), pro, "{id}");
            assert_eq!(unmet(&enterprise_only, &facts), enterprise, "{id}");
        }
        let weird = pc("26300.9457", "Weird");
        assert!(matches!(
            range_availability(&pro_only, &weird),
            Availability::Unknown(_)
        ));
        assert_eq!(
            unmet(&ANY_WINDOWS, &weird),
            None,
            "every edition needs no edition"
        );
        // The build is checked before the edition.
        let new_and_pro = WindowsRange {
            editions: Editions::ProOrHigher,
            ..RECALL_FLOOR
        };
        assert_eq!(unmet(&new_and_pro, &home("22631.4890")), version);
        assert_eq!(unmet(&new_and_pro, &home("26300.9457")), edition);
    }

    /// A floor on the 25H2 enablement build.
    const ENABLEMENT_FLOOR: WindowsRange = WindowsRange {
        from: &[Build::new(26200, 0)],
        through: None,
        editions: Editions::All,
    };

    #[test]
    fn enablement_floor_is_not_met_by_24h2() {
        let floor = ENABLEMENT_FLOOR;
        assert_eq!(
            range_availability(&floor, &pc("26100.9550", "Core")),
            too_old_or_new(
                "Needs Windows 11 25H2 (build 26200.0) or later; this PC runs Windows 11 Home \
                 24H2 (build 26100.9550)."
            )
        );
        for build in ["26200.0", "26200.9550", "26300.9457", "28000.1"] {
            assert_eq!(
                range_availability(&floor, &pc(build, "Core")),
                Availability::Met,
                "{build}"
            );
        }
        const REVISION_FLOOR: WindowsRange = WindowsRange {
            from: &[Build::new(26200, 5000)],
            through: None,
            editions: Editions::All,
        };
        let revision_floor = REVISION_FLOOR;
        assert_eq!(
            unmet(&revision_floor, &pc("26200.4999", "Core")),
            Some(Unavailable::WindowsVersion)
        );
        assert_eq!(
            unmet(&revision_floor, &pc("26100.9999", "Core")),
            Some(Unavailable::WindowsVersion),
            "24H2 never reaches a 25H2 floor, whatever its revision"
        );
        assert_eq!(unmet(&revision_floor, &pc("26300.5000", "Core")), None);
    }

    #[test]
    fn range_texts_are_exact() {
        let home = home_26h2();
        assert_eq!(
            range_availability(&LEGACY_COPILOT, &home),
            too_old_or_new(
                "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 \
                 Home 26H2 (build 26300.9457)."
            )
        );
        assert_eq!(
            range_availability(&RECALL_FLOOR, &pc("22631.4890", "Core")),
            too_old_or_new(
                "Needs Windows 11 24H2 (build 26100.3915) or later; this PC runs Windows 11 Home \
                 23H2 (build 22631.4890)."
            )
        );
        assert_eq!(
            range_availability(&PAINT_FLOORS, &pc("22000.2000", "Core")),
            too_old_or_new(
                "Needs Windows 11 22H2 (build 22621.4870 or later) or 24H2 (build 26100.3360 or \
                 later); this PC runs Windows 11 Home 21H2 (build 22000.2000)."
            )
        );
        const THREE_FLOORS: WindowsRange = WindowsRange {
            from: &[
                Build::new(22621, 1),
                Build::new(26100, 1),
                Build::new(28000, 5),
            ],
            through: None,
            editions: Editions::All,
        };
        assert_eq!(
            range_availability(&THREE_FLOORS, &pc("22000.2000", "Core")),
            too_old_or_new(
                "Needs Windows 11 22H2 (build 22621.1 or later), 24H2 (build 26100.1 or later) \
                 or 26H1 (build 28000.5 or later); this PC runs Windows 11 Home 21H2 (build \
                 22000.2000)."
            )
        );
        let pro_only = WindowsRange {
            editions: Editions::ProOrHigher,
            ..ANY_WINDOWS
        };
        assert_eq!(
            range_availability(&pro_only, &home),
            Availability::Missing(Unmet {
                kind: Unavailable::Edition,
                text: "Needs Windows 11 Pro, Enterprise or Education; this PC runs Windows 11 \
                       Home."
                    .into(),
            })
        );
        let enterprise_only = WindowsRange {
            editions: Editions::EnterpriseOrEducation,
            ..ANY_WINDOWS
        };
        assert_eq!(
            range_availability(&enterprise_only, &pc("26300.9457", "Professional")),
            Availability::Missing(Unmet {
                kind: Unavailable::Edition,
                text: "Needs Windows 11 Enterprise or Education; this PC runs Windows 11 Pro."
                    .into(),
            })
        );
        let no_version = WindowsFacts {
            version: None,
            ..home.clone()
        };
        assert_eq!(
            range_availability(&LEGACY_COPILOT, &no_version),
            too_old_or_new(
                "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 \
                 Home (build 26300.9457)."
            )
        );
        let unknown_next = WindowsRange {
            through: Some(28000),
            ..ANY_WINDOWS
        };
        let future = facts_from(Some("30000"), Some(1), None, Some("Core"), None).unwrap();
        assert_eq!(
            range_availability(&unknown_next, &future),
            too_old_or_new(
                "Windows 11 build 30000 and later no longer use this setting; this PC runs \
                 Windows 11 Home (build 30000.1)."
            )
        );
        for text in [
            NEEDS_NEWER_BUILD,
            NO_LONGER_USED,
            NEEDS_PRO,
            NEEDS_ENTERPRISE,
        ] {
            assert!(
                text.ends_with('.') && text.contains("; this PC runs {"),
                "{text}"
            );
        }
    }

    #[test]
    fn floors_text_agrees_with_the_catalog_description() {
        for range in [RECALL_FLOOR, PAINT_FLOORS, ENABLEMENT_FLOOR] {
            assert_eq!(
                range.describe(),
                Some(format!("Windows 11 {}", floors_text(range.from))),
                "{range:?}"
            );
        }
        assert_eq!(
            floors_text(&[Build::new(26101, 7)]),
            "build 26101.7 or later",
            "a build without a label"
        );
    }

    #[test]
    fn recall_from_cbs_values() {
        use RecallState::*;
        let dword = |v: u32| RegValue::Dword(v);
        assert_eq!(recall_from(true, Some(&dword(1))).unwrap(), Removed);
        assert_eq!(recall_from(true, Some(&dword(2))).unwrap(), Removed);
        assert_eq!(recall_from(true, Some(&dword(0))).unwrap(), Installed);
        assert_eq!(recall_from(true, None).unwrap(), Installed);
        assert_eq!(recall_from(false, None).unwrap(), Absent);
        let e = recall_from(true, Some(&RegValue::Sz("1".into()))).unwrap_err();
        assert!(e.to_string().contains("unexpected Removed value"), "{e}");
    }

    #[test]
    fn recall_key_absent_is_missing_with_the_no_sign_text() {
        let text = "Cairn found no sign of Recall on this PC.";
        assert_eq!(Requirement::RecallComponent.missing_text(), text);
        for probes in [
            Probes {
                recall: || Ok(RecallState::Absent),
                ..ALL_MET
            },
            Probes {
                recall: || Ok(RecallState::Removed),
                ..ALL_MET
            },
        ] {
            assert_eq!(
                Requirements::new(probes).check(Requirement::RecallComponent),
                Availability::Missing(Unmet {
                    kind: Unavailable::NotOnThisPc,
                    text: text.to_string(),
                })
            );
        }
        assert_eq!(
            Requirements::new(ALL_MET).check(Requirement::RecallComponent),
            Availability::Met
        );
        let failed = Requirements::new(Probes {
            recall: || Err(Error::Other("access denied".into())),
            ..ALL_MET
        });
        assert_eq!(
            failed.check(Requirement::RecallComponent),
            Availability::Unknown("access denied".into())
        );
    }

    #[test]
    fn notepad_version_gate_uses_the_newest_package() {
        let gate = |packages: fn(&str) -> Result<Vec<[u16; 4]>>| {
            Requirements::new(Probes {
                packages,
                ..ALL_MET
            })
            .check(Requirement::NotepadAi)
        };
        let met = Availability::Met;
        let not_met = missing(Requirement::NotepadAi);
        assert_eq!(gate(|_| Ok(vec![[11, 2402, 22, 0], [11, 2507, 2, 0]])), met);
        assert_eq!(gate(|_| Ok(vec![[11, 2507, 2, 0], [11, 2402, 22, 0]])), met);
        assert_eq!(
            gate(|_| Ok(vec![[11, 2503, 16, 0]])),
            met,
            "the minimum itself"
        );
        assert_eq!(gate(|_| Ok(vec![[12, 0, 0, 0]])), met);
        assert_eq!(gate(|_| Ok(vec![[11, 2503, 15, 9]])), not_met);
        assert_eq!(
            gate(|_| Ok(vec![[10, 9999, 0, 0], [11, 2402, 0, 0]])),
            not_met
        );
        assert_eq!(gate(|_| Ok(Vec::new())), not_met);
        assert_eq!(
            gate(|_| Err(Error::Other("lookup failed".into()))),
            Availability::Unknown("lookup failed".into())
        );
        // Each requirement asks for its own package family.
        let reqs = Requirements::new(Probes {
            packages: |family| match family {
                PAINT_FAMILY => Ok(vec![[11, 2508, 361, 0]]),
                NOTEPAD_FAMILY => Ok(vec![[11, 2402, 22, 0]]),
                other => panic!("{other} was asked for"),
            },
            ..ALL_MET
        });
        assert_eq!(reqs.check(Requirement::NotepadAi), not_met);
        assert_eq!(reqs.check(Requirement::Paint), Availability::Met);
    }

    #[test]
    fn paint_and_text_generation_requirements() {
        let paint = |packages: fn(&str) -> Result<Vec<[u16; 4]>>| {
            Requirements::new(Probes {
                packages,
                ..ALL_MET
            })
            .check(Requirement::Paint)
        };
        assert_eq!(paint(|_| Ok(vec![[0, 0, 0, 0]])), Availability::Met);
        assert_eq!(paint(|_| Ok(Vec::new())), missing(Requirement::Paint));
        assert_eq!(
            paint(|_| Err(Error::Other("lookup failed".into()))),
            Availability::Unknown("lookup failed".into())
        );
        let models = |system_ai_models: fn() -> Result<bool>| {
            Requirements::new(Probes {
                system_ai_models,
                ..ALL_MET
            })
            .check(Requirement::SystemAiModels)
        };
        assert_eq!(models(|| Ok(true)), Availability::Met);
        assert_eq!(models(|| Ok(false)), missing(Requirement::SystemAiModels));
        assert_eq!(
            models(|| Err(Error::Other("denied".into()))),
            Availability::Unknown("denied".into())
        );
    }

    #[test]
    fn package_versions_come_from_the_full_name() {
        for (full_name, version) in [
            (
                "Microsoft.WindowsNotepad_11.2507.26.0_x64__8wekyb3d8bbwe",
                Some([11, 2507, 26, 0]),
            ),
            (
                "Microsoft.Paint_11.2508.361.0_arm64__8wekyb3d8bbwe",
                Some([11, 2508, 361, 0]),
            ),
            (
                "Microsoft.Paint_11.2508.361.0_neutral_~_8wekyb3d8bbwe",
                Some([11, 2508, 361, 0]),
            ),
            ("Microsoft.Paint", None),
            ("Microsoft.Paint_11.2508_x64__8wekyb3d8bbwe", None),
            ("Microsoft.Paint_11.2508.1.0.5_x64__8wekyb3d8bbwe", None),
            ("Microsoft.Paint_11.70000.1.0_x64__8wekyb3d8bbwe", None),
            ("Microsoft.Paint_11.x.1.0_x64__8wekyb3d8bbwe", None),
        ] {
            assert_eq!(package_version(full_name), version, "{full_name}");
        }
        assert_eq!(
            package_versions("PCOptimizer.SelfTest.Missing_0000000000000").unwrap(),
            Vec::<[u16; 4]>::new(),
            "a family with no installed package"
        );
    }

    #[test]
    fn unmet_conditions_carry_their_kind_and_sentence() {
        for (kind, name) in [
            (Unavailable::NotOnThisPc, "not_on_this_pc"),
            (Unavailable::WindowsVersion, "windows_version"),
            (Unavailable::Edition, "edition"),
            (Unavailable::Retired, "retired"),
        ] {
            let unmet = Unmet {
                kind,
                text: format!("{name}."),
            };
            assert_eq!(serde_json::to_value(unmet.kind).unwrap(), name);
            assert!(unmet.text.ends_with('.'));
        }
    }

    #[test]
    fn system_probes_are_read_only() {
        for line in probe_lines() {
            println!("{line}");
        }
        let reqs = Requirements::new(SYSTEM_PROBES);
        for req in [
            Requirement::Office,
            Requirement::Edge,
            Requirement::GpuScheduling,
            Requirement::RecallComponent,
            Requirement::NotepadAi,
            Requirement::Paint,
            Requirement::SystemAiModels,
        ] {
            println!("{req:?}: {:?}", reqs.check(req));
        }
        for range in [RECALL_FLOOR, PAINT_FLOORS, LEGACY_COPILOT] {
            println!("{:?}: {:?}", range.describe(), reqs.check_windows(&range));
        }
        let facts = windows_facts().expect("the Windows facts of this PC can be read");
        assert!(facts.family >= 22000, "{facts:?}");
        assert_eq!(facts.family, servicing_family(facts.build.number));
        assert_eq!(reqs.windows(), Some(&facts));
        assert_eq!(reqs.windows_warning(), None);
    }
}
