//! Event log queries and rendering.
//!
//! Events are read through `EvtQuery`, `EvtNext` and `EvtRender` with render contexts that
//! pick fields by XPath, so nothing depends on the display language of event messages. A
//! channel query is made without `EvtQueryTolerateQueryErrors`: `EvtQuery` then fails with
//! access denied or a missing channel itself, instead of returning an empty result that
//! would read as "no events".

use std::fmt;

use chrono::{DateTime, Utc};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_EVT_CHANNEL_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_NO_MORE_ITEMS,
};
use windows::Win32::System::EventLog::{
    EvtClose, EvtCreateRenderContext, EvtNext, EvtQuery, EvtQueryChannelPath,
    EvtQueryReverseDirection, EvtRender, EvtRenderContextSystem, EvtRenderContextValues,
    EvtRenderEventValues, EVT_HANDLE, EVT_VARIANT, EVT_VARIANT_TYPE_ARRAY, EVT_VARIANT_TYPE_MASK,
};

use super::{is_win32, wide};
use crate::{Error, Result};

/// Events fetched per `EvtNext` call at most.
const BATCH: usize = 64;
/// How long one `EvtNext` call may wait, in milliseconds.
const NEXT_TIMEOUT_MS: u32 = 5000;

/// Index of `EventID`, `TimeCreated` and `EventRecordID` in a system render
/// (`EVT_SYSTEM_PROPERTY_ID`).
pub(crate) const SYSTEM_EVENT_ID: usize = 2;
pub(crate) const SYSTEM_TIME_CREATED: usize = 8;
pub(crate) const SYSTEM_RECORD_ID: usize = 9;

/// An event log handle, closed on drop.
pub(crate) struct EvtHandle(EVT_HANDLE);

impl Drop for EvtHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle came from the event log API and is closed exactly once.
            let _ = unsafe { EvtClose(self.0) };
        }
    }
}

impl fmt::Debug for EvtHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EvtHandle").field(&self.0 .0).finish()
    }
}

/// Why a channel could not be queried.
#[derive(Debug)]
pub(crate) enum LogError {
    /// The channel's access list does not let this account read it.
    AccessDenied,
    /// No channel of that name exists on this PC.
    ChannelNotFound,
    Other(Error),
}

impl LogError {
    /// Classifies an error of the event log API.
    pub(crate) fn from_error(e: Error) -> LogError {
        match &e {
            Error::Win32(inner) if is_win32(inner, ERROR_ACCESS_DENIED) => LogError::AccessDenied,
            Error::Win32(inner) if is_win32(inner, ERROR_EVT_CHANNEL_NOT_FOUND) => {
                LogError::ChannelNotFound
            }
            _ => LogError::Other(e),
        }
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogError::AccessDenied => f.write_str("access denied"),
            LogError::ChannelNotFound => f.write_str("the event log channel does not exist"),
            LogError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// `EvtQuery` flags of a channel query: the channel path, newest events first when asked;
/// never `EvtQueryTolerateQueryErrors`.
pub(crate) fn query_flags(newest_first: bool) -> u32 {
    let mut flags = EvtQueryChannelPath.0;
    if newest_first {
        flags |= EvtQueryReverseDirection.0;
    }
    flags
}

/// An open query over one channel.
#[derive(Debug)]
pub(crate) struct EventQuery {
    handle: EvtHandle,
}

/// Opens the events of `channel` that match the XPath `xpath`.
pub(crate) fn query(
    channel: &str,
    xpath: &str,
    newest_first: bool,
) -> std::result::Result<EventQuery, LogError> {
    let channel = wide(channel);
    let xpath = wide(xpath);
    // SAFETY: both strings are NUL-terminated and outlive the call; no session is passed, so
    // the local event log is queried.
    let handle = unsafe {
        EvtQuery(
            None,
            PCWSTR(channel.as_ptr()),
            PCWSTR(xpath.as_ptr()),
            query_flags(newest_first),
        )
    }
    .map_err(|e| LogError::from_error(e.into()))?;
    Ok(EventQuery {
        handle: EvtHandle(handle),
    })
}

impl EventQuery {
    /// Up to `max` more events (at most 64 per call); empty when the query is exhausted.
    pub(crate) fn next_batch(&mut self, max: usize) -> Result<Vec<EvtHandle>> {
        let mut raw = [0isize; BATCH];
        let wanted = max.clamp(1, BATCH);
        let mut returned = 0u32;
        // SAFETY: `raw[..wanted]` is writable for `wanted` handles; `returned` is a valid out
        // pointer. Every returned handle is wrapped so it is closed.
        let result = unsafe {
            EvtNext(
                self.handle.0,
                &mut raw[..wanted],
                NEXT_TIMEOUT_MS,
                0,
                &mut returned,
            )
        };
        match result {
            Ok(()) => {}
            Err(e) if is_win32(&e, ERROR_NO_MORE_ITEMS) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        }
        let count = (returned as usize).min(wanted);
        Ok(raw[..count]
            .iter()
            .map(|&h| EvtHandle(EVT_HANDLE(h)))
            .collect())
    }
}

/// Which values a render returns: the system properties, or the fields named by XPath.
#[derive(Debug)]
pub(crate) struct RenderContext {
    handle: EvtHandle,
}

impl RenderContext {
    /// Renders the `EVT_SYSTEM_PROPERTY_ID` values (see `SYSTEM_*`).
    pub(crate) fn system() -> Result<RenderContext> {
        // SAFETY: no value paths are passed for a system context.
        let handle = unsafe { EvtCreateRenderContext(None, EvtRenderContextSystem.0) }?;
        Ok(RenderContext {
            handle: EvtHandle(handle),
        })
    }

    /// Renders the values at `paths`, in order (see [`data_path`]).
    pub(crate) fn values(paths: &[&str]) -> Result<RenderContext> {
        let wide_paths: Vec<Vec<u16>> = paths.iter().map(|p| wide(p)).collect();
        let pointers: Vec<PCWSTR> = wide_paths.iter().map(|w| PCWSTR(w.as_ptr())).collect();
        // SAFETY: every pointer refers to a NUL-terminated string in `wide_paths`, which
        // outlives the call.
        let handle = unsafe { EvtCreateRenderContext(Some(&pointers), EvtRenderContextValues.0) }?;
        Ok(RenderContext {
            handle: EvtHandle(handle),
        })
    }

    /// The context's values for `event`. A field the event does not have is `Null`.
    pub(crate) fn render(&self, event: &EvtHandle) -> Result<Vec<EvtValue>> {
        let mut used = 0u32;
        let mut count = 0u32;
        // SAFETY: size probe without a buffer; the expected failure is
        // ERROR_INSUFFICIENT_BUFFER with the needed size in `used`.
        let probe = unsafe {
            EvtRender(
                Some(self.handle.0),
                event.0,
                EvtRenderEventValues.0,
                0,
                None,
                &mut used,
                &mut count,
            )
        };
        match probe {
            Ok(()) => return Ok(Vec::new()),
            Err(e) if is_win32(&e, ERROR_INSUFFICIENT_BUFFER) => {}
            Err(e) => return Err(e.into()),
        }
        // A u64 buffer keeps the EVT_VARIANT array 8-byte aligned.
        let mut buf = vec![0u64; (used as usize).div_ceil(8)];
        let size = (buf.len() * 8) as u32;
        // SAFETY: `buf` holds `size` writable bytes and outlives the call.
        unsafe {
            EvtRender(
                Some(self.handle.0),
                event.0,
                EvtRenderEventValues.0,
                size,
                Some(buf.as_mut_ptr().cast()),
                &mut used,
                &mut count,
            )
        }?;
        let fits = (used as usize).min(buf.len() * 8) / std::mem::size_of::<EVT_VARIANT>();
        let count = (count as usize).min(fits);
        let variants = buf.as_ptr().cast::<EVT_VARIANT>();
        Ok((0..count)
            // SAFETY: the render wrote `count` EVT_VARIANTs at the start of `buf`, whose
            // strings point into the same buffer, which is alive while they are decoded.
            .map(|i| unsafe { decode(&*variants.add(i)) })
            .collect())
    }
}

/// A rendered value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvtValue {
    Null,
    Bool(bool),
    UInt(u64),
    Int(i64),
    Text(String),
    /// A FILETIME (100 ns intervals since 1601).
    FileTime(u64),
    Guid(String),
    /// Any other type, by its `EVT_VARIANT_TYPE`.
    Other(u32),
}

impl EvtValue {
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            EvtValue::UInt(v) => Some(*v),
            EvtValue::Int(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            EvtValue::Bool(b) => Some(*b),
            EvtValue::UInt(v) => Some(*v != 0),
            _ => None,
        }
    }

    pub(crate) fn as_text(&self) -> Option<&str> {
        match self {
            EvtValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_time(&self) -> Option<DateTime<Utc>> {
        match self {
            EvtValue::FileTime(ft) => filetime_to_utc(*ft),
            _ => None,
        }
    }
}

/// Decodes one EVT_VARIANT by its type. Arrays are `Other`.
///
/// # Safety
/// `v` must come from `EvtRender`, with any string or GUID pointer valid for reading.
unsafe fn decode(v: &EVT_VARIANT) -> EvtValue {
    if v.Type & EVT_VARIANT_TYPE_ARRAY != 0 {
        return EvtValue::Other(v.Type);
    }
    let a = &v.Anonymous;
    // SAFETY: each union field read below is the one `Type` says is valid; pointers are
    // checked for null before they are read (the caller guarantees they are otherwise valid).
    unsafe {
        match v.Type & EVT_VARIANT_TYPE_MASK {
            0 => EvtValue::Null,
            1 => {
                if a.StringVal.is_null() {
                    EvtValue::Null
                } else {
                    EvtValue::Text(String::from_utf16_lossy(a.StringVal.as_wide()))
                }
            }
            3 => EvtValue::Int(i64::from(a.SByteVal)),
            4 => EvtValue::UInt(u64::from(a.ByteVal)),
            5 => EvtValue::Int(i64::from(a.Int16Val)),
            6 => EvtValue::UInt(u64::from(a.UInt16Val)),
            7 => EvtValue::Int(i64::from(a.Int32Val)),
            8 | 20 => EvtValue::UInt(u64::from(a.UInt32Val)),
            9 => EvtValue::Int(a.Int64Val),
            10 | 21 => EvtValue::UInt(a.UInt64Val),
            13 => EvtValue::Bool(a.BooleanVal.as_bool()),
            15 => {
                if a.GuidVal.is_null() {
                    EvtValue::Null
                } else {
                    EvtValue::Guid(format!("{:?}", *a.GuidVal))
                }
            }
            16 => EvtValue::UInt(a.SizeTVal as u64),
            17 => EvtValue::FileTime(a.FileTimeVal),
            other => EvtValue::Other(other),
        }
    }
}

/// XPath of an event data field by name: `Event/EventData/Data[@Name='{name}']`.
pub(crate) fn data_path(name: &str) -> String {
    format!("Event/EventData/Data[@Name='{name}']")
}

/// UTC time of a FILETIME; `None` for 0 and times before 1970.
pub(crate) fn filetime_to_utc(ft: u64) -> Option<DateTime<Utc>> {
    super::filetime::to_utc(ft)
}

/// `TimeCreated` filter value for an XPath query: `2026-09-28T10:00:00.000Z`.
pub(crate) fn xpath_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::EventLog::{EvtQueryTolerateQueryErrors, EVT_VARIANT_0};

    #[test]
    fn channel_queries_never_tolerate_query_errors() {
        assert_eq!(query_flags(false), 0x1);
        assert_eq!(query_flags(true), 0x1 | 0x200);
        for newest in [false, true] {
            assert_eq!(query_flags(newest) & EvtQueryTolerateQueryErrors.0, 0);
            assert_ne!(query_flags(newest) & EvtQueryChannelPath.0, 0);
        }
    }

    #[test]
    fn errors_are_classified() {
        let win32 = |code: windows::Win32::Foundation::WIN32_ERROR| {
            Error::Win32(windows::core::Error::from_hresult(
                windows::core::HRESULT::from_win32(code.0),
            ))
        };
        assert!(matches!(
            LogError::from_error(win32(ERROR_ACCESS_DENIED)),
            LogError::AccessDenied
        ));
        assert!(matches!(
            LogError::from_error(win32(ERROR_EVT_CHANNEL_NOT_FOUND)),
            LogError::ChannelNotFound
        ));
        assert!(matches!(
            LogError::from_error(Error::Other("x".into())),
            LogError::Other(_)
        ));
        assert_eq!(LogError::AccessDenied.to_string(), "access denied");
    }

    #[test]
    fn filetimes_convert_to_utc() {
        assert_eq!(filetime_to_utc(0), None);
        let t = filetime_to_utc(116_444_736_000_000_000 + 10_000_000).unwrap();
        assert_eq!(t.timestamp(), 1);
        assert_eq!(
            EvtValue::FileTime(116_444_736_000_000_000)
                .as_time()
                .unwrap()
                .timestamp(),
            0
        );
    }

    #[test]
    fn data_paths_and_query_times() {
        assert_eq!(
            data_path("BootTime"),
            "Event/EventData/Data[@Name='BootTime']"
        );
        let t = "2026-09-28T10:00:05Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(xpath_time(t), "2026-09-28T10:00:05.000Z");
    }

    #[test]
    fn variants_decode_by_type() {
        let text = wide("OneDrive.exe");
        let cases: Vec<(u32, EVT_VARIANT_0, EvtValue)> = vec![
            (0, EVT_VARIANT_0::default(), EvtValue::Null),
            (
                1,
                EVT_VARIANT_0 {
                    StringVal: PCWSTR(text.as_ptr()),
                },
                EvtValue::Text("OneDrive.exe".into()),
            ),
            (1, EVT_VARIANT_0::default(), EvtValue::Null),
            (8, EVT_VARIANT_0 { UInt32Val: 41200 }, EvtValue::UInt(41200)),
            (20, EVT_VARIANT_0 { UInt32Val: 7 }, EvtValue::UInt(7)),
            (6, EVT_VARIANT_0 { UInt16Val: 100 }, EvtValue::UInt(100)),
            (10, EVT_VARIANT_0 { UInt64Val: 9 }, EvtValue::UInt(9)),
            (7, EVT_VARIANT_0 { Int32Val: -5 }, EvtValue::Int(-5)),
            (
                13,
                EVT_VARIANT_0 {
                    BooleanVal: true.into(),
                },
                EvtValue::Bool(true),
            ),
            (
                17,
                EVT_VARIANT_0 { FileTimeVal: 42 },
                EvtValue::FileTime(42),
            ),
            (18, EVT_VARIANT_0::default(), EvtValue::Other(18)),
            (
                8 | EVT_VARIANT_TYPE_ARRAY,
                EVT_VARIANT_0::default(),
                EvtValue::Other(136),
            ),
        ];
        for (kind, value, expected) in cases {
            let v = EVT_VARIANT {
                Anonymous: value,
                Count: 0,
                Type: kind,
            };
            // SAFETY: the only pointer, the string, points into `text`, which is alive.
            assert_eq!(unsafe { decode(&v) }, expected, "type {kind}");
        }
        assert_eq!(EvtValue::UInt(3).as_u64(), Some(3));
        assert_eq!(EvtValue::Int(-3).as_u64(), None);
        assert_eq!(EvtValue::UInt(0).as_bool(), Some(false));
        assert_eq!(EvtValue::Text("x".into()).as_text(), Some("x"));
        assert_eq!(EvtValue::Null.as_time(), None);
    }

    #[test]
    fn the_system_log_is_readable() {
        // Read-only: the System log is readable by a standard user.
        let mut q = query("System", "*[System[EventID=12]]", true).unwrap();
        let events = q.next_batch(2).unwrap();
        let system = RenderContext::system().unwrap();
        for event in &events {
            let values = system.render(event).unwrap();
            assert!(values.len() > SYSTEM_RECORD_ID);
            assert_eq!(values[SYSTEM_EVENT_ID], EvtValue::UInt(12));
            assert!(values[SYSTEM_TIME_CREATED].as_time().is_some());
        }
    }

    #[test]
    fn a_missing_channel_is_reported() {
        let err = query("PCOptimizer-NoSuchChannel/Operational", "*", true).unwrap_err();
        assert!(matches!(err, LogError::ChannelNotFound), "{err}");
    }
}
