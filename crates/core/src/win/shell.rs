//! Windows shell helpers: restarting File Explorer, and opening a web page in the browser
//! of the signed-in user through the desktop shell, so the browser never runs with this
//! process's administrator rights.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{Interface, BSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Com::{CoCreateInstance, IDispatch, IServiceProvider, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellView, IShellWindows,
    SID_STopLevelBrowser, ShellWindows, CSIDL_DESKTOP, SVGIO_BACKGROUND, SWC_DESKTOP,
    SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::com::ComApartment;
use super::is_win32;
use crate::{Error, Result};

/// Longest web address handed to the desktop shell, in characters.
pub const MAX_URL_LEN: usize = 2048;

/// Characters a web address may not hold unescaped, beside spaces and control characters.
const UNSAFE_URL_CHARS: &str = "\"<>\\^`{|}";

/// Checks that `url` is a web page the desktop shell may open: `https://` followed by a host
/// name (letters, digits, dots, hyphens and an optional port), printable ASCII without spaces
/// or characters a web address may not hold unescaped, and at most [`MAX_URL_LEN`]
/// characters. Anything else could make the shell start a program instead of the browser.
pub fn check_web_url(url: &str) -> Result<()> {
    if url.len() > MAX_URL_LEN {
        return Err(Error::Other(format!(
            "the link is longer than {MAX_URL_LEN} characters"
        )));
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(Error::Other(format!(
            "{} opens only https:// links, not {url}",
            crate::APP_NAME
        )));
    };
    if !url
        .bytes()
        .all(|b| b.is_ascii_graphic() && !UNSAFE_URL_CHARS.contains(char::from(b)))
    {
        return Err(Error::Other(format!(
            "the link {url:?} holds spaces or characters a web address cannot hold"
        )));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let name_ok = !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let port_ok = port.map_or(true, |p| {
        !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
    });
    if !(name_ok && port_ok) {
        return Err(Error::Other(format!("the link {url} names no web site")));
    }
    Ok(())
}

/// How long opening a web page waits for the desktop shell to take it.
const OPEN_URL_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens the web page `url` in the signed-in user's default browser through the desktop
/// shell (Explorer), so the browser starts with the shell's unelevated token even when this
/// process is elevated. The shell is asked through its automation objects (`IShellWindows`
/// finds the desktop window, its active view gives `IShellDispatch2::ShellExecute`), on a
/// thread of its own in a single-threaded apartment; no process is started here. Fails for a
/// URL [`check_web_url`] refuses, when no desktop shell runs in this session and when the
/// shell does not answer within 10 s (the page may still open later).
pub fn open_url_as_desktop_user(url: &str) -> Result<()> {
    check_web_url(url)?;
    let url = url.to_string();
    let (done, outcome) = mpsc::channel();
    thread::Builder::new()
        .name("shell-open-url".into())
        .spawn(move || {
            let _com = ComApartment::enter();
            // After a timeout nobody waits any more, and the result is dropped.
            let _ = done.send(shell_execute_on_desktop(&url));
        })
        .map_err(|e| {
            Error::Other(format!(
                "could not start the thread that opens the page: {e}"
            ))
        })?;
    match outcome.recv_timeout(OPEN_URL_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(Error::Other(format!(
            "the desktop shell did not answer within {} s",
            OPEN_URL_TIMEOUT.as_secs()
        ))),
        Err(RecvTimeoutError::Disconnected) => {
            Err(Error::Other("opening the page failed unexpectedly".into()))
        }
    }
}

/// Hands `url` to the desktop shell's `ShellExecute`. COM must be initialized on the calling
/// thread.
fn shell_execute_on_desktop(url: &str) -> Result<()> {
    // SAFETY: COM is initialized on this thread; the interface is released before it ends.
    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }?;
    let location = VARIANT::from(CSIDL_DESKTOP as i32);
    let root = VARIANT::default();
    let mut hwnd = 0i32;
    // SAFETY: both VARIANTs and `hwnd` outlive the call; the dispatch interface it returns, if
    // any, is owned by the caller.
    let desktop: IDispatch = unsafe {
        shell_windows.FindWindowSW(&location, &root, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)
    }
    .map_err(no_desktop_window)?;
    let provider: IServiceProvider = desktop.cast()?;
    // SAFETY: the service id outlives the call; the browser interface is owned by the caller.
    let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser) }?;
    // SAFETY: plain interface calls on live interfaces of this thread.
    let view: IShellView = unsafe { browser.QueryActiveShellView() }?;
    // SAFETY: as above; the background object is the view's automation object.
    let background: IDispatch = unsafe { view.GetItemObject(SVGIO_BACKGROUND) }?;
    let folder_view: IShellFolderViewDual = background.cast()?;
    // SAFETY: as above.
    let application = unsafe { folder_view.Application() }?;
    let shell: IShellDispatch2 = application.cast()?;
    let empty = VARIANT::from("");
    let show = VARIANT::from(SW_SHOWNORMAL.0);
    // SAFETY: the BSTR and VARIANTs outlive the call. An empty operation is the default verb,
    // which opens a web address in the default browser.
    unsafe { shell.ShellExecute(&BSTR::from(url), &empty, &empty, &empty, &show) }?;
    Ok(())
}

/// The error of the desktop window lookup. A lookup that succeeds without a window (S_FALSE,
/// which windows-rs reports as an error with a success code) means no desktop shell runs in
/// this session.
fn no_desktop_window(e: windows::core::Error) -> Error {
    if e.code().is_ok() {
        Error::Other("no desktop shell runs in this session".into())
    } else {
        e.into()
    }
}

/// How long to wait for a new shell, once after terminating and once after starting one.
const RESTART_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for a terminated explorer.exe to finish exiting.
const EXIT_TIMEOUT_MS: u32 = 5_000;
/// Exit code for terminated shells. Winlogon's AutoRestartShell restarts a shell that
/// exits with a non-zero code.
const TERMINATE_EXIT_CODE: u32 = 1;
/// Buffer size, in UTF-16 units, for process image paths.
const IMAGE_PATH_BUFFER: usize = 32_768;

/// Restarts File Explorer so interface settings take effect without signing out.
/// Terminates the current user's explorer.exe processes and relies on Winlogon's
/// AutoRestartShell to start a new shell under the user's own (non-elevated) token;
/// starts one directly only if none has reappeared after a few seconds.
///
/// Every instance is opened and verified before any is terminated, and termination goes
/// through those handles only: an open handle keeps its process id from being reused, so
/// a process started while the shell restarts is never terminated in its place.
///
/// When one instance cannot be terminated, the others are still restarted and the first
/// such error is returned once a shell is running.
pub fn restart_explorer() -> Result<()> {
    let session = current_session()?;
    let mut first_error = None;
    let shells = open_explorers(session, &mut first_error)?;
    for shell in &shells {
        if let Err(e) = terminate(shell) {
            first_error.get_or_insert(e);
        }
    }
    drop(shells);
    if !wait_for_explorer(session)? {
        // Started directly, not through win::process::hardened_command: the shell keeps
        // this process's environment on purpose (it hands it to every program the user
        // starts), so only the program itself comes from an absolute path.
        Command::new(super::paths::windows_dir()?.join("explorer.exe")).spawn()?;
        if !wait_for_explorer(session)? {
            return Err(Error::Other(
                "explorer.exe did not start again after it was terminated".to_string(),
            ));
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl OwnedHandle {
    /// True when the process behind this handle has exited. Requires SYNCHRONIZE access.
    fn has_exited(&self) -> bool {
        // SAFETY: waiting with a zero timeout only reads the handle's signaled state.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

fn process_session(pid: u32) -> windows::core::Result<u32> {
    let mut session = 0u32;
    // SAFETY: `session` is a valid out pointer for the duration of the call.
    unsafe { ProcessIdToSessionId(pid, &mut session)? };
    Ok(session)
}

fn session_of(pid: u32) -> Option<u32> {
    process_session(pid).ok()
}

fn current_session() -> Result<u32> {
    // SAFETY: GetCurrentProcessId has no preconditions.
    Ok(process_session(unsafe { GetCurrentProcessId() })?)
}

/// Process ids of the explorer.exe instances running in `session`.
fn explorer_pids(session: u32) -> Result<Vec<u32>> {
    // SAFETY: a process snapshot takes no caller-owned memory; the handle is closed on drop.
    let snapshot = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut pids = Vec::new();
    // SAFETY: `entry` is a properly sized PROCESSENTRY32W for every call.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
    while more {
        let exe = &entry.szExeFile;
        let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
        let name = String::from_utf16_lossy(&exe[..len]);
        if name.eq_ignore_ascii_case("explorer.exe")
            && session_of(entry.th32ProcessID) == Some(session)
        {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
    }
    Ok(pids)
}

/// Open handles to the explorer.exe instances in `session`, each verified through its
/// handle to still be the Windows shell binary running in that session. Instances that
/// exited in the meantime are skipped; an instance that cannot be opened or verified is
/// skipped and its error kept in `first_error`.
fn open_explorers(session: u32, first_error: &mut Option<Error>) -> Result<Vec<OwnedHandle>> {
    let expected = shell_image();
    let access = PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    let mut handles = Vec::new();
    for pid in explorer_pids(session)? {
        // SAFETY: plain handle request; the handle is closed on drop.
        let handle = match unsafe { OpenProcess(access, false, pid) } {
            Ok(h) => OwnedHandle(h),
            Err(e) if is_win32(&e, ERROR_INVALID_PARAMETER) => continue,
            Err(e) => {
                first_error.get_or_insert(e.into());
                continue;
            }
        };
        // The open handle pins `pid`, so the checks below describe the process that
        // would be terminated.
        if session_of(pid) != Some(session) {
            continue;
        }
        match image_path(&handle) {
            Ok(image) if is_shell_image(&image, expected.as_deref()) => handles.push(handle),
            Ok(_) => {}
            Err(_) if handle.has_exited() => {}
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    Ok(handles)
}

/// `%SystemRoot%\explorer.exe`, when the variable is set.
fn shell_image() -> Option<PathBuf> {
    std::env::var_os("SystemRoot").map(|root| PathBuf::from(root).join("explorer.exe"))
}

/// Whether `image` is the shell binary: exactly `expected` (ignoring case) when known,
/// otherwise any file named explorer.exe.
fn is_shell_image(image: &Path, expected: Option<&Path>) -> bool {
    match expected {
        Some(expected) => image
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy()),
        None => image
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe")),
    }
}

/// Full Win32 path of the image of the process behind `handle`. Requires
/// PROCESS_QUERY_LIMITED_INFORMATION access.
fn image_path(handle: &OwnedHandle) -> Result<PathBuf> {
    let mut buf = vec![0u16; IMAGE_PATH_BUFFER];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is writable for `len` UTF-16 units and outlives the call.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )?;
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// Terminates one process through its handle and waits for it to exit. A process that has
/// already exited counts as terminated.
fn terminate(handle: &OwnedHandle) -> Result<()> {
    // SAFETY: `handle` was opened with PROCESS_TERMINATE.
    if let Err(e) = unsafe { TerminateProcess(handle.0, TERMINATE_EXIT_CODE) } {
        // TerminateProcess fails with access denied on a process that is already exiting.
        if !handle.has_exited() {
            return Err(e.into());
        }
    }
    // SAFETY: `handle` was opened with SYNCHRONIZE.
    unsafe { WaitForSingleObject(handle.0, EXIT_TIMEOUT_MS) };
    Ok(())
}

/// Polls for an explorer.exe in `session` for up to [`RESTART_TIMEOUT`].
fn wait_for_explorer(session: u32) -> Result<bool> {
    let deadline = Instant::now() + RESTART_TIMEOUT;
    loop {
        if !explorer_pids(session)?.is_empty() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::E_ACCESSDENIED;

    use super::*;

    #[test]
    fn web_urls_are_checked() {
        for url in [
            "https://github.com/Dray973/Cairn",
            "https://aka.ms/getsecureboot",
            "https://support.microsoft.com/topic/5d1bf6b4-7972-455a-a421-0184f1e1ed7d",
            "https://example.com",
            "https://example.com:8443/a?b=c&d=%20e#f",
        ] {
            assert!(check_web_url(url).is_ok(), "{url}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        let at_limit = &long[..MAX_URL_LEN];
        assert!(check_web_url(at_limit).is_ok());
        for url in [
            "http://example.com/",
            "HTTPS://example.com/",
            "ftp://example.com/",
            "file:///C:/Windows/System32/cmd.exe",
            "ms-settings:windowsupdate",
            "windowsdefender://threat/",
            "javascript:alert(1)",
            "C:\\Windows\\System32\\cmd.exe",
            "cmd.exe",
            "",
            " https://example.com/",
            "https://example.com/a b",
            "https://example.com/\ta",
            "https://example.com/\n",
            "https://example.com/\"x\"",
            "https://example.com/<x>",
            "https://example.com/a\\b",
            "https://example.com/a|b",
            "https://example.com/ä",
            "https://",
            "https:///path",
            "https://user@example.com/",
            "https://.example.com/",
            "https://example.com:/",
            "https://example.com:80a/",
            &long,
        ] {
            assert!(check_web_url(url).is_err(), "{url:?}");
        }
        let err = check_web_url("http://example.com/").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Cairn opens only https:// links, not http://example.com/"
        );
        assert!(check_web_url(&long)
            .unwrap_err()
            .to_string()
            .contains("longer than 2048 characters"));
    }

    #[test]
    fn a_lookup_without_a_desktop_window_names_the_missing_shell() {
        // FindWindowSW answers S_FALSE and no window when none is registered; windows-rs
        // reports the missing interface as an error with a success code.
        let missing = no_desktop_window(windows::core::Error::empty());
        assert_eq!(missing.to_string(), "no desktop shell runs in this session");
        let denied = no_desktop_window(windows::core::Error::from_hresult(E_ACCESSDENIED));
        assert!(matches!(denied, Error::Win32(_)), "{denied}");
    }

    #[test]
    #[ignore = "opens a web page in the browser; run by exact name from an elevated shell"]
    fn open_url_opens_as_the_desktop_user() {
        // Opens the project page through the desktop shell. Task Manager's Details view with
        // the Elevated column shows whether the browser process runs elevated.
        open_url_as_desktop_user("https://github.com/Dray973/Cairn").unwrap();
    }

    #[test]
    fn shell_image_match_is_exact_and_case_insensitive() {
        let expected = Path::new(r"C:\Windows\explorer.exe");
        assert!(is_shell_image(
            Path::new(r"C:\WINDOWS\Explorer.EXE"),
            Some(expected)
        ));
        assert!(!is_shell_image(
            Path::new(r"C:\Windows\SysWOW64\explorer.exe"),
            Some(expected)
        ));
        assert!(!is_shell_image(
            Path::new(r"C:\Windows\System32\RuntimeBroker.exe"),
            Some(expected)
        ));
        assert!(is_shell_image(Path::new(r"D:\OS\explorer.exe"), None));
        assert!(!is_shell_image(Path::new(r"D:\OS\notepad.exe"), None));
    }

    #[test]
    fn running_shells_are_opened_and_verified_without_terminating() {
        let session = current_session().unwrap();
        let pids = explorer_pids(session).unwrap();
        let mut first_error = None;
        let handles = open_explorers(session, &mut first_error).unwrap();
        assert!(first_error.is_none(), "{first_error:?}");
        assert!(handles.len() <= pids.len());
        for handle in &handles {
            let image = image_path(handle).unwrap();
            assert!(
                is_shell_image(&image, shell_image().as_deref()),
                "{image:?}"
            );
            assert!(!handle.has_exited());
        }
    }

    #[test]
    fn own_image_path_is_readable() {
        // SAFETY: GetCurrentProcessId has no preconditions; the handle is closed on drop.
        let own = OwnedHandle(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    GetCurrentProcessId(),
                )
            }
            .unwrap(),
        );
        let image = image_path(&own).unwrap();
        let expected = std::env::current_exe().unwrap();
        assert!(
            image
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.to_string_lossy()),
            "{image:?} {expected:?}"
        );
        assert!(!own.has_exited());
    }
}
