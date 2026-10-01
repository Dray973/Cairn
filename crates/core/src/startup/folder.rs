//! Startup folder entries. Shortcuts are resolved with `IShellLinkW`; Internet shortcuts
//! report their URL; any other file starts itself.

use std::io::ErrorKind;
use std::path::Path;

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
};
use windows::Win32::UI::Shell::{FOLDERID_CommonStartup, FOLDERID_Startup, IShellLinkW, ShellLink};

use super::command::{expand_env, quote};
use super::StartupSource;
use crate::win::com::ComApartment;
use crate::win::paths::known_folder;
use crate::win::{from_wide_nul, wide};
use crate::Result;

/// Buffer size, in UTF-16 units, for shortcut targets and arguments.
const LINK_BUFFER: usize = 32_768;

/// One file of a Startup folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FolderItem {
    /// File name, which is also the StartupApproved value name.
    pub name: String,
    /// Shortcut target with arguments, the URL of an Internet shortcut, or the file itself.
    pub command: String,
    /// Executable the item starts; empty when it cannot be determined.
    pub path: String,
}

/// Files of the Startup folder behind `source`; empty for Run-key sources and when the
/// folder does not exist.
pub(super) fn items(source: StartupSource) -> Result<Vec<FolderItem>> {
    let folder = match source {
        StartupSource::UserFolder => FOLDERID_Startup,
        StartupSource::CommonFolder => FOLDERID_CommonStartup,
        _ => return Ok(Vec::new()),
    };
    items_in(&known_folder(&folder)?)
}

/// Files directly inside `dir`, skipping `desktop.ini` and subdirectories.
pub(super) fn items_in(dir: &Path) -> Result<Vec<FolderItem>> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let _com = ComApartment::enter();
    let mut items = Vec::new();
    for entry in read {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let (command, target) = describe(&path);
        items.push(FolderItem {
            name,
            command,
            path: target,
        });
    }
    Ok(items)
}

/// `(command, executable)` of one Startup folder file.
fn describe(path: &Path) -> (String, String) {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("lnk") => match shortcut_target(path) {
            Some((target, args)) => {
                let target = expand_env(&target);
                let mut command = quote(&target);
                if !args.is_empty() {
                    command.push(' ');
                    command.push_str(&args);
                }
                (command, target)
            }
            None => (String::new(), String::new()),
        },
        Some("url") => (
            internet_shortcut_url(path).unwrap_or_default(),
            String::new(),
        ),
        _ => {
            let file = path.display().to_string();
            (quote(&file), file)
        }
    }
}

/// Target path and arguments of a `.lnk` file. `None` when it cannot be loaded or has no
/// file-system target (for example a shortcut to a Store app).
fn shortcut_target(lnk: &Path) -> Option<(String, String)> {
    let file_name = wide(&lnk.to_string_lossy());
    let mut target = vec![0u16; LINK_BUFFER];
    let mut args = vec![0u16; LINK_BUFFER];
    // SAFETY: COM calls on interfaces created here; every buffer outlives the call using it
    // and GetPath accepts a null WIN32_FIND_DATAW pointer.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        persist.Load(PCWSTR(file_name.as_ptr()), STGM_READ).ok()?;
        link.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        if link.GetArguments(&mut args).is_err() {
            args[0] = 0;
        }
    }
    let target = from_wide_nul(&target);
    if target.is_empty() {
        return None;
    }
    Some((target, from_wide_nul(&args).trim().to_string()))
}

/// `URL=` entry of an Internet shortcut (an INI file, ANSI/UTF-8 or UTF-16LE).
fn internet_shortcut_url(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = match bytes.strip_prefix(&[0xFF, 0xFE]) {
        Some(utf16) => {
            let units: Vec<u16> = utf16
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        None => String::from_utf8_lossy(&bytes).into_owned(),
    };
    text.lines().find_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("URL")
            .then(|| value.trim().to_string())
    })
}
