//! Service Control Manager access: query config/status, change start type,
//! delayed-auto-start flag, stop and start.

use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_NOT_ACTIVE,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    OpenSCManagerW, OpenServiceW, QueryServiceConfig2W, QueryServiceConfigW, QueryServiceStatusEx,
    StartServiceW, ENUM_SERVICE_TYPE, QUERY_SERVICE_CONFIGW, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_STOP,
    SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONTROL_STOP,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_ERROR, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_START_TYPE, SERVICE_STATUS,
    SERVICE_STATUS_PROCESS, SERVICE_STOP,
};

use super::{is_win32, wide};
use crate::{Error, Result};

pub const READ_ACCESS: u32 = SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS;
pub const MUTATE_ACCESS: u32 = READ_ACCESS | SERVICE_CHANGE_CONFIG | SERVICE_START | SERVICE_STOP;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartType {
    Boot,
    System,
    Automatic,
    Manual,
    Disabled,
}

impl StartType {
    pub fn from_raw(v: u32) -> Option<StartType> {
        Some(match v {
            0 => StartType::Boot,
            1 => StartType::System,
            2 => StartType::Automatic,
            3 => StartType::Manual,
            4 => StartType::Disabled,
            _ => return None,
        })
    }

    pub fn raw(self) -> u32 {
        match self {
            StartType::Boot => 0,
            StartType::System => 1,
            StartType::Automatic => 2,
            StartType::Manual => 3,
            StartType::Disabled => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StartType::Boot => "boot",
            StartType::System => "system",
            StartType::Automatic => "automatic",
            StartType::Manual => "manual",
            StartType::Disabled => "disabled",
        }
    }

    pub fn parse(s: &str) -> Option<StartType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "boot" => Some(StartType::Boot),
            "system" => Some(StartType::System),
            "auto" | "automatic" => Some(StartType::Automatic),
            "manual" | "demand" => Some(StartType::Manual),
            "disabled" => Some(StartType::Disabled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    Unknown,
}

impl ServiceState {
    fn from_raw(v: u32) -> ServiceState {
        match v {
            1 => ServiceState::Stopped,
            2 => ServiceState::StartPending,
            3 => ServiceState::StopPending,
            4 => ServiceState::Running,
            5 => ServiceState::ContinuePending,
            6 => ServiceState::PausePending,
            7 => ServiceState::Paused,
            _ => ServiceState::Unknown,
        }
    }

    /// Anything other than stopped / stopping counts as active.
    pub fn is_active(self) -> bool {
        !matches!(self, ServiceState::Stopped | ServiceState::StopPending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub display_name: String,
    pub start_type: StartType,
    pub delayed_auto_start: bool,
    pub binary_path: String,
    pub service_type: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub pid: u32,
    pub accepts_stop: bool,
}

/// Connection to the local SCM. Closed on drop.
pub struct Scm(SC_HANDLE);

impl std::fmt::Debug for Scm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Scm").field(&self.0 .0).finish()
    }
}

impl Drop for Scm {
    fn drop(&mut self) {
        // SAFETY: handle from OpenSCManagerW, closed once.
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

impl Scm {
    pub fn connect() -> Result<Scm> {
        // SAFETY: null machine/database = local active database.
        let h = unsafe {
            OpenSCManagerW(
                PCWSTR::null(),
                PCWSTR::null(),
                SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
            )?
        };
        Ok(Scm(h))
    }

    /// Opens a service. `Ok(None)` when no such service exists.
    pub fn open(&self, name: &str, access: u32) -> Result<Option<Service>> {
        let w = wide(name);
        // SAFETY: valid SCM handle and string.
        match unsafe { OpenServiceW(self.0, PCWSTR(w.as_ptr()), access) } {
            Ok(h) => Ok(Some(Service {
                h,
                name: name.to_string(),
            })),
            Err(e) if is_win32(&e, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn open_required(&self, name: &str, access: u32) -> Result<Service> {
        self.open(name, access)?
            .ok_or_else(|| Error::ServiceNotFound(name.to_string()))
    }
}

/// An open service handle. Closed on drop.
pub struct Service {
    h: SC_HANDLE,
    name: String,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").field("name", &self.name).finish()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: handle from OpenServiceW, closed once.
        unsafe {
            let _ = CloseServiceHandle(self.h);
        }
    }
}

fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: SCM guarantees NUL-terminated strings inside the config buffer.
        unsafe { p.to_string() }.unwrap_or_default()
    }
}

impl Service {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn config(&self) -> Result<ServiceConfig> {
        let mut needed = 0u32;
        // SAFETY: size probe with a null buffer is the documented protocol.
        match unsafe { QueryServiceConfigW(self.h, None, 0, &mut needed) } {
            Err(e) if is_win32(&e, ERROR_INSUFFICIENT_BUFFER) => {}
            Err(e) => return Err(e.into()),
            Ok(()) => {
                return Err(Error::Other(
                    "QueryServiceConfigW probe unexpectedly succeeded".into(),
                ))
            }
        }
        // u64-backed buffer keeps the embedded pointers 8-byte aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: buffer is at least `needed` bytes and suitably aligned.
        unsafe {
            QueryServiceConfigW(
                self.h,
                Some(buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
                needed,
                &mut needed,
            )?;
        }
        // SAFETY: SCM filled a QUERY_SERVICE_CONFIGW at the start of the buffer.
        let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
        let start_type = StartType::from_raw(cfg.dwStartType.0).unwrap_or(StartType::Manual);
        let delayed = if start_type == StartType::Automatic {
            self.delayed_auto_start().unwrap_or(false)
        } else {
            false
        };
        Ok(ServiceConfig {
            name: self.name.clone(),
            display_name: pwstr_to_string(cfg.lpDisplayName),
            start_type,
            delayed_auto_start: delayed,
            binary_path: pwstr_to_string(cfg.lpBinaryPathName),
            service_type: cfg.dwServiceType.0,
        })
    }

    pub fn delayed_auto_start(&self) -> Result<bool> {
        let mut info = SERVICE_DELAYED_AUTO_START_INFO::default();
        let mut needed = 0u32;
        // SAFETY: the buffer is exactly one SERVICE_DELAYED_AUTO_START_INFO.
        unsafe {
            let buf = std::slice::from_raw_parts_mut(
                &mut info as *mut _ as *mut u8,
                size_of::<SERVICE_DELAYED_AUTO_START_INFO>(),
            );
            QueryServiceConfig2W(
                self.h,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(buf),
                &mut needed,
            )?;
        }
        Ok(info.fDelayedAutostart.as_bool())
    }

    pub fn status(&self) -> Result<ServiceStatus> {
        let mut ssp = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0u32;
        // SAFETY: the buffer is exactly one SERVICE_STATUS_PROCESS.
        unsafe {
            let buf = std::slice::from_raw_parts_mut(
                &mut ssp as *mut _ as *mut u8,
                size_of::<SERVICE_STATUS_PROCESS>(),
            );
            QueryServiceStatusEx(self.h, SC_STATUS_PROCESS_INFO, Some(buf), &mut needed)?;
        }
        Ok(ServiceStatus {
            state: ServiceState::from_raw(ssp.dwCurrentState.0),
            pid: ssp.dwProcessId,
            accepts_stop: ssp.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0,
        })
    }

    pub fn set_start_type(&self, start: StartType) -> Result<()> {
        // SAFETY: SERVICE_NO_CHANGE / null strings leave every other field untouched.
        unsafe {
            ChangeServiceConfigW(
                self.h,
                ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                SERVICE_START_TYPE(start.raw()),
                SERVICE_ERROR(SERVICE_NO_CHANGE),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )?;
        }
        Ok(())
    }

    pub fn set_delayed_auto_start(&self, on: bool) -> Result<()> {
        let info = SERVICE_DELAYED_AUTO_START_INFO {
            fDelayedAutostart: BOOL::from(on),
        };
        // SAFETY: info outlives the call.
        unsafe {
            ChangeServiceConfig2W(
                self.h,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(&info as *const _ as *const c_void),
            )?;
        }
        Ok(())
    }

    /// Sends SERVICE_CONTROL_STOP and waits up to `timeout` for the service to report stopped.
    pub fn stop(&self, timeout: Duration) -> Result<ServiceState> {
        let mut st = SERVICE_STATUS::default();
        // SAFETY: st is a valid out-pointer.
        match unsafe { ControlService(self.h, SERVICE_CONTROL_STOP, &mut st) } {
            Ok(()) => {}
            Err(e) if is_win32(&e, ERROR_SERVICE_NOT_ACTIVE) => return Ok(ServiceState::Stopped),
            Err(e) => return Err(e.into()),
        }
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.status()?.state;
            if state == ServiceState::Stopped || Instant::now() >= deadline {
                return Ok(state);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn start(&self) -> Result<()> {
        // SAFETY: no arguments passed to the service.
        match unsafe { StartServiceW(self.h, None) } {
            Ok(()) => Ok(()),
            Err(e) if is_win32(&e, ERROR_SERVICE_ALREADY_RUNNING) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
