//! Read-only WMI queries.
//!
//! A connection is made through `IWbemLocator::ConnectServer` and secured with
//! `CoSetProxyBlanket` at packet privacy with impersonation, which the BitLocker namespace
//! requires. The same blanket is set on every enumerator a query returns, because a fresh
//! enumerator proxy keeps COM's process defaults. The engine never calls
//! `CoInitializeSecurity`: it runs as a DLL inside another program's process, whose
//! security defaults it must not claim.
//!
//! Only queries and property reads are made here; methods are never executed and instances
//! are never written. The calling thread must be in a COM apartment (see `win::com`).

use std::fmt;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use windows::core::{IUnknown, Param, BSTR, HRESULT, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Variant::{
    VARIANT, VT_BOOL, VT_BSTR, VT_EMPTY, VT_I1, VT_I2, VT_I4, VT_I8, VT_INT, VT_NULL, VT_R4, VT_R8,
    VT_UI1, VT_UI2, VT_UI4, VT_UI8, VT_UINT,
};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemContext, IWbemLocator, IWbemServices, WbemLocator,
    WBEMSTATUS, WBEM_E_NOT_FOUND, WBEM_FLAG_CONNECT_USE_MAX_WAIT, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_S_TIMEDOUT,
};

use super::wide;
use crate::{Error, Result};

/// `RPC_C_AUTHN_WINNT` (Win32_System_Rpc, declared here to keep that module out).
const RPC_C_AUTHN_WINNT: u32 = 10;
/// `RPC_C_AUTHZ_NONE`.
const RPC_C_AUTHZ_NONE: u32 = 0;

/// A connection to one WMI namespace of this PC.
pub(crate) struct WmiConnection {
    services: IWbemServices,
    namespace: String,
}

impl fmt::Debug for WmiConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WmiConnection")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

/// Sets the packet-privacy, impersonation blanket on a WMI proxy.
fn secure(proxy: impl Param<IUnknown>) -> Result<()> {
    // SAFETY: `proxy` is a live interface; no server principal name or authentication
    // identity is passed, so the process's own credentials are used.
    unsafe {
        CoSetProxyBlanket(
            proxy,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }?;
    Ok(())
}

impl WmiConnection {
    /// Connects to `namespace` (for example `ROOT\Microsoft\Windows\Defender`) as the
    /// account this process runs as.
    pub(crate) fn connect(namespace: &str) -> Result<WmiConnection> {
        // SAFETY: the caller's thread is in a COM apartment; the locator is released when it
        // goes out of scope.
        let locator: IWbemLocator =
            unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }?;
        let empty = BSTR::new();
        // SAFETY: every BSTR outlives the call; no context object is passed.
        let services = unsafe {
            locator.ConnectServer(
                &BSTR::from(namespace),
                &empty,
                &empty,
                &empty,
                WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                &empty,
                None::<&IWbemContext>,
            )
        }?;
        secure(&services)?;
        Ok(WmiConnection {
            services,
            namespace: namespace.to_string(),
        })
    }

    /// Objects returned by the WQL query `wql`. Each object is waited for at most
    /// `timeout_ms`; a query that does not deliver within that time fails.
    pub(crate) fn query(&self, wql: &str, timeout_ms: i32) -> Result<Vec<WmiObject>> {
        // SAFETY: both BSTRs outlive the call; no context object is passed.
        let enumerator: IEnumWbemClassObject = unsafe {
            self.services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(wql),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None::<&IWbemContext>,
            )
        }?;
        secure(&enumerator)?;
        let mut out = Vec::new();
        loop {
            let mut slot: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            // SAFETY: `slot` holds room for one object and `returned` is a valid out pointer.
            let hr = unsafe { enumerator.Next(timeout_ms, &mut slot, &mut returned) };
            if hr == HRESULT(WBEM_S_TIMEDOUT.0) {
                return Err(Error::Other("WMI did not answer".into()));
            }
            hr.ok()?;
            match slot[0].take() {
                Some(obj) if returned > 0 => out.push(WmiObject { obj }),
                // WBEM_S_FALSE: the enumeration is complete.
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One object of a query result.
pub(crate) struct WmiObject {
    obj: IWbemClassObject,
}

impl fmt::Debug for WmiObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WmiObject").finish_non_exhaustive()
    }
}

impl WmiObject {
    /// Value of property `name`; `WmiValue::Null` when the class has no such property.
    pub(crate) fn get(&self, name: &str) -> Result<WmiValue> {
        let w = wide(name);
        let mut value = VARIANT::default();
        // SAFETY: `w` is NUL-terminated and outlives the call; `value` receives a copy that
        // is cleared when it drops.
        match unsafe { self.obj.Get(PCWSTR(w.as_ptr()), 0, &mut value, None, None) } {
            Ok(()) => Ok(decode_variant(&value)),
            Err(e) if e.code() == HRESULT(WBEM_E_NOT_FOUND.0) => Ok(WmiValue::Null),
            Err(e) => Err(e.into()),
        }
    }
}

/// True when `e` is the WMI status `code` (for example `WBEM_E_INVALID_NAMESPACE`).
pub(crate) fn is_wbem(e: &Error, code: WBEMSTATUS) -> bool {
    matches!(e, Error::Win32(inner) if inner.code() == HRESULT(code.0))
}

/// A property value decoded by its VARIANT type, never coerced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WmiValue {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    /// Any other VARIANT type, by its `vt`.
    Other(u16),
}

impl WmiValue {
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            WmiValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The value as an unsigned 32-bit number. CIM `uint32` arrives as `VT_I4`, so a
    /// negative 32-bit value keeps its bits: -1 is 4294967295.
    pub(crate) fn as_u32_bits(&self) -> Option<u32> {
        match self {
            WmiValue::Int(v) if (0..=i64::from(u32::MAX)).contains(v) => Some(*v as u32),
            WmiValue::Int(v) if (i64::from(i32::MIN)..0).contains(v) => Some(*v as i32 as u32),
            WmiValue::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub(crate) fn as_text(&self) -> Option<&str> {
        match self {
            WmiValue::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// Decodes a VARIANT by its type tag. Arrays and references are `Other`.
pub(crate) fn decode_variant(v: &VARIANT) -> WmiValue {
    let vt = v.vt();
    // SAFETY: every union field read below is the one `vt` says is valid.
    unsafe {
        let inner = &v.Anonymous.Anonymous.Anonymous;
        match vt {
            VT_EMPTY | VT_NULL => WmiValue::Null,
            VT_BOOL => WmiValue::Bool(inner.boolVal.0 != 0),
            VT_I1 => WmiValue::Int(i64::from(inner.cVal)),
            VT_I2 => WmiValue::Int(i64::from(inner.iVal)),
            VT_I4 => WmiValue::Int(i64::from(inner.lVal)),
            VT_INT => WmiValue::Int(i64::from(inner.intVal)),
            VT_I8 => WmiValue::Int(inner.llVal),
            VT_UI1 => WmiValue::Int(i64::from(inner.bVal)),
            VT_UI2 => WmiValue::Int(i64::from(inner.uiVal)),
            VT_UI4 => WmiValue::Int(i64::from(inner.ulVal)),
            VT_UINT => WmiValue::Int(i64::from(inner.uintVal)),
            VT_UI8 => match i64::try_from(inner.ullVal) {
                Ok(v) => WmiValue::Int(v),
                Err(_) => WmiValue::Other(vt.0),
            },
            VT_R4 => WmiValue::Real(f64::from(inner.fltVal)),
            VT_R8 => WmiValue::Real(inner.dblVal),
            VT_BSTR => WmiValue::Text(inner.bstrVal.to_string()),
            other => WmiValue::Other(other.0),
        }
    }
}

/// Parses a CIM DATETIME, `yyyymmddHHMMSS.ffffff±UUU` with the offset from UTC in minutes.
/// Wildcard fields (`*`) and malformed text are `None`.
pub(crate) fn cim_datetime(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if text.len() != 25 || text.contains('*') || !text.is_ascii() {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = &text[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    if &text[14..15] != "." {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(num(0..4)? as i32, num(4..6)?, num(6..8)?)?;
    let micros = num(15..21)?;
    let time = date.and_hms_micro_opt(num(8..10)?, num(10..12)?, num(12..14)?, micros)?;
    let sign = match &text[21..22] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let offset_minutes = i64::from(num(22..25)?) * sign;
    let utc = time.and_utc() - ChronoDuration::minutes(offset_minutes);
    Some(utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Wmi::{
        WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE, WBEM_E_NOT_FOUND,
    };

    #[test]
    fn cim_datetimes_are_converted_to_utc() {
        let expected = "2026-09-28T00:08:37Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(cim_datetime("20260928000837.000000+000"), Some(expected));
        assert_eq!(cim_datetime("20260927170837.000000-420"), Some(expected));
        assert_eq!(cim_datetime("20260928023837.000000+150"), Some(expected));
        let micro = cim_datetime("20260928000837.250000+000").unwrap();
        assert_eq!(micro.timestamp_subsec_micros(), 250_000);
    }

    #[test]
    fn malformed_cim_datetimes_are_none() {
        for text in [
            "",
            "2026092800083.000000+000",
            "20260928000837.******+000",
            "********000837.000000+000",
            "20261328000837.000000+000",
            "20260928000837x000000+000",
            "20260928000837.000000*000",
            "20260928000837.000000+0a0",
            "20260928000837.000000+0000",
        ] {
            assert_eq!(cim_datetime(text), None, "{text:?}");
        }
    }

    fn with_vt(vt: windows::Win32::System::Variant::VARENUM) -> VARIANT {
        let mut v = VARIANT::default();
        // SAFETY: only the type tag of an empty VARIANT is set; no payload needs clearing.
        unsafe { (*v.Anonymous.Anonymous).vt = vt };
        v
    }

    #[test]
    fn variants_decode_by_type() {
        assert_eq!(decode_variant(&VARIANT::default()), WmiValue::Null);
        assert_eq!(decode_variant(&with_vt(VT_NULL)), WmiValue::Null);
        assert_eq!(decode_variant(&VARIANT::from(true)), WmiValue::Bool(true));
        assert_eq!(decode_variant(&VARIANT::from(false)), WmiValue::Bool(false));
        assert_eq!(decode_variant(&VARIANT::from(-1i32)), WmiValue::Int(-1));
        assert_eq!(decode_variant(&VARIANT::from(7u32)), WmiValue::Int(7));
        assert_eq!(decode_variant(&VARIANT::from(300u16)), WmiValue::Int(300));
        assert_eq!(decode_variant(&VARIANT::from(2.5f64)), WmiValue::Real(2.5));
        assert_eq!(
            decode_variant(&VARIANT::from(BSTR::from("Normal"))),
            WmiValue::Text("Normal".into())
        );
        assert_eq!(
            decode_variant(&with_vt(windows::Win32::System::Variant::VT_DATE)),
            WmiValue::Other(7)
        );
    }

    #[test]
    fn uint32_bits_survive_the_signed_variant() {
        let minus_one = decode_variant(&VARIANT::from(-1i32));
        assert_eq!(minus_one.as_u32_bits(), Some(u32::MAX));
        assert_eq!(WmiValue::Int(0).as_u32_bits(), Some(0));
        assert_eq!(
            WmiValue::Int(i64::from(u32::MAX)).as_u32_bits(),
            Some(u32::MAX)
        );
        assert_eq!(WmiValue::Int(i64::from(u32::MAX) + 1).as_u32_bits(), None);
        assert_eq!(WmiValue::Text(" 42 ".into()).as_u32_bits(), Some(42));
        assert_eq!(WmiValue::Null.as_u32_bits(), None);
        assert_eq!(WmiValue::Bool(true).as_bool(), Some(true));
        assert_eq!(WmiValue::Int(1).as_bool(), None);
        assert_eq!(WmiValue::Text("x".into()).as_text(), Some("x"));
        assert_eq!(WmiValue::Null.as_text(), None);
    }

    #[test]
    fn wbem_statuses_are_recognized() {
        let e = Error::Win32(windows::core::Error::from_hresult(HRESULT(
            WBEM_E_INVALID_NAMESPACE.0,
        )));
        assert!(is_wbem(&e, WBEM_E_INVALID_NAMESPACE));
        assert!(!is_wbem(&e, WBEM_E_INVALID_CLASS));
        assert!(!is_wbem(&Error::Other("x".into()), WBEM_E_NOT_FOUND));
    }

    #[test]
    fn defender_status_reads_through_wmi() {
        // Read-only: the Defender status class is readable by a standard user; a PC without
        // Defender reports an invalid namespace or class, which is not a failure here.
        let _com = crate::win::com::enter_mta();
        let conn = match WmiConnection::connect(r"ROOT\Microsoft\Windows\Defender") {
            Ok(conn) => conn,
            Err(e) if is_wbem(&e, WBEM_E_INVALID_NAMESPACE) => return,
            Err(e) => panic!("{e}"),
        };
        match conn.query("SELECT * FROM MSFT_MpComputerStatus", 5000) {
            Ok(objects) => {
                for obj in &objects {
                    assert_eq!(
                        obj.get("PCOptimizerNoSuchProperty").unwrap(),
                        WmiValue::Null
                    );
                    let mode = obj.get("AMRunningMode").unwrap();
                    assert!(
                        matches!(mode, WmiValue::Text(_) | WmiValue::Null),
                        "{mode:?}"
                    );
                }
            }
            Err(e) if is_wbem(&e, WBEM_E_INVALID_CLASS) => {}
            Err(e) => panic!("{e}"),
        }
    }
}
