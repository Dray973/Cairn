//! Active displays through `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`.
//!
//! Each active path gives the monitor (its friendly name from
//! `DisplayConfigGetDeviceInfo`), the connection, the refresh rate and, through the source
//! mode it points to, the desktop resolution and position. A remote or locked session is
//! refused access; that is reported as "not available" rather than as an error.

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR,
};

use super::gpu::luid_value;
use super::report::refresh_hz;
use super::DisplayInfo;
use crate::win::{check, from_wide_nul};
use crate::{Error, Result};

/// Attempts of a query whose path or mode count grew between the calls.
const MAX_ATTEMPTS: usize = 3;

/// Each active display with the LUID of the adapter that drives it; `None` when display
/// details are not available in this session.
pub(super) fn read() -> Result<Option<Vec<(DisplayInfo, i64)>>> {
    let Some((paths, modes)) = query()? else {
        return Ok(None);
    };
    let modes: Vec<RawMode> = modes.iter().map(raw_mode).collect();
    let displays = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let raw = raw_path(path);
            let name = target_name(path);
            (display_from(&raw, &modes, name, index), raw.adapter_luid)
        })
        .collect();
    Ok(Some(displays))
}

/// `Ok(None)` for access denied (a remote or locked session); any other failure is an
/// error.
pub(super) fn map_display_error<T>(code: WIN32_ERROR) -> Result<Option<T>> {
    if code == ERROR_ACCESS_DENIED {
        return Ok(None);
    }
    check(code)?;
    Err(Error::Other(
        "the display configuration query returned no result".into(),
    ))
}

type Config = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

/// The active paths and their modes, truncated to the counts the query returned.
fn query() -> Result<Option<Config>> {
    for _ in 0..MAX_ATTEMPTS {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        // SAFETY: both counts are valid out pointers.
        let err = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if err != ERROR_SUCCESS {
            return map_display_error(err);
        }
        if path_count == 0 {
            // No active display (a headless machine).
            return Ok(Some((Vec::new(), Vec::new())));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the arrays hold as many elements as the counts passed with them; no
        // topology id is requested with QDC_ONLY_ACTIVE_PATHS.
        let err = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if err == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if err != ERROR_SUCCESS {
            return map_display_error(err);
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok(Some((paths, modes)));
    }
    Err(Error::Other(
        "the display configuration kept changing while it was read".into(),
    ))
}

/// The fields of one active path the report uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RawPath {
    pub adapter_luid: i64,
    /// Index into the mode array of the source mode (desktop size and position).
    pub source_mode_index: u32,
    pub output_technology: u32,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
}

/// A mode entry: a source mode (desktop area) or another kind of mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum RawMode {
    Source {
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    },
    Other,
}

fn raw_path(path: &DISPLAYCONFIG_PATH_INFO) -> RawPath {
    RawPath {
        adapter_luid: luid_value(path.targetInfo.adapterId),
        // SAFETY: without QDC_VIRTUAL_MODE_AWARE the union holds `modeInfoIdx`.
        source_mode_index: unsafe { path.sourceInfo.Anonymous.modeInfoIdx },
        output_technology: path.targetInfo.outputTechnology.0 as u32,
        refresh_numerator: path.targetInfo.refreshRate.Numerator,
        refresh_denominator: path.targetInfo.refreshRate.Denominator,
    }
}

fn raw_mode(mode: &DISPLAYCONFIG_MODE_INFO) -> RawMode {
    if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
        return RawMode::Other;
    }
    // SAFETY: `infoType` says the union holds a source mode.
    let source = unsafe { mode.Anonymous.sourceMode };
    RawMode::Source {
        width: source.width,
        height: source.height,
        x: source.position.x,
        y: source.position.y,
    }
}

/// Friendly name of the monitor on `path`; `None` when it has none (common for built-in
/// panels) or the query fails.
fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the pointer covers the whole DISPLAYCONFIG_TARGET_DEVICE_NAME, whose header
    // starts it and states its type and size.
    let result = unsafe {
        DisplayConfigGetDeviceInfo(
            (&mut request as *mut DISPLAYCONFIG_TARGET_DEVICE_NAME)
                .cast::<DISPLAYCONFIG_DEVICE_INFO_HEADER>(),
        )
    };
    if result != ERROR_SUCCESS.0 as i32 {
        return None;
    }
    let name = from_wide_nul(&request.monitorFriendlyDeviceName)
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// Label of a `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` and whether it is a built-in panel.
pub(super) fn connection_label(technology: u32) -> (&'static str, bool) {
    match technology {
        0 => ("VGA", false),
        1..=3 => ("Analog", false),
        4 => ("DVI", false),
        5 => ("HDMI", false),
        6 => ("Built-in", true),
        10 => ("DisplayPort", false),
        11 => ("Built-in (eDP)", true),
        12 => ("UDI", false),
        13 => ("Built-in", true),
        15 => ("Wireless (Miracast)", false),
        16 => ("USB display", false),
        17 => ("Virtual", false),
        18 => ("DisplayPort over USB-C", false),
        0x8000_0000 => ("Built-in", true),
        _ => ("Other", false),
    }
}

/// One display: the source mode the path points to gives its size and whether it is the
/// main display (its desktop starts at (0, 0)); a missing or mistyped mode leaves both
/// unknown.
pub(super) fn display_from(
    path: &RawPath,
    modes: &[RawMode],
    name: Option<String>,
    index: usize,
) -> DisplayInfo {
    let (connection, built_in) = connection_label(path.output_technology);
    let source = modes
        .get(path.source_mode_index as usize)
        .and_then(|m| match *m {
            RawMode::Source {
                width,
                height,
                x,
                y,
            } => Some((width, height, x == 0 && y == 0)),
            RawMode::Other => None,
        });
    let (width, height, primary) = source.unwrap_or((0, 0, false));
    let name = name.unwrap_or_else(|| {
        if built_in {
            "Built-in display".to_string()
        } else {
            format!("Display {}", index + 1)
        }
    });
    DisplayInfo {
        name,
        width,
        height,
        refresh_hz: refresh_hz(path.refresh_numerator, path.refresh_denominator),
        connection: connection.to_string(),
        built_in,
        primary,
        gpu: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::report::fmt_refresh;
    use super::*;

    fn path(source_mode_index: u32, technology: u32) -> RawPath {
        RawPath {
            adapter_luid: 60570,
            source_mode_index,
            output_technology: technology,
            refresh_numerator: 143_981,
            refresh_denominator: 1000,
        }
    }

    fn source(width: u32, height: u32, x: i32, y: i32) -> RawMode {
        RawMode::Source {
            width,
            height,
            x,
            y,
        }
    }

    #[test]
    fn connection_labels_and_built_in() {
        let cases: &[(u32, &str, bool)] = &[
            (0, "VGA", false),
            (1, "Analog", false),
            (2, "Analog", false),
            (3, "Analog", false),
            (4, "DVI", false),
            (5, "HDMI", false),
            (6, "Built-in", true),
            (10, "DisplayPort", false),
            (11, "Built-in (eDP)", true),
            (12, "UDI", false),
            (13, "Built-in", true),
            (15, "Wireless (Miracast)", false),
            (16, "USB display", false),
            (17, "Virtual", false),
            (18, "DisplayPort over USB-C", false),
            (0x8000_0000, "Built-in", true),
            (7, "Other", false),
            (u32::MAX, "Other", false),
        ];
        for &(technology, label, built_in) in cases {
            assert_eq!(
                connection_label(technology),
                (label, built_in),
                "{technology}"
            );
        }
    }

    #[test]
    fn refresh_rate_formatting() {
        assert_eq!(fmt_refresh(refresh_hz(60, 1)).as_deref(), Some("60 Hz"));
        assert_eq!(
            fmt_refresh(refresh_hz(60_000, 1001)).as_deref(),
            Some("59.94 Hz")
        );
        assert_eq!(
            fmt_refresh(refresh_hz(143_981, 1000)).as_deref(),
            Some("143.98 Hz")
        );
        assert_eq!(
            fmt_refresh(refresh_hz(144_000, 1000)).as_deref(),
            Some("144 Hz")
        );
        assert_eq!(fmt_refresh(refresh_hz(165, 0)), None, "zero denominator");
        assert_eq!(fmt_refresh(refresh_hz(0, 1)), None);
        assert_eq!(fmt_refresh(None), None);
        let display = display_from(&path(0, 5), &[source(2560, 1440, 0, 0)], None, 0);
        assert_eq!(
            fmt_refresh(display.refresh_hz).as_deref(),
            Some("143.98 Hz")
        );
    }

    #[test]
    fn primary_is_the_source_at_origin() {
        let modes = [
            RawMode::Other,
            source(2560, 1440, 0, 0),
            RawMode::Other,
            source(1920, 1080, 2560, 0),
        ];
        let main = display_from(&path(1, 10), &modes, Some("LG ULTRAGEAR".into()), 0);
        assert_eq!((main.width, main.height, main.primary), (2560, 1440, true));
        assert_eq!(main.name, "LG ULTRAGEAR");
        assert_eq!(main.connection, "DisplayPort");
        assert!(!main.built_in);
        let second = display_from(&path(3, 5), &modes, None, 1);
        assert_eq!(
            (second.width, second.height, second.primary),
            (1920, 1080, false)
        );
        assert_eq!(second.name, "Display 2");

        // An index past the array or at a mode of another type leaves the size unknown.
        let missing = display_from(&path(9, 5), &modes, None, 0);
        assert_eq!(
            (missing.width, missing.height, missing.primary),
            (0, 0, false)
        );
        let wrong_type = display_from(&path(2, 5), &modes, None, 0);
        assert_eq!((wrong_type.width, wrong_type.primary), (0, false));
        let invalid = display_from(&path(u32::MAX, 5), &modes, None, 0);
        assert_eq!(invalid.width, 0);

        let panel = display_from(&path(1, 11), &modes, None, 0);
        assert_eq!(panel.name, "Built-in display");
        assert!(panel.built_in);
    }

    #[test]
    fn access_denied_means_unavailable() {
        assert!(matches!(
            map_display_error::<u8>(ERROR_ACCESS_DENIED),
            Ok(None)
        ));
        let err = map_display_error::<u8>(WIN32_ERROR(87)).unwrap_err();
        assert_eq!(err.win32_code(), Some(87));
        assert!(map_display_error::<u8>(ERROR_SUCCESS).is_err());
    }

    #[test]
    fn this_session_reads_its_displays_or_reports_them_unavailable() {
        // A remote or locked session reports no details; either answer is valid.
        if let Some(displays) = read().unwrap() {
            for (display, _) in displays {
                assert!(!display.name.is_empty());
                assert!(!display.connection.is_empty());
            }
        }
    }
}
