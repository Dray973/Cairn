//! Health of the Windows component store, read through the DISM API (`dismapi.dll`).
//!
//! `DISM /CheckHealth` and `/ScanHealth` print their verdict only as localized text, so
//! after a successful run the verdict is read again from `DismCheckImageHealth`, which
//! reports the state the check recorded without scanning again.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};
use windows::core::{s, w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use crate::win::wide;
use crate::{Error, Result};

/// State of the component store, the source System File Checker repairs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreHealth {
    Healthy,
    /// Damaged, and `DISM /RestoreHealth` can repair it.
    Repairable,
    /// Damaged beyond what DISM can repair.
    NonRepairable,
}

/// Maps a `DismImageHealthState` value.
pub(crate) fn health_from_raw(raw: i32) -> Result<StoreHealth> {
    match raw {
        0 => Ok(StoreHealth::Healthy),
        1 => Ok(StoreHealth::Repairable),
        2 => Ok(StoreHealth::NonRepairable),
        other => Err(Error::Other(format!(
            "DISM reported an unknown component store state ({other})"
        ))),
    }
}

/// `DISM_ONLINE_IMAGE`: the image path that opens a session on the running Windows.
const DISM_ONLINE_IMAGE: &str = "DISM_{53BFAE52-B167-4E2F-A258-0A37B57FF845}";
/// `DismLogErrors`.
const DISM_LOG_ERRORS: i32 = 0;

type DismInitialize = unsafe extern "system" fn(i32, PCWSTR, PCWSTR) -> i32;
type DismOpenSession = unsafe extern "system" fn(PCWSTR, PCWSTR, PCWSTR, *mut u32) -> i32;
type DismCheckImageHealth =
    unsafe extern "system" fn(u32, i32, *mut c_void, *const c_void, *mut c_void, *mut i32) -> i32;
type DismCloseSession = unsafe extern "system" fn(u32) -> i32;
type DismShutdown = unsafe extern "system" fn() -> i32;
type RawProc = unsafe extern "system" fn() -> isize;

/// A DLL loaded from System32, freed on drop.
struct Library(HMODULE);

impl Library {
    fn load_system32(name: PCWSTR) -> Result<Library> {
        // SAFETY: `name` is a NUL-terminated literal; the search is limited to System32.
        let module = unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
        Ok(Library(module))
    }

    fn proc(&self, name: PCSTR, label: &str) -> Result<RawProc> {
        // SAFETY: the module handle is valid while `self` lives; `name` is NUL-terminated.
        unsafe { GetProcAddress(self.0, name) }
            .ok_or_else(|| Error::Other(format!("dismapi.dll does not export {label}")))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module was loaded by `load_system32` and is freed exactly once, after
        // every function taken from it has returned.
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

fn check(call: &str, hr: i32) -> Result<()> {
    if hr < 0 {
        let error = windows::core::Error::from_hresult(windows::core::HRESULT(hr));
        return Err(Error::Other(format!("{call} failed: {error}")));
    }
    Ok(())
}

/// Calls `DismShutdown` when dropped.
struct Initialized(DismShutdown);

impl Drop for Initialized {
    fn drop(&mut self) {
        // SAFETY: DismInitialize succeeded; DismShutdown takes no arguments.
        unsafe {
            (self.0)();
        }
    }
}

/// Calls `DismCloseSession` when dropped.
struct Session(DismCloseSession, u32);

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session was opened by DismOpenSession and is closed exactly once.
        unsafe {
            (self.0)(self.1);
        }
    }
}

/// Reads the component store state of the running Windows through the DISM API. Needs an
/// elevated process. Starts DISM's servicing host, so it runs only after a real DISM check
/// finished, never in tests.
pub fn check_online_image() -> Result<StoreHealth> {
    let library = Library::load_system32(w!("dismapi.dll"))?;
    let initialize = library.proc(s!("DismInitialize"), "DismInitialize")?;
    let open_session = library.proc(s!("DismOpenSession"), "DismOpenSession")?;
    let check_health = library.proc(s!("DismCheckImageHealth"), "DismCheckImageHealth")?;
    let close_session = library.proc(s!("DismCloseSession"), "DismCloseSession")?;
    let shutdown = library.proc(s!("DismShutdown"), "DismShutdown")?;
    // SAFETY: each pointer is the export of that name from dismapi.dll, whose documented
    // signature (stdcall, 32-bit DismSession and enum values, BOOL as i32) the target
    // type spells out.
    let (initialize, open_session, check_health, close_session, shutdown) = unsafe {
        (
            std::mem::transmute::<RawProc, DismInitialize>(initialize),
            std::mem::transmute::<RawProc, DismOpenSession>(open_session),
            std::mem::transmute::<RawProc, DismCheckImageHealth>(check_health),
            std::mem::transmute::<RawProc, DismCloseSession>(close_session),
            std::mem::transmute::<RawProc, DismShutdown>(shutdown),
        )
    };

    // SAFETY: null log file and scratch directory select DISM's defaults.
    let hr = unsafe { initialize(DISM_LOG_ERRORS, PCWSTR::null(), PCWSTR::null()) };
    check("DismInitialize", hr)?;
    let _initialized = Initialized(shutdown);

    let image = wide(DISM_ONLINE_IMAGE);
    let mut session = 0u32;
    // SAFETY: `image` is NUL-terminated and outlives the call; the Windows directory and
    // system drive are null for the online image; `session` is a valid out pointer.
    let hr = unsafe {
        open_session(
            PCWSTR(image.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            &mut session,
        )
    };
    check("DismOpenSession", hr)?;
    let _session = Session(close_session, session);

    let mut state = -1i32;
    // SAFETY: an open session; ScanImage FALSE reads the recorded state without a scan; no
    // cancel event, progress callback or user data; `state` is a valid out pointer.
    let hr = unsafe {
        check_health(
            session,
            0,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut state,
        )
    };
    check("DismCheckImageHealth", hr)?;
    health_from_raw(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_from_raw_maps_values() {
        assert_eq!(health_from_raw(0).unwrap(), StoreHealth::Healthy);
        assert_eq!(health_from_raw(1).unwrap(), StoreHealth::Repairable);
        assert_eq!(health_from_raw(2).unwrap(), StoreHealth::NonRepairable);
        for raw in [-1, 3, 42] {
            let err = health_from_raw(raw).unwrap_err().to_string();
            assert!(err.contains(&raw.to_string()), "{err}");
        }
        assert_eq!(
            serde_json::to_string(&StoreHealth::NonRepairable).unwrap(),
            "\"non_repairable\""
        );
    }

    #[test]
    fn failing_hresults_are_errors() {
        assert!(check("X", 0).is_ok());
        assert!(check("X", 1).is_ok(), "S_FALSE is not a failure");
        let err = check("DismOpenSession", 0x8007_0005_u32 as i32).unwrap_err();
        assert!(
            err.to_string().starts_with("DismOpenSession failed"),
            "{err}"
        );
    }
}
