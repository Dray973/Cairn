//! Power source: AC or battery, and the battery charge.

use serde::Serialize;
use tracing::debug;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

/// `SYSTEM_POWER_STATUS::BatteryFlag` bit for "no system battery". The value 255
/// ("status unknown") also has this bit set, so both read as no battery.
const BATTERY_FLAG_NO_SYSTEM_BATTERY: u8 = 0x80;
/// `SYSTEM_POWER_STATUS::BatteryLifePercent` when the charge is unknown.
const PERCENT_UNKNOWN: u8 = 255;

/// Where the PC draws its power from, as far as Windows reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PowerSource {
    /// Windows reports a system battery.
    pub has_battery: bool,
    /// `Some(true)` on battery, `Some(false)` on AC, `None` when unknown.
    pub on_battery: Option<bool>,
    /// Battery charge in percent; `None` when unknown.
    pub percent: Option<u8>,
}

/// The current power source (`GetSystemPowerStatus`). When the status cannot be read: no
/// battery, source and charge unknown.
pub fn power_source() -> PowerSource {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS for the duration of the call.
    match unsafe { GetSystemPowerStatus(&mut status) } {
        Ok(()) => from_status(
            status.ACLineStatus,
            status.BatteryFlag,
            status.BatteryLifePercent,
        ),
        Err(e) => {
            debug!(error = %e, "GetSystemPowerStatus failed; power source unknown");
            PowerSource {
                has_battery: false,
                on_battery: None,
                percent: None,
            }
        }
    }
}

/// Pure mapping of the `SYSTEM_POWER_STATUS` fields: `ACLineStatus` 0 is battery, 1 is AC,
/// anything else unknown; a percent of 255 is unknown.
pub fn from_status(ac_line: u8, battery_flag: u8, percent: u8) -> PowerSource {
    PowerSource {
        has_battery: battery_flag_reports_battery(battery_flag),
        on_battery: match ac_line {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        },
        percent: (percent != PERCENT_UNKNOWN).then_some(percent),
    }
}

/// True when a `BatteryFlag` value reports a system battery.
pub(crate) fn battery_flag_reports_battery(flag: u8) -> bool {
    flag & BATTERY_FLAG_NO_SYSTEM_BATTERY == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_flags() {
        assert!(!battery_flag_reports_battery(128));
        assert!(!battery_flag_reports_battery(255));
        for present in [0u8, 1, 2, 4, 8, 9] {
            assert!(battery_flag_reports_battery(present), "{present}");
        }
    }

    #[test]
    fn status_fields_map_to_a_power_source() {
        let cases = [
            ((0u8, 1u8, 80u8), (true, Some(true), Some(80u8))),
            ((1, 8, 100), (true, Some(false), Some(100))),
            ((1, 128, 255), (false, Some(false), None)),
            ((255, 255, 255), (false, None, None)),
            ((7, 2, 5), (true, None, Some(5))),
            ((0, 4, 0), (true, Some(true), Some(0))),
        ];
        for ((ac, flag, percent), (has_battery, on_battery, charge)) in cases {
            assert_eq!(
                from_status(ac, flag, percent),
                PowerSource {
                    has_battery,
                    on_battery,
                    percent: charge,
                },
                "{ac} {flag} {percent}"
            );
        }
    }

    #[test]
    fn the_live_power_source_is_readable() {
        let source = power_source();
        if !source.has_battery {
            assert_ne!(source.on_battery, Some(true), "{source:?}");
        }
        if let Some(percent) = source.percent {
            assert!(percent <= 100, "{source:?}");
        }
    }

    #[test]
    fn power_source_serializes_with_snake_case_keys() {
        let json = serde_json::to_value(from_status(1, 8, 50)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"has_battery": true, "on_battery": false, "percent": 50})
        );
    }
}
