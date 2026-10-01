//! Identity of the interactive user versus the account this process runs as.
//!
//! A standard user who approves a UAC prompt with an administrator's credentials runs the
//! elevated process as that administrator. HKEY_CURRENT_USER, per-user Store packages and
//! per-user startup entries then belong to the administrator, not to the person at the
//! keyboard, so per-user changes would silently land in the wrong profile.
//!
//! The signed-in user is the account the session manager reports for this process's
//! session (`WTSQuerySessionInformationW`). No other process is opened: the shell of
//! another account typically denies an elevated administrator even query access.

use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, LookupAccountNameW, LookupAccountSidW, TokenUser, PSID,
    SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSDomainName, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_INFO_CLASS,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};

use super::wide;
use crate::{Error, Result};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened here and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// TOKEN_USER buffer of a process token, 8-byte aligned for the embedded SID pointer.
fn token_user(process: HANDLE) -> Result<Vec<u64>> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; the handle is closed on drop.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token)? };
    let token = OwnedHandle(token);
    let mut needed = 0u32;
    // SAFETY: size probe with no buffer; the expected failure is ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(Error::Other(
            "GetTokenInformation(TokenUser) reported no size".into(),
        ));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            needed,
            &mut needed,
        )?;
    }
    Ok(buf)
}

fn sid_of(buf: &[u64]) -> PSID {
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser) and starts with a
    // TOKEN_USER whose SID pointer refers into the same buffer.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}

/// One string of this process's session information; empty when the session reports none.
fn session_string(class: WTS_INFO_CLASS) -> Result<String> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: both out pointers are valid for the call; the returned buffer is released
    // with WTSFreeMemory below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            class,
            &mut buf,
            &mut bytes,
        )?;
    }
    if buf.is_null() {
        return Ok(String::new());
    }
    // SAFETY: on success `buf` is a NUL-terminated UTF-16 string allocated by the WTS API,
    // read before it is freed exactly once.
    let text = unsafe {
        let text = String::from_utf16_lossy(buf.as_wide());
        WTSFreeMemory(buf.as_ptr() as *mut c_void);
        text
    };
    Ok(text)
}

/// SID of the account named `account` (`DOMAIN\user` or a bare user name), in an 8-byte
/// aligned buffer.
fn account_sid(account: &str) -> Result<Vec<u64>> {
    let name = wide(account);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut sid_use = SID_NAME_USE::default();
    // SAFETY: size probe with no buffers; the lengths are valid out pointers.
    let probe = unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(name.as_ptr()),
            None,
            &mut sid_len,
            None,
            &mut domain_len,
            &mut sid_use,
        )
    };
    if sid_len == 0 {
        return Err(match probe {
            Err(e) => e.into(),
            Ok(()) => Error::Other(format!("LookupAccountNameW({account:?}) reported no size")),
        });
    }
    let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    domain_len = domain.len() as u32;
    // SAFETY: `sid` holds at least `sid_len` bytes and `domain` holds `domain_len` UTF-16
    // units; both outlive the call.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(name.as_ptr()),
            Some(PSID(sid.as_mut_ptr() as *mut c_void)),
            &mut sid_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut sid_use,
        )?;
    }
    Ok(sid)
}

/// `(domain, user)` of the account behind `sid`.
fn account_name(sid: PSID) -> Result<(String, String)> {
    let mut name_len = 0u32;
    let mut domain_len = 0u32;
    let mut sid_use = SID_NAME_USE::default();
    // SAFETY: size probe with no buffers; `sid` points into a live buffer.
    let probe = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            None,
            &mut name_len,
            None,
            &mut domain_len,
            &mut sid_use,
        )
    };
    if name_len == 0 {
        return Err(match probe {
            Err(e) => e.into(),
            Ok(()) => Error::Other("LookupAccountSidW reported no size".into()),
        });
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    domain_len = domain.len() as u32;
    // SAFETY: `name` and `domain` hold `name_len` and `domain_len` UTF-16 units and outlive
    // the call.
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut sid_use,
        )?;
    }
    // On success both lengths exclude the terminating NUL.
    let text =
        |buf: &[u16], len: u32| String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    Ok((text(&domain, domain_len), text(&name, name_len)))
}

/// Whether the account `domain\user` differs from the account behind `own_sid`. The name
/// is resolved to a SID and compared with `own_sid`; when it cannot be resolved, the name
/// `own_sid` resolves to is compared with it instead (ignoring case). Fails when neither
/// lookup succeeds.
fn is_other_account(own_sid: PSID, domain: &str, user: &str) -> Result<bool> {
    let account = if domain.is_empty() {
        user.to_string()
    } else {
        format!(r"{domain}\{user}")
    };
    let sid_error = match account_sid(&account) {
        Ok(sid) => {
            // SAFETY: both SIDs point into live buffers filled by the account lookups.
            let same = unsafe { EqualSid(PSID(sid.as_ptr() as *mut c_void), own_sid) }.is_ok();
            return Ok(!same);
        }
        Err(e) => e,
    };
    match account_name(own_sid) {
        Ok((own_domain, own_user)) => {
            let same = own_user.to_lowercase() == user.to_lowercase()
                && own_domain.to_lowercase() == domain.to_lowercase();
            Ok(!same)
        }
        Err(name_error) => Err(Error::Other(format!(
            "cannot resolve the signed-in account {account:?} ({sid_error}) or the account \
             this process runs as ({name_error})"
        ))),
    }
}

/// True when this process runs as a different account than the user signed in to its
/// session: UAC elevation with another administrator's credentials, or a window opened
/// with another account's credentials ("Run as different user"). Returns
/// `Ok(false)` when the session has no signed-in user (for example a service session), as
/// there is no interactive user to protect then. Fails when the session cannot be queried
/// or the accounts cannot be compared; callers refuse per-user changes then.
pub fn elevated_as_other_user() -> Result<bool> {
    let user = session_string(WTSUserName)?;
    if user.is_empty() {
        return Ok(false);
    }
    let domain = session_string(WTSDomainName)?;
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    let own = token_user(unsafe { GetCurrentProcess() })?;
    is_other_account(sid_of(&own), &domain, &user)
}

/// Error returned for per-user changes (HKCU values, per-user Store packages) attempted
/// while [`elevated_as_other_user`] is true.
pub fn other_user_error() -> Error {
    Error::Other(
        "this window runs as a different account than the signed-in user, so per-user settings \
         would change that account instead of the signed-in user's; nothing was changed. \
         Per-user settings always change the account Cairn runs as, which is the account \
         whose credentials opened it or approved its administrator prompt. \
         To change your own, run Cairn as your own account; where administrator rights \
         are needed, your own account must be an administrator and approve the prompt itself. \
         To change the other account's settings, sign in as that account"
            .to_string(),
    )
}

/// String form (`S-1-5-…`) of a SID.
pub(crate) fn sid_string(sid: PSID) -> Result<String> {
    let mut text = PWSTR::null();
    // SAFETY: `sid` points to a valid SID owned by the caller for the duration of the call;
    // the returned string is freed below with LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &mut text)? };
    // SAFETY: on success `text` is a NUL-terminated string allocated by LocalAlloc, read
    // before it is freed exactly once.
    let result = unsafe {
        let value = text.to_string();
        let _ = LocalFree(Some(HLOCAL(text.0 as *mut c_void)));
        value
    };
    result.map_err(|e| Error::Other(format!("a SID is not valid UTF-16: {e}")))
}

/// SID of the account this process runs as, for example `S-1-5-21-…-1001`.
pub fn current_user_sid() -> Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    let own = token_user(unsafe { GetCurrentProcess() })?;
    sid_string(sid_of(&own))
}

/// `DOMAIN\user` of the account this process runs as.
pub fn process_account_name() -> Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    let own = token_user(unsafe { GetCurrentProcess() })?;
    let (domain, user) = account_name(sid_of(&own))?;
    Ok(if domain.is_empty() {
        user
    } else {
        format!(r"{domain}\{user}")
    })
}

/// True for the service accounts LocalSystem (S-1-5-18), LocalService (S-1-5-19) and
/// NetworkService (S-1-5-20).
pub fn is_service_account_sid(sid: &str) -> bool {
    ["S-1-5-18", "S-1-5-19", "S-1-5-20"]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(sid.trim()))
}

/// The user signed in to this process's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUser {
    pub domain: String,
    pub name: String,
    /// `S-1-5-…` of the account.
    pub sid: String,
}

/// The user signed in to this process's session (`WTSQuerySessionInformationW`), with the
/// account's SID; `Ok(None)` when the session has no signed-in user. Fails when the session
/// cannot be queried or the account's SID cannot be resolved.
pub fn session_user() -> Result<Option<SessionUser>> {
    let name = session_string(WTSUserName)?;
    if name.is_empty() {
        return Ok(None);
    }
    let domain = session_string(WTSDomainName)?;
    let account = if domain.is_empty() {
        name.clone()
    } else {
        format!(r"{domain}\{name}")
    };
    let sid = account_sid(&account)?;
    let sid = sid_string(PSID(sid.as_ptr() as *mut c_void))?;
    Ok(Some(SessionUser { domain, name, sid }))
}

/// Remote Desktop Services session id of this process (0 is the services session).
pub fn current_session_id() -> Result<u32> {
    let mut id = 0u32;
    // SAFETY: `id` is a valid out pointer; GetCurrentProcessId takes no arguments.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id)? };
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_user() -> Vec<u64> {
        // SAFETY: pseudo-handle, no closing needed.
        token_user(unsafe { GetCurrentProcess() }).unwrap()
    }

    #[test]
    fn same_user_as_the_session_when_run_normally() {
        // Tests run in the developer's own session and account.
        assert!(!elevated_as_other_user().unwrap());
    }

    #[test]
    fn own_token_user_is_readable() {
        assert!(!sid_of(&own_user()).is_invalid());
    }

    #[test]
    fn session_user_is_this_process_account() {
        let user = session_string(WTSUserName).unwrap();
        let domain = session_string(WTSDomainName).unwrap();
        assert!(!user.is_empty());
        let own = own_user();
        let (own_domain, own_name) = account_name(sid_of(&own)).unwrap();
        assert!(own_name.eq_ignore_ascii_case(&user), "{own_name} {user}");
        assert!(
            own_domain.eq_ignore_ascii_case(&domain),
            "{own_domain} {domain}"
        );
        let sid = account_sid(&format!(r"{domain}\{user}")).unwrap();
        // SAFETY: both SIDs point into live buffers.
        assert!(unsafe { EqualSid(PSID(sid.as_ptr() as *mut c_void), sid_of(&own)) }.is_ok());
    }

    #[test]
    fn other_accounts_are_detected_by_sid_and_by_name() {
        let own = own_user();
        let own_sid = sid_of(&own);
        let (domain, user) = account_name(own_sid).unwrap();
        assert!(!is_other_account(own_sid, &domain, &user).unwrap());
        assert!(!is_other_account(own_sid, &domain.to_uppercase(), &user.to_uppercase()).unwrap());
        assert!(is_other_account(own_sid, "NT AUTHORITY", "SYSTEM").unwrap());
        // An unresolvable name falls back to comparing names.
        assert!(is_other_account(own_sid, "", "PCOptimizerNoSuchAccount").unwrap());
    }

    #[test]
    fn current_user_sid_is_a_string_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert!(!is_service_account_sid(&sid));
        let session = session_user().unwrap().expect("a signed-in user");
        assert_eq!(session.sid, sid);
        assert!(!session.name.is_empty());
        let account = process_account_name().unwrap();
        assert!(
            account.eq_ignore_ascii_case(&format!(r"{}\{}", session.domain, session.name)),
            "{account}"
        );
    }

    #[test]
    fn service_accounts_are_recognized() {
        for sid in ["S-1-5-18", "S-1-5-19", "S-1-5-20", "s-1-5-18", " S-1-5-20 "] {
            assert!(is_service_account_sid(sid), "{sid}");
        }
        for sid in [
            "S-1-5-21-1111111111-2222222222-3333333333-1001",
            "S-1-5-32-544",
            "S-1-5-180",
            "S-1-5-1",
            "",
        ] {
            assert!(!is_service_account_sid(sid), "{sid}");
        }
    }

    #[test]
    fn current_session_id_is_readable() {
        let id = current_session_id().unwrap();
        // Tests run in an interactive session, never in the services session.
        assert_ne!(id, 0);
    }
}
