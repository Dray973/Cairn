//! System model, motherboard and firmware from `HKLM\HARDWARE\DESCRIPTION\System\BIOS`,
//! which Windows fills from SMBIOS at boot, and the firmware type from `GetFirmwareType`.

use chrono::NaiveDate;
use windows::Win32::System::SystemInformation::{GetFirmwareType, FIRMWARE_TYPE};

use super::smbios::clean_oem;
use super::{open_hklm, reg_text, BoardInfo, FirmwareKind};
use crate::{Error, Result};

const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";

pub(super) fn read() -> Result<BoardInfo> {
    let key = open_hklm(BIOS_KEY)
        .ok_or_else(|| Error::Other("the firmware description key cannot be read".into()))?;
    let text = |name: &str| reg_text(&key, name).and_then(|v| clean_oem(&v));
    Ok(BoardInfo {
        system_manufacturer: text("SystemManufacturer"),
        system_product: text("SystemProductName"),
        system_family: text("SystemFamily"),
        board_manufacturer: text("BaseBoardManufacturer"),
        board_product: text("BaseBoardProduct"),
        bios_vendor: text("BIOSVendor"),
        bios_version: text("BIOSVersion"),
        bios_date: reg_text(&key, "BIOSReleaseDate").and_then(|d| bios_date(&d)),
        firmware: firmware_type(),
    })
}

/// `BIOSReleaseDate` is "MM/DD/YYYY".
pub(super) fn bios_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%m/%d/%Y").ok()
}

/// Maps a `FIRMWARE_TYPE` value: 1 legacy BIOS, 2 UEFI.
pub(super) fn firmware_kind(raw: i32) -> FirmwareKind {
    match raw {
        1 => FirmwareKind::Bios,
        2 => FirmwareKind::Uefi,
        _ => FirmwareKind::Unknown,
    }
}

/// How this PC booted; `Unknown` when the query fails.
pub(super) fn firmware_type() -> FirmwareKind {
    let mut kind = FIRMWARE_TYPE(0);
    // SAFETY: `kind` is a valid out pointer for the call.
    match unsafe { GetFirmwareType(&mut kind) } {
        Ok(()) => firmware_kind(kind.0),
        Err(_) => FirmwareKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bios_release_date_parses_month_day_year() {
        assert_eq!(
            bios_date("01/15/2026"),
            NaiveDate::from_ymd_opt(2026, 1, 15)
        );
        assert_eq!(bios_date(" 1/2/2019 "), NaiveDate::from_ymd_opt(2019, 1, 2));
        assert_eq!(bios_date("2026-01-15"), None);
        assert_eq!(bios_date("13/40/2020"), None);
        assert_eq!(bios_date(""), None);
    }

    #[test]
    fn firmware_type_mapping() {
        assert_eq!(firmware_kind(1), FirmwareKind::Bios);
        assert_eq!(firmware_kind(2), FirmwareKind::Uefi);
        assert_eq!(firmware_kind(0), FirmwareKind::Unknown);
        assert_eq!(firmware_kind(3), FirmwareKind::Unknown);
    }

    #[test]
    fn this_pc_reports_its_firmware_type() {
        // Windows 11 needs UEFI; older installations may still boot in legacy mode.
        assert_ne!(firmware_type(), FirmwareKind::Unknown);
    }
}
