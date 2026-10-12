//! The curated list of apps the Install apps view offers.
//!
//! The list is saved per Windows account as `app_list.json` in Cairn's data folder
//! (`{"version": 1, "apps": [{"id", "name", "category", "source"}]}`); without that file the
//! built-in [`DEFAULT_APPS`] are offered. The file is read without following links and
//! written to a new temporary file that then replaces it, so a link planted in its place is
//! replaced, not written through. Reading changes nothing. Each entry is read on its own: an
//! entry this version cannot use, such as one with a category a later version added, is left
//! out and counted in the list's warning, and the other entries are still shown.

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
    Creative,
    Productivity,
    Utilities,
    Security,
    Developer,
}

impl AppCategory {
    /// Every category, in display order.
    pub const ALL: [AppCategory; 9] = [
        AppCategory::Browsers,
        AppCategory::Chat,
        AppCategory::Gaming,
        AppCategory::Media,
        AppCategory::Creative,
        AppCategory::Productivity,
        AppCategory::Utilities,
        AppCategory::Security,
        AppCategory::Developer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AppCategory::Browsers => "browsers",
            AppCategory::Chat => "chat",
            AppCategory::Gaming => "gaming",
            AppCategory::Media => "media",
            AppCategory::Creative => "creative",
            AppCategory::Productivity => "productivity",
            AppCategory::Utilities => "utilities",
            AppCategory::Security => "security",
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
            AppCategory::Creative => "Creative tools",
            AppCategory::Productivity => "Productivity",
            AppCategory::Utilities => "Utilities",
            AppCategory::Security => "Security and privacy",
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
    /// The ids of [`DEFAULT_APPS`] next to an account's own list, so the view can say which
    /// default apps that list lacks; empty when the defaults are shown.
    pub default_ids: Vec<String>,
}

/// The built-in list: winget package id, name shown, category. Every id is in the `winget`
/// source. `docs/app-catalog.md` gives each app's publisher, installer and notes.
pub const DEFAULT_APPS: &[(&str, &str, AppCategory)] = &[
    ("Mozilla.Firefox", "Firefox", AppCategory::Browsers),
    ("Google.Chrome", "Google Chrome", AppCategory::Browsers),
    ("Brave.Brave", "Brave", AppCategory::Browsers),
    ("Vivaldi.Vivaldi", "Vivaldi", AppCategory::Browsers),
    ("LibreWolf.LibreWolf", "LibreWolf", AppCategory::Browsers),
    (
        "DuckDuckGo.DesktopBrowser",
        "DuckDuckGo",
        AppCategory::Browsers,
    ),
    ("Discord.Discord", "Discord", AppCategory::Chat),
    ("SlackTechnologies.Slack", "Slack", AppCategory::Chat),
    ("Zoom.Zoom", "Zoom", AppCategory::Chat),
    ("Microsoft.Teams", "Microsoft Teams", AppCategory::Chat),
    ("Telegram.TelegramDesktop", "Telegram", AppCategory::Chat),
    ("OpenWhisperSystems.Signal", "Signal", AppCategory::Chat),
    ("Cisco.Webex", "Webex", AppCategory::Chat),
    (
        "TeamSpeakSystems.TeamSpeakClient",
        "TeamSpeak 3",
        AppCategory::Chat,
    ),
    ("Element.Element", "Element", AppCategory::Chat),
    ("Valve.Steam", "Steam", AppCategory::Gaming),
    (
        "EpicGames.EpicGamesLauncher",
        "Epic Games Launcher",
        AppCategory::Gaming,
    ),
    ("GOG.Galaxy", "GOG Galaxy", AppCategory::Gaming),
    ("ElectronicArts.EADesktop", "EA app", AppCategory::Gaming),
    ("Ubisoft.Connect", "Ubisoft Connect", AppCategory::Gaming),
    ("Amazon.Games", "Amazon Games", AppCategory::Gaming),
    (
        "RockstarGames.Launcher",
        "Rockstar Games Launcher",
        AppCategory::Gaming,
    ),
    ("Nvidia.GeForceNow", "GeForce NOW", AppCategory::Gaming),
    ("Playnite.Playnite", "Playnite", AppCategory::Gaming),
    ("VideoLAN.VLC", "VLC", AppCategory::Media),
    ("clsid2.mpc-hc", "MPC-HC", AppCategory::Media),
    ("HandBrake.HandBrake", "HandBrake", AppCategory::Media),
    (
        "PeterPawlowski.foobar2000",
        "foobar2000",
        AppCategory::Media,
    ),
    ("XBMCFoundation.Kodi", "Kodi", AppCategory::Media),
    ("Plex.Plex", "Plex", AppCategory::Media),
    ("Deezer.Deezer", "Deezer", AppCategory::Media),
    ("calibre.calibre", "calibre", AppCategory::Media),
    ("Amazon.Kindle", "Kindle", AppCategory::Media),
    ("FlorianHeidenreich.Mp3tag", "Mp3tag", AppCategory::Media),
    ("DuongDieuPhap.ImageGlass", "ImageGlass", AppCategory::Media),
    ("IrfanSkiljan.IrfanView", "IrfanView", AppCategory::Media),
    ("TIDALMusicAS.TIDAL", "TIDAL", AppCategory::Media),
    ("OBSProject.OBSStudio", "OBS Studio", AppCategory::Creative),
    ("Audacity.Audacity", "Audacity", AppCategory::Creative),
    ("GIMP.GIMP.3", "GIMP", AppCategory::Creative),
    ("dotPDN.PaintDotNet", "Paint.NET", AppCategory::Creative),
    ("KDE.Krita", "Krita", AppCategory::Creative),
    ("Inkscape.Inkscape", "Inkscape", AppCategory::Creative),
    (
        "BlenderFoundation.Blender",
        "Blender",
        AppCategory::Creative,
    ),
    ("darktable.darktable", "darktable", AppCategory::Creative),
    ("Meltytech.Shotcut", "Shotcut", AppCategory::Creative),
    ("KDE.Kdenlive", "Kdenlive", AppCategory::Creative),
    ("Canva.Canva", "Canva", AppCategory::Creative),
    ("Canva.Affinity", "Affinity", AppCategory::Creative),
    (
        "Musescore.Musescore",
        "MuseScore Studio",
        AppCategory::Creative,
    ),
    (
        "NickeManarin.ScreenToGif",
        "ScreenToGif",
        AppCategory::Creative,
    ),
    ("Figma.Figma", "Figma", AppCategory::Creative),
    (
        "TheDocumentFoundation.LibreOffice",
        "LibreOffice",
        AppCategory::Productivity,
    ),
    (
        "ONLYOFFICE.DesktopEditors",
        "ONLYOFFICE",
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
        "geeksoftwareGmbH.PDF24Creator",
        "PDF24 Creator",
        AppCategory::Productivity,
    ),
    (
        "Notepad++.Notepad++",
        "Notepad++",
        AppCategory::Productivity,
    ),
    (
        "Mozilla.Thunderbird",
        "Thunderbird",
        AppCategory::Productivity,
    ),
    ("Obsidian.Obsidian", "Obsidian", AppCategory::Productivity),
    ("Notion.Notion", "Notion", AppCategory::Productivity),
    ("Joplin.Joplin", "Joplin", AppCategory::Productivity),
    ("Doist.Todoist", "Todoist", AppCategory::Productivity),
    ("Anki.Anki", "Anki", AppCategory::Productivity),
    ("DigitalScholar.Zotero", "Zotero", AppCategory::Productivity),
    (
        "Google.GoogleDrive",
        "Google Drive",
        AppCategory::Productivity,
    ),
    ("Dropbox.Dropbox", "Dropbox", AppCategory::Productivity),
    (
        "Proton.ProtonDrive",
        "Proton Drive",
        AppCategory::Productivity,
    ),
    ("Mega.MEGASync", "MEGA", AppCategory::Productivity),
    ("Cyanfish.NAPS2", "NAPS2", AppCategory::Productivity),
    (
        "Google.EarthPro",
        "Google Earth Pro",
        AppCategory::Productivity,
    ),
    (
        "Perplexity.Perplexity",
        "Perplexity",
        AppCategory::Productivity,
    ),
    ("Anthropic.Claude", "Claude", AppCategory::Productivity),
    ("7zip.7zip", "7-Zip", AppCategory::Utilities),
    ("M2Team.NanaZip", "NanaZip", AppCategory::Utilities),
    ("voidtools.Everything", "Everything", AppCategory::Utilities),
    ("Microsoft.PowerToys", "PowerToys", AppCategory::Utilities),
    ("ShareX.ShareX", "ShareX", AppCategory::Utilities),
    ("Greenshot.Greenshot", "Greenshot", AppCategory::Utilities),
    (
        "AutoHotkey.AutoHotkey",
        "AutoHotkey",
        AppCategory::Utilities,
    ),
    ("LocalSend.LocalSend", "LocalSend", AppCategory::Utilities),
    ("Google.QuickShare", "Quick Share", AppCategory::Utilities),
    (
        "qBittorrent.qBittorrent",
        "qBittorrent",
        AppCategory::Utilities,
    ),
    ("CPUID.CPU-Z", "CPU-Z", AppCategory::Utilities),
    ("CPUID.HWMonitor", "HWMonitor", AppCategory::Utilities),
    ("TechPowerUp.GPU-Z", "GPU-Z", AppCategory::Utilities),
    ("REALiX.HWiNFO", "HWiNFO", AppCategory::Utilities),
    (
        "CrystalDewWorld.CrystalDiskInfo",
        "CrystalDiskInfo",
        AppCategory::Utilities,
    ),
    (
        "Microsoft.VCRedist.2015+.x64",
        "Visual C++ Redistributable (x64)",
        AppCategory::Utilities,
    ),
    (
        "Microsoft.VCRedist.2015+.x86",
        "Visual C++ Redistributable (x86)",
        AppCategory::Utilities,
    ),
    (
        "Microsoft.DotNet.DesktopRuntime.10",
        ".NET Desktop Runtime 10",
        AppCategory::Utilities,
    ),
    ("Bitwarden.Bitwarden", "Bitwarden", AppCategory::Security),
    (
        "KeePassXCTeam.KeePassXC",
        "KeePassXC",
        AppCategory::Security,
    ),
    ("AgileBits.1Password", "1Password", AppCategory::Security),
    ("Proton.ProtonPass", "Proton Pass", AppCategory::Security),
    ("Proton.ProtonVPN", "Proton VPN", AppCategory::Security),
    (
        "MullvadVPN.MullvadVPN",
        "Mullvad VPN",
        AppCategory::Security,
    ),
    ("WireGuard.WireGuard", "WireGuard", AppCategory::Security),
    (
        "Cryptomator.Cryptomator",
        "Cryptomator",
        AppCategory::Security,
    ),
    (
        "Microsoft.VisualStudioCode",
        "Visual Studio Code",
        AppCategory::Developer,
    ),
    ("Git.Git", "Git", AppCategory::Developer),
    ("GitHub.cli", "GitHub CLI", AppCategory::Developer),
    ("Python.Python.3.14", "Python 3.14", AppCategory::Developer),
    ("OpenJS.NodeJS.LTS", "Node.js LTS", AppCategory::Developer),
    (
        "Microsoft.PowerShell",
        "PowerShell 7",
        AppCategory::Developer,
    ),
    (
        "Microsoft.VisualStudio.Community",
        "Visual Studio Community 2026",
        AppCategory::Developer,
    ),
    (
        "JetBrains.Toolbox",
        "JetBrains Toolbox",
        AppCategory::Developer,
    ),
    ("GoLang.Go", "Go", AppCategory::Developer),
    (
        "EclipseAdoptium.Temurin.25.JDK",
        "Eclipse Temurin JDK 25",
        AppCategory::Developer,
    ),
    (
        "Microsoft.DotNet.SDK.10",
        ".NET SDK 10",
        AppCategory::Developer,
    ),
    ("Anysphere.Cursor", "Cursor", AppCategory::Developer),
    ("ZedIndustries.Zed", "Zed", AppCategory::Developer),
    ("Unity.UnityHub", "Unity Hub", AppCategory::Developer),
    (
        "Google.AndroidStudio",
        "Android Studio",
        AppCategory::Developer,
    ),
    ("WinSCP.WinSCP", "WinSCP", AppCategory::Developer),
    ("PuTTY.PuTTY", "PuTTY", AppCategory::Developer),
    ("Oracle.VirtualBox", "VirtualBox", AppCategory::Developer),
    ("WinMerge.WinMerge", "WinMerge", AppCategory::Developer),
    (
        "GitHub.GitHubDesktop",
        "GitHub Desktop",
        AppCategory::Developer,
    ),
    ("Postman.Postman", "Postman", AppCategory::Developer),
    (
        "Docker.DockerDesktop",
        "Docker Desktop",
        AppCategory::Developer,
    ),
];

/// Apps held out of [`DEFAULT_APPS`] until a supervised install shows that Cairn's install
/// and its result are right for them (`docs/app-catalog.md` says what each needs and what the
/// installs made so far showed). Nothing in Cairn offers them; tests check them like the
/// defaults.
pub const HELD_APPS: &[(&str, &str, AppCategory)] = &[
    ("Roblox.Roblox", "Roblox", AppCategory::Gaming),
    ("Rustlang.Rustup", "Rust (rustup)", AppCategory::Developer),
];

/// Most apps a list may hold.
pub const MAX_APPS: usize = 500;
/// Largest list file read.
pub const MAX_LIST_BYTES: u64 = 256 * 1024;
/// Longest app name, in characters.
pub const MAX_NAME_CHARS: usize = 80;

const FILE_NAME: &str = "app_list.json";
const FORMAT_VERSION: u32 = 1;

/// The list file as it is written.
#[derive(Serialize)]
struct ListFile<'a> {
    version: u32,
    apps: &'a [AppEntry],
}

/// The list file as it is read: each entry stays raw JSON until it is checked on its own.
#[derive(Deserialize)]
struct StoredList {
    version: u32,
    apps: Vec<serde_json::Value>,
}

/// One entry of the list file with its category as text, so an unknown category makes only
/// this entry invalid.
#[derive(Deserialize)]
struct StoredEntry {
    id: String,
    name: String,
    category: String,
    #[serde(default = "winget_source")]
    source: String,
}

/// The entry `value` stands for, or None when it is not an entry this version can use: not
/// an object of text fields, or a category it does not know. The id and name are checked by
/// [`validate_apps`] afterwards.
fn stored_entry(value: serde_json::Value) -> Option<AppEntry> {
    let entry: StoredEntry = serde_json::from_value(value).ok()?;
    Some(AppEntry {
        category: AppCategory::parse(&entry.category)?,
        id: entry.id,
        name: entry.name,
        source: entry.source,
    })
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

/// The ids of the built-in list, in its order.
fn default_ids() -> Vec<String> {
    DEFAULT_APPS
        .iter()
        .map(|(id, _, _)| id.to_string())
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
        default_ids: Vec::new(),
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
    let file: StoredList = match serde_json::from_slice(bytes) {
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
    for value in file.apps.into_iter().take(MAX_APPS) {
        let valid = stored_entry(value)
            .and_then(|app| validate_apps(std::slice::from_ref(&app)).ok())
            .and_then(|mut valid| valid.pop());
        match valid {
            Some(app) if !apps.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id)) => {
                apps.push(app);
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
        default_ids: default_ids(),
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
        apps: &apps,
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
        default_ids: default_ids(),
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

    fn entries(list: &[(&str, &str, AppCategory)]) -> Vec<AppEntry> {
        list.iter()
            .map(|&(id, name, category)| AppEntry {
                id: id.into(),
                name: name.into(),
                category,
                source: "winget".into(),
            })
            .collect()
    }

    /// Publishers none of whose apps the defaults may offer: cleaners and optimizers,
    /// uninstallers that remove leftovers without a journal, driver updaters, other app
    /// installers and updaters, remote-access tools, a VPN that takes over the DNS servers
    /// Cairn manages, a browser with a bundled promotional component, and Spotify, whose
    /// winget installer refuses an administrator app.
    const LEFT_OUT_PUBLISHERS: &[&str] = &[
        "Piriform",
        "Klocman",
        "RevoUninstaller",
        "WiseCleaner",
        "IObit",
        "GlennDelahoy",
        "Devolutions",
        "MartiCliment",
        "BitSum",
        "AnyDesk",
        "TeamViewer",
        "GlavSoft",
        "Parsec",
        "RustDesk",
        "Cloudflare",
        "Opera",
        "Spotify",
    ];
    /// Single apps of publishers whose other apps may be offered, and apps left out for their
    /// installers or for what they install.
    const LEFT_OUT_IDS: &[&str] = &[
        "TechPowerUp.NVCleanstall",
        "Wagnardsoft.DisplayDriverUninstaller",
        "Nilesoft.Shell",
        "StartIsBack.StartAllBack",
        "Fleex255.PolicyPlus",
        "rcmaehl.MSEdgeRedirect",
        "xM4ddy.OFGB",
        "IDRIX.VeraCrypt",
        "Apple.iTunes",
        "Blizzard.BattleNet",
        "Amazon.Music",
    ];

    /// Why `id` must stay out of the built-in lists: its publisher segment or the whole id
    /// is listed above (compared ignoring ASCII case, never as a substring).
    fn left_out(id: &str) -> Option<&'static str> {
        let publisher = id.split('.').next().unwrap_or_default();
        if LEFT_OUT_PUBLISHERS
            .iter()
            .any(|p| p.eq_ignore_ascii_case(publisher))
        {
            return Some("publisher");
        }
        LEFT_OUT_IDS
            .iter()
            .any(|i| i.eq_ignore_ascii_case(id))
            .then_some("app")
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
    }

    #[test]
    fn the_lists_have_their_planned_sizes() {
        assert_eq!(DEFAULT_APPS.len(), 121);
        assert_eq!(HELD_APPS.len(), 2);
        assert_eq!(AppCategory::ALL.len(), 9);
    }

    #[test]
    fn the_defaults_fit_one_batch() {
        assert!(DEFAULT_APPS.len() <= crate::updates::winget::MAX_BATCH_ITEMS);
        assert!(DEFAULT_APPS.len() + HELD_APPS.len() <= MAX_APPS);
    }

    #[test]
    fn the_defaults_leave_out_tools_that_conflict_with_cairn() {
        for (id, _, _) in DEFAULT_APPS.iter().chain(HELD_APPS) {
            assert_eq!(left_out(id), None, "{id}");
        }
        // The rule compares whole publisher segments and whole ids, not substrings.
        assert_eq!(left_out("Opera.OperaGX"), Some("publisher"));
        assert_eq!(left_out("opera.opera"), Some("publisher"));
        assert_eq!(left_out("Spotify.Spotify"), Some("publisher"));
        assert_eq!(left_out("IDRIX.VeraCrypt"), Some("app"));
        assert_eq!(left_out("TechPowerUp.NVCleanstall"), Some("app"));
        assert_eq!(left_out("TechPowerUp.GPU-Z"), None);
        assert_eq!(left_out("Cooperative.Operations"), None);
        assert_eq!(left_out("Contoso.Spotify"), None);
    }

    #[test]
    fn held_apps_are_valid_and_not_offered() {
        let held = entries(HELD_APPS);
        assert_eq!(validate_apps(&held).unwrap(), held);
        for (id, _, _) in HELD_APPS {
            assert!(
                !DEFAULT_APPS
                    .iter()
                    .any(|(default, _, _)| default.eq_ignore_ascii_case(id)),
                "{id} is held and offered"
            );
        }
        // Roblox failed its supervised install from an elevated Cairn (installed machine-wide,
        // the player started with administrator rights); rustup's test needs a PC without
        // Visual Studio's C++ build tools.
        assert_eq!(
            sorted_ids(HELD_APPS),
            ["Roblox.Roblox", "Rustlang.Rustup"].map(String::from)
        );
    }

    /// Apps with a Squirrel installer, which returns before the install finishes, so winget
    /// reports success a few seconds before Windows lists the app.
    const SQUIRREL_APPS: &[&str] = &[
        "Discord.Discord",
        "SlackTechnologies.Slack",
        "Element.Element",
        "TIDALMusicAS.TIDAL",
        "Figma.Figma",
        "Anthropic.Claude",
        "GitHub.GitHubDesktop",
        "Postman.Postman",
    ];

    #[test]
    fn apps_that_passed_their_supervised_install_are_offered() {
        // Element's supervised install stood for the Squirrel apps: Cairn reported "not listed
        // yet" until a later check found it, and the app did not start by itself. Docker
        // Desktop installed and ran a Linux container; the restart its installer needs is
        // reported from Windows' pending-restart flags after the batch.
        for id in SQUIRREL_APPS.iter().chain(&["Docker.DockerDesktop"]) {
            assert!(
                DEFAULT_APPS.iter().any(|(default, _, _)| default == id),
                "{id} is offered"
            );
        }
    }

    #[test]
    fn categories_have_their_keys_and_titles() {
        let shown: Vec<(&str, &str)> = AppCategory::ALL
            .iter()
            .map(|c| (c.as_str(), c.title()))
            .collect();
        assert_eq!(
            shown,
            [
                ("browsers", "Browsers"),
                ("chat", "Chat and calls"),
                ("gaming", "Gaming"),
                ("media", "Media"),
                ("creative", "Creative tools"),
                ("productivity", "Productivity"),
                ("utilities", "Utilities"),
                ("security", "Security and privacy"),
                ("developer", "Developer tools"),
            ]
        );
        for category in AppCategory::ALL {
            let json = serde_json::to_string(&category).unwrap();
            assert_eq!(json, format!("\"{}\"", category.as_str()));
            assert_eq!(
                serde_json::from_str::<AppCategory>(&json).unwrap(),
                category
            );
        }
        assert_eq!(
            AppCategory::parse(" Security "),
            Some(AppCategory::Security)
        );
        assert_eq!(AppCategory::parse("games"), None);
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
        assert!(!text.contains("default_ids"), "{text}");

        let reset = save_app_list_at(&path, None).unwrap();
        assert!(!reset.custom);
        assert!(!path.exists());
        assert_eq!(app_list_at(&path).apps, default_apps());
        // Resetting again is fine.
        save_app_list_at(&path, None).unwrap();
    }

    #[test]
    fn a_custom_list_carries_the_default_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let ids: Vec<String> = DEFAULT_APPS
            .iter()
            .map(|(id, _, _)| id.to_string())
            .collect();
        assert!(app_list_at(&path).default_ids.is_empty(), "the defaults");

        let saved = save_app_list_at(&path, Some(&[entry("Contoso.Editor", "Editor")])).unwrap();
        assert_eq!(saved.default_ids, ids);
        assert_eq!(app_list_at(&path).default_ids, ids);
        let json = serde_json::to_value(&saved).unwrap();
        assert_eq!(
            json["default_ids"].as_array().unwrap().len(),
            DEFAULT_APPS.len()
        );

        std::fs::write(&path, b"{not json").unwrap();
        assert!(
            app_list_at(&path).default_ids.is_empty(),
            "an unreadable list shows the defaults"
        );
        assert!(save_app_list_at(&path, None)
            .unwrap()
            .default_ids
            .is_empty());
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
            br#"{"version": 1, "apps": {"id": "Contoso.Editor"}}"#,
        )
        .unwrap();
        let list = app_list_at(&path);
        assert!(!list.custom, "the apps must be a list");
        assert_eq!(list.warnings.len(), 1);

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
    fn entries_this_version_cannot_use_are_left_out_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            br#"{"version": 1, "apps": [
                {"id": "Contoso.Paint", "name": "Contoso Paint", "category": "creative"},
                {"id": "Contoso.Vault", "name": "Contoso Vault", "category": "Security"},
                {"id": "Fabrikam.Arcade", "name": "Fabrikam Arcade", "category": "arcade"},
                {"id": "Fabrikam.Notes", "name": "Fabrikam Notes"},
                {"id": "Northwind.Chat", "name": 7, "category": "chat"},
                42,
                {"id": "Northwind.Player", "name": "Northwind Player", "category": "media",
                 "source": "winget", "added_by": "a later version"}
            ]}"#,
        )
        .unwrap();
        let list = app_list_at(&path);
        assert!(list.custom, "the list stays the account's own");
        let read: Vec<(&str, AppCategory)> = list
            .apps
            .iter()
            .map(|a| (a.id.as_str(), a.category))
            .collect();
        assert_eq!(
            read,
            [
                ("Contoso.Paint", AppCategory::Creative),
                ("Contoso.Vault", AppCategory::Security),
                ("Northwind.Player", AppCategory::Media),
            ]
        );
        assert_eq!(
            list.warnings,
            ["4 entries of your app list were not valid and are left out."]
        );
        assert_eq!(list.default_ids.len(), DEFAULT_APPS.len());
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

    /// The winget ids in the first column of the table rows under the document heading
    /// `heading`, up to the next heading of that level.
    fn documented_ids(doc: &str, heading: &str) -> Vec<String> {
        let mut ids = Vec::new();
        let mut inside = false;
        for line in doc.lines() {
            if line.starts_with("## ") {
                inside = line.trim_end() == heading;
                continue;
            }
            let Some(rest) = line.strip_prefix("| `").filter(|_| inside) else {
                continue;
            };
            if let Some(end) = rest.find('`') {
                ids.push(rest[..end].to_string());
            }
        }
        ids.sort_by_key(|id| id.to_ascii_lowercase());
        ids
    }

    fn sorted_ids(list: &[(&str, &str, AppCategory)]) -> Vec<String> {
        let mut ids: Vec<String> = list.iter().map(|(id, _, _)| id.to_string()).collect();
        ids.sort_by_key(|id| id.to_ascii_lowercase());
        ids
    }

    #[test]
    fn the_catalog_document_lists_every_default_and_held_app() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/app-catalog.md");
        let doc = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            documented_ids(&doc, "## The default list"),
            sorted_ids(DEFAULT_APPS)
        );
        assert_eq!(documented_ids(&doc, "## Held apps"), sorted_ids(HELD_APPS));
    }
}
