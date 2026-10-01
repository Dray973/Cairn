//! Executable of a startup command line.
//!
//! Run values are free-form command lines. The executable is the quoted first token when
//! the command starts with a quote. An unquoted command is resolved the way `CreateProcess`
//! resolves an unquoted module name containing spaces: each space- or tab-delimited prefix
//! is tried from shortest to longest. A prefix naming an existing file wins as written,
//! whatever its extension; a prefix naming a directory is skipped; otherwise `<prefix>.exe`
//! wins when it exists. That order is what lets a stray `C:\Program.exe` (or `C:\Program`)
//! run instead of `C:\Program Files\App\app.exe --flag`, so the entry shows the binary that
//! actually starts. Environment variables are expanded first. Bare names such as
//! `rundll32.exe` are looked up in the system directories and on `PATH`.

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;

use crate::win::wide;

/// Executable started by `command`, environment variables expanded; empty if none.
pub(super) fn executable_path(command: &str) -> String {
    let command = command.trim();
    let token = match command.strip_prefix('"') {
        Some(rest) => {
            let inner = rest.split('"').next().unwrap_or(rest);
            expand_env(inner.trim())
        }
        None => unquoted_path(&expand_env(command)),
    };
    if token.is_empty() {
        return String::new();
    }
    if is_bare_name(&token) {
        if let Some(found) = search_path(&token) {
            return found;
        }
    }
    token
}

/// Expands `%VAR%` references; unknown variables stay as written.
pub(super) fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = wide(s);
    let mut buf = vec![0u16; s.len() + 260];
    loop {
        // SAFETY: `src` is NUL-terminated and `buf` is a writable slice of its full length.
        let needed = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) };
        let needed = needed as usize;
        if needed == 0 {
            return s.to_string();
        }
        if needed <= buf.len() {
            // `needed` counts the terminating NUL.
            return String::from_utf16_lossy(&buf[..needed - 1]);
        }
        buf.resize(needed, 0);
    }
}

/// Quotes a path that contains spaces so it reads as a single command-line token.
pub(super) fn quote(path: &str) -> String {
    if path.contains(char::is_whitespace) && !path.starts_with('"') {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Executable of an unquoted, already expanded command line: the shortest space- or
/// tab-delimited prefix that `CreateProcess` would run.
fn unquoted_path(command: &str) -> String {
    let ends = command
        .char_indices()
        .filter(|&(_, c)| is_separator(c))
        .map(|(i, _)| i)
        .chain(std::iter::once(command.len()));

    for end in ends {
        let candidate = command[..end].trim_end_matches(is_separator);
        let path = Path::new(candidate);
        if candidate.is_empty() || !path.is_absolute() || path.is_dir() {
            continue;
        }
        if path.is_file() {
            return candidate.to_string();
        }
        let exe = format!("{candidate}.exe");
        if Path::new(&exe).is_file() {
            return exe;
        }
    }

    // Nothing on disk: end the path after the first ".exe" that closes a token, else at
    // the first separator.
    let lower = command.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(".exe") {
        let end = from + pos + ".exe".len();
        let closes_token = match command[end..].chars().next() {
            Some(c) => is_separator(c),
            None => true,
        };
        if closes_token {
            return command[..end].to_string();
        }
        from = end;
    }
    command
        .split(is_separator)
        .find(|token| !token.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Characters that end a module name in an unquoted command line.
fn is_separator(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// True for a file name without any directory or drive component.
fn is_bare_name(token: &str) -> bool {
    !token.contains(['\\', '/', ':'])
}

/// Finds a bare executable name the way `CreateProcess` does after the application
/// directory: the system directory, the Windows directory, then `PATH`.
fn search_path(name: &str) -> Option<String> {
    let mut names = vec![name.to_string()];
    if Path::new(name).extension().is_none() {
        names.push(format!("{name}.exe"));
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        let root = PathBuf::from(root);
        dirs.push(root.join("System32"));
        dirs.push(root);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| d.is_absolute()));
    }

    dirs.iter()
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|candidate| candidate.is_file())
        .map(|found| found.display().to_string())
}
