//! Named mutexes, with an owner check for administrator-only locks.
//!
//! Any process may create a mutex in the `Global\` or `Local\` namespace, so a name alone
//! proves nothing about who holds it. A lock that only administrators may hold is created
//! with [`ADMIN_LOCK_SDDL`], and an existing object of that name counts only when its owner
//! is Administrators or SYSTEM.

use serde::Serialize;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    ERROR_FILE_NOT_FOUND, HANDLE, HLOCAL, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
    SYNCHRONIZATION_SYNCHRONIZE,
};

use super::session::sid_string;
use super::{is_win32, wide};
use crate::{Error, Result};

/// Owner Administrators, full access for SYSTEM and Administrators, READ_CONTROL and
/// SYNCHRONIZE for Authenticated Users (so an unelevated process can read the owner).
pub const ADMIN_LOCK_SDDL: &str = "O:BAD:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00120000;;;AU)";

/// SIDs whose ownership makes an existing lock count as an administrator's lock.
const ADMIN_OWNERS: [&str; 2] = ["S-1-5-32-544", "S-1-5-18"];
/// `READ_CONTROL` standard access right.
const READ_CONTROL: u32 = 0x0002_0000;

/// A named mutex this thread owns. Dropping it releases and closes it; it must be dropped
/// on the thread that acquired it (the raw handle keeps it from leaving that thread).
#[derive(Debug)]
pub struct NamedMutex {
    handle: HANDLE,
    name: String,
}

/// Result of [`NamedMutex::try_acquire`].
#[derive(Debug)]
pub enum Acquire {
    /// This thread now owns the mutex (a mutex abandoned by an ended owner included).
    Acquired(NamedMutex),
    /// Another thread or process owns it.
    Busy,
    /// An object of that name exists but is not a lock this program may trust: its owner is
    /// not Administrators or SYSTEM, or this process may not open it.
    Foreign,
}

impl NamedMutex {
    /// Creates or opens the mutex `name` and takes it without waiting.
    ///
    /// `CreateMutexW` with a security descriptor built from `sddl` when it is `Some`, then
    /// `WaitForSingleObject(handle, 0)`: `WAIT_OBJECT_0` or `WAIT_ABANDONED` is
    /// [`Acquire::Acquired`]; `WAIT_TIMEOUT` is [`Acquire::Busy`]. With `sddl` set and an
    /// object that already existed, its owner must be Administrators or SYSTEM, else
    /// [`Acquire::Foreign`] without waiting. `ERROR_ACCESS_DENIED` is [`Acquire::Foreign`].
    /// Without `sddl` the owner is not checked.
    pub fn try_acquire(name: &str, sddl: Option<&str>) -> Result<Acquire> {
        let descriptor = sddl.map(SecurityDescriptor::from_sddl).transpose()?;
        let attributes = descriptor.as_ref().map(|sd| SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0 .0,
            bInheritHandle: false.into(),
        });
        let name_w = wide(name);
        // SAFETY: `attributes` (when present) points to a SECURITY_ATTRIBUTES whose
        // descriptor outlives the call; `name_w` is NUL-terminated.
        let created = unsafe {
            CreateMutexW(
                attributes.as_ref().map(|a| a as *const SECURITY_ATTRIBUTES),
                false,
                PCWSTR(name_w.as_ptr()),
            )
        };
        // SAFETY: reads this thread's last error right after CreateMutexW.
        let last = unsafe { GetLastError() };
        let handle = match created {
            Ok(handle) => Handle(handle),
            Err(e) if is_win32(&e, ERROR_ACCESS_DENIED) => return Ok(Acquire::Foreign),
            Err(e) => return Err(e.into()),
        };
        if sddl.is_some() && last == ERROR_ALREADY_EXISTS {
            let trusted = owner_sid(handle.0)
                .map(|owner| owner_is_admin(&owner))
                .unwrap_or(false);
            if !trusted {
                return Ok(Acquire::Foreign);
            }
        }
        // SAFETY: `handle` is a valid mutex handle owned here.
        let wait = unsafe { WaitForSingleObject(handle.0, 0) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            Ok(Acquire::Acquired(NamedMutex {
                handle: handle.into_raw(),
                name: name.to_string(),
            }))
        } else if wait == WAIT_TIMEOUT {
            Ok(Acquire::Busy)
        } else {
            Err(Error::Win32(windows::core::Error::from_win32()))
        }
    }

    /// The name the mutex was acquired under.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex (it acquired it and the handle cannot leave the
        // thread); the handle is released and closed exactly once.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Who holds a mutex name, as far as this process can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutexPresence {
    /// No object of that name exists.
    Absent,
    /// It exists and its owner is Administrators or SYSTEM.
    Admin,
    /// It exists with another owner, its owner cannot be read, or it cannot be opened.
    Other,
}

/// Looks the mutex `name` up without waiting on it (`OpenMutexW` with SYNCHRONIZE and
/// READ_CONTROL). Not found → [`MutexPresence::Absent`]; opened →
/// [`MutexPresence::Admin`] when [`owner_is_admin`] holds for its owner, else
/// [`MutexPresence::Other`]; any other failure (access denied included) →
/// [`MutexPresence::Other`].
pub fn presence(name: &str) -> MutexPresence {
    let name_w = wide(name);
    let access = SYNCHRONIZATION_ACCESS_RIGHTS(SYNCHRONIZATION_SYNCHRONIZE.0 | READ_CONTROL);
    // SAFETY: `name_w` is NUL-terminated; the handle is closed by `Handle`.
    let opened = unsafe { OpenMutexW(access, false, PCWSTR(name_w.as_ptr())) };
    match opened {
        Ok(handle) => {
            let handle = Handle(handle);
            match owner_sid(handle.0) {
                Ok(owner) if owner_is_admin(&owner) => MutexPresence::Admin,
                _ => MutexPresence::Other,
            }
        }
        Err(e) if is_win32(&e, ERROR_FILE_NOT_FOUND) => MutexPresence::Absent,
        Err(_) => MutexPresence::Other,
    }
}

/// True for the Administrators group (S-1-5-32-544) and LocalSystem (S-1-5-18).
pub fn owner_is_admin(sid: &str) -> bool {
    ADMIN_OWNERS.iter().any(|s| s.eq_ignore_ascii_case(sid))
}

/// True when an object of that name exists, whoever owns it; for names whose owner does not
/// matter, such as `Global\_MSIExecute`.
pub fn exists(name: &str) -> bool {
    presence(name) != MutexPresence::Absent
}

/// A handle closed on drop unless it was taken with [`Handle::into_raw`].
struct Handle(HANDLE);

impl Handle {
    fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened here and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A security descriptor allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
/// freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<SecurityDescriptor> {
        let text = wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is NUL-terminated and `sd` is a valid out pointer; the descriptor
        // is freed with LocalFree on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?
        };
        Ok(SecurityDescriptor(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated with LocalAlloc by the conversion above and freed exactly once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// Owner SID of a kernel object opened with READ_CONTROL.
fn owner_sid(handle: HANDLE) -> Result<String> {
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `handle` is a valid kernel object handle; `owner` and `sd` are valid out
    // pointers; `owner` points into `sd`, which is freed below after the SID is copied.
    let err = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    let sd = SecurityDescriptor(sd);
    super::check(err)?;
    if owner.is_invalid() {
        return Err(Error::Other("the object has no owner".into()));
    }
    let text = sid_string(owner);
    drop(sd);
    text
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn unique_name() -> String {
        format!(
            r"Local\CairnTest.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn acquire_elsewhere(name: &str, sddl: Option<&'static str>) -> &'static str {
        let name = name.to_string();
        std::thread::spawn(
            move || match NamedMutex::try_acquire(&name, sddl).unwrap() {
                Acquire::Acquired(_) => "acquired",
                Acquire::Busy => "busy",
                Acquire::Foreign => "foreign",
            },
        )
        .join()
        .unwrap()
    }

    #[test]
    fn a_held_mutex_is_busy_for_another_thread_and_absent_after_drop() {
        let name = unique_name();
        assert_eq!(presence(&name), MutexPresence::Absent);
        assert!(!exists(&name));
        let held = match NamedMutex::try_acquire(&name, None).unwrap() {
            Acquire::Acquired(held) => held,
            other => panic!("{other:?}"),
        };
        assert_eq!(held.name(), name);
        assert_eq!(acquire_elsewhere(&name, None), "busy");
        assert_ne!(presence(&name), MutexPresence::Absent);
        assert!(exists(&name));
        let expected = if crate::is_elevated() {
            MutexPresence::Admin
        } else {
            MutexPresence::Other
        };
        assert_eq!(presence(&name), expected);
        drop(held);
        assert_eq!(presence(&name), MutexPresence::Absent);
        // Released and closed: another thread can take it now.
        assert_eq!(acquire_elsewhere(&name, None), "acquired");
    }

    #[test]
    fn the_same_thread_may_acquire_again() {
        let name = unique_name();
        let first = NamedMutex::try_acquire(&name, None).unwrap();
        let second = NamedMutex::try_acquire(&name, None).unwrap();
        assert!(matches!(first, Acquire::Acquired(_)));
        assert!(matches!(second, Acquire::Acquired(_)));
    }

    #[test]
    fn admin_owners_are_recognized() {
        assert!(owner_is_admin("S-1-5-32-544"));
        assert!(owner_is_admin("S-1-5-18"));
        assert!(owner_is_admin("s-1-5-18"));
        for other in [
            "S-1-5-21-1111111111-2222222222-3333333333-1001",
            "S-1-5-19",
            "S-1-5-20",
            "S-1-5-32-545",
            "S-1-5-11",
            "S-1-1-0",
            "",
        ] {
            assert!(!owner_is_admin(other), "{other}");
        }
    }

    #[test]
    fn an_admin_lock_is_owned_by_administrators() {
        if !crate::is_elevated() {
            eprintln!("skipped: an administrator-owned lock needs an elevated process");
            return;
        }
        let name = unique_name();
        let held = match NamedMutex::try_acquire(&name, Some(ADMIN_LOCK_SDDL)).unwrap() {
            Acquire::Acquired(held) => held,
            other => panic!("{other:?}"),
        };
        assert_eq!(presence(&name), MutexPresence::Admin);
        assert_eq!(acquire_elsewhere(&name, Some(ADMIN_LOCK_SDDL)), "busy");
        drop(held);
        assert_eq!(presence(&name), MutexPresence::Absent);
    }

    #[test]
    fn an_invalid_sddl_is_an_error() {
        assert!(NamedMutex::try_acquire(&unique_name(), Some("not an sddl")).is_err());
    }

    #[test]
    fn another_object_type_under_the_name_is_not_absent() {
        use windows::Win32::System::Threading::CreateEventW;
        let name = unique_name();
        let name_w = wide(&name);
        // SAFETY: creates an event with default security under a unique test name; the
        // handle is closed when `_event` drops.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(name_w.as_ptr())) }.unwrap();
        let _event = Handle(event);
        assert_eq!(presence(&name), MutexPresence::Other);
        assert!(exists(&name));
    }
}
