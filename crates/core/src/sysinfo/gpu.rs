//! Graphics adapters through DXGI, with driver details from the registry.
//!
//! Adapters come from `IDXGIFactory1::EnumAdapters1`; software and remote adapters are
//! skipped. The driver version, date and provider come from the display adapter class key,
//! matched by PCI vendor and device id, or else from `HKLM\SOFTWARE\Microsoft\DirectX`,
//! matched by adapter LUID. DXGI needs no COM initialization for this.

use chrono::NaiveDate;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_REMOTE, DXGI_ADAPTER_FLAG_SOFTWARE,
    DXGI_ERROR_NOT_FOUND,
};

use super::{open_hklm, reg_qword, reg_text, GpuInfo};
use crate::win::from_wide_nul;
use crate::win::registry::{subkey_names, Hive};
use crate::Result;

/// Display adapter device class.
const DISPLAY_CLASS: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
const DIRECTX_KEY: &str = r"SOFTWARE\Microsoft\DirectX";
/// PCI vendor id of the Microsoft Basic Display Adapter.
const MICROSOFT_VENDOR: u32 = 0x1414;
/// Vendor id DXGI reports for Qualcomm Adreno: the bytes of "QCOM", little-endian.
const QUALCOMM_ACPI_VENDOR: u32 = 0x4D4F_4351;
/// Upper bound on adapters enumerated, far above any real machine.
const MAX_ADAPTERS: u32 = 64;

pub(super) fn read() -> Result<Vec<GpuInfo>> {
    // SAFETY: CreateDXGIFactory1 takes no caller-owned memory and returns an owned
    // interface.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut adapters = Vec::new();
    for index in 0..MAX_ADAPTERS {
        // SAFETY: `factory` is a valid interface; the index is a plain value.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        // SAFETY: `adapter` is a valid interface; the description is returned by value
        // and contains no `bool` fields.
        let desc = unsafe { adapter.GetDesc1() }?;
        adapters.push(AdapterDesc {
            name: from_wide_nul(&desc.Description).trim().to_string(),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            dedicated_bytes: desc.DedicatedVideoMemory as u64,
            shared_bytes: desc.SharedSystemMemory as u64,
            luid: luid_value(desc.AdapterLuid),
            flags: desc.Flags,
        });
    }
    let mut gpus: Vec<GpuInfo> = adapters.into_iter().filter_map(gpu_from).collect();
    attach_drivers(&mut gpus, &class_drivers(), &directx_drivers());
    Ok(gpus)
}

/// A LUID as one signed 64-bit value: `HighPart` in the upper half.
pub(super) fn luid_value(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

/// What DXGI reports for one adapter.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AdapterDesc {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
    pub luid: i64,
    pub flags: u32,
}

/// The adapter as a GPU; `None` for software (WARP) and remote adapters.
pub(super) fn gpu_from(desc: AdapterDesc) -> Option<GpuInfo> {
    let skipped = (DXGI_ADAPTER_FLAG_SOFTWARE.0 | DXGI_ADAPTER_FLAG_REMOTE.0) as u32;
    if desc.flags & skipped != 0 {
        return None;
    }
    Some(GpuInfo {
        vendor: vendor_name(desc.vendor_id),
        basic_driver: desc.vendor_id == MICROSOFT_VENDOR,
        name: desc.name,
        vendor_id: desc.vendor_id,
        device_id: desc.device_id,
        dedicated_bytes: desc.dedicated_bytes,
        shared_bytes: desc.shared_bytes,
        driver_version: None,
        driver_date: None,
        driver_provider: None,
        luid: desc.luid,
    })
}

/// Name of a DXGI vendor id: a PCI vendor id, or Qualcomm's ACPI one ("QCOM" as ASCII,
/// which DXGI reports for Adreno on Windows on Snapdragon).
pub(super) fn vendor_name(vendor_id: u32) -> String {
    match vendor_id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        0x5143 | QUALCOMM_ACPI_VENDOR => "Qualcomm",
        0x15AD => "VMware",
        0x80EE => "VirtualBox",
        0x1AB8 => "Parallels",
        0x1234 => "QEMU",
        other => return format!("Vendor {other:04X}"),
    }
    .to_string()
}

/// Vendor and device id of a `MatchingDeviceId` such as
/// `pci\ven_10de&dev_2d04&subsys_12345678`; `None` for any other bus or a malformed id.
pub(super) fn parse_pci_ids(matching_device_id: &str) -> Option<(u32, u32)> {
    let lower = matching_device_id.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix(r"pci\")?;
    let mut vendor = None;
    let mut device = None;
    for part in rest.split('&') {
        if let Some(hex) = part.strip_prefix("ven_") {
            vendor = u32::from_str_radix(hex, 16).ok();
        } else if let Some(hex) = part.strip_prefix("dev_") {
            device = u32::from_str_radix(hex, 16).ok();
        }
    }
    Some((vendor?, device?))
}

/// `DriverDate` is "M-D-YYYY".
pub(super) fn driver_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%m-%d-%Y").ok()
}

/// A `DriverVersion` QWORD as four 16-bit fields: `0x2000000010069c` is "32.0.16.1692".
pub(super) fn fmt_driver_version(version: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        version >> 48,
        (version >> 32) & 0xFFFF,
        (version >> 16) & 0xFFFF,
        version & 0xFFFF
    )
}

/// Driver details of one installed display driver (a class subkey).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClassDriver {
    pub vendor_id: u32,
    pub device_id: u32,
    pub version: Option<String>,
    pub date: Option<NaiveDate>,
    pub provider: Option<String>,
}

/// Driver version DirectX recorded for an adapter LUID.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DirectXDriver {
    pub luid: i64,
    pub version: Option<String>,
}

/// Fills the driver fields: from the class driver with the same PCI ids, else the version
/// DirectX recorded for the same LUID. The basic display driver gets none.
pub(super) fn attach_drivers(
    gpus: &mut [GpuInfo],
    class: &[ClassDriver],
    directx: &[DirectXDriver],
) {
    for gpu in gpus.iter_mut().filter(|g| !g.basic_driver) {
        if let Some(driver) = class
            .iter()
            .find(|d| d.vendor_id == gpu.vendor_id && d.device_id == gpu.device_id)
        {
            gpu.driver_version = driver.version.clone();
            gpu.driver_date = driver.date;
            gpu.driver_provider = driver.provider.clone();
        } else if let Some(entry) = directx.iter().find(|d| d.luid == gpu.luid) {
            gpu.driver_version = entry.version.clone();
        }
    }
}

/// Installed display drivers: the four-digit subkeys of the display adapter class.
fn class_drivers() -> Vec<ClassDriver> {
    let names = subkey_names(Hive::LocalMachine, DISPLAY_CLASS).unwrap_or_default();
    names
        .iter()
        .filter(|n| n.len() == 4 && n.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|name| {
            let key = open_hklm(&format!(r"{DISPLAY_CLASS}\{name}"))?;
            let (vendor_id, device_id) = parse_pci_ids(&reg_text(&key, "MatchingDeviceId")?)?;
            Some(ClassDriver {
                vendor_id,
                device_id,
                version: reg_text(&key, "DriverVersion"),
                date: reg_text(&key, "DriverDate").and_then(|d| driver_date(&d)),
                provider: reg_text(&key, "ProviderName"),
            })
        })
        .collect()
}

/// Adapters DirectX has seen, keyed by LUID.
fn directx_drivers() -> Vec<DirectXDriver> {
    let names = subkey_names(Hive::LocalMachine, DIRECTX_KEY).unwrap_or_default();
    names
        .iter()
        .filter(|n| n.starts_with('{'))
        .filter_map(|name| {
            let key = open_hklm(&format!(r"{DIRECTX_KEY}\{name}"))?;
            let luid = reg_qword(&key, "AdapterLuid")? as i64;
            Some(DirectXDriver {
                luid,
                version: reg_qword(&key, "DriverVersion").map(fmt_driver_version),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvidia() -> AdapterDesc {
        AdapterDesc {
            name: "NVIDIA GeForce RTX 5060 Ti".into(),
            vendor_id: 0x10DE,
            device_id: 0x2D04,
            dedicated_bytes: 16 * 1024 * 1024 * 1024,
            shared_bytes: 16 * 1024 * 1024 * 1024,
            luid: 60570,
            flags: 0,
        }
    }

    #[test]
    fn pci_ids_parse_from_matching_device_id() {
        assert_eq!(
            parse_pci_ids(r"pci\ven_10de&dev_2d04&subsys_12345678"),
            Some((0x10DE, 0x2D04))
        );
        assert_eq!(
            parse_pci_ids(r"PCI\VEN_8086&DEV_A780"),
            Some((0x8086, 0xA780))
        );
        assert_eq!(
            parse_pci_ids(r"pci\ven_1002&dev_73bf&subsys_0e3a1002&rev_c1"),
            Some((0x1002, 0x73BF))
        );
        assert_eq!(parse_pci_ids(r"root\basicdisplay"), None);
        assert_eq!(parse_pci_ids(r"pci\ven_10de"), None);
        assert_eq!(parse_pci_ids(r"pci\ven_zz&dev_1"), None);
        assert_eq!(parse_pci_ids(""), None);
    }

    #[test]
    fn driver_date_parses_m_d_yyyy() {
        assert_eq!(driver_date("9-4-2026"), NaiveDate::from_ymd_opt(2026, 9, 4));
        assert_eq!(
            driver_date("12-31-2024"),
            NaiveDate::from_ymd_opt(2024, 12, 31)
        );
        assert_eq!(driver_date("2026-09-04"), None);
        assert_eq!(driver_date("2-30-2025"), None);
    }

    #[test]
    fn driver_version_from_qword() {
        assert_eq!(fmt_driver_version(0x2000000010069c), "32.0.16.1692");
        assert_eq!(fmt_driver_version(9_007_199_255_791_260), "32.0.16.1692");
        assert_eq!(
            fmt_driver_version(0x001F_0065_0A1E_1B2A),
            "31.101.2590.6954"
        );
        assert_eq!(fmt_driver_version(0), "0.0.0.0");
    }

    #[test]
    fn software_and_remote_adapters_are_skipped() {
        let warp = AdapterDesc {
            name: "Microsoft Basic Render Driver".into(),
            vendor_id: 0x1414,
            device_id: 0x8C,
            flags: DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32,
            ..nvidia()
        };
        let remote = AdapterDesc {
            name: "Microsoft Remote Display Adapter".into(),
            flags: DXGI_ADAPTER_FLAG_REMOTE.0 as u32,
            ..nvidia()
        };
        assert_eq!(gpu_from(warp), None);
        assert_eq!(gpu_from(remote), None);
        let gpu = gpu_from(nvidia()).unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 5060 Ti");
        assert_eq!(gpu.vendor, "NVIDIA");
        assert!(!gpu.basic_driver);
        assert_eq!(gpu.luid, 60570);
    }

    #[test]
    fn basic_display_adapter_is_detected_by_vendor_id() {
        let basic = AdapterDesc {
            name: "Microsoft Basic Display Adapter".into(),
            vendor_id: 0x1414,
            device_id: 0x8C,
            dedicated_bytes: 0,
            ..nvidia()
        };
        let mut gpus = vec![gpu_from(basic).unwrap()];
        assert!(gpus[0].basic_driver);
        assert_eq!(gpus[0].vendor, "Microsoft");
        let directx = [DirectXDriver {
            luid: 60570,
            version: Some("10.0.26100.1".into()),
        }];
        attach_drivers(&mut gpus, &[], &directx);
        assert_eq!(
            gpus[0].driver_version, None,
            "the basic driver is not a graphics driver"
        );
    }

    #[test]
    fn drivers_attach_by_pci_ids_then_by_luid() {
        let intel = AdapterDesc {
            name: "Intel(R) Graphics".into(),
            vendor_id: 0x8086,
            device_id: 0x7D67,
            luid: (1_i64 << 32) | 0x1234,
            ..nvidia()
        };
        let mut gpus: Vec<GpuInfo> = [nvidia(), intel].into_iter().filter_map(gpu_from).collect();
        let class = [ClassDriver {
            vendor_id: 0x10DE,
            device_id: 0x2D04,
            version: Some("32.0.16.1692".into()),
            date: NaiveDate::from_ymd_opt(2026, 9, 4),
            provider: Some("NVIDIA".into()),
        }];
        let directx = [
            DirectXDriver {
                luid: 60570,
                version: Some("1.2.3.4".into()),
            },
            DirectXDriver {
                luid: (1_i64 << 32) | 0x1234,
                version: Some("32.0.101.6881".into()),
            },
        ];
        attach_drivers(&mut gpus, &class, &directx);
        assert_eq!(
            gpus[0].driver_version.as_deref(),
            Some("32.0.16.1692"),
            "PCI ids win"
        );
        assert_eq!(gpus[0].driver_date, NaiveDate::from_ymd_opt(2026, 9, 4));
        assert_eq!(gpus[0].driver_provider.as_deref(), Some("NVIDIA"));
        assert_eq!(
            gpus[1].driver_version.as_deref(),
            Some("32.0.101.6881"),
            "by LUID"
        );
        assert_eq!(gpus[1].driver_date, None);
        assert_eq!(gpus[1].driver_provider, None);
    }

    #[test]
    fn luid_combines_high_and_low_parts() {
        assert_eq!(
            luid_value(LUID {
                LowPart: 60570,
                HighPart: 0
            }),
            60570
        );
        assert_eq!(
            luid_value(LUID {
                LowPart: 0x1234,
                HighPart: 1
            }),
            (1 << 32) | 0x1234
        );
        assert_eq!(
            luid_value(LUID {
                LowPart: u32::MAX,
                HighPart: -1
            }),
            -1
        );
    }

    #[test]
    fn vendor_names() {
        assert_eq!(vendor_name(0x10DE), "NVIDIA");
        assert_eq!(vendor_name(0x1002), "AMD");
        assert_eq!(vendor_name(0x1022), "AMD");
        assert_eq!(vendor_name(0x8086), "Intel");
        assert_eq!(vendor_name(0x1414), "Microsoft");
        assert_eq!(vendor_name(0x5143), "Qualcomm");
        assert_eq!(vendor_name(0x4D4F_4351), "Qualcomm", "Adreno through DXGI");
        assert_eq!(u32::from_le_bytes(*b"QCOM"), QUALCOMM_ACPI_VENDOR);
        assert_eq!(vendor_name(0x15AD), "VMware");
        assert_eq!(vendor_name(0x80EE), "VirtualBox");
        assert_eq!(vendor_name(0x1AB8), "Parallels");
        assert_eq!(vendor_name(0x1234), "QEMU");
        assert_eq!(vendor_name(0xABCD), "Vendor ABCD");
    }
}
