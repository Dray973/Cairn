//! Rendering of a [`SystemInfo`] into titled sections of labelled rows, a one-line summary
//! and a plain-text report. Pure: nothing here reads the system.
//!
//! Rows carry a level (good or warning) and an optional note for the UI; the plain text
//! leaves out levels and private rows (the computer name), so it can be pasted into a
//! support request as is.

use std::fmt::Write as _;

use chrono::{DateTime, Offset, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::smbios::clean_oem;
use super::{
    Activation, Architecture, BoardInfo, CpuInfo, DiskInfo, DisplayInfo, DriveKind, FeatureState,
    FirmwareKind, GpuInfo, MemoryInfo, MemoryModule, OsInfo, Section, SecureBoot, SecurityInfo,
    SystemInfo, Virtualization, VolumeInfo,
};
use crate::win::storage::MediaKind;

/// Separator between the parts of one value.
const SEP: &str = "  ·  ";
/// Indent of section rows in the plain text.
const SECTION_INDENT: usize = 2;
/// Indent of group rows in the plain text.
const GROUP_INDENT: usize = 4;
/// Column where the values of a section start in the plain text, unless a longer label
/// moves them further right: a 21-column label field after the section indent.
const VALUE_COLUMN: usize = SECTION_INDENT + 21;
/// Spaces at least between a label and its value in the plain text.
const LABEL_GAP: usize = 2;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

pub(crate) const DISPLAYS_UNAVAILABLE: &str =
    "Display details are not available in a remote or locked session.";
const INSTALLED_NOTE: &str = "Date of the last clean install or feature update";
const FAST_STARTUP_NOTE: &str =
    "Fast Startup is on: shutting down does not reset this; restarting does.";
const BASIC_DRIVER_NOTE: &str =
    "No graphics driver is installed; Windows is using its basic display driver.";
const LOW_SPACE_NOTE: &str = "Less than 10% free";
const SYSTEM_VOLUME_NOTE: &str = "Windows is installed here";
const TPM_12_NOTE: &str = "Windows 11 requires TPM 2.0";
const NO_TPM_NOTE: &str = "No TPM, or it is turned off in the firmware setup";
const VIRTUALIZATION_OFF_NOTE: &str =
    "Virtualization is disabled in the firmware (UEFI/BIOS) setup, so it cannot start.";
const NEXT_RESTART_NOTE: &str = "It starts after the next restart.";

// ───────────────────────────── Types ─────────────────────────────

/// How a row is highlighted in the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Normal,
    Good,
    Warning,
}

/// One labelled value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub note: Option<String>,
    /// Share of a capacity in use (0 to 1), shown as a meter.
    #[serde(default)]
    pub fraction: Option<f64>,
    /// Left out of the plain text.
    #[serde(default)]
    pub private: bool,
}

impl Row {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            value: value.into(),
            level: Level::Normal,
            note: None,
            fraction: None,
            private: false,
        }
    }

    fn good(mut self) -> Row {
        self.level = Level::Good;
        self
    }

    fn warning(mut self) -> Row {
        self.level = Level::Warning;
        self
    }

    fn with_note(mut self, note: impl Into<String>) -> Row {
        self.note = Some(note.into());
        self
    }

    fn with_fraction(mut self, fraction: f64) -> Row {
        self.fraction = fraction.is_finite().then(|| fraction.clamp(0.0, 1.0));
        self
    }

    fn private(mut self) -> Row {
        self.private = true;
        self
    }
}

/// Rows under a sub-heading (one GPU, one disk).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub title: String,
    pub rows: Vec<Row>,
}

/// One card of the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSection {
    pub id: Section,
    pub title: String,
    /// Why the section could not be read; it then has no rows.
    #[serde(default)]
    pub error: Option<String>,
    /// A remark about the whole section.
    #[serde(default)]
    pub note: Option<String>,
    pub rows: Vec<Row>,
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// A rendered snapshot: the raw data, a one-line summary, the sections in display order
/// and the plain text for the clipboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub info: SystemInfo,
    pub summary: String,
    pub sections: Vec<ReportSection>,
    pub text: String,
}

// ───────────────────────────── Rendering ─────────────────────────────

/// Renders `info`; times are shown in the time zone `tz`, each with the offset the zone had
/// at that time (a date from before a daylight saving change keeps its own offset).
pub fn render<Tz: TimeZone>(info: SystemInfo, tz: &Tz) -> Snapshot {
    let sections: Vec<ReportSection> = Section::ALL
        .iter()
        .map(|&id| section(&info, id, tz))
        .collect();
    let summary = summary_line(&info);
    let text = plain_text(&info, &sections, tz);
    Snapshot {
        info,
        summary,
        sections,
        text,
    }
}

fn section<Tz: TimeZone>(info: &SystemInfo, id: Section, tz: &Tz) -> ReportSection {
    let mut out = ReportSection {
        id,
        title: id.title().to_string(),
        error: info
            .errors
            .iter()
            .find(|e| e.section == id)
            .map(|e| e.message.clone()),
        note: None,
        rows: Vec::new(),
        groups: Vec::new(),
    };
    if out.error.is_some() {
        return out;
    }
    match id {
        Section::Windows => {
            if let Some(os) = &info.os {
                out.rows = windows_rows(os, tz);
            }
        }
        Section::Processor => {
            if let Some(cpu) = &info.cpu {
                out.rows = processor_rows(cpu);
            }
        }
        Section::Memory => {
            if let Some(memory) = &info.memory {
                out.rows = memory_rows(memory);
            }
        }
        Section::Graphics => {
            if info.gpus.is_empty() {
                out.rows.push(Row::new("Adapters", "None found"));
            }
            out.groups = info.gpus.iter().map(gpu_group).collect();
        }
        Section::Displays => {
            if info.displays_unavailable {
                out.note = Some(DISPLAYS_UNAVAILABLE.to_string());
            } else {
                out.rows = display_rows(&info.displays, info.gpus.len());
            }
        }
        Section::Board => {
            if let Some(board) = &info.board {
                out.rows = board_rows(board);
            }
        }
        Section::Storage => {
            out.groups = storage_groups(&info.disks, &info.volumes);
            if out.groups.is_empty() {
                out.rows.push(Row::new("Drives", "None found"));
            }
        }
        Section::Security => {
            if let Some(security) = &info.security {
                let home = info
                    .os
                    .as_ref()
                    .is_some_and(|os| os.edition_id.starts_with("Core"));
                out.rows = security_rows(security, home);
            }
        }
    }
    out
}

/// The local date of `at` in `tz`, with the offset `tz` had at that instant.
fn local_date<Tz: TimeZone>(at: &DateTime<Utc>, tz: &Tz) -> String {
    at.with_timezone(tz)
        .naive_local()
        .format("%Y-%m-%d")
        .to_string()
}

fn windows_rows<Tz: TimeZone>(os: &OsInfo, tz: &Tz) -> Vec<Row> {
    let mut rows = vec![Row::new("Edition", &os.product_name)];
    if let Some(version) = &os.display_version {
        rows.push(Row::new("Version", version));
    }
    let build = match os.revision {
        Some(revision) => format!("{}.{revision}", os.build),
        None => os.build.to_string(),
    };
    rows.push(Row::new("OS build", build));
    let mut architecture = Row::new("Architecture", architecture_text(os.architecture));
    if os.emulated {
        architecture = architecture.with_note("Cairn runs as an x64 app under emulation.");
    }
    rows.push(architecture);
    rows.push(match os.activation {
        Activation::Activated => Row::new("Activation", "Activated").good(),
        Activation::NotActivated => Row::new("Activation", "Not activated").warning(),
        Activation::Unknown => Row::new("Activation", "Unknown"),
    });
    if let Some(installed) = &os.installed_at {
        rows.push(Row::new("Installed", local_date(installed, tz)).with_note(INSTALLED_NOTE));
    }
    let mut uptime = Row::new("Up time", fmt_uptime(os.uptime_secs));
    if os.fast_startup == Some(true) {
        uptime = uptime.with_note(FAST_STARTUP_NOTE);
    }
    rows.push(uptime);
    if let Some(on) = os.fast_startup {
        rows.push(Row::new("Fast Startup", if on { "On" } else { "Off" }));
    }
    if !os.computer_name.is_empty() {
        rows.push(Row::new("Computer name", &os.computer_name).private());
    }
    rows
}

fn architecture_text(architecture: Architecture) -> &'static str {
    match architecture {
        Architecture::X64 => "64-bit (x64)",
        Architecture::Arm64 => "64-bit (ARM64)",
        Architecture::X86 => "32-bit (x86)",
        Architecture::Other => "Other",
    }
}

fn processor_rows(cpu: &CpuInfo) -> Vec<Row> {
    let mut rows = Vec::new();
    let name = short_cpu_name(&cpu.name);
    if !name.is_empty() {
        rows.push(Row::new("Name", name));
    }
    if cpu.packages > 1 {
        rows.push(Row::new("Sockets", cpu.packages.to_string()));
    }
    let cores = match (cpu.performance_cores, cpu.efficiency_cores) {
        (Some(p), Some(e)) => format!("{} ({p} performance, {e} efficiency)", cpu.cores),
        _ => cpu.cores.to_string(),
    };
    rows.push(Row::new("Cores", cores));
    rows.push(Row::new(
        "Logical processors",
        cpu.logical_processors.to_string(),
    ));
    if let Some(mhz) = cpu.base_mhz {
        rows.push(Row::new("Base speed", fmt_mhz(mhz)));
    }
    for (label, kib) in [
        ("L1 cache", cpu.cache.l1_kib),
        ("L2 cache", cpu.cache.l2_kib),
        ("L3 cache", cpu.cache.l3_kib),
    ] {
        if kib > 0 {
            rows.push(Row::new(label, fmt_cache(kib)));
        }
    }
    if !cpu.identifier.is_empty() {
        rows.push(Row::new("Identifier", &cpu.identifier));
    }
    rows
}

/// Installed memory: as the firmware reports it, else the sum of the modules.
fn installed_memory(memory: &MemoryInfo) -> Option<u64> {
    memory.installed_bytes.or_else(|| {
        let sizes: Vec<u64> = memory.modules.iter().filter_map(|m| m.size_bytes).collect();
        (!sizes.is_empty()).then(|| sizes.iter().sum())
    })
}

fn memory_rows(memory: &MemoryInfo) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(installed) = installed_memory(memory) {
        rows.push(Row::new("Installed", fmt_binary(installed)));
    }
    rows.push(Row::new("Usable", fmt_binary(memory.usable_bytes)));
    if memory.usable_bytes > 0 {
        let used = memory.usable_bytes.saturating_sub(memory.available_bytes);
        rows.push(
            Row::new(
                "In use",
                format!("{} ({}%)", fmt_binary(used), memory.load_percent),
            )
            .with_fraction(used as f64 / memory.usable_bytes as f64),
        );
    }
    if let Some(slots) = memory.slots {
        rows.push(Row::new(
            "Slots",
            format!("{} of {slots} used", memory.modules.len()),
        ));
    }
    if let Some(max) = memory.max_capacity_bytes {
        rows.push(Row::new("Maximum capacity", fmt_binary(max)));
    }
    for (label, module) in module_labels(&memory.modules)
        .into_iter()
        .zip(&memory.modules)
    {
        rows.push(Row::new(label, module_text(module)));
    }
    rows
}

/// The words of a locator or bank: '-' and '_' separate words as spaces do.
fn slot_words(text: &str) -> Vec<&str> {
    text.split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect()
}

/// "Controller0", "controller12": the memory controller part of a locator.
fn is_controller_word(word: &str) -> bool {
    const PREFIX: &str = "controller";
    let digits = word.get(PREFIX.len()..).unwrap_or("");
    word.get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether each label occurs more than once.
fn repeated(labels: &[String]) -> Vec<bool> {
    labels
        .iter()
        .map(|label| labels.iter().filter(|other| *other == label).count() > 1)
        .collect()
}

/// Row labels of the memory modules, one per module and all different.
///
/// A locator is shown as words ("Controller0-ChannelA-DIMM1" reads "Controller0 ChannelA
/// DIMM1"), so a narrow label column wraps between words. A controller word that starts
/// every locator is dropped ("ChannelA DIMM1"); one that differs between modules stays. A
/// locator that several modules share (boards that name "DIMM 1" in every channel) is
/// preceded by the module's bank ("P0 CHANNEL A DIMM 1"), and a label that still repeats
/// ends with the module's number. A module without a locator is "Module N".
fn module_labels(modules: &[MemoryModule]) -> Vec<String> {
    let mut locators: Vec<Option<Vec<&str>>> = modules
        .iter()
        .map(|m| {
            m.locator
                .as_deref()
                .map(slot_words)
                .filter(|words| !words.is_empty())
        })
        .collect();
    let controller = match locators.first() {
        Some(Some(words)) if is_controller_word(words[0]) => Some(words[0]),
        _ => None,
    };
    if let Some(controller) = controller {
        let shared = locators.iter().all(|l| {
            l.as_ref()
                .is_some_and(|w| w.len() > 1 && w[0] == controller)
        });
        if shared {
            for words in locators.iter_mut().flatten() {
                words.remove(0);
            }
        }
    }
    let mut labels: Vec<String> = locators
        .iter()
        .enumerate()
        .map(|(i, words)| match words {
            Some(words) => words.join(" "),
            None => format!("Module {}", i + 1),
        })
        .collect();
    let shared_locator = repeated(&labels);
    for (i, module) in modules.iter().enumerate() {
        if !shared_locator[i] || locators[i].is_none() {
            continue;
        }
        let bank = module.bank.as_deref().map(slot_words).unwrap_or_default();
        if !bank.is_empty() {
            labels[i] = format!("{} {}", bank.join(" "), labels[i]);
        }
    }
    let still_shared = repeated(&labels);
    for (i, label) in labels.iter_mut().enumerate() {
        if still_shared[i] {
            let _ = write!(label, " (module {})", i + 1);
        }
    }
    labels
}

fn module_text(module: &MemoryModule) -> String {
    let mut parts = Vec::new();
    let kind: Vec<String> = [
        module.size_bytes.map(fmt_binary),
        module.kind.clone(),
        module.form_factor.clone(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !kind.is_empty() {
        parts.push(kind.join(" "));
    }
    match (module.configured_mts, module.speed_mts) {
        (Some(configured), Some(rated)) if configured != rated => {
            parts.push(format!("{configured} MT/s (rated {rated} MT/s)"));
        }
        (Some(mts), _) | (None, Some(mts)) => parts.push(format!("{mts} MT/s")),
        (None, None) => {}
    }
    let maker = join_present(&[
        module.manufacturer.as_deref(),
        module.part_number.as_deref(),
    ]);
    if !maker.is_empty() {
        parts.push(maker);
    }
    if parts.is_empty() {
        "Unknown module".to_string()
    } else {
        parts.join(SEP)
    }
}

fn gpu_group(gpu: &GpuInfo) -> Group {
    let mut rows = vec![Row::new("Manufacturer", &gpu.vendor)];
    if gpu.basic_driver {
        rows.push(
            Row::new("Driver", "Microsoft Basic Display Adapter")
                .warning()
                .with_note(BASIC_DRIVER_NOTE),
        );
    }
    if gpu.dedicated_bytes > 0 {
        rows.push(Row::new(
            "Dedicated memory",
            fmt_binary(gpu.dedicated_bytes),
        ));
    }
    if gpu.shared_bytes > 0 {
        rows.push(Row::new("Shared memory", fmt_binary(gpu.shared_bytes)));
    }
    if !gpu.basic_driver {
        if let Some(version) = &gpu.driver_version {
            rows.push(Row::new("Driver version", version));
        }
        if let Some(date) = &gpu.driver_date {
            rows.push(Row::new("Driver date", date.format("%Y-%m-%d").to_string()));
        }
        if let Some(provider) = &gpu.driver_provider {
            rows.push(Row::new("Driver provider", provider));
        }
    }
    Group {
        title: gpu.name.clone(),
        rows,
    }
}

fn display_rows(displays: &[DisplayInfo], gpu_count: usize) -> Vec<Row> {
    if displays.is_empty() {
        return vec![Row::new("Active displays", "None")];
    }
    displays
        .iter()
        .map(|d| {
            let mut parts = Vec::new();
            if d.width > 0 && d.height > 0 {
                parts.push(format!("{} × {}", d.width, d.height));
            }
            if let Some(hz) = fmt_refresh(d.refresh_hz) {
                parts.push(hz);
            }
            parts.push(d.connection.clone());
            if d.primary && displays.len() > 1 {
                parts.push("main display".to_string());
            }
            if gpu_count > 1 {
                if let Some(gpu) = &d.gpu {
                    parts.push(format!("on {gpu}"));
                }
            }
            Row::new(&d.name, parts.join(SEP))
        })
        .collect()
}

fn board_rows(board: &BoardInfo) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(model) = system_model(
        board.system_manufacturer.as_deref(),
        board.system_family.as_deref(),
        board.system_product.as_deref(),
    ) {
        rows.push(Row::new("System", model));
    }
    if let Some(board_name) = system_model(
        board.board_manufacturer.as_deref(),
        None,
        board.board_product.as_deref(),
    ) {
        rows.push(Row::new("Motherboard", board_name));
    }
    rows.push(Row::new(
        "Firmware",
        match board.firmware {
            FirmwareKind::Uefi => "UEFI",
            FirmwareKind::Bios => "Legacy BIOS",
            FirmwareKind::Unknown => "Unknown",
        },
    ));
    let bios = join_present(&[board.bios_vendor.as_deref(), board.bios_version.as_deref()]);
    if !bios.is_empty() {
        rows.push(Row::new("BIOS", bios));
    }
    if let Some(date) = &board.bios_date {
        rows.push(Row::new("BIOS date", date.format("%Y-%m-%d").to_string()));
    }
    rows
}

fn media_text(media: MediaKind) -> Option<&'static str> {
    match media {
        MediaKind::Ssd => Some("SSD"),
        MediaKind::Hdd => Some("HDD"),
        MediaKind::Unknown => None,
    }
}

/// "NVMe SSD", "USB", "SATA HDD": the bus unless it is unknown, then the media kind.
fn drive_type(disk: &DiskInfo) -> String {
    let bus = (disk.bus != "Other").then_some(disk.bus.as_str());
    join_present(&[bus, media_text(disk.media)])
}

fn storage_groups(disks: &[DiskInfo], volumes: &[VolumeInfo]) -> Vec<Group> {
    let mut placed = vec![false; volumes.len()];
    let mut groups = Vec::new();
    for disk in disks {
        let mut parts = Vec::new();
        if let Some(size) = disk.size_bytes {
            parts.push(fmt_decimal(size));
        }
        let kind = drive_type(disk);
        if !kind.is_empty() {
            parts.push(kind);
        }
        if disk.removable {
            parts.push("removable".to_string());
        }
        let mut rows = vec![Row::new(
            "Drive",
            if parts.is_empty() {
                "Unknown".to_string()
            } else {
                parts.join(SEP)
            },
        )];
        if let Some(firmware) = &disk.firmware {
            rows.push(Row::new("Firmware", firmware));
        }
        for (i, volume) in volumes.iter().enumerate() {
            if volume.disk_numbers.first() == Some(&disk.number) {
                rows.push(volume_row(volume));
                placed[i] = true;
            }
        }
        groups.push(Group {
            title: format!("Disk {}: {}", disk.number, disk.model),
            rows,
        });
    }
    let others: Vec<Row> = volumes
        .iter()
        .zip(&placed)
        .filter(|(_, &placed)| !placed)
        .map(|(volume, _)| volume_row(volume))
        .collect();
    if !others.is_empty() {
        groups.push(Group {
            title: "Other drives".to_string(),
            rows: others,
        });
    }
    groups
}

fn volume_row(volume: &VolumeInfo) -> Row {
    let label = if volume.label.is_empty() {
        volume.letter.clone()
    } else {
        format!("{} {}", volume.letter, volume.label)
    };
    if volume.not_responding {
        return Row::new(label, "Not responding").warning();
    }
    if volume.kind == DriveKind::Optical {
        return Row::new(label, "Optical drive");
    }
    if !volume.ready {
        return Row::new(label, "No media");
    }
    if let Some(error) = &volume.error {
        return Row::new(label, format!("Could not read: {error}")).warning();
    }
    let (Some(size), Some(free)) = (volume.size_bytes, volume.free_bytes) else {
        let mut row = Row::new(
            label,
            if volume.file_system.is_empty() {
                "Size unknown".to_string()
            } else {
                volume.file_system.clone()
            },
        );
        if volume.system {
            row = row.with_note(SYSTEM_VOLUME_NOTE);
        }
        return row;
    };
    let mut value = format!("{} free of {}", fmt_binary(free), fmt_binary(size));
    if !volume.file_system.is_empty() {
        value.push_str(SEP);
        value.push_str(&volume.file_system);
    }
    let mut row = Row::new(label, value);
    if size > 0 {
        let free = free.min(size);
        row = row.with_fraction((size - free) as f64 / size as f64);
        // At least 90% used.
        if u128::from(free) * 10 <= u128::from(size) {
            return row.warning().with_note(LOW_SPACE_NOTE);
        }
    }
    if volume.system {
        row = row.with_note(SYSTEM_VOLUME_NOTE);
    }
    row
}

fn security_rows(security: &SecurityInfo, home: bool) -> Vec<Row> {
    let mut rows = vec![match security.secure_boot {
        SecureBoot::On => Row::new("Secure Boot", "On").good(),
        SecureBoot::Off => Row::new("Secure Boot", "Off").warning(),
        SecureBoot::Unsupported => {
            Row::new("Secure Boot", "Not supported (legacy BIOS mode)").warning()
        }
        SecureBoot::Unknown => Row::new("Secure Boot", "Unknown"),
    }];
    rows.push(
        match (security.tpm_found, security.tpm_version.as_deref()) {
            (Some(true), Some("2.0")) => Row::new("TPM", "2.0").good(),
            (Some(true), Some("1.2")) => Row::new("TPM", "1.2").warning().with_note(TPM_12_NOTE),
            (Some(true), Some(version)) => Row::new("TPM", version),
            (Some(true), None) => Row::new("TPM", "Present"),
            (Some(false), _) => Row::new("TPM", "Not found")
                .warning()
                .with_note(NO_TPM_NOTE),
            (None, _) => Row::new("TPM", "Unknown"),
        },
    );
    rows.push(match security.virtualization {
        Virtualization::Enabled => Row::new("Virtualization", "Enabled in firmware"),
        Virtualization::Disabled => {
            Row::new("Virtualization", "Disabled in firmware").with_note(if home {
                "Needed for memory integrity and WSL 2"
            } else {
                "Needed for memory integrity, WSL 2 and Windows Sandbox"
            })
        }
        Virtualization::InUse => Row::new(
            "Virtualization",
            format!(
                "In use by a hypervisor ({})",
                security.hypervisor.as_deref().unwrap_or("unknown")
            ),
        ),
        Virtualization::Unknown => Row::new("Virtualization", "Unknown"),
    });
    rows.push(feature_row(
        "Memory integrity",
        security.memory_integrity,
        security.virtualization,
        "Unknown",
    ));
    rows.push(feature_row(
        "VBS",
        security.vbs,
        security.virtualization,
        "Turned on",
    ));
    rows
}

/// Memory integrity and VBS: on, off, or turned on but not running (with the reason).
fn feature_row(
    label: &str,
    state: FeatureState,
    virtualization: Virtualization,
    unknown: &str,
) -> Row {
    match state {
        FeatureState::Running => Row::new(label, "On").good(),
        FeatureState::NotRunning => Row::new(label, "Turned on, not running")
            .warning()
            .with_note(if virtualization == Virtualization::Disabled {
                VIRTUALIZATION_OFF_NOTE
            } else {
                NEXT_RESTART_NOTE
            }),
        FeatureState::Off => Row::new(label, "Off"),
        FeatureState::Unknown => Row::new(label, unknown),
    }
}

// ───────────────────────────── Summary and text ─────────────────────────────

/// "Windows 11 Home 25H2  ·  Intel Core Ultra 7 265F  ·  32 GB RAM  ·  NVIDIA GeForce
/// RTX 5060 Ti  ·  1.02 TB NVMe SSD"; unknown parts are left out.
fn summary_line(info: &SystemInfo) -> String {
    let mut parts = Vec::new();
    if let Some(os) = &info.os {
        parts.push(match &os.display_version {
            Some(version) => format!("{} {version}", os.product_name),
            None => os.product_name.clone(),
        });
    }
    if let Some(cpu) = &info.cpu {
        let name = short_cpu_name(&cpu.name);
        if !name.is_empty() {
            parts.push(name);
        }
    }
    if let Some(memory) = &info.memory {
        let bytes =
            installed_memory(memory).or((memory.usable_bytes > 0).then_some(memory.usable_bytes));
        if let Some(bytes) = bytes {
            parts.push(format!("{} RAM", fmt_binary(bytes)));
        }
    }
    // The adapter with the most dedicated memory; the first one on a tie.
    let gpu = info
        .gpus
        .iter()
        .filter(|g| !g.basic_driver)
        .fold(None::<&GpuInfo>, |best, g| match best {
            Some(b) if b.dedicated_bytes >= g.dedicated_bytes => Some(b),
            _ => Some(g),
        });
    if let Some(gpu) = gpu {
        parts.push(gpu.name.clone());
    }
    if let Some(disk) = info.disks.iter().find(|d| d.system).or(info.disks.first()) {
        let text = join_present(&[
            disk.size_bytes.map(fmt_decimal).as_deref(),
            Some(drive_type(disk).as_str()).filter(|t| !t.is_empty()),
        ]);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(SEP)
}

fn plain_text<Tz: TimeZone>(info: &SystemInfo, sections: &[ReportSection], tz: &Tz) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Cairn {} system summary", crate::VERSION);
    let taken = info.taken_at.with_timezone(tz);
    let _ = writeln!(
        out,
        "Captured {} (UTC{})",
        taken.naive_local().format("%Y-%m-%d %H:%M"),
        taken.offset().fix()
    );
    for section in sections {
        out.push('\n');
        out.push_str(&section.title);
        out.push('\n');
        if let Some(note) = &section.note {
            let _ = writeln!(out, "  {note}");
        }
        if let Some(error) = &section.error {
            let _ = writeln!(out, "  Could not read: {error}");
        }
        let column = value_column(section);
        for row in &section.rows {
            push_row(&mut out, row, SECTION_INDENT, column);
        }
        for group in &section.groups {
            let _ = writeln!(out, "  {}", group.title);
            for row in &group.rows {
                push_row(&mut out, row, GROUP_INDENT, column);
            }
        }
    }
    out
}

/// Column where every value of `section` starts in the plain text: [`VALUE_COLUMN`], or
/// further right when a label needs it, so the values of a section stay in one column with
/// at least two spaces after the longest label. Private rows are not printed and do not
/// count.
fn value_column(section: &ReportSection) -> usize {
    let rows = section.rows.iter().map(|row| (SECTION_INDENT, row));
    let group_rows = section
        .groups
        .iter()
        .flat_map(|group| group.rows.iter().map(|row| (GROUP_INDENT, row)));
    rows.chain(group_rows)
        .filter(|(_, row)| !row.private)
        .map(|(indent, row)| indent + row.label.chars().count() + LABEL_GAP)
        .fold(VALUE_COLUMN, usize::max)
}

/// One row: indent, the label padded so the value starts at `column` (at least two spaces
/// after the label), the value, then the note on its own line under the value.
fn push_row(out: &mut String, row: &Row, indent: usize, column: usize) {
    if row.private {
        return;
    }
    let label_end = indent + row.label.chars().count();
    let column = column.max(label_end + LABEL_GAP);
    out.push_str(&" ".repeat(indent));
    out.push_str(&row.label);
    out.push_str(&" ".repeat(column - label_end));
    out.push_str(&row.value);
    out.push('\n');
    if let Some(note) = &row.note {
        out.push_str(&" ".repeat(column));
        out.push_str(note);
        out.push('\n');
    }
}

// ───────────────────────────── Formatters ─────────────────────────────

/// The present, non-empty parts joined with a space.
fn join_present(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// "31.70" → "31.7", "32.0" → "32", "1.02" → "1.02".
fn trim_decimals(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Binary size with GiB shown as "GB" (as Windows shows memory and volumes): "512 MB",
/// "31.7 GB", "32 GB", "952 GB", "1.86 TB".
pub(crate) fn fmt_binary(bytes: u64) -> String {
    if bytes < MIB {
        return format!("{} KB", bytes / KIB);
    }
    let mib = (bytes as f64 / MIB as f64).round();
    if bytes < GIB && mib < 1024.0 {
        return format!("{mib:.0} MB");
    }
    let gib = bytes as f64 / GIB as f64;
    if gib < 100.0 {
        return format!("{} GB", trim_decimals(format!("{gib:.1}")));
    }
    if gib.round() < 1024.0 {
        return format!("{gib:.0} GB");
    }
    format!("{} TB", trim_decimals(format!("{:.2}", gib / 1024.0)))
}

/// Decimal size as drives are marketed: "500 GB", "1.02 TB".
pub(crate) fn fmt_decimal(bytes: u64) -> String {
    let b = bytes as f64;
    let gb = b / 1e9;
    if gb.round() >= 1000.0 {
        return format!("{} TB", trim_decimals(format!("{:.2}", b / 1e12)));
    }
    // Thresholds sit half a unit below the next unit, so nothing shows as "1000 MB".
    if b >= 999.5e6 {
        return if gb < 100.0 {
            format!("{} GB", trim_decimals(format!("{gb:.1}")))
        } else {
            format!("{gb:.0} GB")
        };
    }
    if b >= 999.5e3 {
        return format!("{:.0} MB", b / 1e6);
    }
    format!("{} KB", bytes / 1000)
}

/// "2.40 GHz", "800 MHz".
pub(crate) fn fmt_mhz(mhz: u32) -> String {
    if mhz >= 1000 {
        format!("{:.2} GHz", f64::from(mhz) / 1000.0)
    } else {
        format!("{mhz} MHz")
    }
}

/// "768 KB", "2.0 MB", "36 MB".
pub(crate) fn fmt_cache(kib: u64) -> String {
    if kib < 1024 {
        return format!("{kib} KB");
    }
    let mib = kib as f64 / 1024.0;
    // One decimal below 10 MB, unless it would round up to "10.0".
    if mib < 9.95 {
        format!("{mib:.1} MB")
    } else {
        format!("{mib:.0} MB")
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "1 day 18 hours", "3 hours 12 minutes", "12 minutes".
pub(crate) fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = secs % 86_400 / 3_600;
    let minutes = secs % 3_600 / 60;
    if days > 0 {
        let mut text = plural(days, "day", "days");
        if hours > 0 {
            text.push(' ');
            text.push_str(&plural(hours, "hour", "hours"));
        }
        text
    } else if hours > 0 {
        let mut text = plural(hours, "hour", "hours");
        if minutes > 0 {
            text.push(' ');
            text.push_str(&plural(minutes, "minute", "minutes"));
        }
        text
    } else if minutes > 0 {
        plural(minutes, "minute", "minutes")
    } else {
        "less than a minute".to_string()
    }
}

/// Refresh rate of a `DISPLAYCONFIG_RATIONAL`; `None` for a zero numerator or denominator.
pub(crate) fn refresh_hz(numerator: u32, denominator: u32) -> Option<f64> {
    (numerator > 0 && denominator > 0).then(|| f64::from(numerator) / f64::from(denominator))
}

/// "60 Hz", "59.94 Hz", "143.98 Hz".
pub(crate) fn fmt_refresh(hz: Option<f64>) -> Option<String> {
    let hz = hz.filter(|hz| hz.is_finite() && *hz > 0.0)?;
    Some(format!("{} Hz", trim_decimals(format!("{hz:.2}"))))
}

/// Removes every ASCII case-insensitive occurrence of `needle`.
fn remove_ignoring_case(text: &str, needle: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(found) = lower[i..].find(&needle) {
        out.push_str(&text[i..i + found]);
        i += found + needle.len();
    }
    out.push_str(&text[i..]);
    out
}

/// "Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz" → "Intel Core i7-8700K": drops "(R)",
/// "(TM)", the word "CPU", a trailing clock speed and extra spaces.
pub(crate) fn short_cpu_name(name: &str) -> String {
    let mut text = name.trim();
    if let Some(at) = text.find(" @ ") {
        let clock = text[at + 3..].trim().to_ascii_lowercase();
        if clock.ends_with("ghz") || clock.ends_with("mhz") {
            text = &text[..at];
        }
    }
    let text = remove_ignoring_case(&remove_ignoring_case(text, "(R)"), "(TM)");
    text.split_whitespace()
        .filter(|word| *word != "CPU")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes a single model line from a manufacturer, a product family and a product name:
/// "{manufacturer} {family} ({product})" when the family is not part of the product name,
/// else "{manufacturer} {product}". Firmware placeholders are dropped, and the manufacturer
/// is not repeated when the model already starts with it.
pub(crate) fn system_model(
    manufacturer: Option<&str>,
    family: Option<&str>,
    product: Option<&str>,
) -> Option<String> {
    let manufacturer = manufacturer.and_then(clean_oem);
    let family = family.and_then(clean_oem);
    let product = product.and_then(clean_oem);
    let model = match (family, product) {
        (Some(family), Some(product))
            if !product
                .to_ascii_lowercase()
                .contains(&family.to_ascii_lowercase()) =>
        {
            format!("{family} ({product})")
        }
        (_, Some(product)) => product,
        (Some(family), None) => family,
        (None, None) => return manufacturer,
    };
    Some(match manufacturer {
        Some(m)
            if !model
                .to_ascii_lowercase()
                .starts_with(&m.to_ascii_lowercase()) =>
        {
            format!("{m} {model}")
        }
        _ => model,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use chrono::{FixedOffset, MappedLocalTime, NaiveDate, NaiveDateTime};

    use super::*;
    use crate::sysinfo::{CacheSizes, SectionError};

    const GIB_U: u64 = 1024 * 1024 * 1024;

    /// A desktop PC with placeholder names for its maker, board, memory, disk and monitor.
    pub(crate) fn fixture_info() -> SystemInfo {
        SystemInfo {
            taken_at: Utc.with_ymd_and_hms(2026, 9, 25, 20, 30, 0).unwrap(),
            duration_ms: 412,
            os: Some(fixture_os()),
            cpu: Some(fixture_cpu()),
            memory: Some(fixture_memory()),
            gpus: vec![fixture_gpu()],
            displays: vec![fixture_display()],
            displays_unavailable: false,
            board: Some(fixture_board()),
            disks: vec![fixture_disk()],
            volumes: vec![fixture_volume()],
            security: Some(fixture_security()),
            errors: Vec::new(),
        }
    }

    pub(crate) fn fixture_os() -> OsInfo {
        OsInfo {
            product_name: "Windows 11 Home".into(),
            edition_id: "Core".into(),
            display_version: Some("25H2".into()),
            build: 26200,
            revision: Some(9457),
            architecture: Architecture::X64,
            emulated: false,
            installed_at: DateTime::<Utc>::from_timestamp(1_773_480_600, 0),
            uptime_secs: 151_500,
            fast_startup: Some(true),
            activation: Activation::Activated,
            computer_name: "TEST-PC".into(),
        }
    }

    pub(crate) fn fixture_cpu() -> CpuInfo {
        CpuInfo {
            name: "Intel(R) Core(TM) Ultra 7 265F".into(),
            vendor: "GenuineIntel".into(),
            identifier: "Intel64 Family 6 Model 198 Stepping 2".into(),
            packages: 1,
            cores: 20,
            logical_processors: 20,
            performance_cores: Some(8),
            efficiency_cores: Some(12),
            base_mhz: Some(2400),
            cache: CacheSizes {
                l1_kib: 2048,
                l2_kib: 36 * 1024,
                l3_kib: 30 * 1024,
            },
        }
    }

    fn dimm(locator: &str) -> MemoryModule {
        MemoryModule {
            locator: Some(locator.into()),
            bank: Some("BANK 0".into()),
            size_bytes: Some(16 * GIB_U),
            kind: Some("DDR5".into()),
            form_factor: Some("DIMM".into()),
            speed_mts: Some(5600),
            configured_mts: Some(5600),
            manufacturer: Some("Fabrikam".into()),
            part_number: Some("FD5-16G-5600".into()),
        }
    }

    pub(crate) fn fixture_memory() -> MemoryInfo {
        MemoryInfo {
            installed_bytes: Some(32 * GIB_U),
            usable_bytes: 34_037_616_640,
            available_bytes: 34_037_616_640 - 12 * GIB_U,
            load_percent: 38,
            slots: Some(4),
            max_capacity_bytes: Some(256 * GIB_U),
            modules: vec![
                dimm("Controller0-ChannelA-DIMM1"),
                dimm("Controller0-ChannelB-DIMM1"),
            ],
        }
    }

    pub(crate) fn fixture_gpu() -> GpuInfo {
        GpuInfo {
            name: "NVIDIA GeForce RTX 5060 Ti".into(),
            vendor: "NVIDIA".into(),
            vendor_id: 0x10DE,
            device_id: 0x2D04,
            dedicated_bytes: 8 * GIB_U,
            shared_bytes: 16_995_868_672,
            driver_version: Some("32.0.16.1692".into()),
            driver_date: NaiveDate::from_ymd_opt(2026, 9, 4),
            driver_provider: Some("NVIDIA".into()),
            basic_driver: false,
            luid: 60570,
        }
    }

    pub(crate) fn fixture_display() -> DisplayInfo {
        DisplayInfo {
            name: "Contoso 27Q".into(),
            width: 1920,
            height: 1080,
            refresh_hz: Some(60.0),
            connection: "HDMI".into(),
            built_in: false,
            primary: true,
            gpu: Some("NVIDIA GeForce RTX 5060 Ti".into()),
        }
    }

    pub(crate) fn fixture_board() -> BoardInfo {
        BoardInfo {
            system_manufacturer: Some("CONTOSO".into()),
            system_product: Some("T5G1-0001".into()),
            system_family: Some("TOWER T5 G1".into()),
            board_manufacturer: Some("CONTOSO".into()),
            board_product: Some("CB-100".into()),
            bios_vendor: Some("CONTOSO".into()),
            bios_version: Some("1.20.0".into()),
            bios_date: NaiveDate::from_ymd_opt(2026, 1, 15),
            firmware: FirmwareKind::Uefi,
        }
    }

    pub(crate) fn fixture_disk() -> DiskInfo {
        DiskInfo {
            number: 0,
            model: "Northwind NV1000 1TB".into(),
            firmware: Some("FW100201".into()),
            bus: "NVMe".into(),
            media: MediaKind::Ssd,
            size_bytes: Some(1_024_209_543_168),
            removable: false,
            system: true,
        }
    }

    pub(crate) fn fixture_volume() -> VolumeInfo {
        VolumeInfo {
            letter: "C:".into(),
            label: "Windows".into(),
            file_system: "NTFS".into(),
            kind: DriveKind::Fixed,
            size_bytes: Some(1_021_821_579_264),
            free_bytes: Some(637_404_598_272),
            disk_numbers: vec![0],
            system: true,
            ready: true,
            not_responding: false,
            error: None,
        }
    }

    pub(crate) fn fixture_security() -> SecurityInfo {
        SecurityInfo {
            firmware: FirmwareKind::Uefi,
            secure_boot: SecureBoot::On,
            tpm_found: Some(true),
            tpm_version: Some("2.0".into()),
            virtualization: Virtualization::Disabled,
            hypervisor: None,
            memory_integrity: FeatureState::NotRunning,
            vbs: FeatureState::NotRunning,
        }
    }

    fn utc() -> &'static Utc {
        &Utc
    }

    /// Pacific time in 2026 until November: UTC-8, and UTC-7 from 2026-03-08 10:00 UTC on.
    /// Local times are looked up as if they were UTC; rendering only converts from UTC.
    #[derive(Debug, Clone, Copy)]
    struct Pacific2026;

    impl Pacific2026 {
        fn offset_at(utc: &NaiveDateTime) -> FixedOffset {
            let daylight_saving = NaiveDate::from_ymd_opt(2026, 3, 8)
                .and_then(|d| d.and_hms_opt(10, 0, 0))
                .unwrap();
            let hours = if *utc < daylight_saving { 8 } else { 7 };
            FixedOffset::west_opt(hours * 3600).unwrap()
        }
    }

    impl TimeZone for Pacific2026 {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Pacific2026 {
            Pacific2026
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            MappedLocalTime::Single(self.offset_from_utc_date(local))
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            MappedLocalTime::Single(Self::offset_at(local))
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            Self::offset_at(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Self::offset_at(utc)
        }
    }

    fn section_of(snap: &Snapshot, id: Section) -> &ReportSection {
        snap.sections.iter().find(|s| s.id == id).unwrap()
    }

    fn row<'a>(rows: &'a [Row], label: &str) -> &'a Row {
        rows.iter()
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row {label:?} in {rows:?}"))
    }

    const GOLDEN: &str = "\
Cairn {VERSION} system summary
Captured 2026-09-25 20:30 (UTC+00:00)

Windows
  Edition              Windows 11 Home
  Version              25H2
  OS build             26200.9457
  Architecture         64-bit (x64)
  Activation           Activated
  Installed            2026-03-14
                       Date of the last clean install or feature update
  Up time              1 day 18 hours
                       Fast Startup is on: shutting down does not reset this; restarting does.
  Fast Startup         On

Processor
  Name                 Intel Core Ultra 7 265F
  Cores                20 (8 performance, 12 efficiency)
  Logical processors   20
  Base speed           2.40 GHz
  L1 cache             2.0 MB
  L2 cache             36 MB
  L3 cache             30 MB
  Identifier           Intel64 Family 6 Model 198 Stepping 2

Memory
  Installed            32 GB
  Usable               31.7 GB
  In use               12 GB (38%)
  Slots                2 of 4 used
  Maximum capacity     256 GB
  ChannelA DIMM1       16 GB DDR5 DIMM  ·  5600 MT/s  ·  Fabrikam FD5-16G-5600
  ChannelB DIMM1       16 GB DDR5 DIMM  ·  5600 MT/s  ·  Fabrikam FD5-16G-5600

Graphics
  NVIDIA GeForce RTX 5060 Ti
    Manufacturer       NVIDIA
    Dedicated memory   8 GB
    Shared memory      15.8 GB
    Driver version     32.0.16.1692
    Driver date        2026-09-04
    Driver provider    NVIDIA

Displays
  Contoso 27Q          1920 × 1080  ·  60 Hz  ·  HDMI

Motherboard and firmware
  System               CONTOSO TOWER T5 G1 (T5G1-0001)
  Motherboard          CONTOSO CB-100
  Firmware             UEFI
  BIOS                 CONTOSO 1.20.0
  BIOS date            2026-01-15

Storage
  Disk 0: Northwind NV1000 1TB
    Drive              1.02 TB  ·  NVMe SSD
    Firmware           FW100201
    C: Windows         594 GB free of 952 GB  ·  NTFS
                       Windows is installed here

Security
  Secure Boot          On
  TPM                  2.0
  Virtualization       Disabled in firmware
                       Needed for memory integrity and WSL 2
  Memory integrity     Turned on, not running
                       Virtualization is disabled in the firmware (UEFI/BIOS) setup, so it cannot start.
  VBS                  Turned on, not running
                       Virtualization is disabled in the firmware (UEFI/BIOS) setup, so it cannot start.
";

    #[test]
    fn sections_are_in_fixed_order_with_titles() {
        let snap = render(fixture_info(), utc());
        let ids: Vec<Section> = snap.sections.iter().map(|s| s.id).collect();
        assert_eq!(ids, Section::ALL);
        let titles: Vec<&str> = snap.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Windows",
                "Processor",
                "Memory",
                "Graphics",
                "Displays",
                "Motherboard and firmware",
                "Storage",
                "Security"
            ]
        );
        let json = serde_json::to_value(&snap).unwrap();
        let ids: Vec<&str> = json["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "windows",
                "processor",
                "memory",
                "graphics",
                "displays",
                "board",
                "storage",
                "security"
            ]
        );
        for (section, id) in Section::ALL.iter().zip(&ids) {
            assert_eq!(section.id(), *id);
        }
        for section in &snap.sections {
            assert_eq!(section.error, None);
            assert_eq!(section.note, None);
            assert!(
                !section.rows.is_empty() || !section.groups.is_empty(),
                "{}",
                section.title
            );
        }
    }

    #[test]
    fn text_matches_golden_output() {
        let snap = render(fixture_info(), utc());
        assert_eq!(snap.text, GOLDEN.replace("{VERSION}", crate::VERSION));
    }

    #[test]
    fn text_uses_the_local_offset() {
        let offset = FixedOffset::west_opt(7 * 3600).unwrap();
        let snap = render(fixture_info(), &offset);
        assert!(
            snap.text
                .contains("Captured 2026-09-25 13:30 (UTC-07:00)\n"),
            "{}",
            snap.text
        );
        assert!(snap.text.contains("  Installed            2026-03-14\n"));
    }

    #[test]
    fn install_date_uses_the_offset_of_its_own_time() {
        // Installed 2026-01-15 23:30 PST; read in September, when the zone is at PDT.
        let mut info = fixture_info();
        info.os.as_mut().unwrap().installed_at =
            Utc.with_ymd_and_hms(2026, 1, 16, 7, 30, 0).single();
        let snap = render(info, &Pacific2026);
        assert!(
            snap.text
                .contains("Captured 2026-09-25 13:30 (UTC-07:00)\n"),
            "{}",
            snap.text
        );
        assert!(
            snap.text.contains("  Installed            2026-01-15\n"),
            "{}",
            snap.text
        );
        let windows = &section_of(&snap, Section::Windows).rows;
        assert_eq!(row(windows, "Installed").value, "2026-01-15");

        // In the daylight saving period the same zone gives the later offset.
        let mut info = fixture_info();
        info.os.as_mut().unwrap().installed_at =
            Utc.with_ymd_and_hms(2026, 7, 16, 6, 30, 0).single();
        let snap = render(info, &Pacific2026);
        let windows = &section_of(&snap, Section::Windows).rows;
        assert_eq!(row(windows, "Installed").value, "2026-07-15");
    }

    fn module(locator: Option<&str>, bank: Option<&str>) -> MemoryModule {
        MemoryModule {
            locator: locator.map(str::to_string),
            bank: bank.map(str::to_string),
            ..dimm("unused")
        }
    }

    #[test]
    fn module_labels_are_words_without_a_shared_controller() {
        // The fixture: every locator starts with the same controller.
        assert_eq!(
            module_labels(&fixture_memory().modules),
            ["ChannelA DIMM1", "ChannelB DIMM1"]
        );
        assert_eq!(
            module_labels(&[module(Some("Controller0-ChannelA-DIMM1"), None)]),
            ["ChannelA DIMM1"]
        );
        // Modules on two controllers keep them, or the labels would repeat.
        assert_eq!(
            module_labels(&[
                module(Some("Controller0-ChannelA-DIMM0"), Some("BANK 0")),
                module(Some("Controller1-ChannelA-DIMM0"), Some("BANK 0")),
            ]),
            ["Controller0 ChannelA DIMM0", "Controller1 ChannelA DIMM0"]
        );
        // Other shared first words are part of the slot name.
        assert_eq!(
            module_labels(&[
                module(Some("DIMM_A2"), Some("BANK 0")),
                module(Some("DIMM_B2"), Some("BANK 1")),
            ]),
            ["DIMM A2", "DIMM B2"]
        );
        assert_eq!(
            module_labels(&[
                module(Some("ControllerX-DIMM0"), None),
                module(Some("ControllerX-DIMM1"), None),
            ]),
            ["ControllerX DIMM0", "ControllerX DIMM1"]
        );
        // A locator of only the controller word is kept whole.
        assert_eq!(
            module_labels(&[
                module(Some("Controller0"), None),
                module(Some("Controller0-DIMM1"), None),
            ]),
            ["Controller0", "Controller0 DIMM1"]
        );
        assert_eq!(
            module_labels(&[module(None, Some("BANK 0")), module(Some(" - "), None)]),
            ["Module 1", "Module 2"]
        );
        assert!(module_labels(&[]).is_empty());
    }

    #[test]
    fn repeated_locators_get_their_bank_then_their_number() {
        // AMD boards name "DIMM 1" in each channel and tell them apart by bank.
        assert_eq!(
            module_labels(&[
                module(Some("DIMM 1"), Some("P0 CHANNEL A")),
                module(Some("DIMM 1"), Some("P0 CHANNEL B")),
            ]),
            ["P0 CHANNEL A DIMM 1", "P0 CHANNEL B DIMM 1"]
        );
        // The bank is only added where a locator repeats.
        assert_eq!(
            module_labels(&[
                module(Some("DIMM 1"), Some("P0 CHANNEL A")),
                module(Some("DIMM 1"), Some("P0 CHANNEL B")),
                module(Some("DIMM 2"), Some("P0 CHANNEL B")),
            ]),
            ["P0 CHANNEL A DIMM 1", "P0 CHANNEL B DIMM 1", "DIMM 2"]
        );
        // The same or no bank: the module number keeps the rows apart.
        assert_eq!(
            module_labels(&[
                module(Some("DIMM 1"), Some("BANK 0")),
                module(Some("DIMM 1"), Some("BANK 0")),
                module(Some("DIMM-1"), None),
            ]),
            [
                "BANK 0 DIMM 1 (module 1)",
                "BANK 0 DIMM 1 (module 2)",
                "DIMM 1"
            ]
        );
        assert_eq!(
            module_labels(&[module(Some("DIMM 1"), None), module(Some("DIMM 1"), None)]),
            ["DIMM 1 (module 1)", "DIMM 1 (module 2)"]
        );
        // A locator that reads like the label of a module without one.
        assert_eq!(
            module_labels(&[module(None, None), module(Some("Module 1"), None)]),
            ["Module 1 (module 1)", "Module 1 (module 2)"]
        );

        let mut info = fixture_info();
        info.memory.as_mut().unwrap().modules = vec![
            module(Some("DIMM 1"), Some("P0 CHANNEL A")),
            module(Some("DIMM 1"), Some("P0 CHANNEL B")),
        ];
        let snap = render(info, utc());
        let labels: Vec<&str> = section_of(&snap, Section::Memory)
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "Installed",
                "Usable",
                "In use",
                "Slots",
                "Maximum capacity",
                "P0 CHANNEL A DIMM 1",
                "P0 CHANNEL B DIMM 1"
            ]
        );
    }

    /// Column where the value of each printed row starts, by section title; a note line
    /// counts as a value.
    fn value_columns(text: &str, snap: &Snapshot) -> Vec<(String, Vec<usize>)> {
        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        let titles: Vec<&str> = snap.sections.iter().map(|s| s.title.as_str()).collect();
        let groups: Vec<&str> = snap
            .sections
            .iter()
            .flat_map(|s| s.groups.iter().map(|g| g.title.as_str()))
            .collect();
        for line in text.lines().skip(2) {
            if titles.contains(&line) {
                out.push((line.to_string(), Vec::new()));
                continue;
            }
            let body = line.trim_start();
            if line.is_empty() || groups.contains(&body) || body.starts_with("Could not read") {
                continue;
            }
            let indent = line.len() - body.len();
            // The value starts after the first run of two or more spaces past the label,
            // or right at the indent for a note line.
            let value = match body.find("  ") {
                Some(gap) => {
                    let rest = &body[gap..];
                    indent + gap + (rest.len() - rest.trim_start().len())
                }
                None => indent,
            };
            let chars = line[..value].chars().count();
            out.last_mut().unwrap().1.push(chars);
        }
        out
    }

    #[test]
    fn text_keeps_each_sections_values_in_one_column() {
        let mut info = fixture_info();
        info.memory.as_mut().unwrap().modules = vec![
            module(Some("Controller0-ChannelA-DIMM0"), None),
            module(Some("Controller1-ChannelA-DIMM0"), None),
        ];
        info.volumes.push(VolumeInfo {
            letter: "D:".into(),
            label: "Photos and home videos".into(),
            system: false,
            ..fixture_volume()
        });
        let snap = render(info, utc());
        let columns = value_columns(&snap.text, &snap);
        assert_eq!(columns.len(), Section::ALL.len());
        for (title, starts) in &columns {
            assert!(!starts.is_empty(), "{title}: {}", snap.text);
            assert!(
                starts.iter().all(|&s| s == starts[0]),
                "{title}: {starts:?}\n{}",
                snap.text
            );
        }
        let column = |title: &str| {
            columns
                .iter()
                .find(|(t, _)| t == title)
                .map(|(_, starts)| starts[0])
                .unwrap()
        };
        // "Controller0 ChannelA DIMM0" is 26 characters after the 2-space indent.
        assert_eq!(column("Memory"), 2 + 26 + 2);
        // "D: Photos and home videos" is 25 characters after the 4-space group indent.
        assert_eq!(column("Storage"), 4 + 25 + 2);
        assert_eq!(column("Windows"), VALUE_COLUMN);
        assert_eq!(column("Graphics"), VALUE_COLUMN);
        assert!(snap.text.contains(
            "\n  Controller0 ChannelA DIMM0  16 GB DDR5 DIMM  ·  5600 MT/s  ·  Fabrikam FD5-16G-5600\n"
        ));
        assert!(snap
            .text
            .contains("\n  Installed                   32 GB\n"));
        assert!(snap
            .text
            .contains("\n    Drive                      1.02 TB  ·  NVMe SSD\n"));
        assert!(snap.text.contains(
            "\n    C: Windows                 594 GB free of 952 GB  ·  NTFS\n                               Windows is installed here\n"
        ));

        // The golden fixture lines up the same way.
        let golden = render(fixture_info(), utc());
        for (title, starts) in value_columns(&golden.text, &golden) {
            assert!(
                starts.iter().all(|&s| s == VALUE_COLUMN),
                "{title}: {starts:?}"
            );
        }
    }

    #[test]
    fn a_private_row_does_not_widen_the_text_column() {
        let mut section = ReportSection {
            id: Section::Windows,
            title: "Windows".into(),
            error: None,
            note: None,
            rows: vec![
                Row::new("Edition", "Windows 11 Home"),
                Row::new("A private label longer than the field", "hidden").private(),
            ],
            groups: Vec::new(),
        };
        assert_eq!(value_column(&section), VALUE_COLUMN);
        section.rows[1].private = false;
        assert_eq!(value_column(&section), 2 + 37 + 2);
        section.groups.push(Group {
            title: "Group".into(),
            rows: vec![Row::new("A group label longer than the other one", "x")],
        });
        assert_eq!(value_column(&section), 4 + 39 + 2);
    }

    #[test]
    fn private_rows_are_left_out_of_text() {
        let snap = render(fixture_info(), utc());
        let name = row(&section_of(&snap, Section::Windows).rows, "Computer name");
        assert_eq!(name.value, "TEST-PC");
        assert!(name.private);
        assert!(!snap.text.contains("TEST-PC"));
        assert!(!snap.text.contains("Computer name"));
        assert!(!snap.summary.contains("TEST-PC"));

        let mut info = fixture_info();
        info.os.as_mut().unwrap().computer_name.clear();
        let snap = render(info, utc());
        let windows = section_of(&snap, Section::Windows);
        assert!(windows.rows.iter().all(|r| r.label != "Computer name"));
    }

    #[test]
    fn failed_section_renders_could_not_read() {
        let mut info = fixture_info();
        info.memory = None;
        info.errors.push(SectionError {
            section: Section::Memory,
            message: "access denied".into(),
        });
        let snap = render(info, utc());
        let memory = section_of(&snap, Section::Memory);
        assert_eq!(memory.error.as_deref(), Some("access denied"));
        assert!(memory.rows.is_empty() && memory.groups.is_empty());
        assert!(snap
            .text
            .contains("\nMemory\n  Could not read: access denied\n\nGraphics\n"));
        // The other sections are still rendered.
        assert!(!section_of(&snap, Section::Processor).rows.is_empty());
        assert!(!snap.summary.contains("RAM"));

        // A failed list section shows the error instead of "None found".
        let mut info = fixture_info();
        info.gpus.clear();
        info.errors.push(SectionError {
            section: Section::Graphics,
            message: "internal error: boom".into(),
        });
        let snap = render(info, utc());
        let graphics = section_of(&snap, Section::Graphics);
        assert!(graphics.rows.is_empty() && graphics.groups.is_empty());
        assert!(snap
            .text
            .contains("\nGraphics\n  Could not read: internal error: boom\n\n"));
    }

    #[test]
    fn displays_unavailable_renders_the_note() {
        let mut info = fixture_info();
        info.displays.clear();
        info.displays_unavailable = true;
        let snap = render(info, utc());
        let displays = section_of(&snap, Section::Displays);
        assert_eq!(displays.note.as_deref(), Some(DISPLAYS_UNAVAILABLE));
        assert_eq!(displays.error, None);
        assert!(displays.rows.is_empty());
        assert!(snap.text.contains(
            "\nDisplays\n  Display details are not available in a remote or locked session.\n\n"
        ));

        let mut info = fixture_info();
        info.displays.clear();
        let snap = render(info, utc());
        let displays = section_of(&snap, Section::Displays);
        assert_eq!(displays.note, None);
        assert_eq!(row(&displays.rows, "Active displays").value, "None");
    }

    #[test]
    fn displays_name_their_adapter_only_with_several_gpus() {
        let mut info = fixture_info();
        info.displays.push(DisplayInfo {
            name: "Built-in display".into(),
            width: 2560,
            height: 1600,
            refresh_hz: Some(60_000.0 / 1001.0),
            connection: "Built-in (eDP)".into(),
            built_in: true,
            primary: false,
            gpu: Some("Intel Graphics".into()),
        });
        let snap = render(info.clone(), utc());
        let rows = &section_of(&snap, Section::Displays).rows;
        assert_eq!(
            row(rows, "Contoso 27Q").value,
            "1920 × 1080  ·  60 Hz  ·  HDMI  ·  main display"
        );
        assert_eq!(
            row(rows, "Built-in display").value,
            "2560 × 1600  ·  59.94 Hz  ·  Built-in (eDP)"
        );

        let mut intel = fixture_gpu();
        intel.name = "Intel Graphics".into();
        info.gpus.push(intel);
        let snap = render(info, utc());
        let rows = &section_of(&snap, Section::Displays).rows;
        assert!(row(rows, "Contoso 27Q")
            .value
            .ends_with("  ·  on NVIDIA GeForce RTX 5060 Ti"));
        assert!(row(rows, "Built-in display")
            .value
            .ends_with("  ·  on Intel Graphics"));
    }

    fn security_rows_for(security: SecurityInfo, edition: &str) -> Vec<Row> {
        let mut info = fixture_info();
        info.os.as_mut().unwrap().edition_id = edition.into();
        info.security = Some(security);
        let snap = render(info, utc());
        section_of(&snap, Section::Security).rows.clone()
    }

    #[test]
    fn levels_and_notes() {
        // Security of the fixture.
        let rows = security_rows_for(fixture_security(), "Core");
        let secure_boot = row(&rows, "Secure Boot");
        assert_eq!(
            (secure_boot.value.as_str(), secure_boot.level),
            ("On", Level::Good)
        );
        let tpm = row(&rows, "TPM");
        assert_eq!(
            (tpm.value.as_str(), tpm.level, tpm.note.as_deref()),
            ("2.0", Level::Good, None)
        );
        let integrity = row(&rows, "Memory integrity");
        assert_eq!(integrity.value, "Turned on, not running");
        assert_eq!(integrity.level, Level::Warning);
        assert_eq!(integrity.note.as_deref(), Some(VIRTUALIZATION_OFF_NOTE));
        assert_eq!(row(&rows, "VBS").level, Level::Warning);
        let virtualization = row(&rows, "Virtualization");
        assert_eq!(virtualization.value, "Disabled in firmware");
        assert_eq!(virtualization.level, Level::Normal);

        // The other security states.
        let other = SecurityInfo {
            secure_boot: SecureBoot::Unsupported,
            tpm_version: Some("1.2".into()),
            virtualization: Virtualization::InUse,
            hypervisor: Some("Microsoft Hyper-V".into()),
            memory_integrity: FeatureState::NotRunning,
            vbs: FeatureState::Unknown,
            ..fixture_security()
        };
        let rows = security_rows_for(other, "Professional");
        let secure_boot = row(&rows, "Secure Boot");
        assert_eq!(secure_boot.value, "Not supported (legacy BIOS mode)");
        assert_eq!(secure_boot.level, Level::Warning);
        let tpm = row(&rows, "TPM");
        assert_eq!((tpm.value.as_str(), tpm.level), ("1.2", Level::Warning));
        assert_eq!(tpm.note.as_deref(), Some("Windows 11 requires TPM 2.0"));
        assert_eq!(
            row(&rows, "Virtualization").value,
            "In use by a hypervisor (Microsoft Hyper-V)"
        );
        assert_eq!(
            row(&rows, "Memory integrity").note.as_deref(),
            Some(NEXT_RESTART_NOTE)
        );
        let vbs = row(&rows, "VBS");
        assert_eq!(
            (vbs.value.as_str(), vbs.level),
            ("Turned on", Level::Normal)
        );

        let off = SecurityInfo {
            secure_boot: SecureBoot::Off,
            tpm_found: Some(false),
            tpm_version: None,
            virtualization: Virtualization::Enabled,
            memory_integrity: FeatureState::Off,
            vbs: FeatureState::Running,
            ..fixture_security()
        };
        let rows = security_rows_for(off, "Core");
        assert_eq!(row(&rows, "Secure Boot").level, Level::Warning);
        let tpm = row(&rows, "TPM");
        assert_eq!(
            (tpm.value.as_str(), tpm.level),
            ("Not found", Level::Warning)
        );
        assert_eq!(
            tpm.note.as_deref(),
            Some("No TPM, or it is turned off in the firmware setup")
        );
        let virtualization = row(&rows, "Virtualization");
        assert_eq!(virtualization.value, "Enabled in firmware");
        assert_eq!(virtualization.note, None);
        let integrity = row(&rows, "Memory integrity");
        assert_eq!(
            (integrity.value.as_str(), integrity.level),
            ("Off", Level::Normal)
        );
        let vbs = row(&rows, "VBS");
        assert_eq!((vbs.value.as_str(), vbs.level), ("On", Level::Good));

        // Memory integrity runs inside VBS, so a running memory integrity comes with VBS on.
        let running = SecurityInfo {
            virtualization: Virtualization::InUse,
            hypervisor: Some("Microsoft Hyper-V".into()),
            memory_integrity: FeatureState::Running,
            vbs: FeatureState::Running,
            ..fixture_security()
        };
        let rows = security_rows_for(running, "Core");
        let integrity = row(&rows, "Memory integrity");
        assert_eq!(
            (
                integrity.value.as_str(),
                integrity.level,
                integrity.note.as_deref()
            ),
            ("On", Level::Good, None)
        );
        let vbs = row(&rows, "VBS");
        assert_eq!((vbs.value.as_str(), vbs.level), ("On", Level::Good));
        let off = SecurityInfo {
            memory_integrity: FeatureState::Off,
            vbs: FeatureState::Off,
            ..fixture_security()
        };
        let rows = security_rows_for(off, "Core");
        assert_eq!(row(&rows, "Memory integrity").value, "Off");
        assert_eq!(row(&rows, "VBS").value, "Off");

        // Windows rows.
        let snap = render(fixture_info(), utc());
        let windows = &section_of(&snap, Section::Windows).rows;
        assert_eq!(row(windows, "Activation").level, Level::Good);
        assert_eq!(
            row(windows, "Up time").note.as_deref(),
            Some(FAST_STARTUP_NOTE)
        );
        assert_eq!(
            row(windows, "Installed").note.as_deref(),
            Some(INSTALLED_NOTE)
        );
        let mut info = fixture_info();
        let os = info.os.as_mut().unwrap();
        os.activation = Activation::NotActivated;
        os.fast_startup = Some(false);
        let snap = render(info.clone(), utc());
        let windows = &section_of(&snap, Section::Windows).rows;
        let activation = row(windows, "Activation");
        assert_eq!(
            (activation.value.as_str(), activation.level),
            ("Not activated", Level::Warning)
        );
        assert_eq!(row(windows, "Up time").note, None);
        assert_eq!(row(windows, "Fast Startup").value, "Off");
        info.os.as_mut().unwrap().fast_startup = None;
        let snap = render(info, utc());
        let windows = &section_of(&snap, Section::Windows).rows;
        assert_eq!(row(windows, "Up time").note, None);
        assert!(windows.iter().all(|r| r.label != "Fast Startup"));

        // A basic display driver.
        let mut info = fixture_info();
        let gpu = &mut info.gpus[0];
        gpu.basic_driver = true;
        gpu.name = "Microsoft Basic Display Adapter".into();
        let snap = render(info, utc());
        let group = &section_of(&snap, Section::Graphics).groups[0];
        let driver = row(&group.rows, "Driver");
        assert_eq!(driver.value, "Microsoft Basic Display Adapter");
        assert_eq!(driver.level, Level::Warning);
        assert_eq!(driver.note.as_deref(), Some(BASIC_DRIVER_NOTE));
        assert!(group.rows.iter().all(|r| r.label != "Driver version"));

        // Memory use carries a meter.
        let snap = render(fixture_info(), utc());
        let in_use = row(&section_of(&snap, Section::Memory).rows, "In use");
        let fraction = in_use.fraction.unwrap();
        assert!((fraction - 12.0 / 31.7).abs() < 0.001, "{fraction}");
    }

    #[test]
    fn volume_levels_and_notes() {
        let system = volume_row(&fixture_volume());
        assert_eq!(system.label, "C: Windows");
        assert_eq!(system.level, Level::Normal);
        assert_eq!(system.note.as_deref(), Some(SYSTEM_VOLUME_NOTE));
        let fraction = system.fraction.unwrap();
        assert!((fraction - 0.3762).abs() < 0.001, "{fraction}");

        // Exactly 90% used is a warning, and the warning note wins over the system note.
        let full = volume_row(&VolumeInfo {
            size_bytes: Some(1000 * GIB_U),
            free_bytes: Some(100 * GIB_U),
            ..fixture_volume()
        });
        assert_eq!(full.level, Level::Warning);
        assert_eq!(full.note.as_deref(), Some(LOW_SPACE_NOTE));
        assert!((full.fraction.unwrap() - 0.9).abs() < 1e-9);
        let almost = volume_row(&VolumeInfo {
            size_bytes: Some(1000 * GIB_U),
            free_bytes: Some(100 * GIB_U + 1),
            system: false,
            ..fixture_volume()
        });
        assert_eq!(almost.level, Level::Normal);
        assert_eq!(almost.note, None);

        let card = volume_row(&VolumeInfo {
            letter: "E:".into(),
            label: String::new(),
            kind: DriveKind::Removable,
            size_bytes: None,
            free_bytes: None,
            ready: false,
            system: false,
            ..fixture_volume()
        });
        assert_eq!(
            (card.label.as_str(), card.value.as_str()),
            ("E:", "No media")
        );
        let slow = volume_row(&VolumeInfo {
            not_responding: true,
            ready: false,
            ..fixture_volume()
        });
        assert_eq!(
            (slow.value.as_str(), slow.level),
            ("Not responding", Level::Warning)
        );
        let optical = volume_row(&VolumeInfo {
            letter: "F:".into(),
            label: String::new(),
            kind: DriveKind::Optical,
            ready: false,
            system: false,
            ..fixture_volume()
        });
        assert_eq!(optical.value, "Optical drive");
        let failed = volume_row(&VolumeInfo {
            error: Some("The volume does not contain a recognized file system.".into()),
            size_bytes: None,
            free_bytes: None,
            ..fixture_volume()
        });
        assert_eq!(failed.level, Level::Warning);
        assert!(failed.value.starts_with("Could not read: "));
    }

    #[test]
    fn storage_groups_hold_their_volumes_and_other_drives() {
        let mut info = fixture_info();
        info.disks.push(DiskInfo {
            number: 1,
            model: "Seagate Expansion HDD".into(),
            firmware: None,
            bus: "USB".into(),
            media: MediaKind::Unknown,
            size_bytes: Some(2_000_398_934_016),
            removable: false,
            system: false,
        });
        info.volumes.push(VolumeInfo {
            letter: "D:".into(),
            label: "Backup".into(),
            disk_numbers: vec![1, 0],
            system: false,
            ..fixture_volume()
        });
        info.volumes.push(VolumeInfo {
            letter: "R:".into(),
            label: "RAM".into(),
            kind: DriveKind::RamDisk,
            disk_numbers: Vec::new(),
            system: false,
            ..fixture_volume()
        });
        let snap = render(info, utc());
        let groups = &section_of(&snap, Section::Storage).groups;
        let titles: Vec<&str> = groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Disk 0: Northwind NV1000 1TB",
                "Disk 1: Seagate Expansion HDD",
                "Other drives"
            ]
        );
        assert_eq!(row(&groups[1].rows, "Drive").value, "2 TB  ·  USB");
        assert!(
            groups[1].rows.iter().any(|r| r.label == "D: Backup"),
            "first disk wins"
        );
        assert!(groups[0].rows.iter().all(|r| r.label != "D: Backup"));
        assert_eq!(groups[2].rows[0].label, "R: RAM");

        let mut info = fixture_info();
        info.disks.clear();
        info.volumes.clear();
        let snap = render(info, utc());
        let storage = section_of(&snap, Section::Storage);
        assert_eq!(row(&storage.rows, "Drives").value, "None found");
    }

    #[test]
    fn virtualization_note_leaves_out_sandbox_on_home() {
        let note = |edition: &str| {
            row(
                &security_rows_for(fixture_security(), edition),
                "Virtualization",
            )
            .note
            .clone()
        };
        let home = "Needed for memory integrity and WSL 2";
        let other = "Needed for memory integrity, WSL 2 and Windows Sandbox";
        assert_eq!(note("Core").as_deref(), Some(home));
        assert_eq!(note("CoreSingleLanguage").as_deref(), Some(home));
        assert_eq!(note("CoreN").as_deref(), Some(home));
        assert_eq!(note("Professional").as_deref(), Some(other));
        assert_eq!(note("Enterprise").as_deref(), Some(other));

        let mut info = fixture_info();
        info.os = None;
        let snap = render(info, utc());
        let rows = &section_of(&snap, Section::Security).rows;
        assert_eq!(row(rows, "Virtualization").note.as_deref(), Some(other));
    }

    #[test]
    fn summary_line() {
        let snap = render(fixture_info(), utc());
        assert_eq!(
            snap.summary,
            "Windows 11 Home 25H2  ·  Intel Core Ultra 7 265F  ·  32 GB RAM  ·  \
             NVIDIA GeForce RTX 5060 Ti  ·  1.02 TB NVMe SSD"
        );

        // Unknown parts are left out; the real adapter with the most memory is named.
        let mut info = fixture_info();
        info.os.as_mut().unwrap().display_version = None;
        info.cpu = None;
        let memory = info.memory.as_mut().unwrap();
        memory.installed_bytes = None;
        memory.modules.clear();
        let mut basic = fixture_gpu();
        basic.name = "Microsoft Basic Display Adapter".into();
        basic.basic_driver = true;
        basic.dedicated_bytes = 64 * GIB_U;
        let mut small = fixture_gpu();
        small.name = "Intel Graphics".into();
        small.dedicated_bytes = 128 * 1024 * 1024;
        info.gpus = vec![small, basic, fixture_gpu()];
        info.disks.clear();
        let snap = render(info, utc());
        assert_eq!(
            snap.summary,
            "Windows 11 Home  ·  31.7 GB RAM  ·  NVIDIA GeForce RTX 5060 Ti"
        );

        let empty = SystemInfo {
            os: None,
            cpu: None,
            memory: None,
            gpus: Vec::new(),
            disks: Vec::new(),
            ..fixture_info()
        };
        assert_eq!(render(empty, utc()).summary, "");
    }

    #[test]
    fn formatters_with_boundary_cases() {
        let mib = 1024 * 1024;
        assert_eq!(fmt_binary(0), "0 KB");
        assert_eq!(fmt_binary(768 * 1024), "768 KB");
        assert_eq!(fmt_binary(mib), "1 MB");
        assert_eq!(fmt_binary(512 * mib), "512 MB");
        assert_eq!(fmt_binary(GIB_U - 1), "1 GB", "rounds up to the next unit");
        assert_eq!(fmt_binary(GIB_U), "1 GB");
        assert_eq!(fmt_binary(34_037_616_640), "31.7 GB");
        assert_eq!(fmt_binary(32 * GIB_U), "32 GB");
        assert_eq!(fmt_binary(99 * GIB_U + GIB_U / 2), "99.5 GB");
        assert_eq!(fmt_binary(100 * GIB_U), "100 GB");
        assert_eq!(fmt_binary(1_021_821_579_264), "952 GB");
        assert_eq!(fmt_binary(1023 * GIB_U), "1023 GB");
        assert_eq!(fmt_binary(1024 * GIB_U), "1 TB");
        assert_eq!(fmt_binary(2_000_398_934_016), "1.82 TB");
        assert_eq!(fmt_binary(2_048_000_000_000), "1.86 TB");

        assert_eq!(fmt_decimal(1_024_209_543_168), "1.02 TB");
        assert_eq!(fmt_decimal(500_107_862_016), "500 GB");
        assert_eq!(fmt_decimal(256_060_514_304), "256 GB");
        assert_eq!(fmt_decimal(64_023_257_088), "64 GB");
        assert_eq!(fmt_decimal(31_914_983_424), "31.9 GB");
        assert_eq!(fmt_decimal(999_600_000_000), "1 TB");
        assert_eq!(fmt_decimal(999_400_000_000), "999 GB");
        assert_eq!(fmt_decimal(999_700_000), "1 GB");
        assert_eq!(fmt_decimal(15_000_000), "15 MB");
        assert_eq!(fmt_decimal(4_000), "4 KB");
        assert_eq!(fmt_decimal(4_000_787_030_016), "4 TB");

        assert_eq!(fmt_mhz(2400), "2.40 GHz");
        assert_eq!(fmt_mhz(3701), "3.70 GHz");
        assert_eq!(fmt_mhz(1000), "1.00 GHz");
        assert_eq!(fmt_mhz(999), "999 MHz");
        assert_eq!(fmt_mhz(800), "800 MHz");

        assert_eq!(fmt_cache(768), "768 KB");
        assert_eq!(fmt_cache(1023), "1023 KB");
        assert_eq!(fmt_cache(1024), "1.0 MB");
        assert_eq!(fmt_cache(2048), "2.0 MB");
        assert_eq!(fmt_cache(10 * 1024 - 10), "10 MB");
        assert_eq!(fmt_cache(36 * 1024), "36 MB");

        assert_eq!(fmt_uptime(0), "less than a minute");
        assert_eq!(fmt_uptime(59), "less than a minute");
        assert_eq!(fmt_uptime(60), "1 minute");
        assert_eq!(fmt_uptime(12 * 60), "12 minutes");
        assert_eq!(fmt_uptime(3600), "1 hour");
        assert_eq!(fmt_uptime(3600 + 60), "1 hour 1 minute");
        assert_eq!(fmt_uptime(3 * 3600 + 12 * 60 + 59), "3 hours 12 minutes");
        assert_eq!(fmt_uptime(86_400), "1 day");
        assert_eq!(fmt_uptime(86_400 + 18 * 3600 + 5 * 60), "1 day 18 hours");
        assert_eq!(fmt_uptime(2 * 86_400 + 3600), "2 days 1 hour");
        assert_eq!(fmt_uptime(40 * 86_400 + 59 * 60), "40 days");

        assert_eq!(fmt_refresh(refresh_hz(60, 1)).as_deref(), Some("60 Hz"));
        assert_eq!(
            fmt_refresh(refresh_hz(60_000, 1001)).as_deref(),
            Some("59.94 Hz")
        );
        assert_eq!(
            fmt_refresh(refresh_hz(143_981, 1000)).as_deref(),
            Some("143.98 Hz")
        );
        assert_eq!(fmt_refresh(refresh_hz(1, 0)), None);
        assert_eq!(fmt_refresh(Some(f64::NAN)), None);

        assert_eq!(
            system_model(Some("CONTOSO"), Some("TOWER T5 G1"), Some("T5G1-0001")).as_deref(),
            Some("CONTOSO TOWER T5 G1 (T5G1-0001)")
        );
        assert_eq!(
            system_model(Some("Dell Inc."), Some("XPS"), Some("XPS 15 9530")).as_deref(),
            Some("Dell Inc. XPS 15 9530")
        );
        assert_eq!(
            system_model(
                Some("HP"),
                Some("HP Pavilion"),
                Some("HP Pavilion Laptop 15-eg2xxx")
            )
            .as_deref(),
            Some("HP Pavilion Laptop 15-eg2xxx")
        );
        assert_eq!(
            system_model(
                Some("System manufacturer"),
                Some("To Be Filled By O.E.M."),
                Some("System Product Name")
            ),
            None
        );
        assert_eq!(
            system_model(
                Some("ASUS"),
                Some("Default string"),
                Some("ROG STRIX Z790-E GAMING")
            )
            .as_deref(),
            Some("ASUS ROG STRIX Z790-E GAMING")
        );
        assert_eq!(
            system_model(Some("CONTOSO"), None, Some("CB-100")).as_deref(),
            Some("CONTOSO CB-100")
        );
        assert_eq!(
            system_model(Some("CONTOSO"), None, None).as_deref(),
            Some("CONTOSO")
        );
        assert_eq!(
            system_model(None, Some("Surface"), None).as_deref(),
            Some("Surface")
        );
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snap = render(fixture_info(), utc());
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        let mut expected = snap.clone();
        for gpu in &mut expected.info.gpus {
            gpu.luid = 0; // internal; never serialized
        }
        assert_eq!(back, expected);
        assert!(!json.contains("luid"));

        // Rows written without the optional keys read back with their defaults.
        let row: Row =
            serde_json::from_str(r#"{"label": "Edition", "value": "Windows 11 Home"}"#).unwrap();
        assert_eq!(row.level, Level::Normal);
        assert_eq!((row.note, row.fraction, row.private), (None, None, false));
        let value = serde_json::to_value(&snap.sections[0].rows[0]).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["fraction", "label", "level", "note", "private", "value"]
        );
        assert_eq!(value["level"], "normal");
        let section = serde_json::to_value(&snap.sections[0]).unwrap();
        let mut keys: Vec<&str> = section
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error", "groups", "id", "note", "rows", "title"]);
    }
}
