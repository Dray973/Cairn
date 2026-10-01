//! Processor name, topology, cache and base clock.
//!
//! The name comes from the registry, the topology from
//! `GetLogicalProcessorInformationEx(RelationAll)` parsed as bytes with every access
//! bounds-checked, and the base clock from `CallNtPowerInformation(ProcessorInformation)`.

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, STATUS_BUFFER_TOO_SMALL};
use windows::Win32::System::Power::{CallNtPowerInformation, ProcessorInformation};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationAll, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use super::{open_hklm, reg_dword, reg_text, CacheSizes, CpuInfo};
use crate::win::is_win32;
use crate::{Error, Result};

const PROCESSOR_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

/// `Relationship` values of `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`.
const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
/// `CacheTrace` of `PROCESSOR_CACHE_TYPE`.
const CACHE_TRACE: u32 = 3;

/// `Relationship` and `Size` DWORDs that start every record.
const RECORD_HEADER: usize = 8;
/// Offsets inside a `PROCESSOR_RELATIONSHIP` record (header included).
const CORE_EFFICIENCY_CLASS: usize = 9;
const CORE_GROUP_COUNT: usize = 30;
const CORE_GROUP_MASKS: usize = 32;
/// `GROUP_AFFINITY`: the 64-bit mask, the group number and three reserved words.
const GROUP_AFFINITY_SIZE: usize = 16;
/// Offsets inside a `CACHE_RELATIONSHIP` record (header included).
const CACHE_LEVEL: usize = 8;
const CACHE_SIZE: usize = 12;
const CACHE_TYPE: usize = 16;

/// `PROCESSOR_POWER_INFORMATION`: six DWORDs, `MaxMhz` second.
const POWER_INFO_SIZE: usize = 24;
const POWER_INFO_MAX_MHZ: usize = 4;
/// Attempts of a read that reports a larger buffer each time.
const MAX_ATTEMPTS: usize = 3;

pub(super) fn read() -> Result<CpuInfo> {
    let key = open_hklm(PROCESSOR_KEY);
    let text = |name: &str| key.as_ref().and_then(|k| reg_text(k, name));
    let topology = parse_topology(&logical_processor_information()?);
    if topology.cores == 0 || topology.logical_processors == 0 {
        return Err(Error::Other(
            "Windows reported no processor cores".to_string(),
        ));
    }
    let registry_mhz = key
        .as_ref()
        .and_then(|k| reg_dword(k, "~MHz"))
        .filter(|&mhz| mhz > 0);
    Ok(CpuInfo {
        name: collapse_whitespace(&text("ProcessorNameString").unwrap_or_default()),
        vendor: text("VendorIdentifier").unwrap_or_default(),
        identifier: collapse_whitespace(&text("Identifier").unwrap_or_default()),
        packages: topology.packages.max(1),
        cores: topology.cores,
        logical_processors: topology.logical_processors,
        performance_cores: topology.performance_cores,
        efficiency_cores: topology.efficiency_cores,
        base_mhz: base_mhz(topology.logical_processors).or(registry_mhz),
        cache: topology.cache,
    })
}

/// Words separated by single spaces: the registry pads processor names with spaces.
pub(super) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Counts from a `RelationAll` processor information buffer.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Topology {
    pub packages: u32,
    pub cores: u32,
    pub logical_processors: u32,
    pub performance_cores: Option<u32>,
    pub efficiency_cores: Option<u32>,
    pub cache: CacheSizes,
}

fn u16_at(buf: &[u8], offset: usize) -> Option<u16> {
    let b = buf.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(buf: &[u8], offset: usize) -> Option<u32> {
    let b = buf.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(buf: &[u8], offset: usize) -> Option<u64> {
    let b = buf.get(offset..offset.checked_add(8)?)?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(b);
    Some(u64::from_le_bytes(bytes))
}

/// Parses the variable-size `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records of a
/// `RelationAll` query. The walk stops at a record shorter than its header, at a size that
/// overflows, and at a record that runs past the buffer.
///
/// Logical processors are the bits set in each core's group masks (a group count of 0 is
/// read as 1). Caches are totalled per level; level 1 adds the data and instruction caches
/// and trace caches are skipped. Performance and efficiency cores are only split when the
/// cores report more than one efficiency class; performance cores have the highest class.
pub(super) fn parse_topology(buf: &[u8]) -> Topology {
    let mut topology = Topology::default();
    let mut classes: Vec<u8> = Vec::new();
    let mut cache_bytes = [0u64; 3];
    let mut pos = 0usize;
    while let (Some(relationship), Some(size)) = (u32_at(buf, pos), u32_at(buf, pos + 4)) {
        let size = size as usize;
        if size < RECORD_HEADER {
            break;
        }
        let Some(end) = pos.checked_add(size) else {
            break;
        };
        let Some(record) = buf.get(pos..end) else {
            break;
        };
        match relationship {
            RELATION_PROCESSOR_CORE => {
                topology.cores += 1;
                classes.push(record.get(CORE_EFFICIENCY_CLASS).copied().unwrap_or(0));
                let groups = u16_at(record, CORE_GROUP_COUNT).unwrap_or(0).max(1);
                for i in 0..usize::from(groups) {
                    let Some(mask) = u64_at(record, CORE_GROUP_MASKS + i * GROUP_AFFINITY_SIZE)
                    else {
                        break;
                    };
                    topology.logical_processors += mask.count_ones();
                }
            }
            RELATION_CACHE => {
                let level = record.get(CACHE_LEVEL).copied().unwrap_or(0);
                let bytes = u32_at(record, CACHE_SIZE).unwrap_or(0);
                let kind = u32_at(record, CACHE_TYPE).unwrap_or(0);
                if kind != CACHE_TRACE && (1..=3).contains(&level) {
                    cache_bytes[usize::from(level) - 1] += u64::from(bytes);
                }
            }
            RELATION_PROCESSOR_PACKAGE => topology.packages += 1,
            _ => {}
        }
        pos = end;
    }
    topology.cache = CacheSizes {
        l1_kib: cache_bytes[0] / 1024,
        l2_kib: cache_bytes[1] / 1024,
        l3_kib: cache_bytes[2] / 1024,
    };
    if let (Some(&max), Some(&min)) = (classes.iter().max(), classes.iter().min()) {
        if max != min {
            let performance = classes.iter().filter(|&&c| c == max).count() as u32;
            topology.performance_cores = Some(performance);
            topology.efficiency_cores = Some(topology.cores - performance);
        }
    }
    topology
}

/// The raw `RelationAll` buffer. It is allocated as `u64`s so the records the API writes
/// are aligned, then copied out as bytes.
fn logical_processor_information() -> Result<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: without a buffer the call only writes the required length to `len`.
    match unsafe { GetLogicalProcessorInformationEx(RelationAll, None, &mut len) } {
        Ok(()) => return Ok(Vec::new()),
        Err(e) if is_win32(&e, ERROR_INSUFFICIENT_BUFFER) => {}
        Err(e) => return Err(e.into()),
    }
    for _ in 0..MAX_ATTEMPTS {
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let mut size = (buf.len() * 8) as u32;
        // SAFETY: `buf` is writable for `size` bytes and aligned for the records; `size`
        // is a valid in/out pointer.
        let result = unsafe {
            GetLogicalProcessorInformationEx(
                RelationAll,
                Some(
                    buf.as_mut_ptr()
                        .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                ),
                &mut size,
            )
        };
        match result {
            Ok(()) => {
                let mut bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_le_bytes()).collect();
                bytes.truncate(size as usize);
                return Ok(bytes);
            }
            Err(e) if is_win32(&e, ERROR_INSUFFICIENT_BUFFER) && size > len => len = size,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Other(
        "the processor information kept growing while it was read".to_string(),
    ))
}

/// Highest non-zero `MaxMhz` of an array of `PROCESSOR_POWER_INFORMATION`.
pub(super) fn max_mhz(buf: &[u8]) -> Option<u32> {
    buf.chunks_exact(POWER_INFO_SIZE)
        .filter_map(|entry| u32_at(entry, POWER_INFO_MAX_MHZ))
        .filter(|&mhz| mhz > 0)
        .max()
}

/// Rated clock of the fastest core type; `None` when the power information is not
/// available.
fn base_mhz(logical_processors: u32) -> Option<u32> {
    let mut entries = logical_processors.max(1) as usize;
    for _ in 0..2 {
        let mut buf = vec![0u32; entries * POWER_INFO_SIZE / 4];
        let len = (buf.len() * 4) as u32;
        // SAFETY: the output buffer is writable for `len` bytes; no input buffer is passed.
        let status = unsafe {
            CallNtPowerInformation(
                ProcessorInformation,
                None,
                0,
                Some(buf.as_mut_ptr().cast()),
                len,
            )
        };
        if status == STATUS_BUFFER_TOO_SMALL {
            entries = entries.saturating_mul(4).max(256);
            continue;
        }
        if status.is_err() {
            return None;
        }
        let bytes: Vec<u8> = buf.iter().flat_map(|v| v.to_le_bytes()).collect();
        return max_mhz(&bytes);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::report::short_cpu_name;
    use super::*;

    /// A `PROCESSOR_RELATIONSHIP` record (core or package) with one mask per group.
    fn processor(relationship: u32, efficiency_class: u8, masks: &[u64]) -> Vec<u8> {
        processor_with_count(relationship, efficiency_class, masks.len() as u16, masks)
    }

    fn processor_with_count(
        relationship: u32,
        efficiency_class: u8,
        group_count: u16,
        masks: &[u64],
    ) -> Vec<u8> {
        let size = CORE_GROUP_MASKS + masks.len().max(1) * GROUP_AFFINITY_SIZE;
        let mut out = vec![0u8; size];
        out[0..4].copy_from_slice(&relationship.to_le_bytes());
        out[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        out[8] = 1; // LTP_PC_SMT when set; not read
        out[CORE_EFFICIENCY_CLASS] = efficiency_class;
        out[CORE_GROUP_COUNT..CORE_GROUP_COUNT + 2].copy_from_slice(&group_count.to_le_bytes());
        for (i, mask) in masks.iter().enumerate() {
            let at = CORE_GROUP_MASKS + i * GROUP_AFFINITY_SIZE;
            out[at..at + 8].copy_from_slice(&mask.to_le_bytes());
            out[at + 8..at + 10].copy_from_slice(&(i as u16).to_le_bytes());
        }
        out
    }

    fn core(efficiency_class: u8, mask: u64) -> Vec<u8> {
        processor(RELATION_PROCESSOR_CORE, efficiency_class, &[mask])
    }

    fn package(masks: &[u64]) -> Vec<u8> {
        processor(RELATION_PROCESSOR_PACKAGE, 0, masks)
    }

    /// A `CACHE_RELATIONSHIP` record; `kind` 0 unified, 1 instruction, 2 data, 3 trace.
    fn cache(level: u8, bytes: u32, kind: u32, mask: u64) -> Vec<u8> {
        let size = 48;
        let mut out = vec![0u8; size];
        out[0..4].copy_from_slice(&RELATION_CACHE.to_le_bytes());
        out[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        out[CACHE_LEVEL] = level;
        out[9] = 12;
        out[10..12].copy_from_slice(&64u16.to_le_bytes());
        out[CACHE_SIZE..CACHE_SIZE + 4].copy_from_slice(&bytes.to_le_bytes());
        out[CACHE_TYPE..CACHE_TYPE + 4].copy_from_slice(&kind.to_le_bytes());
        out[38..40].copy_from_slice(&1u16.to_le_bytes());
        out[40..48].copy_from_slice(&mask.to_le_bytes());
        out
    }

    /// A NUMA node or group record, which the parser skips.
    fn other(relationship: u32, size: usize) -> Vec<u8> {
        let mut out = vec![0u8; size];
        out[0..4].copy_from_slice(&relationship.to_le_bytes());
        out[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        out
    }

    #[test]
    fn hybrid_topology_splits_performance_and_efficiency_cores() {
        // This PC: 8 performance cores (class 1) and 12 efficiency cores (class 0), one
        // logical processor each.
        let mut buf = package(&[(1 << 20) - 1]);
        for i in 0..8 {
            buf.extend(core(1, 1 << i));
        }
        for i in 8..20 {
            buf.extend(core(0, 1 << i));
        }
        buf.extend(other(1, 80));
        buf.extend(other(4, 76));
        let t = parse_topology(&buf);
        assert_eq!(t.packages, 1);
        assert_eq!(t.cores, 20);
        assert_eq!(t.logical_processors, 20);
        assert_eq!(t.performance_cores, Some(8));
        assert_eq!(t.efficiency_cores, Some(12));
    }

    #[test]
    fn smt_topology_counts_threads() {
        let mut buf = package(&[0xFFF]);
        for i in 0..6 {
            buf.extend(core(0, 0b11 << (2 * i)));
        }
        let t = parse_topology(&buf);
        assert_eq!((t.packages, t.cores, t.logical_processors), (1, 6, 12));
        assert_eq!(t.performance_cores, None);
        assert_eq!(t.efficiency_cores, None);
    }

    #[test]
    fn uniform_efficiency_class_reports_no_split() {
        let mut buf = package(&[0xFF]);
        for i in 0..8 {
            buf.extend(core(1, 1 << i));
        }
        let t = parse_topology(&buf);
        assert_eq!(t.cores, 8);
        assert_eq!(
            t.performance_cores, None,
            "one class is not a hybrid processor"
        );
        assert_eq!(t.efficiency_cores, None);
    }

    #[test]
    fn two_processor_groups_add_up() {
        // Two packages of 40 threads each (20 cores with two threads), one per group.
        let mut buf = package(&[(1 << 40) - 1]);
        for i in 0..20 {
            buf.extend(core(0, 0b11 << (2 * i)));
        }
        buf.extend(package(&[(1 << 40) - 1]));
        for i in 0..20 {
            buf.extend(processor(RELATION_PROCESSOR_CORE, 0, &[0b11 << (2 * i)]));
        }
        // A core record listing two group masks counts the bits of both.
        buf.extend(processor(RELATION_PROCESSOR_CORE, 0, &[0b1, 0b1]));
        let t = parse_topology(&buf);
        assert_eq!(t.packages, 2);
        assert_eq!(t.cores, 41);
        assert_eq!(t.logical_processors, 82);
    }

    #[test]
    fn cache_totals_per_level_and_group_count_zero() {
        let mut buf = Vec::new();
        for i in 0..2 {
            buf.extend(cache(1, 48 * 1024, 2, 1 << i));
            buf.extend(cache(1, 64 * 1024, 1, 1 << i));
            buf.extend(cache(2, 3 * 1024 * 1024, 0, 1 << i));
        }
        buf.extend(cache(3, 30 * 1024 * 1024, 0, 0b11));
        // A trace cache is skipped.
        buf.extend(cache(1, 12 * 1024, 3, 0b1));
        // No level-4 total.
        buf.extend(cache(4, 64 * 1024 * 1024, 0, 0b11));
        // Windows 10 leaves the group count of a core record at 0; one mask still counts.
        buf.extend(processor_with_count(RELATION_PROCESSOR_CORE, 0, 0, &[0b11]));
        let t = parse_topology(&buf);
        assert_eq!(t.cache.l1_kib, 2 * (48 + 64));
        assert_eq!(t.cache.l2_kib, 2 * 3 * 1024);
        assert_eq!(t.cache.l3_kib, 30 * 1024);
        assert_eq!((t.cores, t.logical_processors), (1, 2));
        assert_eq!(t.packages, 0);
    }

    #[test]
    fn truncated_record_stops_parsing() {
        let mut buf = core(0, 0b1);
        buf.extend(core(0, 0b10));
        let whole = parse_topology(&buf);
        assert_eq!(whole.cores, 2);
        for cut in 0..buf.len() {
            let t = parse_topology(&buf[..cut]);
            assert!(t.cores <= 1, "cut at {cut}");
        }

        // A record whose size runs past the buffer is not read.
        let mut long = core(0, 0b1);
        long.extend(core(0, 0b10));
        let second = CORE_GROUP_MASKS + GROUP_AFFINITY_SIZE;
        long[second + 4..second + 8].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse_topology(&long).cores, 1);

        // Sizes below the header, zero and near the end of the address space stop the walk.
        for size in [0u32, 4, 7, u32::MAX] {
            let mut bad = core(0, 0b1);
            bad.extend(core(0, 0b10));
            bad[second + 4..second + 8].copy_from_slice(&size.to_le_bytes());
            bad.extend(core(0, 0b100));
            assert_eq!(parse_topology(&bad).cores, 1, "size {size}");
        }
        assert_eq!(parse_topology(&[]), Topology::default());
    }

    #[test]
    fn name_whitespace_is_collapsed() {
        assert_eq!(
            collapse_whitespace("  Intel(R) Core(TM) Ultra 7 265F      "),
            "Intel(R) Core(TM) Ultra 7 265F"
        );
        assert_eq!(
            collapse_whitespace("AMD Ryzen 9 7950X 16-Core Processor\t\t "),
            "AMD Ryzen 9 7950X 16-Core Processor"
        );
        assert_eq!(collapse_whitespace("   "), "");
    }

    #[test]
    fn short_cpu_name_cases() {
        assert_eq!(
            short_cpu_name("Intel(R) Core(TM) Ultra 7 265F"),
            "Intel Core Ultra 7 265F"
        );
        assert_eq!(
            short_cpu_name("Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz"),
            "Intel Core i7-8700K"
        );
        assert_eq!(
            short_cpu_name("Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz"),
            "Intel Xeon E5-2680 v4"
        );
        assert_eq!(
            short_cpu_name("AMD Ryzen 7 5800X 8-Core Processor           "),
            "AMD Ryzen 7 5800X 8-Core Processor"
        );
        assert_eq!(
            short_cpu_name("Intel(r) Pentium(tm) CPU G4560 @ 3.50 GHz"),
            "Intel Pentium G4560"
        );
        assert_eq!(
            short_cpu_name("Snapdragon (TM) X Elite - X1E78100 - Qualcomm (TM) Oryon (TM) CPU"),
            "Snapdragon X Elite - X1E78100 - Qualcomm Oryon"
        );
        assert_eq!(short_cpu_name("Custom CPU @ fast"), "Custom @ fast");
        assert_eq!(short_cpu_name(""), "");
    }

    #[test]
    fn max_mhz_is_the_highest_rated_clock() {
        let entry = |number: u32, max: u32| -> Vec<u8> {
            [number, max, max, max, 0, 0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect()
        };
        let mut buf = Vec::new();
        for i in 0..8 {
            buf.extend(entry(i, 2400));
        }
        for i in 8..20 {
            buf.extend(entry(i, 1800));
        }
        buf.extend(entry(20, 0));
        assert_eq!(max_mhz(&buf), Some(2400));
        assert_eq!(max_mhz(&entry(0, 0)), None);
        assert_eq!(max_mhz(&buf[..10]), None, "a partial entry is ignored");
        assert_eq!(max_mhz(&[]), None);
    }
}
