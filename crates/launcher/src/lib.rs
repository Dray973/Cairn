//! Cairn.exe: starts the embedded Python runtime and decides elevation.
//!
//! The install folder holds a private CPython 3.12 (`python312.dll`, `Lib`, `DLLs`, `tcl`),
//! isolated by `python312._pth`, and the app under `app\`. The launcher's manifest asks for
//! no elevation (asInvoker), so the launcher decides:
//!
//! - `--check` and `--self-test` always run here and never prompt, activate a window or show
//!   a message box;
//! - when a Cairn window of this session is open, it is brought to the front (no prompt),
//!   unless this start replaces a process (`--after PID`);
//! - an administrator with a split token gets one UAC prompt for an elevated copy, but only
//!   when the install folder can be changed by administrators alone
//!   ([`optimizer_core::win::acl::install_location_problem`]); a declined or failed prompt
//!   runs the app here without administrator rights, with a note it shows;
//! - otherwise the app runs here, told with `--no-elevate` when the folder is not trusted.
//!
//! Before the runtime loads, DLL searches are limited to the system folders and a DLL's own
//! folder, the variables of [`plan::REMOVED_VARIABLES`] and the Tcl module-path variables
//! ([`plan::is_tcl_module_path`]) are removed, Tcl and Tk are pointed at the install's own
//! script folders and the working folder becomes the install folder. The launcher never
//! touches the journal, the registry or the network.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod plan;
mod python;
mod system;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
};

use plan::{
    app_extras, command_line, elevated_args, failed_note, is_tcl_module_path, missing_files,
    parse_args, plan, python_argv, tcl_variables, Args, Start, Token, EXIT_APP_REPORTED,
    EXIT_MISSING_FILES, EXIT_NO_PATH, EXIT_NO_PYTHON, EXIT_OK, NOTE_DECLINED, REMOVED_VARIABLES,
};
use system::Elevation;

/// Runs the launcher and returns the process exit code.
pub fn run() -> i32 {
    // First, before any other DLL loads: later implicit searches see only the system folders
    // and the folder of the DLL being loaded, never the working folder or PATH.
    // SAFETY: plain call with a valid flag; it only changes this process's search order.
    let _ = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) };
    let args = parse_args(std::env::args_os().skip(1));
    let interactive = !args.diagnostic();
    let Some((exe, root)) = own_location() else {
        if interactive {
            system::show_error("Cairn can't start: it could not find its own folder.");
        }
        return EXIT_NO_PATH;
    };
    let token = system::token().unwrap_or(Token::Standard);

    // A running window comes to the front. One that stopped accepting activation since the
    // check is closing; this start then continues as if none were running.
    let running = interactive && system::window_running();
    if plan(&args, running, token, false) == Start::Activate && system::signal_running_instance() {
        return EXIT_OK;
    }

    let missing = missing_files(&root, &|path: &Path| path.is_file());
    if !missing.is_empty() {
        if interactive {
            system::show_error(&plan::missing_files_text(&root, &missing));
        }
        return EXIT_MISSING_FILES;
    }

    let trusted = interactive && system::location_trusted(&root);
    let mut note = None;
    if plan(&args, false, token, trusted) == Start::Elevate {
        let params = command_line(&elevated_args(&args, std::process::id()));
        match system::start_elevated(&exe, &params, &root) {
            Elevation::Started => return EXIT_OK,
            Elevation::Declined => note = Some(NOTE_DECLINED.to_string()),
            Elevation::Failed(code) => note = Some(failed_note(code)),
        }
    }
    let extra = if interactive {
        app_extras(trusted, note.as_deref())
    } else {
        Vec::new()
    };
    run_here(&exe, &root, &args, &extra)
}

/// The launcher's own path and folder.
fn own_location() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.to_path_buf();
    Some((exe, root))
}

/// Prepares the environment and runs the app in this process.
fn run_here(exe: &Path, root: &Path, args: &Args, extra: &[OsString]) -> i32 {
    let interactive = !args.diagnostic();
    // Single-threaded here, before the runtime starts any thread.
    for name in REMOVED_VARIABLES {
        std::env::remove_var(name);
    }
    remove_tcl_module_paths();
    for (name, value) in tcl_variables(root) {
        std::env::set_var(name, value);
    }
    let _ = std::env::set_current_dir(root);
    let argv = python_argv(exe.as_os_str(), args, extra);
    match python::run(root, &argv) {
        Err(error) => {
            if interactive {
                system::show_error(&plan::no_python_text(&error));
            }
            EXIT_NO_PYTHON
        }
        Ok(code) => {
            if interactive && code != EXIT_OK && code != EXIT_APP_REPORTED {
                system::show_error(&plan::closed_text(code));
            }
            code
        }
    }
}

/// Removes every variable of this process's environment that names a Tcl module path
/// ([`plan::is_tcl_module_path`]), once per entry found; returns the names removed. The C
/// runtime builds the wide environment that Python and Tcl read from this process's
/// environment when it is first used, which is after this call; the app refuses to start
/// when a module-path variable is still set.
fn remove_tcl_module_paths() -> Vec<OsString> {
    // Collected first, so the environment is not changed while it is read.
    let names: Vec<OsString> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| is_tcl_module_path(name))
        .collect();
    for name in &names {
        std::env::remove_var(name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcl_module_paths_leave_the_process_environment() {
        // Names no other test uses; Windows compares variable names without regard to case.
        let planted = ["TCL8.6_TM_PATH", "tcl8_5_tm_path", "TCL9_0_TM_PATH"];
        let kept = "CAIRN_LAUNCHER_TEST_TCL8_6_TM_PATH";
        for name in planted {
            std::env::set_var(name, r"C:\Users\Test\planted");
        }
        std::env::set_var(kept, "kept");
        let removed: Vec<String> = remove_tcl_module_paths()
            .iter()
            .map(|name| name.to_string_lossy().to_ascii_uppercase())
            .collect();
        let left = std::env::var_os(kept);
        std::env::remove_var(kept);
        for name in planted {
            assert!(std::env::var_os(name).is_none(), "{name} is still set");
            assert!(removed.contains(&name.to_ascii_uppercase()), "{removed:?}");
        }
        assert_eq!(left, Some(OsString::from("kept")));
        assert!(remove_tcl_module_paths().is_empty());
    }
}
