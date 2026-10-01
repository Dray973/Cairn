//! Built-in Windows management tools, opened by absolute path from System32.
//!
//! Every tool is started directly by its absolute path in System32, with System32 as its
//! working directory (never through the shell's file associations), so nothing on the
//! search path or in the working directory can stand in for it. Unlike the maintenance
//! tools, whose output Cairn reads, these tools keep the environment Cairn was started with,
//! `PATH` included: a tool such as Task Manager passes it on to every program the user
//! starts from it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::win::paths::system_dir;
use crate::{Error, Result};

/// A Windows tool Cairn can open.
#[derive(Debug, Clone, Serialize)]
pub struct WindowsTool {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// It opens only from an elevated process.
    pub requires_admin: bool,
    /// File name of the program in System32.
    #[serde(skip)]
    pub program: &'static str,
    /// File name of the console snap-in in System32 that `mmc.exe` opens.
    #[serde(skip)]
    pub snap_in: Option<&'static str>,
}

const fn tool(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    requires_admin: bool,
    program: &'static str,
    snap_in: Option<&'static str>,
) -> WindowsTool {
    WindowsTool {
        id,
        title,
        description,
        requires_admin,
        program,
        snap_in,
    }
}

static WINDOWS_TOOLS: [WindowsTool; 11] = [
    tool(
        "task_manager",
        "Task Manager",
        "Running apps and processes; end one that stopped responding.",
        false,
        "taskmgr.exe",
        None,
    ),
    tool(
        "resource_monitor",
        "Resource Monitor",
        "Live processor, memory, disk and network use of every process.",
        false,
        "resmon.exe",
        None,
    ),
    tool(
        "event_viewer",
        "Event Viewer",
        "Windows' logs of errors, warnings and other events.",
        true,
        "mmc.exe",
        Some("eventvwr.msc"),
    ),
    tool(
        "device_manager",
        "Device Manager",
        "Hardware devices and their drivers.",
        true,
        "mmc.exe",
        Some("devmgmt.msc"),
    ),
    tool(
        "disk_management",
        "Disk Management",
        "Disks, partitions and drive letters.",
        true,
        "mmc.exe",
        Some("diskmgmt.msc"),
    ),
    tool(
        "services",
        "Services",
        "Background services and how they start.",
        true,
        "mmc.exe",
        Some("services.msc"),
    ),
    tool(
        "system_information",
        "System Information",
        "A detailed report of the hardware, drivers and software environment.",
        false,
        "msinfo32.exe",
        None,
    ),
    tool(
        "optimize_drives",
        "Optimize Drives",
        "Windows' drive optimization schedule and the state of each drive.",
        true,
        "dfrgui.exe",
        None,
    ),
    tool(
        "disk_cleanup",
        "Disk Cleanup",
        "Windows' own cleanup of temporary and system files.",
        false,
        "cleanmgr.exe",
        None,
    ),
    tool(
        "system_protection",
        "System Protection",
        "Turn System Protection on or off and manage the space restore points use.",
        true,
        "SystemPropertiesProtection.exe",
        None,
    ),
    tool(
        "system_restore",
        "System Restore",
        "Return Windows to an earlier restore point.",
        true,
        "rstrui.exe",
        None,
    ),
];

/// Windows tools that fix a finding of the security checkup; the Tools section does not
/// list them.
static FIX_TOOLS: [WindowsTool; 4] = [
    tool(
        "uac_settings",
        "User Account Control settings",
        "Choose when Windows asks before apps make changes.",
        false,
        "UserAccountControlSettings.exe",
        None,
    ),
    tool(
        "remote_settings",
        "Remote settings",
        "Remote Assistance and Remote Desktop settings of this PC.",
        true,
        "SystemPropertiesRemote.exe",
        None,
    ),
    tool(
        "user_accounts",
        "User Accounts",
        "Accounts on this PC and whether they must enter a password to sign in.",
        true,
        "netplwiz.exe",
        None,
    ),
    tool(
        "windows_features",
        "Windows Features",
        "Turn optional Windows components on or off.",
        true,
        "OptionalFeatures.exe",
        None,
    ),
];

/// Every Windows tool the Tools section lists, in display order.
pub fn windows_tools() -> &'static [WindowsTool] {
    &WINDOWS_TOOLS
}

/// The Windows tools that fix security checkup findings; not listed in the Tools section.
pub fn fix_tools() -> &'static [WindowsTool] {
    &FIX_TOOLS
}

/// A tool of [`windows_tools`] or [`fix_tools`] by id.
pub fn windows_tool(id: &str) -> Option<&'static WindowsTool> {
    WINDOWS_TOOLS
        .iter()
        .chain(FIX_TOOLS.iter())
        .find(|t| t.id == id)
}

/// `ERROR_ELEVATION_REQUIRED`.
const ERROR_ELEVATION_REQUIRED: i32 = 740;

/// Opens a Windows tool. The process is started and left running on its own.
pub fn open_windows_tool(id: &str) -> Result<()> {
    open_windows_tool_with(id, &mut |program, args| {
        let mut command =
            windows_tool_command(program, args).map_err(|e| io::Error::other(e.to_string()))?;
        command.spawn().map(drop)
    })
}

/// The command that opens a Windows tool: the absolute `program` with `args`, System32 as
/// the working directory, stdin from the null device, and Cairn's own environment (unlike
/// the maintenance tools, which get the computed child environment).
fn windows_tool_command(program: &Path, args: &[PathBuf]) -> Result<Command> {
    if !program.is_absolute() {
        return Err(Error::Other(format!(
            "program path must be absolute: {}",
            program.display()
        )));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(system_dir()?)
        .stdin(Stdio::null());
    Ok(command)
}

/// [`open_windows_tool`] with the start of the process handed to `spawn`, which receives
/// the absolute program path and its arguments (the absolute snap-in path for consoles).
pub fn open_windows_tool_with(
    id: &str,
    spawn: &mut dyn FnMut(&Path, &[PathBuf]) -> io::Result<()>,
) -> Result<()> {
    let tool =
        windows_tool(id).ok_or_else(|| Error::Other(format!("unknown Windows tool {id:?}")))?;
    let system = system_dir()?;
    let program = system.join(tool.program);
    let args: Vec<PathBuf> = tool.snap_in.map(|s| system.join(s)).into_iter().collect();
    match spawn(&program, &args) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED) => Err(Error::NotElevated),
        Err(e) => Err(Error::Other(format!("could not open {}: {e}", tool.title))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn fix_tools_open_by_id_but_are_not_listed() {
        let listed: HashSet<&str> = windows_tools().iter().map(|t| t.id).collect();
        let fixes: Vec<&str> = fix_tools().iter().map(|t| t.id).collect();
        assert_eq!(
            fixes,
            [
                "uac_settings",
                "remote_settings",
                "user_accounts",
                "windows_features"
            ]
        );
        for id in &fixes {
            assert!(!listed.contains(id), "{id} is listed");
            assert_eq!(windows_tool(id).map(|t| t.id), Some(*id));
        }
        assert!(!windows_tool("uac_settings").unwrap().requires_admin);
        for id in ["remote_settings", "user_accounts", "windows_features"] {
            assert!(windows_tool(id).unwrap().requires_admin, "{id}");
        }
        assert_eq!(
            windows_tool("task_manager").map(|t| t.id),
            Some("task_manager")
        );
    }

    #[test]
    fn every_windows_tool_resolves_to_an_existing_system_file() {
        let system = system_dir().unwrap();
        let ids: HashSet<&str> = windows_tools().iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), 11);
        let all: HashSet<&str> = windows_tools()
            .iter()
            .chain(fix_tools())
            .map(|t| t.id)
            .collect();
        assert_eq!(all.len(), 15, "ids are unique across both sets");
        for tool in windows_tools().iter().chain(fix_tools()) {
            let mut calls = Vec::new();
            open_windows_tool_with(tool.id, &mut |program, args| {
                calls.push((program.to_path_buf(), args.to_vec()));
                Ok(())
            })
            .unwrap();
            let [(program, args)] = &calls[..] else {
                panic!("{}: {calls:?}", tool.id);
            };
            assert!(
                program.is_absolute() && program.starts_with(&system),
                "{}",
                tool.id
            );
            assert!(program.is_file(), "{} is missing", program.display());
            let name = program
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_lowercase();
            assert!(name != "eventvwr.exe" && name != "perfmon.exe", "{name}");
            assert_eq!(
                args.len(),
                usize::from(tool.snap_in.is_some()),
                "{}",
                tool.id
            );
            for arg in args {
                assert!(
                    arg.starts_with(&system) && arg.is_file(),
                    "{}",
                    arg.display()
                );
            }
            assert!(tool.description.ends_with('.'), "{}", tool.id);
        }
        let json = serde_json::to_value(windows_tools()).unwrap();
        let keys: Vec<&str> = json[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys.len(), 4, "{keys:?}");
    }

    #[test]
    fn windows_tools_keep_the_environment_cairn_was_started_with() {
        let system = system_dir().unwrap();
        for tool in windows_tools().iter().chain(fix_tools()) {
            let mut built = None;
            open_windows_tool_with(tool.id, &mut |program, args| {
                let command = windows_tool_command(program, args)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                built = Some(command);
                Ok(())
            })
            .unwrap();
            let command = built.unwrap();
            assert_eq!(Path::new(command.get_program()), system.join(tool.program));
            let args: Vec<PathBuf> = command.get_args().map(PathBuf::from).collect();
            let expected: Vec<PathBuf> = tool.snap_in.map(|s| system.join(s)).into_iter().collect();
            assert_eq!(args, expected, "{}", tool.id);
            assert_eq!(command.get_current_dir(), Some(system.as_path()));
            let changed: Vec<_> = command.get_envs().collect();
            assert!(
                changed.is_empty(),
                "{}: PATH and the rest are inherited unchanged: {changed:?}",
                tool.id
            );
        }
        assert!(windows_tool_command(Path::new("taskmgr.exe"), &[]).is_err());
    }

    #[test]
    fn unknown_windows_tool_is_refused() {
        let mut called = false;
        let err = open_windows_tool_with("regedit", &mut |_, _| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown Windows tool"), "{err}");
        assert!(!called);

        let err = open_windows_tool_with("services", &mut |_, _| {
            Err(io::Error::from_raw_os_error(ERROR_ELEVATION_REQUIRED))
        })
        .unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        let err = open_windows_tool_with("task_manager", &mut |_, _| {
            Err(io::Error::from_raw_os_error(2))
        })
        .unwrap_err();
        assert!(
            err.to_string().starts_with("could not open Task Manager"),
            "{err}"
        );
    }
}
