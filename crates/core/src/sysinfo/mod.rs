//! Read-only snapshot of the hardware and Windows configuration.
//!
//! Every source is readable by a standard user: registry values, SMBIOS through
//! `GetSystemFirmwareTable`, `GetLogicalProcessorInformationEx`, DXGI, `QueryDisplayConfig`,
//! storage IOCTLs on handles opened with no access rights, TBS, `NtQuerySystemInformation`
//! and CPUID. No PowerShell, WMI or command-line tool is started, nothing is journaled or
//! logged, and nothing on the system is written.
//!
//! Each section is read on its own: an error or a panic while reading one section is
//! recorded in [`SystemInfo::errors`], and the other sections are still read and rendered.
//!
//! - `os`, `cpu`, `memory` (with `smbios`), `gpu`, `display`, `board`, `storage` and
//!   `security` each read one section through a pure, bounds-checked parse layer and a
//!   small live read
//! - `report` renders a [`SystemInfo`] into titled sections of labelled rows, a one-line
//!   summary and a plain-text report

use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::win::registry::{Hive, Key, RegValue};
use crate::win::storage::MediaKind;
use crate::Result;

mod board;
mod cpu;
mod display;
mod gpu;
mod memory;
pub(crate) mod os;
mod report;
mod security;
mod smbios;
mod storage;

pub use report::{render, Group, Level, ReportSection, Row, Snapshot};

// ───────────────────────────── Sections ─────────────────────────────

/// One card of the report, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Windows,
    Processor,
    Memory,
    Graphics,
    Displays,
    Board,
    Storage,
    Security,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Windows,
        Section::Processor,
        Section::Memory,
        Section::Graphics,
        Section::Displays,
        Section::Board,
        Section::Storage,
        Section::Security,
    ];

    /// Stable id, as serialized.
    pub fn id(self) -> &'static str {
        match self {
            Section::Windows => "windows",
            Section::Processor => "processor",
            Section::Memory => "memory",
            Section::Graphics => "graphics",
            Section::Displays => "displays",
            Section::Board => "board",
            Section::Storage => "storage",
            Section::Security => "security",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Section::Windows => "Windows",
            Section::Processor => "Processor",
            Section::Memory => "Memory",
            Section::Graphics => "Graphics",
            Section::Displays => "Displays",
            Section::Board => "Motherboard and firmware",
            Section::Storage => "Storage",
            Section::Security => "Security",
        }
    }
}

/// A section that could not be read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionError {
    pub section: Section,
    pub message: String,
}

// ───────────────────────────── Enumerations ─────────────────────────────

/// Native processor architecture of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X64,
    Arm64,
    X86,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    Activated,
    NotActivated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareKind {
    Uefi,
    Bios,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    Fixed,
    Removable,
    Optical,
    RamDisk,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecureBoot {
    On,
    Off,
    /// Legacy BIOS boot: Secure Boot does not exist in this mode.
    Unsupported,
    Unknown,
}

/// Hardware virtualization (VT-x / AMD-V) as the running system sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Virtualization {
    Enabled,
    Disabled,
    /// A hypervisor owns the virtualization extensions.
    InUse,
    Unknown,
}

/// State of a security feature that is configured separately from whether it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    Running,
    /// Turned on in the configuration but not running in this boot.
    NotRunning,
    Off,
    Unknown,
}

// ───────────────────────────── Section data ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OsInfo {
    /// "Windows 11 Home": corrected for builds that still report "Windows 10".
    pub product_name: String,
    /// `EditionID` ("Core", "Professional", ...); empty when not reported.
    pub edition_id: String,
    /// "25H2", or the older `ReleaseId` ("2009").
    pub display_version: Option<String>,
    pub build: u32,
    /// Update build revision (`UBR`).
    pub revision: Option<u32>,
    pub architecture: Architecture,
    /// True when this x64 process runs under emulation on an ARM64 machine.
    pub emulated: bool,
    /// Date of the last clean install or feature update.
    pub installed_at: Option<DateTime<Utc>>,
    pub uptime_secs: u64,
    pub fast_startup: Option<bool>,
    pub activation: Activation,
    /// Left out of the plain-text report.
    pub computer_name: String,
}

/// Total cache per level over every instance, in KiB (0 when not reported).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheSizes {
    /// Data plus instruction caches.
    pub l1_kib: u64,
    pub l2_kib: u64,
    pub l3_kib: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuInfo {
    /// Marketing name with whitespace collapsed.
    pub name: String,
    pub vendor: String,
    pub identifier: String,
    pub packages: u32,
    pub cores: u32,
    pub logical_processors: u32,
    /// Only on hybrid processors (more than one efficiency class).
    pub performance_cores: Option<u32>,
    pub efficiency_cores: Option<u32>,
    pub base_mhz: Option<u32>,
    pub cache: CacheSizes,
}

/// One populated memory slot. Serial numbers and asset tags are never read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryModule {
    pub locator: Option<String>,
    pub bank: Option<String>,
    pub size_bytes: Option<u64>,
    /// "DDR5", "LPDDR4", ...
    pub kind: Option<String>,
    /// "DIMM", "SO-DIMM" or "Soldered".
    pub form_factor: Option<String>,
    /// Rated speed in MT/s.
    pub speed_mts: Option<u32>,
    /// Speed the firmware configured, in MT/s.
    pub configured_mts: Option<u32>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// Physically installed memory, as the firmware reports it.
    pub installed_bytes: Option<u64>,
    /// Memory available to Windows.
    pub usable_bytes: u64,
    pub available_bytes: u64,
    pub load_percent: u32,
    pub slots: Option<u32>,
    pub max_capacity_bytes: Option<u64>,
    pub modules: Vec<MemoryModule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
    pub driver_version: Option<String>,
    pub driver_date: Option<NaiveDate>,
    pub driver_provider: Option<String>,
    /// Microsoft Basic Display Adapter: no graphics driver is installed.
    pub basic_driver: bool,
    /// Adapter LUID, used to attach displays to their adapter.
    #[serde(skip)]
    pub(crate) luid: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Option<f64>,
    /// "HDMI", "DisplayPort", "Built-in", ...
    pub connection: String,
    pub built_in: bool,
    /// The display whose desktop starts at (0, 0).
    pub primary: bool,
    /// Name of the adapter the display is connected to.
    pub gpu: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub system_manufacturer: Option<String>,
    pub system_product: Option<String>,
    pub system_family: Option<String>,
    pub board_manufacturer: Option<String>,
    pub board_product: Option<String>,
    pub bios_vendor: Option<String>,
    pub bios_version: Option<String>,
    pub bios_date: Option<NaiveDate>,
    pub firmware: FirmwareKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskInfo {
    /// N of `\\.\PhysicalDriveN`.
    pub number: u32,
    pub model: String,
    pub firmware: Option<String>,
    /// "NVMe", "SATA", "USB", ...
    pub bus: String,
    pub media: MediaKind,
    pub size_bytes: Option<u64>,
    pub removable: bool,
    /// Holds the Windows volume.
    pub system: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// "C:".
    pub letter: String,
    pub label: String,
    pub file_system: String,
    pub kind: DriveKind,
    pub size_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Physical disks the volume spans; the first one holds its start.
    pub disk_numbers: Vec<u32>,
    /// Holds the Windows directory.
    pub system: bool,
    /// False for an empty card reader or optical drive.
    pub ready: bool,
    /// The query did not answer within the deadline.
    pub not_responding: bool,
    /// Why a ready volume could not be read.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityInfo {
    pub firmware: FirmwareKind,
    pub secure_boot: SecureBoot,
    /// None when TPM Base Services gave no answer.
    pub tpm_found: Option<bool>,
    /// "2.0" or "1.2".
    pub tpm_version: Option<String>,
    pub virtualization: Virtualization,
    /// Name of the running hypervisor.
    pub hypervisor: Option<String>,
    /// Hypervisor-protected code integrity.
    pub memory_integrity: FeatureState,
    /// Virtualization-based security.
    pub vbs: FeatureState,
}

/// Everything one snapshot read. Sections that failed are `None` or empty and have an
/// entry in `errors`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemInfo {
    pub taken_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub os: Option<OsInfo>,
    pub cpu: Option<CpuInfo>,
    pub memory: Option<MemoryInfo>,
    pub gpus: Vec<GpuInfo>,
    pub displays: Vec<DisplayInfo>,
    /// `QueryDisplayConfig` refused access: a remote or locked session.
    #[serde(default)]
    pub displays_unavailable: bool,
    pub board: Option<BoardInfo>,
    pub disks: Vec<DiskInfo>,
    pub volumes: Vec<VolumeInfo>,
    pub security: Option<SecurityInfo>,
    pub errors: Vec<SectionError>,
}

// ───────────────────────────── Collection ─────────────────────────────

/// Reads the system and renders the report in the local time zone. Read-only.
pub fn snapshot() -> Result<Snapshot> {
    Ok(render(collect(), &Local))
}

/// Reads every section of this PC. Read-only; never fails as a whole.
pub fn collect() -> SystemInfo {
    collect_with(&Live)
}

/// Windows edition, version, build, activation and Fast Startup of this PC. Read-only.
pub fn os_info() -> Result<OsInfo> {
    os::read()
}

/// Secure Boot, TPM, virtualization, memory integrity and VBS of this PC. Read-only.
pub fn security_info() -> Result<SecurityInfo> {
    security::read(board::firmware_type())
}

/// Disks and volumes of this PC. Drives that do not answer within the storage section's
/// shared 5 s deadline are reported as not responding. Read-only.
pub fn storage_devices() -> Result<(Vec<DiskInfo>, Vec<VolumeInfo>)> {
    storage::read()
}

/// The readers behind each section; `Live` reads this PC.
pub(crate) trait Source {
    fn os(&self) -> Result<OsInfo>;
    fn cpu(&self) -> Result<CpuInfo>;
    fn memory(&self) -> Result<MemoryInfo>;
    fn gpus(&self) -> Result<Vec<GpuInfo>>;
    /// Each display with the LUID of its adapter; `None` when display details are not
    /// available in this session.
    fn displays(&self) -> Result<Option<Vec<(DisplayInfo, i64)>>>;
    fn board(&self) -> Result<BoardInfo>;
    /// Firmware type on its own, used when the board section failed.
    fn firmware(&self) -> FirmwareKind;
    fn storage(&self) -> Result<(Vec<DiskInfo>, Vec<VolumeInfo>)>;
    fn security(&self, firmware: FirmwareKind) -> Result<SecurityInfo>;
}

#[derive(Debug, Clone, Copy)]
struct Live;

impl Source for Live {
    fn os(&self) -> Result<OsInfo> {
        os::read()
    }

    fn cpu(&self) -> Result<CpuInfo> {
        cpu::read()
    }

    fn memory(&self) -> Result<MemoryInfo> {
        memory::read()
    }

    fn gpus(&self) -> Result<Vec<GpuInfo>> {
        gpu::read()
    }

    fn displays(&self) -> Result<Option<Vec<(DisplayInfo, i64)>>> {
        display::read()
    }

    fn board(&self) -> Result<BoardInfo> {
        board::read()
    }

    fn firmware(&self) -> FirmwareKind {
        board::firmware_type()
    }

    fn storage(&self) -> Result<(Vec<DiskInfo>, Vec<VolumeInfo>)> {
        storage::read()
    }

    fn security(&self, firmware: FirmwareKind) -> Result<SecurityInfo> {
        security::read(firmware)
    }
}

/// Reads every section through `source`, each one guarded on its own.
pub(crate) fn collect_with(source: &dyn Source) -> SystemInfo {
    let started = Instant::now();
    let taken_at = Utc::now();
    let mut errors = Vec::new();

    let os = guarded(Section::Windows, &mut errors, || source.os());
    let cpu = guarded(Section::Processor, &mut errors, || source.cpu());
    let memory = guarded(Section::Memory, &mut errors, || source.memory());
    let gpus = guarded(Section::Graphics, &mut errors, || source.gpus()).unwrap_or_default();
    let displays = guarded(Section::Displays, &mut errors, || source.displays());
    let board = guarded(Section::Board, &mut errors, || source.board());
    let (disks, volumes) =
        guarded(Section::Storage, &mut errors, || source.storage()).unwrap_or_default();
    let board_firmware = board.as_ref().map(|b| b.firmware);
    let security = guarded(Section::Security, &mut errors, || {
        let firmware = match board_firmware {
            Some(firmware) => firmware,
            None => source.firmware(),
        };
        source.security(firmware)
    });

    let (displays, displays_unavailable) = match displays {
        Some(Some(list)) => {
            let named = list
                .into_iter()
                .map(|(mut display, luid)| {
                    display.gpu = gpus.iter().find(|g| g.luid == luid).map(|g| g.name.clone());
                    display
                })
                .collect();
            (named, false)
        }
        Some(None) => (Vec::new(), true),
        None => (Vec::new(), false),
    };

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::debug!(
        duration_ms,
        failed_sections = errors.len(),
        "system information read"
    );
    SystemInfo {
        taken_at,
        duration_ms,
        os,
        cpu,
        memory,
        gpus,
        displays,
        displays_unavailable,
        board,
        disks,
        volumes,
        security,
        errors,
    }
}

// ───────────────────────────── Registry helpers ─────────────────────────────

/// Read-only HKLM key; `None` when it is missing or cannot be opened.
fn open_hklm(path: &str) -> Option<Key> {
    Key::open(Hive::LocalMachine, path, false).ok().flatten()
}

/// Trimmed, non-empty string value; any other type, a missing value or a read error is
/// `None`.
fn reg_text(key: &Key, name: &str) -> Option<String> {
    match key.query(name).ok()?? {
        RegValue::Sz(s) | RegValue::ExpandSz(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        _ => None,
    }
}

fn reg_dword(key: &Key, name: &str) -> Option<u32> {
    match key.query(name).ok()?? {
        RegValue::Dword(v) => Some(v),
        _ => None,
    }
}

fn reg_qword(key: &Key, name: &str) -> Option<u64> {
    match key.query(name).ok()?? {
        RegValue::Qword(v) => Some(v),
        _ => None,
    }
}

/// Runs one section reader. An error or a panic is recorded for `section` and gives `None`.
fn guarded<T>(
    section: Section,
    errors: &mut Vec<SectionError>,
    read: impl FnOnce() -> Result<T>,
) -> Option<T> {
    match panic::catch_unwind(AssertUnwindSafe(read)) {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            errors.push(SectionError {
                section,
                message: e.to_string(),
            });
            None
        }
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            errors.push(SectionError {
                section,
                message: format!("internal error: {text}"),
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::report::tests::{
        fixture_board, fixture_cpu, fixture_disk, fixture_display, fixture_gpu, fixture_memory,
        fixture_os, fixture_security, fixture_volume,
    };
    use super::*;
    use crate::Error;

    /// A source serving the report fixture; `fail` makes one section return an error and
    /// `panic` makes one panic.
    struct FakeSource {
        fail: Option<Section>,
        panic: Option<Section>,
        gpus: Vec<GpuInfo>,
        displays: Option<Vec<(DisplayInfo, i64)>>,
        firmware: FirmwareKind,
        firmware_calls: Cell<usize>,
        security_firmware: Cell<Option<FirmwareKind>>,
    }

    impl FakeSource {
        fn new() -> FakeSource {
            FakeSource {
                fail: None,
                panic: None,
                gpus: vec![fixture_gpu()],
                displays: Some(vec![(fixture_display(), fixture_gpu().luid)]),
                firmware: FirmwareKind::Bios,
                firmware_calls: Cell::new(0),
                security_firmware: Cell::new(None),
            }
        }

        fn section<T>(&self, section: Section, value: T) -> Result<T> {
            if self.panic == Some(section) {
                panic!("{} reader crashed", section.id());
            }
            if self.fail == Some(section) {
                return Err(Error::Other(format!("cannot read {}", section.id())));
            }
            Ok(value)
        }
    }

    impl Source for FakeSource {
        fn os(&self) -> Result<OsInfo> {
            self.section(Section::Windows, fixture_os())
        }

        fn cpu(&self) -> Result<CpuInfo> {
            self.section(Section::Processor, fixture_cpu())
        }

        fn memory(&self) -> Result<MemoryInfo> {
            self.section(Section::Memory, fixture_memory())
        }

        fn gpus(&self) -> Result<Vec<GpuInfo>> {
            self.section(Section::Graphics, self.gpus.clone())
        }

        fn displays(&self) -> Result<Option<Vec<(DisplayInfo, i64)>>> {
            self.section(Section::Displays, self.displays.clone())
        }

        fn board(&self) -> Result<BoardInfo> {
            self.section(Section::Board, fixture_board())
        }

        fn firmware(&self) -> FirmwareKind {
            self.firmware_calls.set(self.firmware_calls.get() + 1);
            self.firmware
        }

        fn storage(&self) -> Result<(Vec<DiskInfo>, Vec<VolumeInfo>)> {
            self.section(
                Section::Storage,
                (vec![fixture_disk()], vec![fixture_volume()]),
            )
        }

        fn security(&self, firmware: FirmwareKind) -> Result<SecurityInfo> {
            self.security_firmware.set(Some(firmware));
            self.section(Section::Security, fixture_security())
        }
    }

    #[test]
    fn every_section_is_read_from_the_source() {
        let source = FakeSource::new();
        let info = collect_with(&source);
        assert!(info.errors.is_empty(), "{:?}", info.errors);
        assert_eq!(info.os, Some(fixture_os()));
        assert_eq!(info.cpu, Some(fixture_cpu()));
        assert_eq!(info.memory, Some(fixture_memory()));
        assert_eq!(info.board, Some(fixture_board()));
        assert_eq!(info.disks, vec![fixture_disk()]);
        assert_eq!(info.volumes, vec![fixture_volume()]);
        assert_eq!(info.security, Some(fixture_security()));
        assert!(!info.displays_unavailable);
        // Security gets the firmware type the board section read.
        assert_eq!(source.security_firmware.get(), Some(FirmwareKind::Uefi));
        assert_eq!(source.firmware_calls.get(), 0);
    }

    #[test]
    fn failed_section_is_reported_and_others_kept() {
        let source = FakeSource {
            fail: Some(Section::Memory),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert_eq!(
            info.errors,
            vec![SectionError {
                section: Section::Memory,
                message: "cannot read memory".into(),
            }]
        );
        assert_eq!(info.memory, None);
        assert!(info.os.is_some() && info.cpu.is_some() && info.security.is_some());
        assert_eq!(info.gpus.len(), 1);

        let snap = report::render(info, &Local);
        let memory = snap
            .sections
            .iter()
            .find(|s| s.id == Section::Memory)
            .unwrap();
        assert_eq!(memory.error.as_deref(), Some("cannot read memory"));
        assert!(snap.text.contains("  Could not read: cannot read memory\n"));
        assert_eq!(snap.sections.len(), Section::ALL.len());

        // A failed list section leaves its list empty.
        let source = FakeSource {
            fail: Some(Section::Storage),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert!(info.disks.is_empty() && info.volumes.is_empty());
        assert_eq!(info.errors[0].section, Section::Storage);
    }

    #[test]
    fn panicking_section_becomes_internal_error() {
        let source = FakeSource {
            panic: Some(Section::Graphics),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert_eq!(
            info.errors,
            vec![SectionError {
                section: Section::Graphics,
                message: "internal error: graphics reader crashed".into(),
            }]
        );
        assert!(info.gpus.is_empty());
        assert!(info.os.is_some() && !info.disks.is_empty() && !info.volumes.is_empty());
        // Displays still render, without an adapter name.
        assert_eq!(info.displays.len(), 1);
        assert_eq!(info.displays[0].gpu, None);
    }

    #[test]
    fn failed_board_uses_the_firmware_type_for_security() {
        let source = FakeSource {
            fail: Some(Section::Board),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert_eq!(info.board, None);
        assert_eq!(source.firmware_calls.get(), 1);
        assert_eq!(source.security_firmware.get(), Some(FirmwareKind::Bios));
        assert!(info.security.is_some());
    }

    #[test]
    fn displays_get_gpu_names_by_luid() {
        let mut intel = fixture_gpu();
        intel.name = "Intel Graphics".into();
        intel.luid = (1 << 32) | 7;
        let mut built_in = fixture_display();
        built_in.name = "Built-in display".into();
        let mut orphan = fixture_display();
        orphan.name = "Mirror".into();
        let source = FakeSource {
            gpus: vec![fixture_gpu(), intel],
            displays: Some(vec![
                (built_in, (1 << 32) | 7),
                (fixture_display(), fixture_gpu().luid),
                (orphan, 99),
            ]),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        let names: Vec<Option<&str>> = info.displays.iter().map(|d| d.gpu.as_deref()).collect();
        assert_eq!(
            names,
            [
                Some("Intel Graphics"),
                Some("NVIDIA GeForce RTX 5060 Ti"),
                None
            ]
        );
    }

    #[test]
    fn unavailable_displays_set_the_flag() {
        let source = FakeSource {
            displays: None,
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert!(info.displays_unavailable);
        assert!(info.displays.is_empty());
        assert!(info.errors.is_empty());

        let source = FakeSource {
            fail: Some(Section::Displays),
            ..FakeSource::new()
        };
        let info = collect_with(&source);
        assert!(
            !info.displays_unavailable,
            "an error is not an unavailable session"
        );
        assert_eq!(info.errors[0].section, Section::Displays);
    }

    #[test]
    fn section_ids_and_titles() {
        for section in Section::ALL {
            let json = serde_json::to_string(&section).unwrap();
            assert_eq!(json, format!("\"{}\"", section.id()));
        }
        assert_eq!(Section::Board.title(), "Motherboard and firmware");
    }
}
