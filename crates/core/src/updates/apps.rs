//! The curated list of apps the Install apps view offers.
//!
//! The list is saved per Windows account as `app_list.json` in Cairn's data folder
//! (`{"version": 1, "apps": [{"id", "name", "category", "source"}]}`); without that file the
//! built-in [`DEFAULT_APPS`] are offered. The file is read without following links and
//! written to a new temporary file that then replaces it, so a link planted in its place is
//! replaced, not written through. Reading changes nothing.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::random_hex;
use super::winget::{valid_package_id, valid_source};
use crate::safety::state_log::data_dir;
use crate::tools::logs::refuse_links;
use crate::win::fs::read_small_regular_file;
use crate::{Error, Result};

/// The heading an app is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppCategory {
    Browsers,
    Chat,
    Gaming,
    Media,
    Productivity,
    Utilities,
    Developer,
}

impl AppCategory {
    /// Every category, in display order.
    pub const ALL: [AppCategory; 7] = [
        AppCategory::Browsers,
        AppCategory::Chat,
        AppCategory::Gaming,
        AppCategory::Media,
        AppCategory::Productivity,
        AppCategory::Utilities,
        AppCategory::Developer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AppCategory::Browsers => "browsers",
            AppCategory::Chat => "chat",
            AppCategory::Gaming => "gaming",
            AppCategory::Media => "media",
            AppCategory::Productivity => "productivity",
            AppCategory::Utilities => "utilities",
            AppCategory::Developer => "developer",
        }
    }

    pub fn parse(text: &str) -> Option<AppCategory> {
        let text = text.trim();
        AppCategory::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(text))
    }

    /// The heading shown above the category's apps.
    pub fn title(self) -> &'static str {
        match self {
            AppCategory::Browsers => "Browsers",
            AppCategory::Chat => "Chat and calls",
            AppCategory::Gaming => "Gaming",
            AppCategory::Media => "Media",
            AppCategory::Productivity => "Productivity",
            AppCategory::Utilities => "Utilities",
            AppCategory::Developer => "Developer tools",
        }
    }
}

fn winget_source() -> String {
    "winget".to_string()
}

/// One app of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub category: AppCategory,
    #[serde(default = "winget_source")]
    pub source: String,
}

/// The list the Install apps view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppList {
    pub apps: Vec<AppEntry>,
    /// The list comes from the account's own file rather than the defaults.
    pub custom: bool,
    /// Where the account's list is (or would be) saved.
    pub path: String,
    pub warnings: Vec<String>,
}

/// The built-in list: winget package id, name shown, category. Every id is in the `winget`
/// source.
pub const DEFAULT_APPS: &[(&str, &str, AppCategory)] = &[
    ("Mozilla.Firefox", "Firefox", AppCategory::Browsers),
    ("Google.Chrome", "Google Chrome", AppCategory::Browsers),
    ("Brave.Brave", "Brave", AppCategory::Browsers),
    ("Vivaldi.Vivaldi", "Vivaldi", AppCategory::Browsers),
    ("Opera.Opera", "Opera", AppCategory::Browsers),
    ("Discord.Discord", "Discord", AppCategory::Chat),
    ("SlackTechnologies.Slack", "Slack", AppCategory::Chat),
    ("Zoom.Zoom", "Zoom", AppCategory::Chat),
    ("Microsoft.Teams", "Microsoft Teams", AppCategory::Chat),
    ("Telegram.TelegramDesktop", "Telegram", AppCategory::Chat),
    ("OpenWhisperSystems.Signal", "Signal", AppCategory::Chat),
    ("Valve.Steam", "Steam", AppCategory::Gaming),
    (
        "EpicGames.EpicGamesLauncher",
        "Epic Games Launcher",
        AppCategory::Gaming,
    ),
    ("GOG.Galaxy", "GOG Galaxy", AppCategory::Gaming),
    ("ElectronicArts.EADesktop", "EA app", AppCategory::Gaming),
    ("Ubisoft.Connect", "Ubisoft Connect", AppCategory::Gaming),
    ("VideoLAN.VLC", "VLC", AppCategory::Media),
    ("OBSProject.OBSStudio", "OBS Studio", AppCategory::Media),
    ("Audacity.Audacity", "Audacity", AppCategory::Media),
    ("HandBrake.HandBrake", "HandBrake", AppCategory::Media),
    (
        "TheDocumentFoundation.LibreOffice",
        "LibreOffice",
        AppCategory::Productivity,
    ),
    (
        "Adobe.Acrobat.Reader.64-bit",
        "Adobe Acrobat Reader",
        AppCategory::Productivity,
    ),
    (
        "SumatraPDF.SumatraPDF",
        "SumatraPDF",
        AppCategory::Productivity,
    ),
    (
        "Notepad++.Notepad++",
        "Notepad++",
        AppCategory::Productivity,
    ),
    (
        "Bitwarden.Bitwarden",
        "Bitwarden",
        AppCategory::Productivity,
    ),
    (
        "KeePassXCTeam.KeePassXC",
        "KeePassXC",
        AppCategory::Productivity,
    ),
    ("7zip.7zip", "7-Zip", AppCategory::Utilities),
    ("voidtools.Everything", "Everything", AppCategory::Utilities),
    ("Microsoft.PowerToys", "PowerToys", AppCategory::Utilities),
    ("ShareX.ShareX", "ShareX", AppCategory::Utilities),
    (
        "Microsoft.VisualStudioCode",
        "Visual Studio Code",
        AppCategory::Developer,
    ),
    ("Git.Git", "Git", AppCategory::Developer),
    ("Python.Python.3.14", "Python 3.14", AppCategory::Developer),
    ("OpenJS.NodeJS.LTS", "Node.js LTS", AppCategory::Developer),
    (
        "JetBrains.Toolbox",
        "JetBrains Toolbox",
        AppCategory::Developer,
    ),
];
/// Most apps a list may hold.
pub const MAX_APPS: usize = 500;
/// Largest list file read.
pub const MAX_LIST_BYTES: u64 = 256 * 1024;
/// Longest app name, in characters.
pub const MAX_NAME_CHARS: usize = 80;

const FILE_NAME: &str = "app_list.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ListFile {
    version: u32,
    apps: Vec<AppEntry>,
}

/// The built-in list.
pub fn default_apps() -> Vec<AppEntry> {
    DEFAULT_APPS
        .iter()
        .map(|&(id, name, category)| AppEntry {
            id: id.to_string(),
            name: name.to_string(),
            category,
            source: winget_source(),
        })
        .collect()
}

/// `<data dir>\app_list.json`.
pub fn list_path() -> PathBuf {
    data_dir().join(FILE_NAME)
}

/// The account's list, or the defaults when it has none or its file cannot be read.
pub fn app_list() -> AppList {
    app_list_at(&list_path())
}

/// Saves `apps` as the account's list, or, with `None`, deletes it so the defaults are used
/// again. Returns the list as it is now.
pub fn save_app_list(apps: Option<&[AppEntry]>) -> Result<AppList> {
    save_app_list_at(&list_path(), apps)
}

/// `apps` checked and tidied for saving: ids are winget package ids of a valid source, names
/// are 1 to 80 characters, at most [`MAX_APPS`] entries and no id twice (ignoring ASCII
/// case). Fails with the first problem, worded for the user.
pub fn validate_apps(apps: &[AppEntry]) -> Result<Vec<AppEntry>> {
    if apps.len() > MAX_APPS {
        return Err(Error::Other(format!(
            "The list can hold at most {MAX_APPS} apps."
        )));
    }
    let mut out: Vec<AppEntry> = Vec::with_capacity(apps.len());
    for app in apps {
        let entry = AppEntry {
            id: app.id.trim().to_string(),
            name: app.name.trim().to_string(),
            category: app.category,
            source: app.source.trim().to_string(),
        };
        if !valid_source(&entry.source) {
            return Err(Error::Other(format!(
                "{:?} isn't a winget source name.",
                entry.source
            )));
        }
        if !valid_package_id(&entry.id, &entry.source) {
            return Err(Error::Other(format!(
                "{:?} isn't a winget package id: use letters, digits and dots, like Publisher.App.",
                entry.id
            )));
        }
        let chars = entry.name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS || entry.name.chars().any(char::is_control) {
            return Err(Error::Other(format!(
                "The name of {} must be 1 to {MAX_NAME_CHARS} characters.",
                entry.id
            )));
        }
        if out.iter().any(|a| a.id.eq_ignore_ascii_case(&entry.id)) {
            return Err(Error::Other(format!("{} is on the list twice.", entry.id)));
        }
        out.push(entry);
    }
    Ok(out)
}

fn defaults_at(path: &Path, warnings: Vec<String>) -> AppList {
    AppList {
        apps: default_apps(),
        custom: false,
        path: path.display().to_string(),
        warnings,
    }
}

pub(crate) fn app_list_at(path: &Path) -> AppList {
    let unreadable = |reason: String| {
        vec![format!(
            "Your app list could not be read, so the default list is shown ({reason})."
        )]
    };
    let bytes = match read_small_regular_file(path, MAX_LIST_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return defaults_at(path, Vec::new()),
        Err(e) => return defaults_at(path, unreadable(e.to_string())),
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let file: ListFile = match serde_json::from_slice(bytes) {
        Ok(file) => file,
        Err(e) => return defaults_at(path, unreadable(e.to_string())),
    };
    if file.version != FORMAT_VERSION {
        return defaults_at(
            path,
            unreadable(format!("format version {} is not supported", file.version)),
        );
    }
    let mut warnings = Vec::new();
    let mut apps: Vec<AppEntry> = Vec::new();
    let mut skipped = 0usize;
    for app in file.apps.into_iter().take(MAX_APPS) {
        match validate_apps(std::slice::from_ref(&app)) {
            Ok(mut valid) if !apps.iter().any(|a| a.id.eq_ignore_ascii_case(&valid[0].id)) => {
                apps.push(valid.remove(0));
            }
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        warnings.push(format!(
            "{skipped} {} of your app list {} not valid and {} left out.",
            if skipped == 1 { "entry" } else { "entries" },
            if skipped == 1 { "was" } else { "were" },
            if skipped == 1 { "is" } else { "are" },
        ));
    }
    AppList {
        apps,
        custom: true,
        path: path.display().to_string(),
        warnings,
    }
}

pub(crate) fn save_app_list_at(path: &Path, apps: Option<&[AppEntry]>) -> Result<AppList> {
    let Some(apps) = apps else {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(defaults_at(path, Vec::new()));
    };
    let apps = validate_apps(apps)?;
    let dir = path
        .parent()
        .ok_or_else(|| Error::Other(format!("{} has no folder", path.display())))?;
    refuse_links(dir)?;
    fs::create_dir_all(dir)?;
    refuse_links(dir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| FILE_NAME.to_string());
    let temp = dir.join(format!("{file_name}.{}.tmp", random_hex(4)));
    let json = serde_json::to_vec_pretty(&ListFile {
        version: FORMAT_VERSION,
        apps: apps.clone(),
    })?;
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(&json)?;
            file.sync_all()
        });
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(AppList {
        apps,
        custom: true,
        path: path.display().to_string(),
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: name.into(),
            category: AppCategory::Utilities,
            source: "winget".into(),
        }
    }

    #[test]
    fn the_defaults_are_valid_and_unique() {
        let apps = default_apps();
        assert_eq!(apps.len(), DEFAULT_APPS.len());
        assert_eq!(validate_apps(&apps).unwrap(), apps);
        for category in AppCategory::ALL {
            assert!(apps.iter().any(|a| a.category == category), "{category:?}");
            assert_eq!(AppCategory::parse(category.as_str()), Some(category));
        }
        assert!(!apps
            .iter()
            .any(|a| a.id.to_ascii_lowercase().contains("spotify")));
    }

    #[test]
    fn a_saved_list_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join(FILE_NAME);
        let missing = app_list_at(&path);
        assert!(!missing.custom);
        assert_eq!(missing.apps, default_apps());
        assert!(missing.warnings.is_empty());

        let apps = vec![
            entry(" Contoso.Editor ", "Contoso Editor"),
            entry("Fabrikam.Chat", "Chat"),
        ];
        let saved = save_app_list_at(&path, Some(&apps)).unwrap();
        assert!(saved.custom);
        assert_eq!(saved.apps[0].id, "Contoso.Editor");
        let read = app_list_at(&path);
        assert_eq!(read, saved);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no temporary file stays: {leftovers:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");

        let reset = save_app_list_at(&path, None).unwrap();
        assert!(!reset.custom);
        assert!(!path.exists());
        assert_eq!(app_list_at(&path).apps, default_apps());
        // Resetting again is fine.
        save_app_list_at(&path, None).unwrap();
    }

    #[test]
    fn invalid_lists_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for apps in [
            vec![entry("-h", "Bad")],
            vec![entry("Contoso", "No dot")],
            vec![entry("Contoso.Editor", "")],
            vec![entry("Contoso.Editor", &"x".repeat(81))],
            vec![entry("Contoso.Editor", "A"), entry("contoso.editor", "B")],
        ] {
            assert!(save_app_list_at(&path, Some(&apps)).is_err(), "{apps:?}");
        }
        let many: Vec<AppEntry> = (0..=MAX_APPS)
            .map(|n| entry(&format!("Contoso.App{n}"), "App"))
            .collect();
        assert!(save_app_list_at(&path, Some(&many)).is_err());
        assert!(!path.exists(), "nothing was written");
    }

    #[test]
    fn malformed_or_oversized_files_fall_back_to_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{not json").unwrap();
        let list = app_list_at(&path);
        assert!(!list.custom);
        assert_eq!(list.apps, default_apps());
        assert_eq!(list.warnings.len(), 1);

        std::fs::write(&path, vec![b' '; (MAX_LIST_BYTES + 1) as usize]).unwrap();
        let list = app_list_at(&path);
        assert_eq!(list.apps, default_apps());
        assert_eq!(list.warnings.len(), 1);

        std::fs::write(&path, br#"{"version": 2, "apps": []}"#).unwrap();
        assert_eq!(app_list_at(&path).warnings.len(), 1);

        std::fs::write(
            &path,
            br#"{"version": 1, "apps": [
                {"id": "Contoso.Editor", "name": "Editor", "category": "media"},
                {"id": "bad id", "name": "Bad", "category": "media"},
                {"id": "contoso.editor", "name": "Twice", "category": "media"}
            ]}"#,
        )
        .unwrap();
        let list = app_list_at(&path);
        assert!(list.custom);
        assert_eq!(list.apps.len(), 1);
        assert_eq!(list.apps[0].source, "winget");
        assert_eq!(
            list.warnings,
            ["2 entries of your app list were not valid and are left out."]
        );
    }

    #[test]
    fn a_link_in_place_of_the_file_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, br#"{"version": 1, "apps": []}"#).unwrap();
        let path = dir.path().join(FILE_NAME);
        // Creating a symbolic link needs Developer Mode or an elevated process.
        if std::os::windows::fs::symlink_file(&target, &path).is_err() {
            return;
        }
        let list = app_list_at(&path);
        assert!(!list.custom);
        assert_eq!(list.warnings.len(), 1);
        save_app_list_at(&path, Some(&[entry("Contoso.Editor", "Editor")])).unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            br#"{"version": 1, "apps": []}"#,
            "the link's target is unchanged"
        );
    }
}
