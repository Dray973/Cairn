//! Child processes started from absolute paths with a fixed working directory and an
//! environment computed from the operating system, and the list of running process names.
//!
//! The engine runs elevated, and scheduled maintenance runs elevated and unattended, while
//! both inherit the environment of the signed-in user, which unelevated processes can change
//! (`PSModulePath`, `windir`, `__COMPAT_LAYER` and the like). Children therefore never
//! inherit it: [`hardened_command`] clears the environment and sets only the variables of
//! [`CHILD_ENV_NAMES`], with values read from Windows itself.

use std::collections::HashSet;
use std::ffi::OsString;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::WindowsProgramming::GetComputerNameW;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    FOLDERID_ProgramFilesCommon, FOLDERID_ProgramFilesCommonX86, FOLDERID_ProgramFilesX86,
    FOLDERID_Public, FOLDERID_RoamingAppData,
};

use crate::win::paths::{known_folder, system_dir, windows_dir};
use crate::win::session;
use crate::{Error, Result};

/// `CREATE_NO_WINDOW` process creation flag: console programs get no console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Every environment variable a child started through [`hardened_command`] receives.
pub const CHILD_ENV_NAMES: &[&str] = &[
    "SystemRoot",
    "windir",
    "SystemDrive",
    "ComSpec",
    "PATH",
    "PATHEXT",
    "PSModulePath",
    "ProgramData",
    "ALLUSERSPROFILE",
    "ProgramFiles",
    "ProgramW6432",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "CommonProgramW6432",
    "CommonProgramFiles(x86)",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PUBLIC",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

/// `PATHEXT` as Windows ships it.
const PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
/// SID of LocalSystem, whose temporary folder is under the Windows folder.
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// A [`Command`] for the program at the absolute path `program` that does not depend on the
/// caller's environment: the working directory is System32, the environment is cleared and
/// holds exactly [`child_environment`], and stdin is the null device. Callers add
/// arguments, output handles and creation flags.
pub fn hardened_command(program: &Path) -> Result<Command> {
    if !program.is_absolute() {
        return Err(Error::Other(format!(
            "program path must be absolute: {}",
            program.display()
        )));
    }
    let system = system_dir()?;
    let environment = child_environment()?;
    let mut command = Command::new(program);
    command
        .current_dir(&system)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null());
    Ok(command)
}

/// The environment of a child process: the variables of [`CHILD_ENV_NAMES`] with values
/// computed from Windows (system folders, known folders of the account this process runs
/// as, the account and computer names), never read from this process's environment. A value
/// that cannot be computed is left out. Fails only when the System32 or Windows folder
/// cannot be read.
pub fn child_environment() -> Result<Vec<(&'static str, OsString)>> {
    let system = system_dir()?;
    let windows = windows_dir()?;
    let mut env: Vec<(&'static str, OsString)> = Vec::with_capacity(CHILD_ENV_NAMES.len());

    env.push(("SystemRoot", windows.clone().into_os_string()));
    env.push(("windir", windows.clone().into_os_string()));
    if let Some(drive) = drive_of(&windows) {
        env.push(("SystemDrive", drive.into()));
    }
    env.push(("ComSpec", system.join("cmd.exe").into_os_string()));
    env.push(("PATH", search_path(&system, &windows)));
    env.push(("PATHEXT", PATHEXT.into()));
    env.push((
        "PSModulePath",
        system
            .join(r"WindowsPowerShell\v1.0\Modules")
            .into_os_string(),
    ));

    let folders: [(&[&'static str], windows::core::GUID); 5] = [
        (&["ProgramData", "ALLUSERSPROFILE"], FOLDERID_ProgramData),
        (&["ProgramFiles", "ProgramW6432"], FOLDERID_ProgramFiles),
        (&["ProgramFiles(x86)"], FOLDERID_ProgramFilesX86),
        (
            &["CommonProgramFiles", "CommonProgramW6432"],
            FOLDERID_ProgramFilesCommon,
        ),
        (&["CommonProgramFiles(x86)"], FOLDERID_ProgramFilesCommonX86),
    ];
    for (names, id) in folders {
        if let Ok(path) = known_folder(&id) {
            for &name in names {
                env.push((name, path.clone().into_os_string()));
            }
        }
    }

    if let Ok(profile) = known_folder(&FOLDERID_Profile) {
        if let Some((drive, rest)) = split_home(&profile) {
            env.push(("HOMEDRIVE", drive.into()));
            env.push(("HOMEPATH", rest.into()));
        }
        env.push(("USERPROFILE", profile.into_os_string()));
    }
    if let Ok(roaming) = known_folder(&FOLDERID_RoamingAppData) {
        env.push(("APPDATA", roaming.into_os_string()));
    }
    let local = known_folder(&FOLDERID_LocalAppData).ok();
    if let Some(local) = &local {
        env.push(("LOCALAPPDATA", local.clone().into_os_string()));
    }
    if let Ok(public) = known_folder(&FOLDERID_Public) {
        env.push(("PUBLIC", public.into_os_string()));
    }
    let is_system = session::current_user_sid()
        .map(|sid| sid.eq_ignore_ascii_case(LOCAL_SYSTEM_SID))
        .unwrap_or(false);
    if let Some(temp) = temp_dir(is_system, local.as_deref(), &windows) {
        env.push(("TEMP", temp.clone().into_os_string()));
        env.push(("TMP", temp.into_os_string()));
    }

    if let Ok(account) = session::process_account_name() {
        let (domain, user) = split_account(&account);
        if !user.is_empty() {
            env.push(("USERNAME", user.into()));
        }
        if !domain.is_empty() {
            env.push(("USERDOMAIN", domain.into()));
        }
    }
    if let Some(name) = computer_name() {
        env.push(("COMPUTERNAME", name));
    }
    env.push(("NUMBER_OF_PROCESSORS", processor_count().to_string().into()));
    env.push(("PROCESSOR_ARCHITECTURE", "AMD64".into()));
    env.push(("OS", "Windows_NT".into()));
    Ok(env)
}

/// `PATH` of a child: System32, the Windows folder, WBEM and Windows PowerShell.
fn search_path(system: &Path, windows: &Path) -> OsString {
    format!(
        "{system};{windows};{system}\\Wbem;{system}\\WindowsPowerShell\\v1.0",
        system = system.display(),
        windows = windows.display()
    )
    .into()
}

/// `C:` of `C:\Windows`; `None` for a path without a drive letter.
fn drive_of(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let bytes = text.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        .then(|| text[..2].to_string())
}

/// `("C:", "\Users\Test")` of `C:\Users\Test`; `None` for a path without a drive letter.
fn split_home(profile: &Path) -> Option<(String, String)> {
    let drive = drive_of(profile)?;
    let rest = profile.to_str()?[2..].to_string();
    let rest = if rest.is_empty() {
        "\\".to_string()
    } else {
        rest
    };
    Some((drive, rest))
}

/// `(domain, user)` of `DOMAIN\user`; a name without a backslash is a user name.
fn split_account(account: &str) -> (&str, &str) {
    account.split_once('\\').unwrap_or(("", account))
}

/// The temporary folder of the account: `<LocalAppData>\Temp`; for LocalSystem
/// `<Windows>\SystemTemp` when it exists, else `<Windows>\Temp`.
fn temp_dir(is_system: bool, local: Option<&Path>, windows: &Path) -> Option<PathBuf> {
    if is_system {
        let system_temp = windows.join("SystemTemp");
        return Some(if system_temp.is_dir() {
            system_temp
        } else {
            windows.join("Temp")
        });
    }
    local.map(|local| local.join("Temp"))
}

/// NetBIOS name of this computer (`GetComputerNameW`).
fn computer_name() -> Option<OsString> {
    // MAX_COMPUTERNAME_LENGTH is 15; the buffer is larger in case that ever grows.
    let mut buf = vec![0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is writable for `len` UTF-16 units, which the call updates.
    unsafe { GetComputerNameW(Some(PWSTR(buf.as_mut_ptr())), &mut len) }.ok()?;
    let len = (len as usize).min(buf.len());
    (len > 0).then(|| OsString::from_wide(&buf[..len]))
}

/// Logical processors of this process's processor group (`GetSystemInfo`).
fn processor_count() -> u32 {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is a valid, writable SYSTEM_INFO.
    unsafe { GetSystemInfo(&mut info) };
    info.dwNumberOfProcessors.max(1)
}

/// [`hardened_command`] for the program `exe` in System32. `exe` must be a bare file name
/// such as `sfc.exe`.
pub fn system_command(exe: &str) -> Result<Command> {
    if exe.is_empty() || exe.contains(['\\', '/', ':']) || exe.contains("..") {
        return Err(Error::Other(format!("not a System32 program name: {exe}")));
    }
    hardened_command(&system_dir()?.join(exe))
}

struct Snapshot(HANDLE);

impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: handle from CreateToolhelp32Snapshot, closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Lowercase executable names of every running process (for example `chrome.exe`);
/// `None` when the process list cannot be read.
pub fn running_process_names() -> Option<HashSet<String>> {
    // SAFETY: a process snapshot takes no caller-owned memory.
    let snapshot = Snapshot(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut names = HashSet::new();
    // SAFETY: valid snapshot handle; `entry` is initialised with its size.
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_err() {
        return None;
    }
    loop {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        names.insert(String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase());
        // SAFETY: as above; the call fails once the list is exhausted.
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt;

    use super::*;

    #[test]
    fn system_command_runs_programs_from_system32_only() {
        let system = system_dir().unwrap();
        let command = system_command("cmd.exe").unwrap();
        let program = Path::new(command.get_program());
        assert!(program.is_absolute(), "{}", program.display());
        assert_eq!(program, system.join("cmd.exe"));
        assert_eq!(command.get_current_dir(), Some(system.as_path()));
        let path = command
            .get_envs()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .and_then(|(_, v)| v)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(path.starts_with(&system.display().to_string()), "{path}");

        for bad in ["..\\x.exe", "C:\\x.exe", "sub/x.exe", "x:.exe", ""] {
            assert!(system_command(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(hardened_command(Path::new("cmd.exe")).is_err());
    }

    #[test]
    fn the_child_environment_is_exactly_the_allowlist() {
        let env = child_environment().unwrap();
        let names: Vec<&str> = env.iter().map(|(name, _)| *name).collect();
        for name in &names {
            assert!(CHILD_ENV_NAMES.contains(name), "{name}");
        }
        let unique: HashSet<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        let value = |name: &str| {
            env.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| PathBuf::from(v))
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        let system = system_dir().unwrap();
        let windows = windows_dir().unwrap();
        assert_eq!(value("SystemRoot"), windows);
        assert_eq!(value("windir"), windows);
        assert_eq!(value("ComSpec"), system.join("cmd.exe"));
        assert_eq!(
            value("PSModulePath"),
            system.join(r"WindowsPowerShell\v1.0\Modules")
        );
        assert!(value("PSModulePath").is_dir());
        assert!(value("ProgramFiles").is_dir());
        assert_eq!(value("OS"), PathBuf::from("Windows_NT"));
        // The account and computer names are those of this process, never inherited ones.
        let account = session::process_account_name().unwrap();
        let (_, user) = split_account(&account);
        assert_eq!(value("USERNAME"), PathBuf::from(user));
        assert!(!value("COMPUTERNAME").as_os_str().is_empty());
        let temp = value("TEMP");
        assert_eq!(temp, value("TMP"));
        assert!(temp.is_absolute(), "{}", temp.display());
    }

    #[test]
    fn a_child_sees_only_the_allowlisted_variables() {
        // cargo sets OPTIMIZER_FORBID_RESTORE_POINT for every test process; the child must
        // not see it or any other inherited variable.
        assert!(std::env::vars_os().any(|(k, _)| k
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("OPTIMIZER_")));
        let output = system_command("cmd.exe")
            .unwrap()
            .args(["/d", "/c", "set"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        let allowed: HashSet<String> = CHILD_ENV_NAMES
            .iter()
            .map(|n| n.to_ascii_uppercase())
            .chain(["PROMPT".to_string()])
            .collect();
        let mut seen = Vec::new();
        for line in text.lines() {
            let Some((name, _)) = line.split_once('=') else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let upper = name.to_ascii_uppercase();
            assert!(allowed.contains(&upper), "unexpected variable {name}");
            assert!(!upper.starts_with("OPTIMIZER_"), "{name}");
            assert!(!upper.starts_with("PYTHON"), "{name}");
            seen.push(upper);
        }
        for required in ["SYSTEMROOT", "PATH", "PSMODULEPATH", "COMSPEC"] {
            assert!(seen.iter().any(|n| n == required), "{required} missing");
        }
    }

    #[test]
    fn helper_splits_follow_windows_conventions() {
        assert_eq!(drive_of(Path::new(r"C:\Windows")).as_deref(), Some("C:"));
        assert_eq!(drive_of(Path::new(r"\\server\share")), None);
        assert_eq!(
            split_home(Path::new(r"C:\Users\Test")),
            Some(("C:".to_string(), r"\Users\Test".to_string()))
        );
        assert_eq!(
            split_home(Path::new(r"D:\")),
            Some(("D:".to_string(), r"\".to_string()))
        );
        assert_eq!(split_account(r"TEST-PC\Test"), ("TEST-PC", "Test"));
        assert_eq!(split_account("Test"), ("", "Test"));
        let windows = Path::new(r"C:\Windows");
        assert_eq!(
            temp_dir(
                false,
                Some(Path::new(r"C:\Users\Test\AppData\Local")),
                windows
            ),
            Some(PathBuf::from(r"C:\Users\Test\AppData\Local\Temp"))
        );
        assert_eq!(temp_dir(false, None, windows), None);
        let system_temp = temp_dir(true, None, &windows_dir().unwrap()).unwrap();
        assert!(
            system_temp.ends_with("SystemTemp") || system_temp.ends_with("Temp"),
            "{}",
            system_temp.display()
        );
    }

    #[test]
    fn running_processes_include_this_one() {
        let names = running_process_names().expect("process list");
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(names.contains(&name), "{name} not in the process list");
    }
}
