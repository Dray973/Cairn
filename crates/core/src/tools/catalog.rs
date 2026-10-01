//! The maintenance tools, the exact command each one runs, and how its exit code is judged.
//!
//! Every tool is a program in System32. Only Check Disk can be stopped: it does its work in
//! its own process, while System File Checker, DISM and Optimize Drives hand theirs to
//! Windows services that finish it regardless. The two repairs must run outside Cairn's
//! job object so that closing Cairn cannot end them halfway.

use serde::{Deserialize, Serialize};

use super::runner::JobState;
use super::store_health::StoreHealth;
use crate::win::console_text::{oem_code_page, TextEncoding};
use crate::{Error, Result};

/// A maintenance tool. The serialized ids are stable and used by the UI and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolId {
    SfcVerify,
    SfcScan,
    DismCheck,
    DismScan,
    DismRestore,
    DriveOptimize,
    DriveRetrim,
    DiskCheck,
}

impl ToolId {
    /// Every tool, in display order.
    pub const ALL: [ToolId; 8] = [
        ToolId::SfcVerify,
        ToolId::SfcScan,
        ToolId::DismCheck,
        ToolId::DismScan,
        ToolId::DismRestore,
        ToolId::DriveOptimize,
        ToolId::DriveRetrim,
        ToolId::DiskCheck,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolId::SfcVerify => "sfc_verify",
            ToolId::SfcScan => "sfc_scan",
            ToolId::DismCheck => "dism_check",
            ToolId::DismScan => "dism_scan",
            ToolId::DismRestore => "dism_restore",
            ToolId::DriveOptimize => "drive_optimize",
            ToolId::DriveRetrim => "drive_retrim",
            ToolId::DiskCheck => "disk_check",
        }
    }

    /// Parses an id such as `sfc_verify`, ignoring ASCII case and surrounding whitespace.
    pub fn parse(s: &str) -> Option<ToolId> {
        let s = s.trim();
        ToolId::ALL
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s))
    }

    pub fn info(self) -> &'static ToolInfo {
        &CATALOG[self as usize]
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// Section of the Tools tab a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    SystemFiles,
    Drives,
}

/// How a tool writes its output when it goes to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputEncoding {
    Utf16Le,
    /// The OEM code page of this installation.
    Oem,
}

impl OutputEncoding {
    pub(crate) fn text_encoding(self) -> TextEncoding {
        match self {
            OutputEncoding::Utf16Le => TextEncoding::Utf16Le,
            OutputEncoding::Oem => TextEncoding::CodePage(oem_code_page()),
        }
    }
}

/// A program that must not be running when a tool starts, and the name it is shown with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Conflict {
    /// Lowercase executable name.
    pub(crate) exe: &'static str,
    pub(crate) name: &'static str,
}

/// The programs a tool conflicts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Family {
    pub(crate) conflicts: &'static [Conflict],
    /// Whether a running Windows Modules Installer worker (`tiworker.exe`, installing
    /// updates) is worth a note: the tool may wait for it.
    pub(crate) servicing_note: bool,
}

/// What a disabled service means for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IfDisabled {
    /// The tool cannot run; the text says why.
    Block(&'static str),
    /// The tool runs but may not work fully; the text says why.
    Note(&'static str),
}

/// A service a tool depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServiceNeed {
    pub(crate) name: &'static str,
    pub(crate) display: &'static str,
    pub(crate) if_disabled: IfDisabled,
}

/// One maintenance tool as the catalog describes it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub id: ToolId,
    pub group: ToolGroup,
    pub title: &'static str,
    /// Label of the button that runs it.
    pub verb: &'static str,
    pub description: &'static str,
    /// Drive tools run on one fixed volume.
    pub needs_volume: bool,
    pub requires_admin: bool,
    /// Only a tool that does its work in its own process can be stopped safely.
    pub cancellable: bool,
    /// The tool refuses to start unless it can run outside Cairn's job object.
    pub requires_detach: bool,
    /// The tool changes the system (repairs or optimizes); the others only check.
    pub changes_system: bool,
    pub duration_hint: &'static str,
    /// File name of the program in System32.
    pub program: &'static str,
    /// Arguments after the program (and after the drive, for drive tools).
    #[serde(skip)]
    pub(crate) args: &'static [&'static str],
    #[serde(skip)]
    pub(crate) encoding: OutputEncoding,
    #[serde(skip)]
    pub(crate) family: Family,
    #[serde(skip)]
    pub(crate) services: &'static [ServiceNeed],
}

const SERVICING: Family = Family {
    conflicts: &[
        Conflict {
            exe: "sfc.exe",
            name: "System File Checker",
        },
        Conflict {
            exe: "dism.exe",
            name: "DISM",
        },
        Conflict {
            exe: "dismhost.exe",
            name: "DISM",
        },
    ],
    servicing_note: true,
};

const DEFRAG: Family = Family {
    conflicts: &[Conflict {
        exe: "defrag.exe",
        name: "Optimize Drives",
    }],
    servicing_note: false,
};

const CHKDSK: Family = Family {
    conflicts: &[Conflict {
        exe: "chkdsk.exe",
        name: "Check Disk",
    }],
    servicing_note: false,
};

const MODULES_INSTALLER: ServiceNeed = ServiceNeed {
    name: "TrustedInstaller",
    display: "Windows Modules Installer",
    if_disabled: IfDisabled::Block(
        "The Windows Modules Installer service is disabled; System File Checker and DISM need it.",
    ),
};

const WINDOWS_UPDATE: ServiceNeed = ServiceNeed {
    name: "wuauserv",
    display: "Windows Update",
    if_disabled: IfDisabled::Note(
        "Windows Update is disabled, so DISM probably can't download repair files.",
    ),
};

const OPTIMIZE_DRIVES: ServiceNeed = ServiceNeed {
    name: "defragsvc",
    display: "Optimize drives",
    if_disabled: IfDisabled::Block("The Optimize drives service is disabled."),
};

/// Indexed by `ToolId as usize`.
static CATALOG: [ToolInfo; 8] = [
    ToolInfo {
        id: ToolId::SfcVerify,
        group: ToolGroup::SystemFiles,
        title: "Check system files",
        verb: "Check",
        description: "System File Checker compares protected Windows files with known-good \
                      copies and reports problems without changing anything.",
        needs_volume: false,
        requires_admin: true,
        cancellable: false,
        requires_detach: false,
        changes_system: false,
        duration_hint: "usually 10–30 minutes",
        program: "sfc.exe",
        args: &["/verifyonly"],
        encoding: OutputEncoding::Utf16Le,
        family: SERVICING,
        services: &[MODULES_INSTALLER],
    },
    ToolInfo {
        id: ToolId::SfcScan,
        group: ToolGroup::SystemFiles,
        title: "Repair system files",
        verb: "Repair",
        description: "System File Checker replaces damaged or missing protected Windows files \
                      with good copies from the component store.",
        needs_volume: false,
        requires_admin: true,
        cancellable: false,
        requires_detach: true,
        changes_system: true,
        duration_hint: "usually 10–30 minutes",
        program: "sfc.exe",
        args: &["/scannow"],
        encoding: OutputEncoding::Utf16Le,
        family: SERVICING,
        services: &[MODULES_INSTALLER],
    },
    ToolInfo {
        id: ToolId::DismCheck,
        group: ToolGroup::SystemFiles,
        title: "Check the component store",
        verb: "Check",
        description: "DISM reports whether Windows' component store (the source System File \
                      Checker repairs from) is already marked as damaged.",
        needs_volume: false,
        requires_admin: true,
        cancellable: false,
        requires_detach: false,
        changes_system: false,
        duration_hint: "under a minute",
        program: "dism.exe",
        args: &["/Online", "/Cleanup-Image", "/CheckHealth"],
        encoding: OutputEncoding::Oem,
        family: SERVICING,
        services: &[MODULES_INSTALLER],
    },
    ToolInfo {
        id: ToolId::DismScan,
        group: ToolGroup::SystemFiles,
        title: "Scan the component store",
        verb: "Scan",
        description: "DISM scans the component store for damage. Nothing is repaired.",
        needs_volume: false,
        requires_admin: true,
        cancellable: false,
        requires_detach: false,
        changes_system: false,
        duration_hint: "usually 5–20 minutes",
        program: "dism.exe",
        args: &["/Online", "/Cleanup-Image", "/ScanHealth"],
        encoding: OutputEncoding::Oem,
        family: SERVICING,
        services: &[MODULES_INSTALLER],
    },
    ToolInfo {
        id: ToolId::DismRestore,
        group: ToolGroup::SystemFiles,
        title: "Repair the component store",
        verb: "Repair",
        description: "DISM downloads good copies of damaged components from Windows Update and \
                      repairs the component store. Run Repair system files again afterwards.",
        needs_volume: false,
        requires_admin: true,
        cancellable: false,
        requires_detach: true,
        changes_system: true,
        duration_hint: "usually 10–60 minutes; needs internet",
        program: "dism.exe",
        args: &["/Online", "/Cleanup-Image", "/RestoreHealth", "/NoRestart"],
        encoding: OutputEncoding::Oem,
        family: SERVICING,
        services: &[MODULES_INSTALLER, WINDOWS_UPDATE],
    },
    ToolInfo {
        id: ToolId::DriveOptimize,
        group: ToolGroup::Drives,
        title: "Optimize drive",
        verb: "Optimize",
        description: "Runs the optimization Windows picks for the drive: TRIM for SSDs, \
                      defragmentation for hard disks.",
        needs_volume: true,
        requires_admin: true,
        cancellable: false,
        requires_detach: false,
        changes_system: true,
        duration_hint: "SSD: about a minute; hard disk: minutes to hours",
        program: "defrag.exe",
        args: &["/O", "/U", "/V"],
        encoding: OutputEncoding::Oem,
        family: DEFRAG,
        services: &[OPTIMIZE_DRIVES],
    },
    ToolInfo {
        id: ToolId::DriveRetrim,
        group: ToolGroup::Drives,
        title: "Retrim SSD",
        verb: "Retrim",
        description: "Tells the SSD which blocks are free so it keeps its write speed. Only for \
                      SSDs and thin-provisioned drives.",
        needs_volume: true,
        requires_admin: true,
        cancellable: false,
        requires_detach: false,
        changes_system: true,
        duration_hint: "about a minute",
        program: "defrag.exe",
        args: &["/L", "/U", "/V"],
        encoding: OutputEncoding::Oem,
        family: DEFRAG,
        services: &[OPTIMIZE_DRIVES],
    },
    ToolInfo {
        id: ToolId::DiskCheck,
        group: ToolGroup::Drives,
        title: "Check disk (read-only)",
        verb: "Check",
        description: "Check Disk scans the drive's file system for errors and reports them \
                      without fixing anything.",
        needs_volume: true,
        requires_admin: true,
        cancellable: true,
        requires_detach: false,
        changes_system: false,
        duration_hint: "a few minutes; longer on large hard disks",
        program: "chkdsk.exe",
        args: &[],
        encoding: OutputEncoding::Oem,
        family: CHKDSK,
        services: &[],
    },
];

/// Every tool, in display order.
pub fn catalog() -> &'static [ToolInfo] {
    &CATALOG
}

/// Normalizes a drive given as one letter A–Z with an optional ':' and an optional
/// trailing '\' ("c", "C:", "c:\") to "C:".
pub fn normalize_volume(text: &str) -> Result<String> {
    let trimmed = text.trim();
    let rest = trimmed.strip_suffix('\\').unwrap_or(trimmed);
    let rest = rest.strip_suffix(':').unwrap_or(rest);
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), None) if letter.is_ascii_alphabetic() => {
            Ok(format!("{}:", letter.to_ascii_uppercase()))
        }
        _ => Err(Error::Other(format!(
            "not a drive letter: {text:?}; expected a letter such as C:"
        ))),
    }
}

/// A tool together with the drive it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequest {
    pub tool: ToolId,
    /// "C:" for drive tools; `None` for the others.
    pub volume: Option<String>,
}

impl ToolRequest {
    /// Drive tools need a volume and the other tools must not have one. An empty volume
    /// counts as none.
    pub fn new(tool: ToolId, volume: Option<&str>) -> Result<ToolRequest> {
        let info = tool.info();
        let volume = volume.map(str::trim).filter(|v| !v.is_empty());
        match (info.needs_volume, volume) {
            (true, None) => Err(Error::Other(format!(
                "{} needs a drive letter such as C:",
                info.title
            ))),
            (false, Some(v)) => Err(Error::Other(format!(
                "{} does not run on a drive; got {v:?}",
                info.title
            ))),
            (true, Some(v)) => Ok(ToolRequest {
                tool,
                volume: Some(normalize_volume(v)?),
            }),
            (false, None) => Ok(ToolRequest { tool, volume: None }),
        }
    }

    pub fn info(&self) -> &'static ToolInfo {
        self.tool.info()
    }

    /// The exact arguments after the program; the drive is an argument of its own.
    pub fn args(&self) -> Vec<String> {
        self.volume
            .iter()
            .cloned()
            .chain(self.info().args.iter().map(|a| a.to_string()))
            .collect()
    }

    /// The command as shown to the user and in the audit log, e.g. "sfc.exe /scannow" or
    /// "defrag.exe C: /O /U /V".
    pub fn command_line(&self) -> String {
        std::iter::once(self.info().program.to_string())
            .chain(self.args())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// How a finished run is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub state: JobState,
    /// What went wrong or needs attention, for attention and failed states.
    pub hint: Option<String>,
    /// One sentence about the result.
    pub summary: Option<String>,
    pub restart_required: bool,
}

impl Outcome {
    fn new(state: JobState) -> Outcome {
        Outcome {
            state,
            hint: None,
            summary: None,
            restart_required: false,
        }
    }

    fn hint(mut self, text: impl Into<String>) -> Outcome {
        self.hint = Some(text.into());
        self
    }

    fn summary(mut self, text: impl Into<String>) -> Outcome {
        self.summary = Some(text.into());
        self
    }

    fn restart(mut self) -> Outcome {
        self.restart_required = true;
        self
    }
}

/// An exit code as Windows error codes are written: `0x800F081F`.
pub fn exit_code_hex(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

/// `ERROR_ELEVATION_REQUIRED`.
const EXIT_NEEDS_ADMIN: u32 = 740;
/// `ERROR_SUCCESS_REBOOT_REQUIRED`.
const EXIT_RESTART: u32 = 3010;
/// `CBS_E_SOURCE_MISSING`.
const EXIT_SOURCE_MISSING: u32 = 0x800F_081F;
/// `CBS_E_DOWNLOAD_FAILURE`.
const EXIT_DOWNLOAD_FAILED: u32 = 0x800F_0906;
/// `CBS_E_GROUPPOLICY_DISALLOWED`.
const EXIT_POLICY: u32 = 0x800F_0907;

fn dism_failure(code: i32) -> Outcome {
    let failed = Outcome::new(JobState::Failed);
    match code as u32 {
        EXIT_NEEDS_ADMIN => failed.hint("needs administrator rights"),
        EXIT_SOURCE_MISSING => failed.hint(
            "Windows couldn't find the files needed to repair the component store \
             (0x800F081F). Check the internet connection; on managed PCs a policy can block \
             Windows Update as a repair source.",
        ),
        EXIT_DOWNLOAD_FAILED => failed.hint("the repair files could not be downloaded"),
        EXIT_POLICY => failed.hint("a policy prevents DISM from downloading repair files"),
        _ => failed.hint(format!("DISM ended with error {}", exit_code_hex(code))),
    }
}

/// Judges a finished run from its exit code, whether it was stopped, and for the DISM
/// checks the component store state read afterwards (`None` when it was not read).
///
/// Checks whose verdict Windows prints only as localized text end as `Completed` (neutral)
/// and point to the output; the DISM checks use the store state instead when it could be
/// read.
pub fn classify(
    tool: ToolId,
    exit_code: i32,
    cancelled: bool,
    health: Option<&Result<StoreHealth>>,
) -> Outcome {
    if cancelled {
        return Outcome::new(JobState::Cancelled).summary("Stopped before it finished.");
    }
    let hex = exit_code_hex(exit_code);
    match tool {
        ToolId::SfcVerify | ToolId::SfcScan => match exit_code {
            0 => Outcome::new(JobState::Completed)
                .summary("System File Checker finished; its result is in the output above."),
            _ => Outcome::new(JobState::Attention).hint(format!(
                "System File Checker ended with code {hex}; its report is above. Details are \
                 in %WINDIR%\\Logs\\CBS\\CBS.log."
            )),
        },
        ToolId::DismCheck | ToolId::DismScan => match exit_code as u32 {
            0 => match health {
                Some(Ok(StoreHealth::Healthy)) => Outcome::new(JobState::Succeeded)
                    .summary("No component store damage was found."),
                Some(Ok(StoreHealth::Repairable)) => Outcome::new(JobState::Attention).hint(
                    "The component store is damaged but can be repaired: run Repair the \
                     component store, then Repair system files.",
                ),
                Some(Ok(StoreHealth::NonRepairable)) => Outcome::new(JobState::Failed).hint(
                    "The component store is damaged and can't be repaired here; Windows may \
                     need an in-place upgrade or reset.",
                ),
                Some(Err(_)) | None => Outcome::new(JobState::Completed)
                    .summary("DISM finished; its result is in the output above."),
            },
            EXIT_RESTART => Outcome::new(JobState::Succeeded)
                .summary("DISM finished; restart Windows to complete pending servicing.")
                .restart(),
            _ => dism_failure(exit_code),
        },
        ToolId::DismRestore => match exit_code as u32 {
            0 => Outcome::new(JobState::Succeeded).summary(
                "DISM finished: the component store is repaired or had nothing to repair. Run \
                 Repair system files again afterwards.",
            ),
            EXIT_RESTART => Outcome::new(JobState::Succeeded)
                .summary("DISM repaired the component store; restart Windows to finish.")
                .restart(),
            _ => dism_failure(exit_code),
        },
        ToolId::DiskCheck => match exit_code {
            0 => Outcome::new(JobState::Succeeded).summary("Check Disk found no problems."),
            1 | 2 => Outcome::new(JobState::Succeeded)
                .summary("Check Disk finished; its result is in the output above."),
            3 => Outcome::new(JobState::Attention).hint(
                "Check Disk found problems or could not check the drive. A read-only check of \
                 a drive that is in use can report problems that are not real; to repair, use \
                 Properties > Tools > Check on the drive in File Explorer.",
            ),
            _ => Outcome::new(JobState::Failed).hint(format!("Check Disk ended with code {hex}")),
        },
        ToolId::DriveOptimize | ToolId::DriveRetrim => match exit_code {
            0 => Outcome::new(JobState::Succeeded).summary("Optimize Drives finished."),
            _ => Outcome::new(JobState::Failed)
                .hint(format!("Optimize Drives ended with error {hex}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_round_trip_and_index_the_catalog() {
        for tool in ToolId::ALL {
            assert_eq!(ToolId::parse(tool.as_str()), Some(tool));
            assert_eq!(ToolId::parse(&tool.as_str().to_uppercase()), Some(tool));
            assert_eq!(tool.info().id, tool, "catalog order");
            let json = serde_json::to_string(&tool).unwrap();
            assert_eq!(json, format!("\"{}\"", tool.as_str()));
            assert_eq!(serde_json::from_str::<ToolId>(&json).unwrap(), tool);
        }
        assert_eq!(ToolId::parse("sfc"), None);
        assert_eq!(ToolId::parse(""), None);
        let ids: Vec<ToolId> = catalog().iter().map(|t| t.id).collect();
        assert_eq!(ids, ToolId::ALL);
    }

    #[test]
    fn catalog_flags_follow_the_stop_and_detach_policy() {
        for info in catalog() {
            assert!(info.requires_admin, "{}", info.id);
            assert_eq!(
                info.cancellable,
                info.id == ToolId::DiskCheck,
                "{}",
                info.id
            );
            assert_eq!(
                info.requires_detach,
                matches!(info.id, ToolId::SfcScan | ToolId::DismRestore),
                "{}",
                info.id
            );
            assert_eq!(
                info.needs_volume,
                info.group == ToolGroup::Drives,
                "{}",
                info.id
            );
            assert!(info.description.ends_with('.'), "{}", info.id);
            assert!(!info.description.contains("  "), "{}", info.id);
            assert!(!info.program.contains('\\'), "{}", info.id);
            assert_eq!(
                info.encoding == OutputEncoding::Utf16Le,
                info.program == "sfc.exe",
                "{}",
                info.id
            );
        }
        let json = serde_json::to_value(ToolId::SfcScan.info()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cancellable",
                "changes_system",
                "description",
                "duration_hint",
                "group",
                "id",
                "needs_volume",
                "program",
                "requires_admin",
                "requires_detach",
                "title",
                "verb"
            ]
        );
        assert_eq!(json["group"], "system_files");
    }

    #[test]
    fn argv_per_tool() {
        let argv = |tool: ToolId, volume: Option<&str>| {
            let request = ToolRequest::new(tool, volume).unwrap();
            (request.args(), request.command_line())
        };
        let cases: [(ToolId, Option<&str>, &[&str], &str); 8] = [
            (
                ToolId::SfcVerify,
                None,
                &["/verifyonly"],
                "sfc.exe /verifyonly",
            ),
            (ToolId::SfcScan, None, &["/scannow"], "sfc.exe /scannow"),
            (
                ToolId::DismCheck,
                None,
                &["/Online", "/Cleanup-Image", "/CheckHealth"],
                "dism.exe /Online /Cleanup-Image /CheckHealth",
            ),
            (
                ToolId::DismScan,
                None,
                &["/Online", "/Cleanup-Image", "/ScanHealth"],
                "dism.exe /Online /Cleanup-Image /ScanHealth",
            ),
            (
                ToolId::DismRestore,
                None,
                &["/Online", "/Cleanup-Image", "/RestoreHealth", "/NoRestart"],
                "dism.exe /Online /Cleanup-Image /RestoreHealth /NoRestart",
            ),
            (
                ToolId::DriveOptimize,
                Some("c"),
                &["C:", "/O", "/U", "/V"],
                "defrag.exe C: /O /U /V",
            ),
            (
                ToolId::DriveRetrim,
                Some("D:\\"),
                &["D:", "/L", "/U", "/V"],
                "defrag.exe D: /L /U /V",
            ),
            (ToolId::DiskCheck, Some("e:"), &["E:"], "chkdsk.exe E:"),
        ];
        for (tool, volume, args, line) in cases {
            let (got_args, got_line) = argv(tool, volume);
            assert_eq!(got_args, args, "{tool}");
            assert_eq!(got_line, line, "{tool}");
        }
    }

    #[test]
    fn tool_request_validates_the_volume() {
        for accepted in ["c", "C:", "c:\\", " C: ", "z"] {
            let request = ToolRequest::new(ToolId::DiskCheck, Some(accepted)).unwrap();
            let letter = accepted.trim().chars().next().unwrap().to_ascii_uppercase();
            assert_eq!(request.volume, Some(format!("{letter}:")), "{accepted:?}");
        }
        for rejected in ["C: /F", "C:\\Windows", "..", "1:", "CC", "C::", "\\\\?\\C:"] {
            assert!(
                ToolRequest::new(ToolId::DiskCheck, Some(rejected)).is_err(),
                "{rejected:?} was accepted"
            );
        }
        assert!(ToolRequest::new(ToolId::SfcVerify, Some("C:")).is_err());
        assert!(ToolRequest::new(ToolId::DiskCheck, None).is_err());
        assert!(ToolRequest::new(ToolId::DriveOptimize, Some("")).is_err());
        assert_eq!(
            ToolRequest::new(ToolId::SfcVerify, Some(""))
                .unwrap()
                .volume,
            None
        );
    }

    fn state(tool: ToolId, code: i32) -> JobState {
        classify(tool, code, false, None).state
    }

    #[test]
    fn classify_judges_every_tool() {
        use JobState::*;
        assert_eq!(exit_code_hex(-2146498529), "0x800F081F");
        assert_eq!(exit_code_hex(0), "0x00000000");
        assert_eq!(exit_code_hex(3010), "0x00000BC2");

        // System File Checker: a clean exit is neutral, anything else needs attention.
        for tool in [ToolId::SfcVerify, ToolId::SfcScan] {
            let done = classify(tool, 0, false, None);
            assert_eq!(done.state, Completed);
            assert!(done.summary.unwrap().contains("output above"));
            let odd = classify(tool, 2, false, None);
            assert_eq!(odd.state, Attention);
            assert!(odd.hint.unwrap().contains("0x00000002"));
        }

        // DISM checks: the verdict comes from the store state.
        for tool in [ToolId::DismCheck, ToolId::DismScan] {
            let healthy = classify(tool, 0, false, Some(&Ok(StoreHealth::Healthy)));
            assert_eq!(healthy.state, Succeeded);
            assert_eq!(
                healthy.summary.as_deref(),
                Some("No component store damage was found.")
            );
            let repairable = classify(tool, 0, false, Some(&Ok(StoreHealth::Repairable)));
            assert_eq!(repairable.state, Attention);
            assert!(repairable
                .hint
                .unwrap()
                .contains("Repair the component store"));
            let broken = classify(tool, 0, false, Some(&Ok(StoreHealth::NonRepairable)));
            assert_eq!(broken.state, Failed);
            assert!(broken.hint.unwrap().contains("in-place upgrade"));
            let unread = Err(Error::Other("probe failed".into()));
            assert_eq!(classify(tool, 0, false, Some(&unread)).state, Completed);
            assert_eq!(classify(tool, 0, false, None).state, Completed);
            let restart = classify(tool, 3010, false, None);
            assert_eq!(restart.state, Succeeded);
            assert!(restart.restart_required);
            assert_eq!(state(tool, 740), Failed);
            assert_eq!(state(tool, 5), Failed);
        }

        // DISM repair.
        assert_eq!(state(ToolId::DismRestore, 0), Succeeded);
        let restart = classify(ToolId::DismRestore, 3010, false, None);
        assert_eq!(restart.state, Succeeded);
        assert!(restart.restart_required);
        let admin = classify(ToolId::DismRestore, 740, false, None);
        assert_eq!(admin.hint.as_deref(), Some("needs administrator rights"));
        let missing = classify(ToolId::DismRestore, -2146498529, false, None);
        assert_eq!(missing.state, Failed);
        assert!(missing.hint.unwrap().contains("(0x800F081F)"));
        let download = classify(ToolId::DismRestore, 0x800F_0906_u32 as i32, false, None);
        assert_eq!(
            download.hint.as_deref(),
            Some("the repair files could not be downloaded")
        );
        let policy = classify(ToolId::DismRestore, 0x800F_0907_u32 as i32, false, None);
        assert_eq!(
            policy.hint.as_deref(),
            Some("a policy prevents DISM from downloading repair files")
        );
        let other = classify(ToolId::DismRestore, 87, false, None);
        assert_eq!(
            other.hint.as_deref(),
            Some("DISM ended with error 0x00000057")
        );
        assert!(!other.restart_required);

        // Check Disk.
        for code in [0, 1, 2] {
            assert_eq!(state(ToolId::DiskCheck, code), Succeeded, "{code}");
        }
        let problems = classify(ToolId::DiskCheck, 3, false, None);
        assert_eq!(problems.state, Attention);
        assert!(problems.hint.unwrap().contains("read-only check"));
        assert_eq!(state(ToolId::DiskCheck, 4), Failed);

        // Optimize Drives.
        for tool in [ToolId::DriveOptimize, ToolId::DriveRetrim] {
            assert_eq!(state(tool, 0), Succeeded);
            let failed = classify(tool, -2_147_024_891, false, None);
            assert_eq!(failed.state, Failed);
            assert_eq!(
                failed.hint.as_deref(),
                Some("Optimize Drives ended with error 0x80070005")
            );
        }

        // A stopped run is cancelled whatever its exit code.
        for tool in ToolId::ALL {
            assert_eq!(classify(tool, 1, true, None).state, Cancelled, "{tool}");
        }
    }
}
