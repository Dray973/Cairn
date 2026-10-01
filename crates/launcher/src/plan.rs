//! What Cairn.exe does for a command line, a token and an install folder. Pure: nothing here
//! calls the system, so every rule is covered by unit tests.

use std::ffi::{OsStr, OsString};
use std::iter;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Event a running Cairn window waits on (`Local\Cairn.Activate`, created by the window's
/// single-instance lock); setting it brings that window to the front.
pub const ACTIVATE_EVENT: &str = r"Local\Cairn.Activate";

/// The Python runtime the launcher loads from its own folder.
pub const PYTHON_DLL: &str = "python312.dll";

/// Files that must exist beside Cairn.exe for the runtime and the app to start.
pub const REQUIRED_FILES: &[&str] = &[
    "python312.dll",
    "python3.dll",
    "python312._pth",
    r"Lib\os.py",
    r"DLLs\_tkinter.pyd",
    r"tcl\tcl8.6\init.tcl",
    r"tcl\tk8.6\tk.tcl",
    r"app\optimizer\__main__.py",
    r"app\optimizer\native\optimizer_engine.pyd",
    r"app\optimizer\native\optimizer_telemetry.dll",
];

/// Variables removed before the runtime loads: each would make Python, Tcl or Cairn read
/// code or data from outside the install folder. The Tcl module-path variables, whose names
/// carry a version, are removed too ([`is_tcl_module_path`]).
pub const REMOVED_VARIABLES: &[&str] = &[
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONUSERBASE",
    "PYTHONINSPECT",
    "OPTIMIZER_DATA_DIR",
    "OPTIMIZER_JOURNAL",
    "TCLLIBPATH",
    "TIX_LIBRARY",
];

/// Launcher switch: start without asking for administrator rights. It is not passed on.
pub const NO_ELEVATE: &str = "--no-elevate";
/// App switch the launcher adds when the install folder fails the trust check: the app then
/// offers no relaunch as administrator.
pub const APP_NO_ELEVATE: &str = "--no-elevate";
/// App switch that says why the app runs without administrator rights.
pub const START_NOTE: &str = "--start-note";
/// Start note after the user declined the UAC prompt.
pub const NOTE_DECLINED: &str = "elevation-declined";

/// Everything went as planned (also: another window was activated, or an elevated copy
/// was started).
pub const EXIT_OK: i32 = 0;
/// The launcher could not find its own path.
pub const EXIT_NO_PATH: i32 = 1;
/// Files of the install are missing.
pub const EXIT_MISSING_FILES: i32 = 2;
/// Exit code of the app after it has shown its own error message.
pub const EXIT_APP_REPORTED: i32 = 3;
/// python312.dll could not be loaded or has no `Py_Main`.
pub const EXIT_NO_PYTHON: i32 = 4;

/// Elevation state of the launcher's token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Elevated administrator, or an administrator without User Account Control.
    Full,
    /// Administrator with a split token that has not been elevated.
    Limited,
    /// Standard user.
    Standard,
}

/// The launcher's command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    /// `--check`: print the versions and the natives check, then exit.
    pub check: bool,
    /// `--self-test PATH`: write the release self-test report to PATH, then exit.
    pub self_test: bool,
    /// `--no-elevate` was given.
    pub no_elevate: bool,
    /// `--after PID`: the process this start replaces (a relaunch).
    pub after: Option<u32>,
    /// Every argument but `--no-elevate`, in order, for the app.
    pub forward: Vec<OsString>,
}

impl Args {
    /// `--check` or `--self-test`: a read-only run that never prompts, activates another
    /// window or shows a message box.
    pub fn diagnostic(&self) -> bool {
        self.check || self.self_test
    }
}

/// What the launcher does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Bring the running window to the front and exit.
    Activate,
    /// Start an elevated copy through the UAC prompt and exit.
    Elevate,
    /// Run the app in this process.
    RunHere,
}

/// Reads the launcher's arguments (without the program name). `--no-elevate` is consumed;
/// `--check`, `--self-test` and `--after` (with a space or `=` before the value) are
/// recognised and forwarded with everything else, in order.
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Args {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_str().map(str::to_owned);
        match text.as_deref() {
            Some(NO_ELEVATE) => {
                parsed.no_elevate = true;
                continue;
            }
            Some("--check") => parsed.check = true,
            Some("--self-test") => {
                parsed.self_test = true;
                parsed.forward.push(arg);
                if let Some(value) = args.next() {
                    parsed.forward.push(value);
                }
                continue;
            }
            Some("--after") => {
                parsed.forward.push(arg);
                if let Some(value) = args.next() {
                    parsed.after = value.to_str().and_then(parse_pid);
                    parsed.forward.push(value);
                }
                continue;
            }
            Some(other) => {
                if other.starts_with("--self-test=") {
                    parsed.self_test = true;
                } else if let Some(value) = other.strip_prefix("--after=") {
                    parsed.after = parse_pid(value);
                }
            }
            None => {}
        }
        parsed.forward.push(arg);
    }
    parsed
}

fn parse_pid(text: &str) -> Option<u32> {
    text.parse().ok()
}

/// The decision: a diagnostic run always runs here; otherwise a running window is
/// activated unless this start replaces a process (`--after`); a limited administrator
/// token asks for elevation unless `--no-elevate` was given or the install folder failed
/// the trust check (`trusted`); everything else runs here.
pub fn plan(args: &Args, running: bool, token: Token, trusted: bool) -> Start {
    if args.diagnostic() {
        return Start::RunHere;
    }
    if running && args.after.is_none() {
        return Start::Activate;
    }
    if token == Token::Limited && !args.no_elevate && trusted {
        return Start::Elevate;
    }
    Start::RunHere
}

/// Arguments of the elevated copy: the forwarded arguments without any `--after`, then
/// `--after <pid>` so the copy waits for this process to end.
pub fn elevated_args(args: &Args, pid: u32) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.forward.len() + 2);
    let mut forward = args.forward.iter();
    while let Some(arg) = forward.next() {
        match arg.to_str() {
            Some("--after") => {
                forward.next();
            }
            Some(text) if text.starts_with("--after=") => {}
            _ => out.push(arg.clone()),
        }
    }
    out.push(OsString::from("--after"));
    out.push(OsString::from(pid.to_string()));
    out
}

/// Arguments the launcher adds for the app when it runs here: `--no-elevate` when the
/// install folder is not trusted, and a start note after a declined or failed elevation.
pub fn app_extras(trusted: bool, note: Option<&str>) -> Vec<OsString> {
    let mut extra = Vec::new();
    if !trusted {
        extra.push(OsString::from(APP_NO_ELEVATE));
    }
    if let Some(note) = note {
        extra.push(OsString::from(START_NOTE));
        extra.push(OsString::from(note));
    }
    extra
}

/// Start note after Windows refused the elevated start with error `code`.
pub fn failed_note(code: u32) -> String {
    format!("elevation-failed:{code}")
}

/// The Win32 error code inside an HRESULT of the Win32 facility, else the HRESULT itself.
pub fn error_code(hresult: i32) -> u32 {
    let value = hresult as u32;
    if value & 0xFFFF_0000 == 0x8007_0000 {
        value & 0xFFFF
    } else {
        value
    }
}

/// `Py_Main`'s argv: the launcher's path, then isolated mode (`-I`, which the install's
/// `python312._pth` also forces), no bytecode writing, UTF-8 mode and the app's module,
/// followed by the forwarded arguments and `extra`.
pub fn python_argv(exe: &OsStr, args: &Args, extra: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = [exe, "-I".as_ref(), "-B".as_ref(), "-X".as_ref()]
        .into_iter()
        .map(OsStr::to_os_string)
        .collect();
    argv.extend(["utf8", "-m", "optimizer"].into_iter().map(OsString::from));
    argv.extend(args.forward.iter().cloned());
    argv.extend(extra.iter().cloned());
    argv
}

/// Joins `args` into one command line that `CommandLineToArgvW` splits back into the same
/// arguments: arguments with spaces, tabs or quotes (and empty ones) are quoted, quotes
/// are escaped, and backslashes are doubled where they precede a quote.
pub fn command_line(args: &[OsString]) -> OsString {
    const SPACE: u16 = b' ' as u16;
    const TAB: u16 = b'\t' as u16;
    const NEWLINE: u16 = b'\n' as u16;
    const VTAB: u16 = 0x0B;
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let mut out: Vec<u16> = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push(SPACE);
        }
        let wide: Vec<u16> = arg.encode_wide().collect();
        let plain = !wide.is_empty()
            && !wide
                .iter()
                .any(|&c| matches!(c, SPACE | TAB | NEWLINE | VTAB | QUOTE));
        if plain {
            out.extend(wide);
            continue;
        }
        out.push(QUOTE);
        let mut backslashes = 0usize;
        for &c in &wide {
            if c == BACKSLASH {
                backslashes += 1;
                continue;
            }
            if c == QUOTE {
                out.extend(iter::repeat(BACKSLASH).take(backslashes * 2 + 1));
            } else {
                out.extend(iter::repeat(BACKSLASH).take(backslashes));
            }
            out.push(c);
            backslashes = 0;
        }
        out.extend(iter::repeat(BACKSLASH).take(backslashes * 2));
        out.push(QUOTE);
    }
    OsString::from_wide(&out)
}

/// The [`REQUIRED_FILES`] under `root` for which `exists` is false, in list order.
pub fn missing_files(root: &Path, exists: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    REQUIRED_FILES
        .iter()
        .map(|name| root.join(name))
        .filter(|path| !exists(path))
        .collect()
}

/// True for a variable that adds folders to the head of Tcl's module search path:
/// `TCL<major>.<minor>_TM_PATH` or `TCL<major>_<minor>_TM_PATH`, which Tcl reads, for its own
/// version and every earlier minor version, the first time an interpreter looks for a package.
/// `package require` takes the highest version of a module found on that path, so a folder
/// named there could replace the `msgcat` module Tk loads at every start. Windows and Tcl's
/// `env` lookup ignore the case of variable names, and so does this check.
pub fn is_tcl_module_path(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let upper = name.to_ascii_uppercase();
    let Some(version) = upper
        .strip_prefix("TCL")
        .and_then(|rest| rest.strip_suffix("_TM_PATH"))
    else {
        return false;
    };
    let Some((major, minor)) = version.split_once(['.', '_']) else {
        return false;
    };
    let number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    number(major) && number(minor)
}

/// Variables set before the runtime loads: Tcl and Tk read their scripts from the
/// install's own folders, never from folders a variable of the user's environment names.
pub fn tcl_variables(root: &Path) -> [(&'static str, PathBuf); 2] {
    [
        ("TCL_LIBRARY", root.join("tcl").join("tcl8.6")),
        ("TK_LIBRARY", root.join("tcl").join("tk8.6")),
    ]
}

/// Message shown when files of the install are missing.
pub fn missing_files_text(root: &Path, missing: &[PathBuf]) -> String {
    let names: Vec<String> = missing
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect();
    format!(
        "Cairn can't start because some of its files are missing:\n{}\n\nReinstall Cairn to repair it.",
        names.join("\n")
    )
}

/// Message shown when the Python runtime could not be loaded.
pub fn no_python_text(error: &str) -> String {
    format!("Cairn can't start: {PYTHON_DLL} could not be loaded ({error}). Reinstall Cairn.")
}

/// Message shown when the app ended with an exit code it did not report itself.
pub fn closed_text(code: i32) -> String {
    format!(
        "Cairn closed unexpectedly (exit code {code}). Details are in \
         %LOCALAPPDATA%\\PCOptimizer\\logs\\cairn.log."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parsed(args: &[&str]) -> Args {
        parse_args(os(args))
    }

    #[test]
    fn parse_consumes_no_elevate_and_forwards_the_rest_in_order() {
        let args = parsed(&["--no-elevate", "--start-note", "x", "--check", "extra"]);
        assert!(args.no_elevate);
        assert!(args.check);
        assert!(!args.self_test);
        assert_eq!(args.after, None);
        assert_eq!(args.forward, os(&["--start-note", "x", "--check", "extra"]));
        assert_eq!(parsed(&[]), Args::default());
    }

    #[test]
    fn parse_reads_after_and_self_test_in_both_spellings() {
        let args = parsed(&["--after", "1234", "--self-test", r"C:\x y\s.json"]);
        assert_eq!(args.after, Some(1234));
        assert!(args.self_test);
        assert_eq!(
            args.forward,
            os(&["--after", "1234", "--self-test", r"C:\x y\s.json"])
        );

        let args = parsed(&["--after=77", "--self-test=out.json"]);
        assert_eq!(args.after, Some(77));
        assert!(args.self_test);
        assert_eq!(args.forward, os(&["--after=77", "--self-test=out.json"]));
    }

    #[test]
    fn parse_keeps_a_bad_after_value_for_the_app_to_reject() {
        let args = parsed(&["--after", "x"]);
        assert_eq!(args.after, None);
        assert_eq!(args.forward, os(&["--after", "x"]));
        let args = parsed(&["--after"]);
        assert_eq!(args.after, None);
        assert_eq!(args.forward, os(&["--after"]));
        let args = parsed(&["--after=-5"]);
        assert_eq!(args.after, None);
        // A self-test path that looks like a switch is still its value.
        let args = parsed(&["--self-test", "--no-elevate"]);
        assert!(args.self_test && !args.no_elevate);
        assert_eq!(args.forward, os(&["--self-test", "--no-elevate"]));
    }

    #[test]
    fn plan_truth_table() {
        let tokens = [Token::Full, Token::Limited, Token::Standard];
        for token in tokens {
            for running in [false, true] {
                for diagnostic in [None, Some("--check"), Some("--self-test")] {
                    for after in [false, true] {
                        for no_elevate in [false, true] {
                            for trusted in [false, true] {
                                let mut raw = Vec::new();
                                if let Some(flag) = diagnostic {
                                    raw.push(flag);
                                    if flag == "--self-test" {
                                        raw.push("s.json");
                                    }
                                }
                                if after {
                                    raw.extend(["--after", "42"]);
                                }
                                if no_elevate {
                                    raw.push("--no-elevate");
                                }
                                let args = parsed(&raw);
                                let expected = if diagnostic.is_some() {
                                    Start::RunHere
                                } else if running && !after {
                                    Start::Activate
                                } else if token == Token::Limited && !no_elevate && trusted {
                                    Start::Elevate
                                } else {
                                    Start::RunHere
                                };
                                assert_eq!(
                                    plan(&args, running, token, trusted),
                                    expected,
                                    "{token:?} running={running} {diagnostic:?} after={after} \
                                     no_elevate={no_elevate} trusted={trusted}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn plan_named_cases() {
        let plain = Args::default();
        assert_eq!(plan(&plain, false, Token::Limited, true), Start::Elevate);
        assert_eq!(plan(&plain, false, Token::Limited, false), Start::RunHere);
        assert_eq!(plan(&plain, false, Token::Standard, true), Start::RunHere);
        assert_eq!(plan(&plain, false, Token::Full, true), Start::RunHere);
        assert_eq!(plan(&plain, true, Token::Limited, true), Start::Activate);
        let relaunch = parsed(&["--after", "9"]);
        assert_eq!(plan(&relaunch, true, Token::Full, true), Start::RunHere);
        let check = parsed(&["--check"]);
        assert_eq!(plan(&check, true, Token::Limited, true), Start::RunHere);
    }

    #[test]
    fn elevated_args_replace_any_after_with_this_process() {
        let args = parsed(&["--no-elevate", "--after", "5", "x", "--after=6", "y"]);
        assert_eq!(
            elevated_args(&args, 4321),
            os(&["x", "y", "--after", "4321"])
        );
        assert_eq!(elevated_args(&Args::default(), 7), os(&["--after", "7"]));
    }

    #[test]
    fn app_extras_mark_an_untrusted_folder_and_the_start_note() {
        assert!(app_extras(true, None).is_empty());
        assert_eq!(app_extras(false, None), os(&["--no-elevate"]));
        assert_eq!(
            app_extras(true, Some(NOTE_DECLINED)),
            os(&["--start-note", "elevation-declined"])
        );
        assert_eq!(
            app_extras(false, Some(&failed_note(1223))),
            os(&["--no-elevate", "--start-note", "elevation-failed:1223"])
        );
    }

    #[test]
    fn error_code_unwraps_win32_hresults() {
        assert_eq!(error_code(0x8007_04C7_u32 as i32), 1223);
        assert_eq!(error_code(0x8007_0005_u32 as i32), 5);
        assert_eq!(error_code(0x8000_4005_u32 as i32), 0x8000_4005);
    }

    #[test]
    fn python_argv_is_exact() {
        let args = parsed(&["--no-elevate", "--after", "12"]);
        let extra = app_extras(false, Some(NOTE_DECLINED));
        assert_eq!(
            python_argv(
                OsStr::new(r"C:\Program Files\Cairn\Cairn.exe"),
                &args,
                &extra
            ),
            os(&[
                r"C:\Program Files\Cairn\Cairn.exe",
                "-I",
                "-B",
                "-X",
                "utf8",
                "-m",
                "optimizer",
                "--after",
                "12",
                "--no-elevate",
                "--start-note",
                "elevation-declined",
            ])
        );
    }

    fn split_back(line: &OsStr) -> Vec<OsString> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::UI::Shell::CommandLineToArgvW;

        // The program name is parsed by different rules, so a plain one goes first.
        let full: Vec<u16> = OsStr::new("x.exe ")
            .encode_wide()
            .chain(line.encode_wide())
            .chain(iter::once(0))
            .collect();
        let mut count = 0i32;
        // SAFETY: `full` is NUL-terminated and outlives the call; `count` is a valid out
        // pointer.
        let argv = unsafe { CommandLineToArgvW(PCWSTR(full.as_ptr()), &mut count) };
        assert!(!argv.is_null());
        let mut out = Vec::new();
        for index in 1..count as usize {
            // SAFETY: CommandLineToArgvW returned `count` valid NUL-terminated strings.
            let arg = unsafe { (*argv.add(index)).as_wide() };
            out.push(OsString::from_wide(arg));
        }
        // SAFETY: the array was allocated by CommandLineToArgvW and is freed once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(argv.cast())));
        }
        out
    }

    #[test]
    fn command_line_quoting_round_trips() {
        let cases: &[&[&str]] = &[
            &["--after", "12"],
            &["with space", "tab\there"],
            &[r#"say "hi""#, r#""quoted""#],
            &[r"C:\dir\", r"C:\dir with space\", r"trailing\\"],
            &[r#"back\"quote"#, r#"back\\"quote"#],
            &["", "--x", ""],
            &["line\nbreak", "é ü"],
        ];
        for case in cases {
            let args = os(case);
            let line = command_line(&args);
            assert_eq!(split_back(&line), args, "{line:?}");
        }
        assert_eq!(
            command_line(&os(&["a", "b c", ""])),
            OsString::from(r#"a "b c" """#)
        );
        assert_eq!(command_line(&os(&[r"x\"])), OsString::from(r"x\"));
        assert_eq!(command_line(&os(&[r"x y\"])), OsString::from(r#""x y\\""#));
        assert_eq!(command_line(&[]), OsString::new());
    }

    #[test]
    fn missing_files_lists_what_is_absent() {
        let dir = std::env::temp_dir().join(format!("cairn-launcher-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"tcl\tcl8.6")).unwrap();
        for name in ["python312.dll", r"tcl\tcl8.6\init.tcl"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let missing = missing_files(&dir, &|path| path.is_file());
        std::fs::remove_dir_all(&dir).unwrap();
        let expected: Vec<PathBuf> = REQUIRED_FILES
            .iter()
            .filter(|name| !["python312.dll", r"tcl\tcl8.6\init.tcl"].contains(name))
            .map(|name| dir.join(name))
            .collect();
        assert_eq!(missing, expected);
        assert!(missing_files(&dir, &|_| true).is_empty());
        let text = missing_files_text(&dir, &missing);
        assert!(text.contains(r"Lib\os.py"), "{text}");
        assert!(!text.contains(&dir.display().to_string()), "{text}");
    }

    #[test]
    fn environment_removes_injection_paths_and_pins_tcl_to_the_install() {
        for name in [
            "PYTHONHOME",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PYTHONUSERBASE",
            "PYTHONINSPECT",
            "OPTIMIZER_DATA_DIR",
            "OPTIMIZER_JOURNAL",
            "TCLLIBPATH",
            "TIX_LIBRARY",
        ] {
            assert!(REMOVED_VARIABLES.contains(&name), "{name}");
        }
        assert_eq!(REMOVED_VARIABLES.len(), 9);
        let root = Path::new(r"C:\Program Files\Cairn");
        let set = tcl_variables(root);
        assert_eq!(
            set,
            [
                (
                    "TCL_LIBRARY",
                    PathBuf::from(r"C:\Program Files\Cairn\tcl\tcl8.6")
                ),
                (
                    "TK_LIBRARY",
                    PathBuf::from(r"C:\Program Files\Cairn\tcl\tk8.6")
                ),
            ]
        );
        for (name, value) in &set {
            assert!(!REMOVED_VARIABLES.contains(name), "{name}");
            assert!(value.starts_with(root));
        }
    }

    #[test]
    fn tcl_module_path_variables_are_recognised_in_any_case() {
        for name in [
            "TCL8.6_TM_PATH",
            "TCL8_6_TM_PATH",
            "TCL8.0_TM_PATH",
            "TCL8_5_TM_PATH",
            "tcl8_6_tm_path",
            "Tcl8.6_Tm_Path",
            "TCL9_0_TM_PATH",
            "TCL10.12_TM_PATH",
        ] {
            assert!(is_tcl_module_path(OsStr::new(name)), "{name}");
        }
        for name in [
            "",
            "PATH",
            "TCL_LIBRARY",
            "TK_LIBRARY",
            "TCLLIBPATH",
            "TCL_TM_PATH",
            "TCL8_TM_PATH",
            "TCL86_TM_PATH",
            "TCL8._TM_PATH",
            "TCL_8_6_TM_PATH",
            "TCL8.6.1_TM_PATH",
            "TCL8-6_TM_PATH",
            "TCL8_6_TM_PATHS",
            "XTCL8_6_TM_PATH",
            " TCL8_6_TM_PATH",
            "TCL\u{0668}_\u{0666}_TM_PATH",
        ] {
            assert!(!is_tcl_module_path(OsStr::new(name)), "{name:?}");
        }
        // A name that is not valid UTF-16 is never one of Tcl's.
        let unpaired: Vec<u16> = "TCL8_6_TM_PATH".encode_utf16().chain([0xD800]).collect();
        assert!(!is_tcl_module_path(&OsString::from_wide(&unpaired)));
    }

    #[test]
    fn texts_name_the_app() {
        assert!(closed_text(7).contains("exit code 7"));
        assert!(closed_text(7).starts_with(optimizer_core::APP_NAME));
        assert!(no_python_text("boom").contains("python312.dll could not be loaded (boom)"));
        assert_eq!(ACTIVATE_EVENT, r"Local\Cairn.Activate");
    }
}
