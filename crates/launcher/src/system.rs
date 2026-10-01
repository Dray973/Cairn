//! The launcher's calls into Windows: the token type, the running window's activation event,
//! the UAC prompt, the install folder's trust check and error message boxes.

use std::ffi::OsStr;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use optimizer_core::app::{LAUNCHER_DLLS, LAUNCHER_FOLDERS, LAUNCHER_PROGRAM};
use optimizer_core::win::acl::install_location_problem;
use optimizer_core::win::handle::OwnedHandle;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited,
    TOKEN_ELEVATION_TYPE, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenEventW, OpenProcessToken, SetEvent, EVENT_MODIFY_STATE,
};
use windows::Win32::UI::Shell::{
    IsUserAnAdmin, ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, MessageBoxW, ASFW_ANY, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
    SW_SHOWNORMAL,
};

use crate::plan::{error_code, Token, ACTIVATE_EVENT};

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(iter::once(0)).collect()
}

/// The elevation state of this process's token. A default token (no split token) counts as
/// [`Token::Full`] for a member of Administrators and [`Token::Standard`] otherwise.
pub(crate) fn token() -> windows::core::Result<Token> {
    let mut raw = HANDLE::default();
    // SAFETY: the pseudo handle of the current process is always valid; `raw` is a valid out
    // pointer and receives a token handle that `OwnedHandle` closes.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }?;
    let token = OwnedHandle::new(raw);
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    let mut returned = 0u32;
    // SAFETY: `kind` is a TOKEN_ELEVATION_TYPE, the type TokenElevationType writes, and the
    // size passed is its size; `returned` is a valid out pointer.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenElevationType,
            Some(std::ptr::addr_of_mut!(kind).cast()),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut returned,
        )
    }?;
    Ok(if kind == TokenElevationTypeFull {
        Token::Full
    } else if kind == TokenElevationTypeLimited {
        Token::Limited
    // SAFETY: IsUserAnAdmin takes no arguments.
    } else if unsafe { IsUserAnAdmin() }.as_bool() {
        Token::Full
    } else {
        Token::Standard
    })
}

/// Opens the running window's activation event with the right to set it.
fn activation_event() -> Option<OwnedHandle> {
    let name = wide(OsStr::new(ACTIVATE_EVENT));
    // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
    unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) }
        .ok()
        .map(OwnedHandle::new)
}

/// Whether a Cairn window of this session accepts activation.
pub(crate) fn window_running() -> bool {
    activation_event().is_some()
}

/// Lets the running window take the foreground and asks it to come to the front; false when
/// no window accepts activation any more.
pub(crate) fn signal_running_instance() -> bool {
    let Some(event) = activation_event() else {
        return false;
    };
    // SAFETY: plain call; failure only means the window may not take the foreground.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    // SAFETY: `event` is an open event handle with EVENT_MODIFY_STATE access.
    unsafe { SetEvent(event.raw()) }.is_ok()
}

/// Result of the UAC prompt.
#[derive(Debug)]
pub(crate) enum Elevation {
    /// The elevated copy was started.
    Started,
    /// The user declined the prompt.
    Declined,
    /// Windows refused the start with this error code.
    Failed(u32),
}

/// Starts `exe` with `params` in `dir` through the UAC prompt ("runas").
pub(crate) fn start_elevated(exe: &Path, params: &OsStr, dir: &Path) -> Elevation {
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(params);
    let dir = wide(dir.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        lpDirectory: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is initialised with its size; every string it points to is
    // NUL-terminated and outlives the call, which returns only after the start (NOASYNC).
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Elevation::Started,
        Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => Elevation::Declined,
        Err(e) => Elevation::Failed(error_code(e.code().0)),
    }
}

/// Whether the install folder may run elevated code: Cairn.exe, the DLLs it loads and the
/// folders it loads code from can be changed only by administrators, SYSTEM and
/// TrustedInstaller. A check that cannot be completed counts as not trusted.
pub(crate) fn location_trusted(root: &Path) -> bool {
    matches!(
        install_location_problem(
            &root.join(LAUNCHER_PROGRAM),
            LAUNCHER_DLLS,
            LAUNCHER_FOLDERS
        ),
        Ok(None)
    )
}

/// An error message box owned by no window.
pub(crate) fn show_error(text: &str) {
    let text = wide(OsStr::new(text));
    let caption = wide(OsStr::new(optimizer_core::APP_NAME));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}
