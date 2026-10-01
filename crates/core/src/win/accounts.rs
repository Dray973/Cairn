//! Local accounts, the Administrators group, token elevation and domain membership.
//!
//! Every call only reads: the local account list (`NetUserEnum`), a user's local groups
//! (`NetUserGetLocalGroups`), the name of the Administrators group in the display language
//! (from its well-known SID), this process's token elevation type and whether the PC is
//! joined to a domain. Buffers returned by the NetAPI are freed with `NetApiBufferFree`.

use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::NetworkManagement::NetManagement::{
    NERR_Success, NetApiBufferFree, NetGetJoinInformation, NetSetupDomainName, NetUserEnum,
    NetUserGetLocalGroups, FILTER_NORMAL_ACCOUNT, LG_INCLUDE_INDIRECT, LOCALGROUP_USERS_INFO_0,
    MAX_PREFERRED_LENGTH, NETSETUP_JOIN_STATUS, UF_ACCOUNTDISABLE, USER_INFO_20,
};
use windows::Win32::Security::{
    CreateWellKnownSid, GetTokenInformation, LookupAccountSidW, TokenElevationType,
    TokenElevationTypeFull, TokenElevationTypeLimited, WinBuiltinAdministratorsSid, PSID,
    SECURITY_MAX_SID_SIZE, SID_NAME_USE, TOKEN_ELEVATION_TYPE, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::wide;
use crate::{Error, Result};

/// `ERROR_MORE_DATA`: the NetAPI has more entries after this buffer.
const ERROR_MORE_DATA: u32 = 234;

/// A local account; its name is a system or user account name and is never shown for the
/// signed-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalAccount {
    /// Relative id: 500 is the built-in Administrator, 501 the Guest account.
    pub(crate) rid: u32,
    pub(crate) name: String,
    pub(crate) enabled: bool,
}

/// A NetAPI buffer, freed on drop.
struct NetBuffer(*mut u8);

impl Drop for NetBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the buffer was allocated by the NetAPI and is freed exactly once.
            unsafe { NetApiBufferFree(Some(self.0 as *const c_void)) };
        }
    }
}

/// Error of a NetAPI call that returned `status`.
fn net_error(api: &str, status: u32) -> Error {
    let inner = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(status));
    Error::Other(format!("{api} failed: {inner}"))
}

/// Text of a NUL-terminated UTF-16 string owned by a NetAPI buffer.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string that stays alive for the call.
unsafe fn pwstr(p: PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: guaranteed by the caller.
        String::from_utf16_lossy(unsafe { p.as_wide() })
    }
}

/// The normal (not machine or trust) accounts of this PC.
pub(crate) fn local_accounts() -> Result<Vec<LocalAccount>> {
    let mut out = Vec::new();
    let mut resume = 0u32;
    loop {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut read = 0u32;
        let mut total = 0u32;
        // SAFETY: every out pointer is valid for the call; the buffer is freed by NetBuffer.
        let status = unsafe {
            NetUserEnum(
                PCWSTR::null(),
                20,
                FILTER_NORMAL_ACCOUNT,
                &mut buf,
                MAX_PREFERRED_LENGTH,
                &mut read,
                &mut total,
                Some(&mut resume),
            )
        };
        let buffer = NetBuffer(buf);
        if status != NERR_Success && status != ERROR_MORE_DATA {
            return Err(net_error("NetUserEnum", status));
        }
        if !buffer.0.is_null() {
            let entries = buffer.0 as *const USER_INFO_20;
            for i in 0..read as usize {
                // SAFETY: the buffer holds `read` USER_INFO_20 entries whose strings live in
                // the same buffer, which is alive until `buffer` drops.
                let entry = unsafe { &*entries.add(i) };
                out.push(LocalAccount {
                    rid: entry.usri20_user_id,
                    // SAFETY: as above.
                    name: unsafe { pwstr(entry.usri20_name) },
                    enabled: entry.usri20_flags.0 & UF_ACCOUNTDISABLE.0 == 0,
                });
            }
        }
        if status != ERROR_MORE_DATA {
            return Ok(out);
        }
    }
}

/// Name of the built-in Administrators group in this PC's language ("Administrators",
/// "Administratoren", …), resolved from its well-known SID.
pub(crate) fn administrators_group_name() -> Result<String> {
    let mut sid = [0u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(8)];
    let mut size = SECURITY_MAX_SID_SIZE;
    let psid = PSID(sid.as_mut_ptr() as *mut c_void);
    // SAFETY: `sid` holds SECURITY_MAX_SID_SIZE bytes, the size passed in `size`.
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(psid), &mut size) }?;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain_len = domain.len() as u32;
    let mut sid_use = SID_NAME_USE::default();
    // SAFETY: `psid` points to the SID built above; both buffers are writable for the lengths
    // passed alongside them.
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            psid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut sid_use,
        )
    }?;
    Ok(String::from_utf16_lossy(
        &name[..(name_len as usize).min(name.len())],
    ))
}

/// Whether the account `domain\name` is in the local Administrators group, directly or
/// through another group.
pub(crate) fn user_is_local_admin(domain: &str, name: &str) -> Result<bool> {
    let account = if domain.is_empty() {
        name.to_string()
    } else {
        format!(r"{domain}\{name}")
    };
    let groups = local_groups(&account)?;
    let admins = administrators_group_name()?;
    Ok(groups
        .iter()
        .any(|g| g.to_lowercase() == admins.to_lowercase()))
}

fn local_groups(account: &str) -> Result<Vec<String>> {
    let user = wide(account);
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: `user` is NUL-terminated and outlives the call; the out pointers are valid and
    // the buffer is freed by NetBuffer.
    let status = unsafe {
        NetUserGetLocalGroups(
            PCWSTR::null(),
            PCWSTR(user.as_ptr()),
            0,
            LG_INCLUDE_INDIRECT,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
        )
    };
    let buffer = NetBuffer(buf);
    if status != NERR_Success {
        return Err(net_error("NetUserGetLocalGroups", status));
    }
    let mut out = Vec::with_capacity(read as usize);
    if !buffer.0.is_null() {
        let entries = buffer.0 as *const LOCALGROUP_USERS_INFO_0;
        for i in 0..read as usize {
            // SAFETY: the buffer holds `read` entries whose strings live in the same buffer.
            out.push(unsafe { pwstr((*entries.add(i)).lgrui0_name) });
        }
    }
    Ok(out)
}

/// Elevation type of this process's token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElevationType {
    /// No split token: UAC is off, the account is a standard user, or it is the built-in
    /// Administrator without admin approval mode.
    Default,
    /// The elevated half of a split administrator token.
    Full,
    /// The filtered half of a split administrator token.
    Limited,
}

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened here and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Elevation type of this process's token.
pub(crate) fn token_elevation_type() -> Result<ElevationType> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle; `token` is a valid out pointer and
    // is closed by TokenHandle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    let token = TokenHandle(token);
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    let mut returned = 0u32;
    // SAFETY: `kind` is exactly the size passed; `returned` is a valid out pointer.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevationType,
            Some(&mut kind as *mut TOKEN_ELEVATION_TYPE as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut returned,
        )
    }?;
    Ok(if kind == TokenElevationTypeFull {
        ElevationType::Full
    } else if kind == TokenElevationTypeLimited {
        ElevationType::Limited
    } else {
        ElevationType::Default
    })
}

/// Whether this PC is joined to an Active Directory domain.
pub(crate) fn domain_joined() -> Result<bool> {
    let mut name = PWSTR::null();
    let mut status = NETSETUP_JOIN_STATUS::default();
    // SAFETY: both out pointers are valid; the returned name buffer is freed below.
    let result = unsafe { NetGetJoinInformation(PCWSTR::null(), &mut name, &mut status) };
    let _buffer = NetBuffer(name.0 as *mut u8);
    if result != NERR_Success {
        return Err(net_error("NetGetJoinInformation", result));
    }
    Ok(status == NetSetupDomainName)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_accounts_include_the_built_in_ones() {
        // Read-only: the account list is readable by a standard user.
        let accounts = local_accounts().unwrap();
        assert!(accounts.iter().any(|a| a.rid == 500), "{}", accounts.len());
        assert!(accounts.iter().any(|a| a.rid == 501), "{}", accounts.len());
    }

    #[test]
    fn the_administrators_group_has_a_name() {
        assert!(!administrators_group_name().unwrap().is_empty());
    }

    #[test]
    fn the_signed_in_user_s_groups_are_readable() {
        let user = crate::win::session::session_user()
            .unwrap()
            .expect("a signed-in user");
        let admin = user_is_local_admin(&user.domain, &user.name).unwrap();
        // An elevated process always belongs to an administrator.
        if crate::is_elevated() {
            assert!(admin);
        }
    }

    #[test]
    fn token_and_domain_state_are_readable() {
        let kind = token_elevation_type().unwrap();
        if crate::is_elevated() {
            assert_ne!(kind, ElevationType::Limited);
        }
        let _ = domain_joined().unwrap();
    }

    #[test]
    fn unknown_accounts_are_an_error() {
        assert!(user_is_local_admin("", "PCOptimizerNoSuchAccount").is_err());
    }
}
