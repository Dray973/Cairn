//! Windows shell helpers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use super::is_win32;
use crate::{Error, Result};

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
    use super::*;

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
