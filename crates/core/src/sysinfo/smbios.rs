//! SMBIOS structure table parsing (the `RSMB` firmware table): physical memory arrays
//! (type 16) and memory devices (type 17).
//!
//! Every read is bounds-checked; a truncated or malformed table ends the walk without
//! panicking. Serial numbers and asset tags are never read.

use super::MemoryModule;

/// `RawSMBIOSData` header: calling method, major, minor, DMI revision, then the table
/// length as a DWORD.
const RAW_HEADER_LEN: usize = 8;
const END_OF_TABLE: u8 = 127;
const PHYSICAL_MEMORY_ARRAY: u8 = 16;
const MEMORY_DEVICE: u8 = 17;
/// `Use` of a physical memory array that holds system memory.
const USE_SYSTEM_MEMORY: u8 = 3;
const MIB: u64 = 1024 * 1024;

/// Text firmware vendors leave in unused fields.
const PLACEHOLDERS: &[&str] = &[
    "To Be Filled By O.E.M.",
    "Default string",
    "System Product Name",
    "System manufacturer",
    "Not Applicable",
    "N/A",
    "None",
    "Unknown",
    "Undefined",
    "Not Specified",
    "0123456789",
    "x.x",
    "NO DIMM",
    "Base Board Product Name",
];

/// Trimmed text, or `None` for an empty value or a firmware placeholder.
pub(crate) fn clean_oem(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || PLACEHOLDERS.iter().any(|p| p.eq_ignore_ascii_case(value)) {
        None
    } else {
        Some(value.to_string())
    }
}

/// A parsed SMBIOS table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Smbios {
    pub major: u8,
    pub minor: u8,
    pub structures: Vec<Structure>,
}

/// One structure: its formatted area and its string set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Structure {
    pub kind: u8,
    pub handle: u16,
    formatted: Vec<u8>,
    strings: Vec<String>,
}

impl Structure {
    /// Byte at `offset` of the formatted area; `None` past its length.
    pub fn byte(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    pub fn word(&self, offset: usize) -> Option<u16> {
        let b = self.formatted.get(offset..offset.checked_add(2)?)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn dword(&self, offset: usize) -> Option<u32> {
        let b = self.formatted.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn qword(&self, offset: usize) -> Option<u64> {
        let b = self.formatted.get(offset..offset.checked_add(8)?)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(b);
        Some(u64::from_le_bytes(bytes))
    }

    /// String whose 1-based index is the byte at `offset`; `None` for index 0, an index
    /// past the string set or an offset past the formatted area.
    pub fn string(&self, offset: usize) -> Option<&str> {
        let index = usize::from(self.byte(offset)?);
        index
            .checked_sub(1)
            .and_then(|i| self.strings.get(i))
            .map(String::as_str)
    }
}

/// Parses the buffer `GetSystemFirmwareTable('RSMB', 0)` fills. The declared table length
/// is clamped to the buffer; the walk stops at the end-of-table structure (type 127), at a
/// structure shorter than its 4-byte header, or at the first truncated structure.
pub(crate) fn parse(raw: &[u8]) -> Smbios {
    let major = raw.get(1).copied().unwrap_or(0);
    let minor = raw.get(2).copied().unwrap_or(0);
    let declared = raw
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .unwrap_or(0);
    let table = raw.get(RAW_HEADER_LEN..).unwrap_or(&[]);
    let table = &table[..declared.min(table.len())];

    let mut structures = Vec::new();
    let mut pos = 0usize;
    while let Some(header) = table.get(pos..pos + 4) {
        let kind = header[0];
        let len = usize::from(header[1]);
        if len < 4 || kind == END_OF_TABLE {
            break;
        }
        let Some(formatted) = table.get(pos..pos + len) else {
            break;
        };
        let Some((strings, next)) = string_set(table, pos + len) else {
            break;
        };
        structures.push(Structure {
            kind,
            handle: u16::from_le_bytes([header[2], header[3]]),
            formatted: formatted.to_vec(),
            strings,
        });
        pos = next;
    }
    Smbios {
        major,
        minor,
        structures,
    }
}

/// The strings that follow a formatted area starting at `start`, and the offset past the
/// empty string that ends the set; `None` when the table ends first.
fn string_set(table: &[u8], start: usize) -> Option<(Vec<String>, usize)> {
    let rest = table.get(start..)?;
    // A structure without strings is followed by two NULs.
    if rest.len() >= 2 && rest[0] == 0 && rest[1] == 0 {
        return Some((Vec::new(), start + 2));
    }
    let mut strings = Vec::new();
    let mut i = 0usize;
    loop {
        let len = rest.get(i..)?.iter().position(|&b| b == 0)?;
        if len == 0 {
            return Some((strings, start + i + 1));
        }
        strings.push(
            String::from_utf8_lossy(&rest[i..i + len])
                .trim()
                .to_string(),
        );
        i += len + 1;
    }
}

/// Memory slots, capacity and modules of the system memory arrays.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MemoryLayout {
    pub slots: Option<u32>,
    pub max_capacity_bytes: Option<u64>,
    pub modules: Vec<MemoryModule>,
}

/// Slots and maximum capacity come from the physical memory arrays that hold system
/// memory; modules installed in other arrays (flash, video memory) are dropped. When the
/// table has no physical memory array at all, every module is kept.
pub(crate) fn memory_layout(smbios: &Smbios) -> MemoryLayout {
    let arrays: Vec<&Structure> = smbios
        .structures
        .iter()
        .filter(|s| s.kind == PHYSICAL_MEMORY_ARRAY)
        .collect();
    let system_arrays: Vec<&Structure> = arrays
        .iter()
        .copied()
        .filter(|a| a.byte(0x05) == Some(USE_SYSTEM_MEMORY))
        .collect();

    let slots = (!system_arrays.is_empty()).then(|| {
        system_arrays
            .iter()
            .map(|a| u32::from(a.word(0x0D).unwrap_or(0)))
            .sum()
    });
    let capacities: Vec<u64> = system_arrays
        .iter()
        .filter_map(|a| array_capacity(a))
        .collect();
    let max_capacity_bytes = (!capacities.is_empty()).then(|| capacities.iter().sum());

    let system_handles: Vec<u16> = system_arrays.iter().map(|a| a.handle).collect();
    let modules = smbios
        .structures
        .iter()
        .filter(|s| s.kind == MEMORY_DEVICE)
        .filter(|s| arrays.is_empty() || s.word(0x04).is_some_and(|h| system_handles.contains(&h)))
        .filter_map(memory_device)
        .collect();

    MemoryLayout {
        slots,
        max_capacity_bytes,
        modules,
    }
}

/// Maximum capacity of a physical memory array in bytes: KiB at 0x07, or the extended
/// value at 0x0F (bytes) when 0x07 holds 0x80000000.
fn array_capacity(array: &Structure) -> Option<u64> {
    match array.dword(0x07)? {
        0 => None,
        0x8000_0000 => array.qword(0x0F).filter(|&bytes| bytes > 0),
        kib => Some(u64::from(kib) * 1024),
    }
}

enum SlotSize {
    Empty,
    Unknown,
    Bytes(u64),
}

/// Size of a memory device: 0 is an empty slot, 0xFFFF unknown, 0x7FFF means the extended
/// size at 0x1C (MiB, bits 0-30); bit 15 selects KiB instead of MiB.
fn slot_size(device: &Structure) -> SlotSize {
    match device.word(0x0C) {
        None | Some(0) => SlotSize::Empty,
        Some(0xFFFF) => SlotSize::Unknown,
        Some(0x7FFF) => match device.dword(0x1C).map(|v| v & 0x7FFF_FFFF) {
            Some(mib) if mib > 0 => SlotSize::Bytes(u64::from(mib) * MIB),
            _ => SlotSize::Unknown,
        },
        Some(v) if v & 0x8000 != 0 => SlotSize::Bytes(u64::from(v & 0x7FFF) * 1024),
        Some(v) => SlotSize::Bytes(u64::from(v) * MIB),
    }
}

/// Speed in MT/s: the word at `offset`, 0 unknown, 0xFFFF means the extended DWORD at
/// `extended` (bits 0-30).
fn speed(device: &Structure, offset: usize, extended: usize) -> Option<u32> {
    match device.word(offset)? {
        0 => None,
        0xFFFF => device
            .dword(extended)
            .map(|v| v & 0x7FFF_FFFF)
            .filter(|&v| v > 0),
        v => Some(u32::from(v)),
    }
}

fn memory_type(code: u8) -> Option<&'static str> {
    Some(match code {
        0x12 => "DDR",
        0x13 => "DDR2",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return None,
    })
}

fn form_factor(code: u8) -> Option<&'static str> {
    Some(match code {
        0x09 => "DIMM",
        0x0D => "SO-DIMM",
        0x0B => "Soldered",
        _ => return None,
    })
}

/// A populated memory device; `None` for an empty slot. The serial number (0x18) and the
/// asset tag (0x19) are never read.
fn memory_device(device: &Structure) -> Option<MemoryModule> {
    let size_bytes = match slot_size(device) {
        SlotSize::Empty => return None,
        SlotSize::Unknown => None,
        SlotSize::Bytes(bytes) => Some(bytes),
    };
    let text = |offset: usize| device.string(offset).and_then(clean_oem);
    Some(MemoryModule {
        locator: text(0x10),
        bank: text(0x11),
        size_bytes,
        kind: device.byte(0x12).and_then(memory_type).map(str::to_string),
        form_factor: device.byte(0x0E).and_then(form_factor).map(str::to_string),
        speed_mts: speed(device, 0x15, 0x54),
        configured_mts: speed(device, 0x20, 0x58),
        manufacturer: text(0x17),
        part_number: text(0x1A),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one structure: a zeroed formatted area of `len` bytes with the header filled
    /// in, `fields` written at their offsets (skipped when they do not fit), then the
    /// string set.
    fn structure(
        kind: u8,
        handle: u16,
        len: usize,
        fields: &[(usize, Vec<u8>)],
        strings: &[&str],
    ) -> Vec<u8> {
        let mut out = vec![0u8; len];
        out[0] = kind;
        out[1] = len as u8;
        out[2..4].copy_from_slice(&handle.to_le_bytes());
        for (offset, bytes) in fields {
            if offset + bytes.len() <= len {
                out[*offset..offset + bytes.len()].copy_from_slice(bytes);
            }
        }
        if strings.is_empty() {
            out.extend([0, 0]);
        } else {
            for s in strings {
                out.extend(s.bytes());
                out.push(0);
            }
            out.push(0);
        }
        out
    }

    /// `RawSMBIOSData` around `structures`, ended by a type 127 structure.
    fn raw_table(major: u8, minor: u8, structures: &[Vec<u8>]) -> Vec<u8> {
        let mut table: Vec<u8> = structures.concat();
        table.extend(structure(END_OF_TABLE, 0xFEFF, 4, &[], &[]));
        let mut raw = vec![0, major, minor, 0];
        raw.extend((table.len() as u32).to_le_bytes());
        raw.extend(table);
        raw
    }

    fn memory_array(handle: u16, usage: u8, max_kib: u32, ext: u64, slots: u16) -> Vec<u8> {
        structure(
            PHYSICAL_MEMORY_ARRAY,
            handle,
            0x17,
            &[
                (0x04, vec![3]),
                (0x05, vec![usage]),
                (0x06, vec![3]),
                (0x07, max_kib.to_le_bytes().to_vec()),
                (0x0B, 0xFFFEu16.to_le_bytes().to_vec()),
                (0x0D, slots.to_le_bytes().to_vec()),
                (0x0F, ext.to_le_bytes().to_vec()),
            ],
            &[],
        )
    }

    /// Fields of a type 17 structure used by the tests.
    #[derive(Clone)]
    struct Dimm {
        len: usize,
        array: u16,
        size: u16,
        extended_size: u32,
        form: u8,
        kind: u8,
        speed: u16,
        configured: u16,
        extended_speed: u32,
        extended_configured: u32,
        locator: &'static str,
        bank: &'static str,
        manufacturer: &'static str,
        serial: &'static str,
        part: &'static str,
    }

    impl Dimm {
        /// A 16 GiB DDR5-5600 module in the SMBIOS 3.8 layout; the part number is padded with
        /// spaces as firmware stores it.
        fn ddr5(locator: &'static str) -> Dimm {
            Dimm {
                len: 0x64,
                array: 0x1000,
                size: 0x4000,
                extended_size: 0,
                form: 0x09,
                kind: 0x22,
                speed: 5600,
                configured: 5600,
                extended_speed: 0,
                extended_configured: 0,
                locator,
                bank: "BANK 0",
                manufacturer: "Fabrikam",
                serial: "S3R14L-00001",
                part: "FD5-16G-5600    ",
            }
        }

        fn empty(locator: &'static str) -> Dimm {
            Dimm {
                size: 0,
                speed: 0,
                configured: 0,
                kind: 0x02,
                manufacturer: "NO DIMM",
                serial: "NO DIMM",
                part: "NO DIMM",
                ..Dimm::ddr5(locator)
            }
        }

        fn bytes(&self, handle: u16) -> Vec<u8> {
            structure(
                MEMORY_DEVICE,
                handle,
                self.len,
                &[
                    (0x04, self.array.to_le_bytes().to_vec()),
                    (0x08, 64u16.to_le_bytes().to_vec()),
                    (0x0A, 64u16.to_le_bytes().to_vec()),
                    (0x0C, self.size.to_le_bytes().to_vec()),
                    (0x0E, vec![self.form]),
                    (0x10, vec![1]),
                    (0x11, vec![2]),
                    (0x12, vec![self.kind]),
                    (0x15, self.speed.to_le_bytes().to_vec()),
                    (0x17, vec![3]),
                    (0x18, vec![4]),
                    (0x19, vec![0]),
                    (0x1A, vec![5]),
                    (0x1C, self.extended_size.to_le_bytes().to_vec()),
                    (0x20, self.configured.to_le_bytes().to_vec()),
                    (0x54, self.extended_speed.to_le_bytes().to_vec()),
                    (0x58, self.extended_configured.to_le_bytes().to_vec()),
                ],
                &[
                    self.locator,
                    self.bank,
                    self.manufacturer,
                    self.serial,
                    self.part,
                ],
            )
        }
    }

    /// A desktop's SMBIOS table, reduced to what the memory section reads: a BIOS
    /// structure, one system memory array with 4 slots and 256 GiB maximum, two populated
    /// and two empty slots.
    fn desktop_table() -> Vec<u8> {
        raw_table(
            3,
            8,
            &[
                structure(
                    0,
                    0x0000,
                    0x1A,
                    &[(0x04, vec![1]), (0x05, vec![2])],
                    &["CONTOSO", "1.20.0"],
                ),
                memory_array(0x1000, USE_SYSTEM_MEMORY, 0x8000_0000, 256 * 1024 * MIB, 4),
                Dimm::empty("Controller0-ChannelA-DIMM0").bytes(0x1100),
                Dimm::ddr5("Controller0-ChannelA-DIMM1").bytes(0x1101),
                Dimm::empty("Controller0-ChannelB-DIMM0").bytes(0x1102),
                Dimm::ddr5("Controller0-ChannelB-DIMM1").bytes(0x1103),
            ],
        )
    }

    #[test]
    fn parses_a_desktop_memory_layout() {
        let smbios = parse(&desktop_table());
        assert_eq!((smbios.major, smbios.minor), (3, 8));
        assert_eq!(smbios.structures.len(), 6);
        let layout = memory_layout(&smbios);
        assert_eq!(layout.slots, Some(4));
        assert_eq!(layout.max_capacity_bytes, Some(256 * 1024 * MIB));
        assert_eq!(layout.modules.len(), 2);
        for (module, locator) in layout
            .modules
            .iter()
            .zip(["Controller0-ChannelA-DIMM1", "Controller0-ChannelB-DIMM1"])
        {
            assert_eq!(module.locator.as_deref(), Some(locator));
            assert_eq!(module.bank.as_deref(), Some("BANK 0"));
            assert_eq!(module.size_bytes, Some(16 * 1024 * MIB));
            assert_eq!(module.kind.as_deref(), Some("DDR5"));
            assert_eq!(module.form_factor.as_deref(), Some("DIMM"));
            assert_eq!(module.speed_mts, Some(5600));
            assert_eq!(module.configured_mts, Some(5600));
            assert_eq!(module.manufacturer.as_deref(), Some("Fabrikam"));
            assert_eq!(module.part_number.as_deref(), Some("FD5-16G-5600"));
        }
        let debug = format!("{layout:?}");
        assert!(
            !debug.contains("S3R14L"),
            "the serial number must never be read"
        );
    }

    #[test]
    fn extended_size_and_speed_are_used_when_flagged() {
        let dimm = Dimm {
            size: 0x7FFF,
            extended_size: 0x8002_0000, // bit 31 is reserved and ignored
            speed: 0xFFFF,
            extended_speed: 70_000,
            configured: 0xFFFF,
            extended_configured: 66_000,
            ..Dimm::ddr5("DIMM A")
        };
        let raw = raw_table(
            3,
            3,
            &[memory_array(0x1000, 3, 0, 0, 1), dimm.bytes(0x1100)],
        );
        let layout = memory_layout(&parse(&raw));
        let module = &layout.modules[0];
        assert_eq!(module.size_bytes, Some(0x2_0000 * MIB));
        assert_eq!(module.speed_mts, Some(70_000));
        assert_eq!(module.configured_mts, Some(66_000));
        assert_eq!(layout.max_capacity_bytes, None, "capacity 0 is unknown");

        let unknown = Dimm {
            size: 0xFFFF,
            ..Dimm::ddr5("DIMM B")
        };
        let raw = raw_table(
            3,
            3,
            &[memory_array(0x1000, 3, 0, 0, 1), unknown.bytes(0x1100)],
        );
        let layout = memory_layout(&parse(&raw));
        assert_eq!(layout.modules.len(), 1, "an unknown size is still a module");
        assert_eq!(layout.modules[0].size_bytes, None);
    }

    #[test]
    fn kilobyte_granularity_size() {
        let dimm = Dimm {
            size: 0x8000 | 512,
            ..Dimm::ddr5("DIMM A")
        };
        let raw = raw_table(
            2,
            8,
            &[
                memory_array(0x1000, 3, 64 * 1024 * 1024, 0, 2),
                dimm.bytes(0x1100),
            ],
        );
        let layout = memory_layout(&parse(&raw));
        assert_eq!(layout.modules[0].size_bytes, Some(512 * 1024));
        assert_eq!(layout.max_capacity_bytes, Some(64 * 1024 * MIB));
        assert_eq!(layout.slots, Some(2));
    }

    #[test]
    fn short_smbios_2_3_structure_has_no_configured_speed() {
        let dimm = Dimm {
            len: 0x1B,
            kind: 0x18,
            speed: 1600,
            ..Dimm::ddr5("DIMM0")
        };
        let raw = raw_table(
            2,
            3,
            &[
                memory_array(0x1000, 3, 16 * 1024 * 1024, 0, 2),
                dimm.bytes(0x1100),
            ],
        );
        let module = &memory_layout(&parse(&raw)).modules[0];
        assert_eq!(module.kind.as_deref(), Some("DDR3"));
        assert_eq!(module.speed_mts, Some(1600));
        assert_eq!(module.configured_mts, None);
        assert_eq!(module.part_number.as_deref(), Some("FD5-16G-5600"));
    }

    #[test]
    fn non_system_arrays_are_excluded() {
        let system = Dimm::ddr5("DIMM 1");
        let video = Dimm {
            array: 0x2000,
            size: 0x2000,
            ..Dimm::ddr5("VRAM")
        };
        let raw = raw_table(
            3,
            0,
            &[
                memory_array(0x1000, 3, 0x8000_0000, 128 * 1024 * MIB, 2),
                memory_array(0x2000, 4, 8 * 1024 * 1024, 0, 8),
                system.bytes(0x1100),
                video.bytes(0x2100),
            ],
        );
        let layout = memory_layout(&parse(&raw));
        assert_eq!(layout.slots, Some(2));
        assert_eq!(layout.max_capacity_bytes, Some(128 * 1024 * MIB));
        assert_eq!(layout.modules.len(), 1);
        assert_eq!(layout.modules[0].locator.as_deref(), Some("DIMM 1"));

        // Without any array, every module is kept and the slot count is unknown.
        let raw = raw_table(3, 0, &[system.bytes(0x1100), video.bytes(0x2100)]);
        let layout = memory_layout(&parse(&raw));
        assert_eq!(layout.slots, None);
        assert_eq!(layout.modules.len(), 2);
    }

    #[test]
    fn placeholders_become_none() {
        for placeholder in PLACEHOLDERS {
            assert_eq!(clean_oem(placeholder), None, "{placeholder}");
            assert_eq!(
                clean_oem(&placeholder.to_ascii_uppercase()),
                None,
                "{placeholder}"
            );
        }
        assert_eq!(clean_oem("   "), None);
        assert_eq!(clean_oem(""), None);
        assert_eq!(clean_oem("  CONTOSO "), Some("CONTOSO".to_string()));
        assert_eq!(
            clean_oem("Unknown Vendor"),
            Some("Unknown Vendor".to_string())
        );

        let dimm = Dimm {
            locator: "Not Specified",
            bank: "  ",
            manufacturer: "Unknown",
            part: "To Be Filled By O.E.M.",
            ..Dimm::ddr5("")
        };
        let raw = raw_table(
            3,
            0,
            &[memory_array(0x1000, 3, 0, 0, 1), dimm.bytes(0x1100)],
        );
        let module = &memory_layout(&parse(&raw)).modules[0];
        assert_eq!(module.locator, None);
        assert_eq!(module.bank, None);
        assert_eq!(module.manufacturer, None);
        assert_eq!(module.part_number, None);
    }

    #[test]
    fn string_index_out_of_range_is_none() {
        let raw = raw_table(
            3,
            0,
            &[structure(
                1,
                0x0001,
                0x08,
                &[(0x04, vec![1]), (0x05, vec![9]), (0x06, vec![0])],
                &["a", "b"],
            )],
        );
        let smbios = parse(&raw);
        let s = &smbios.structures[0];
        assert_eq!(s.string(0x04), Some("a"));
        assert_eq!(s.string(0x05), None, "index 9 of 2 strings");
        assert_eq!(s.string(0x06), None, "index 0 means no string");
        assert_eq!(s.string(0x08), None, "offset past the formatted area");
        assert_eq!(s.byte(0x08), None);
        assert_eq!(s.word(0x07), None);
        assert_eq!(s.dword(0x05), None);
        assert_eq!(s.qword(0x01), None);
        assert_eq!(s.word(usize::MAX), None);
        assert_eq!(
            s.qword(0x00),
            Some(u64::from_le_bytes([1, 8, 1, 0, 1, 9, 0, 0]))
        );
    }

    #[test]
    fn truncated_and_malformed_tables_stop_without_panicking() {
        let raw = desktop_table();
        for cut in 0..raw.len() {
            let smbios = parse(&raw[..cut]);
            assert!(smbios.structures.len() <= 6);
            let _ = memory_layout(&smbios);
        }

        // A declared length past the buffer is clamped to the buffer.
        let mut long = raw.clone();
        long[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&long).structures.len(), 6);

        // A declared length that cuts the table stops at the cut.
        let mut short = raw.clone();
        short[4..8].copy_from_slice(&40u32.to_le_bytes());
        assert!(parse(&short).structures.len() < 2);

        // A structure length below its 4-byte header stops the walk.
        let mut bad = raw_table(
            3,
            0,
            &[structure(1, 1, 8, &[], &[]), structure(2, 2, 8, &[], &[])],
        );
        let second = RAW_HEADER_LEN + 8 + 2;
        bad[second + 1] = 3;
        assert_eq!(parse(&bad).structures.len(), 1);

        // A string set without its terminating empty string stops the walk.
        let mut open = vec![0, 3, 0, 0];
        let body = [1u8, 8, 1, 0, 1, 0, 0, 0, b'a', b'b', b'c'];
        open.extend((body.len() as u32).to_le_bytes());
        open.extend(body);
        assert!(parse(&open).structures.is_empty());

        // Arbitrary bytes never panic.
        let mut noise = vec![0u8, 3, 0, 0, 0, 1, 0, 0];
        noise.extend((0..=255u8).cycle().take(256).map(|b| b.wrapping_mul(37)));
        let _ = memory_layout(&parse(&noise));
        assert!(parse(&[]).structures.is_empty());
    }
}
