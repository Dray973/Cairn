//! The fixed catalog of tweaks and bloatware packages.
//!
//! Every tweak is a list of primitive actions (a registry value, a service start type, a
//! scheduled task's enabled state or the power scheme). The engine never changes anything
//! that is not described here, and every target appears in at most one tweak, so reverting
//! one tweak never touches another.
//!
//! Policy values under `SOFTWARE\Policies` are read by the Windows components themselves
//! on every edition. Where an edition limits a policy, the description says so.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::requirements::{family_label, version_label};
use crate::win::registry::Hive;
use crate::win::scm::StartType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Privacy,
    Gaming,
    Performance,
    Interface,
    Security,
    Bloatware,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Privacy,
        Category::Gaming,
        Category::Performance,
        Category::Interface,
        Category::Security,
        Category::Bloatware,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Privacy => "privacy",
            Category::Gaming => "gaming",
            Category::Performance => "performance",
            Category::Interface => "interface",
            Category::Security => "security",
            Category::Bloatware => "bloatware",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        match s.trim().to_ascii_lowercase().as_str() {
            "privacy" => Some(Category::Privacy),
            "gaming" => Some(Category::Gaming),
            "performance" => Some(Category::Performance),
            "interface" | "tweaks" => Some(Category::Interface),
            "security" => Some(Category::Security),
            "bloatware" | "appx" => Some(Category::Bloatware),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// No functional loss for a typical user.
    Low,
    /// Disables something some users rely on.
    Medium,
    /// Noticeable functional loss; never selected by default.
    High,
}

/// What the user has to do before a change is fully in effect.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RestartNeed {
    #[default]
    None,
    /// File Explorer has to restart (the dashboard can do this without signing out).
    Explorer,
    SignOut,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegData {
    Dword(u32),
    Sz(&'static str),
    /// A bit field stored as a decimal REG_SZ, such as the accessibility `Flags` values.
    /// Apply reads the current number and clears only the bits in `clear`, keeping every
    /// other bit; the action is applied when none of those bits is set. A missing value
    /// stands for `default`, the number Windows uses when the value is absent.
    FlagsSzClear {
        clear: u32,
        default: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryAction {
    pub hive: Hive,
    pub path: &'static str,
    pub name: &'static str,
    pub data: RegData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceAction {
    pub name: &'static str,
    pub start: StartType,
    /// Also stop the service if it is running.
    pub stop: bool,
}

/// The enabled flag of a registered scheduled task. `path` is the task's full path, such
/// as `\Microsoft\Windows\Autochk\Proxy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledTaskAction {
    pub path: &'static str,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerPlan {
    /// The hidden Ultimate Performance scheme, duplicated under an application-owned GUID
    /// and activated. Falls back to High Performance where it cannot be duplicated.
    UltimatePerformance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Registry(RegistryAction),
    Service(ServiceAction),
    ScheduledTask(ScheduledTaskAction),
    Power(PowerPlan),
}

/// A condition this PC must meet for a tweak to have any effect.
///
/// Field-less on purpose: Requirement is Copy and round-trips through CatalogEntry's JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// Microsoft Office 2016 or later (version 16.0; Click-to-Run or Windows Installer).
    Office,
    /// Microsoft Edge.
    Edge,
    /// A graphics driver that supports hardware-accelerated GPU scheduling.
    GpuScheduling,
    /// The Recall optional component, as Component Based Servicing records it.
    RecallComponent,
    /// Notepad 11.2503.16 or later, the first version with its AI features.
    NotepadAi,
    /// Paint.
    Paint,
    /// Windows' text and image generation features for apps.
    SystemAiModels,
    /// Microsoft 365 Apps for business or enterprise, or a volume-licensed Office: the editions
    /// that honour Office policies (consumer Microsoft 365 and Office Home editions ignore them).
    OfficeBusiness,
}

impl Requirement {
    /// Why a tweak with this requirement is not offered on this PC.
    pub fn missing_text(self) -> &'static str {
        match self {
            Requirement::Office => "Microsoft Office (2016 or later) is not installed.",
            Requirement::OfficeBusiness => "Microsoft 365 Home ignores Office policies.",
            Requirement::Edge => "Microsoft Edge is not installed.",
            Requirement::GpuScheduling => {
                "No graphics driver on this PC supports hardware-accelerated GPU scheduling."
            }
            Requirement::RecallComponent => "Cairn found no sign of Recall on this PC.",
            Requirement::NotepadAi => "Notepad 11.2503.16 or later is not installed.",
            Requirement::Paint => "Paint is not installed.",
            Requirement::SystemAiModels => {
                "This version of Windows has no text and image generation features for apps."
            }
        }
    }

    /// Subject of a failed check, as in "cannot check whether <subject>".
    pub fn subject(self) -> &'static str {
        match self {
            Requirement::Office => "Microsoft Office is installed",
            Requirement::OfficeBusiness => "Office for business or enterprise is installed",
            Requirement::Edge => "Microsoft Edge is installed",
            Requirement::GpuScheduling => "the graphics driver supports GPU scheduling",
            Requirement::RecallComponent => "Recall is installed",
            Requirement::NotepadAi => "Notepad 11.2503.16 or later is installed",
            Requirement::Paint => "Paint is installed",
            Requirement::SystemAiModels => "Windows has text and image generation for apps",
        }
    }
}

/// A Windows build: `CurrentBuildNumber` and the update build revision (`UBR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Build {
    pub number: u32,
    pub revision: u32,
}

impl Build {
    pub const fn new(number: u32, revision: u32) -> Build {
        Build { number, revision }
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.number, self.revision)
    }
}

/// Windows editions whose components use a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Editions {
    /// Every edition, Home included.
    All,
    /// Pro, Pro for Workstations, Pro Education, Enterprise, Education and IoT Enterprise.
    ProOrHigher,
    /// Enterprise, Education and IoT Enterprise.
    EnterpriseOrEducation,
}

/// The Windows builds and editions whose components use a tweak's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsRange {
    /// Lowest build on which the settings work, at most one per servicing family (see
    /// `requirements::servicing_family`), oldest family first. Empty: every Windows 11 build.
    pub from: &'static [Build],
    /// Newest servicing family that still uses the settings; `None`: no upper limit.
    pub through: Option<u32>,
    pub editions: Editions,
}

/// Every Windows 11 build and edition.
pub const ANY_WINDOWS: WindowsRange = WindowsRange {
    from: &[],
    through: None,
    editions: Editions::All,
};

impl WindowsRange {
    /// The range in words, for catalog listings: "Windows 11 24H2 (build 26100.3915) or
    /// later", "Windows 11 22H2 (build 22621.2361) or later through 23H2", with
    /// "; Pro, Enterprise or Education" or "; Enterprise or Education" for a limited set of
    /// editions. `None` for [`ANY_WINDOWS`].
    pub fn describe(&self) -> Option<String> {
        let builds = match (self.from, self.through) {
            ([], None) => None,
            ([], Some(last)) => Some(format!("Windows 11 {} and earlier", through_label(last))),
            (floors, through) => {
                let mut text = format!("Windows 11 {}", floors_text(floors));
                if let Some(last) = through {
                    text.push_str(" through ");
                    text.push_str(&through_label(last));
                }
                Some(text)
            }
        };
        let editions = match self.editions {
            Editions::All => None,
            Editions::ProOrHigher => Some("Pro, Enterprise or Education"),
            Editions::EnterpriseOrEducation => Some("Enterprise or Education"),
        };
        match (builds, editions) {
            (None, None) => None,
            (Some(builds), None) => Some(builds),
            (Some(builds), Some(editions)) => Some(format!("{builds}; {editions}")),
            (None, Some(editions)) => Some(format!("Windows 11 {editions}")),
        }
    }
}

/// "24H2 (build 26100.3915) or later" for one floor; "22H2 (build 22621.4870 or later) or
/// 24H2 (build 26100.3360 or later)" for several.
fn floors_text(floors: &[Build]) -> String {
    let named = |floor: &Build, inner: &str| match version_label(floor.number) {
        Some(label) => format!("{label} (build {floor}{inner})"),
        None => format!("build {floor}{inner}"),
    };
    match floors {
        [floor] => format!("{} or later", named(floor, "")),
        _ => {
            let parts: Vec<String> = floors.iter().map(|f| named(f, " or later")).collect();
            match parts.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => String::new(),
            }
        }
    }
}

/// Label of the newest version in servicing family `family` ("23H2" for 22621).
fn through_label(family: u32) -> String {
    family_label(family).map_or_else(|| format!("build {family}"), str::to_string)
}

/// Groups related tweaks across categories in the Optimize list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Telemetry,
    Ads,
    Activity,
    Ai,
    Search,
    Start,
    Sync,
    Edge,
    Office,
    Security,
    Updates,
    Services,
    Power,
    Visuals,
    Taskbar,
    Explorer,
    Files,
    Input,
    Games,
    Network,
}

impl Topic {
    pub const ALL: [Topic; 20] = [
        Topic::Telemetry,
        Topic::Ads,
        Topic::Activity,
        Topic::Ai,
        Topic::Search,
        Topic::Start,
        Topic::Sync,
        Topic::Edge,
        Topic::Office,
        Topic::Security,
        Topic::Updates,
        Topic::Services,
        Topic::Power,
        Topic::Visuals,
        Topic::Taskbar,
        Topic::Explorer,
        Topic::Files,
        Topic::Input,
        Topic::Games,
        Topic::Network,
    ];

    /// Heading of the topic in the Optimize list.
    pub fn label(self) -> &'static str {
        match self {
            Topic::Telemetry => "Diagnostics and feedback",
            Topic::Ads => "Ads and suggestions",
            Topic::Activity => "Activity and personal data",
            Topic::Ai => "AI features",
            Topic::Search => "Search",
            Topic::Start => "Start, lock screen and notifications",
            Topic::Sync => "Clipboard, sync and phone",
            Topic::Edge => "Microsoft Edge",
            Topic::Office => "Microsoft Office",
            Topic::Security => "Security",
            Topic::Updates => "Windows Update",
            Topic::Services => "Background services",
            Topic::Power => "Power and startup",
            Topic::Visuals => "Look and feel",
            Topic::Taskbar => "Taskbar",
            Topic::Explorer => "File Explorer",
            Topic::Files => "Files and storage",
            Topic::Input => "Mouse and keyboard",
            Topic::Games => "Games",
            Topic::Network => "Network",
        }
    }
}

/// A registry value an earlier version of Cairn set for a tweak and this version no longer
/// sets. Its journal records still undo with the tweak, and applying the tweak restores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Former {
    pub action: RegistryAction,
    /// Why the value is no longer set; shown while an earlier version's record of it is active.
    pub reason: &'static str,
}

#[derive(Debug)]
pub struct Tweak {
    pub id: &'static str,
    pub category: Category,
    /// Groups the tweak with related tweaks of every category in the Optimize list.
    pub topic: Topic,
    pub title: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    /// Selected when the user enables the whole category.
    pub default_on: bool,
    pub restart: RestartNeed,
    /// What this PC needs for the tweak to have any effect; without it the tweak is not
    /// offered.
    pub requires: Option<Requirement>,
    /// Windows builds and editions that use the tweak's settings.
    pub windows: WindowsRange,
    /// Why the tweak is no longer offered. A retired tweak is never applied or recommended;
    /// its journal records still undo.
    pub retired: Option<&'static str>,
    /// What Undo cannot bring back, shown before the tweak is applied.
    pub undo_limit: Option<&'static str>,
    pub actions: &'static [Action],
    /// Values earlier versions of Cairn set for this tweak and this version no longer sets.
    pub former: &'static [Former],
}

/// Tweaks earlier versions recommended. Turning their category's mode off still restores
/// them.
pub(crate) const FORMERLY_RECOMMENDED: &[&str] = &[
    "privacy.recall",
    "privacy.cortana",
    "privacy.activity_history",
    "privacy.web_search",
    "privacy.feedback_prompts",
    "privacy.typing_data",
    "privacy.online_speech",
    "privacy.ceip",
    "privacy.office_telemetry",
    "gaming.background_recording",
    "gaming.multimedia_scheduling",
    "gaming.sticky_keys",
    "performance.startup_delay",
    "interface.widgets",
];

/// A Store package considered bloatware. `name` is the package Name (not the full name);
/// a trailing `*` makes it a prefix match, e.g. `king.com.*`.
#[derive(Debug)]
pub struct BloatPackage {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    pub default_on: bool,
}

/// A DWORD registry value.
pub(crate) const fn reg_dword(
    hive: Hive,
    path: &'static str,
    name: &'static str,
    value: u32,
) -> RegistryAction {
    RegistryAction {
        hive,
        path,
        name,
        data: RegData::Dword(value),
    }
}

const fn dword(hive: Hive, path: &'static str, name: &'static str, value: u32) -> Action {
    Action::Registry(reg_dword(hive, path, name, value))
}

const fn sz(hive: Hive, path: &'static str, name: &'static str, value: &'static str) -> Action {
    Action::Registry(RegistryAction {
        hive,
        path,
        name,
        data: RegData::Sz(value),
    })
}

const fn flags_sz_clear(
    hive: Hive,
    path: &'static str,
    name: &'static str,
    clear: u32,
    default: u32,
) -> Action {
    Action::Registry(RegistryAction {
        hive,
        path,
        name,
        data: RegData::FlagsSzClear { clear, default },
    })
}

const fn service(name: &'static str, start: StartType, stop: bool) -> Action {
    Action::Service(ServiceAction { name, start, stop })
}

const fn disable_task(path: &'static str) -> Action {
    Action::ScheduledTask(ScheduledTaskAction {
        path,
        enabled: false,
    })
}

const HKLM: Hive = Hive::LocalMachine;
const HKCU: Hive = Hive::CurrentUser;

const DATA_COLLECTION: &str = r"SOFTWARE\Policies\Microsoft\Windows\DataCollection";
const POLICY_SYSTEM: &str = r"SOFTWARE\Policies\Microsoft\Windows\System";
const CONTENT_DELIVERY: &str = r"Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager";
const MMCSS_PROFILE: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile";
const MMCSS_GAMES: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks\Games";
const EXPLORER_ADVANCED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const INPUT_PERSONALIZATION: &str = r"Software\Microsoft\InputPersonalization";
const EDGE_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Edge";
const OFFICE_TELEMETRY_POLICY: &str = r"Software\Policies\Microsoft\office\common\clienttelemetry";
const OFFICE_PRIVACY_POLICY: &str = r"Software\Policies\Microsoft\office\16.0\common\privacy";
const OFFICE_FEEDBACK_POLICY: &str = r"Software\Policies\Microsoft\office\16.0\common\feedback";
pub(crate) const GRAPHICS_DRIVERS: &str = r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers";
/// Pointer settings of the signed-in user; Windows reads them at sign-in.
pub(crate) const MOUSE_KEY: &str = r"Control Panel\Mouse";
const WINDOWS_AI_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsAI";
const WINDOWS_AI_POLICY_USER: &str = r"Software\Policies\Microsoft\Windows\WindowsAI";
const COPILOT_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsCopilot";
const COPILOT_POLICY_USER: &str = r"Software\Policies\Microsoft\Windows\WindowsCopilot";
const WINDOWS_SEARCH_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\Windows Search";
/// The switches of Settings › Privacy & security › Search, per user.
const SEARCH_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\SearchSettings";
const APP_PRIVACY_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\AppPrivacy";
const NOTEPAD_POLICY: &str = r"SOFTWARE\Policies\WindowsNotepad";
const TEXT_INPUT_POLICY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\TextInput";
/// Settings › Privacy & security › Diagnostics & feedback › 'Improve inking and typing' writes
/// both of these per-user values.
const INKING_TYPING: &str = r"Software\Microsoft\input\TIPC";
const INKING_TYPING_SETTING: &str =
    r"Software\Microsoft\Windows\CurrentVersion\CPSS\Store\ImproveInkingAndTyping";
const DEVICE_METADATA_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\Device Metadata";
const EXPLORER_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\Explorer";
const EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer";
const USER_PROFILE_ENGAGEMENT: &str =
    r"Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement";
/// Settings › System › Notifications › 'Show notifications on the lock screen' writes a value
/// in each of these keys.
const NOTIFICATION_SETTINGS: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Notifications\Settings";
const PUSH_NOTIFICATIONS: &str = r"Software\Microsoft\Windows\CurrentVersion\PushNotifications";
const LANGUAGE_PROFILE: &str = r"Control Panel\International\User Profile";
const FILE_SYSTEM: &str = r"SYSTEM\CurrentControlSet\Control\FileSystem";
/// Desktop icons hidden in the default view, by namespace CLSID.
const HIDDEN_DESKTOP_ICONS: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
/// Per-user overrides of the File Explorer Home and Gallery namespace registrations.
const EXPLORER_HOME_CLSID: &str = r"Software\Classes\CLSID\{f874310e-b6b7-47dc-bc84-b9e6b38f5903}";
const EXPLORER_GALLERY_CLSID: &str =
    r"Software\Classes\CLSID\{e88865ea-0e1c-4e20-9aa6-edcd0212c87c}";
/// Why a 0.2 tweak whose effect could not be observed is retired.
const NO_OBSERVABLE_EFFECT: &str = "Cairn found no way to observe an effect of this setting on \
                                    Windows 11, so it no longer offers it.";
/// Remote Assistance's own setting (System Properties writes it). Shared with the checkup's
/// Remote Assistance check, so the check and the tweak cannot drift apart.
pub(crate) const REMOTE_ASSISTANCE_SETTING: &str =
    r"SYSTEM\CurrentControlSet\Control\Remote Assistance";
/// Policies of Remote Desktop Services, which also hold the Remote Assistance policy.
pub(crate) const TERMINAL_SERVICES_POLICY: &str =
    r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services";
/// The tweak that turns off Remote Assistance invitations: the fix the checkup offers.
pub(crate) const REMOTE_ASSISTANCE_TWEAK: &str = "security.remote_assistance";
/// `HOTKEYACTIVE` in the StickyKeys, FilterKeys (Keyboard Response) and ToggleKeys flags:
/// the keyboard shortcut that turns the feature on and opens its prompt.
const HOTKEYACTIVE: u32 = 0x4;

pub static TWEAKS: &[Tweak] = &[
    // ───────────────────────────── Privacy ─────────────────────────────
    Tweak {
        id: "privacy.diagnostic_data",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Limit diagnostic data",
        description:
            "Sets the AllowTelemetry policy to its lowest value. Enterprise and Education \
                      stop sending diagnostic data; Home and Pro treat the value as 'Required', \
                      the minimum those editions allow. While this is applied, Settings › \
                      Privacy & security › Diagnostics & feedback also shows 'Improve inking and \
                      typing' off, with a note that the diagnostic data setting prevents sharing \
                      inking and typing data.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, DATA_COLLECTION, "AllowTelemetry", 0)],
        former: &[],
    },
    Tweak {
        id: "privacy.diagtrack_service",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off the telemetry service",
        description: "Stops and disables DiagTrack (Connected User Experiences and Telemetry), \
                      the service that uploads diagnostic data.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[service("DiagTrack", StartType::Disabled, true)],
        former: &[],
    },
    Tweak {
        id: "privacy.activity_history",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Turn off Activity History",
        description: "Sets the policies that stop Windows from saving your activity history (the \
                      apps, files and websites you use) on this PC. Clipboard history (Win+V) \
                      keeps working.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(NO_OBSERVABLE_EFFECT),
        undo_limit: None,
        actions: &[
            dword(HKLM, POLICY_SYSTEM, "EnableActivityFeed", 0),
            dword(HKLM, POLICY_SYSTEM, "PublishUserActivities", 0),
            dword(HKLM, POLICY_SYSTEM, "UploadUserActivities", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.phone_link",
        category: Category::Privacy,
        topic: Topic::Sync,
        title: "Stop linking phones to this PC",
        description: "This PC can no longer be linked to a phone in Phone Link. Microsoft says it \
                      also leaves the device list of phones already linked to it and stops \
                      Continue on PC. This is a policy, so Windows Settings will say some settings \
                      are managed by your organization.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: Some(
            "Undo allows linking again, but a phone that was linked has to be linked again in \
             Phone Link.",
        ),
        actions: &[dword(HKLM, POLICY_SYSTEM, "EnableMmx", 0)],
        former: &[],
    },
    Tweak {
        id: "privacy.cortana",
        category: Category::Privacy,
        topic: Topic::Search,
        title: "Disable Cortana",
        description: "Sets the Allow Cortana policy to off. Microsoft retired the Cortana app in \
                      2023, so on Windows 11 this policy no longer turns anything off.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some("Cairn found no effect of this policy on Windows 11."),
        undo_limit: None,
        actions: &[dword(HKLM, WINDOWS_SEARCH_POLICY, "AllowCortana", 0)],
        former: &[],
    },
    Tweak {
        id: "privacy.advertising_id",
        category: Category::Privacy,
        topic: Topic::Ads,
        title: "Disable the advertising ID",
        description: "Stops apps from using a per-user advertising identifier for targeted ads. \
                      The 'Advertising ID' switch in Settings › Privacy & security › \
                      Recommendations & offers (Windows 11 26H2) is greyed out while this is \
                      applied.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\AdvertisingInfo",
            "DisabledByGroupPolicy",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.tailored_experiences",
        category: Category::Privacy,
        topic: Topic::Ads,
        title: "Disable tailored experiences",
        description: "Stops Microsoft from using diagnostic data to personalise tips, ads and \
                      recommendations. In Windows 11 26H2 this is the 'Personalized offers' \
                      switch in Settings › Privacy & security › Recommendations & offers, which \
                      is greyed out while this is applied.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Policies\Microsoft\Windows\CloudContent",
            "DisableTailoredExperiencesWithDiagnosticData",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.suggested_content",
        category: Category::Privacy,
        topic: Topic::Ads,
        title: "Stop suggested apps and ads",
        description: "Turns off silent installation of promoted apps (such as Candy Crush), Start \
                      menu suggestions, tips and suggested content in Settings.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, CONTENT_DELIVERY, "SilentInstalledAppsEnabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SystemPaneSuggestionsEnabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338388Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338389Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338393Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-353694Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-353696Enabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.web_search",
        category: Category::Privacy,
        topic: Topic::Search,
        title: "Remove web results from Start search",
        description: "Sets the policy that turns off search suggestions in the File Explorer \
                      search box.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(NO_OBSERVABLE_EFFECT),
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Policies\Microsoft\Windows\Explorer",
            "DisableSearchBoxSuggestions",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.search_history",
        category: Category::Privacy,
        topic: Topic::Search,
        title: "Turn off search history on this PC",
        description: "Windows stops saving what you search for on this PC, which it otherwise \
                      uses to rank later results. Searches saved earlier stay until you choose \
                      'Clear device search history' in Settings › Privacy & security › Search. \
                      Applies to your Windows account only.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            SEARCH_SETTINGS,
            "IsDeviceSearchHistoryEnabled",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.search_cloud",
        category: Category::Privacy,
        topic: Topic::Search,
        title: "Turn off cloud content in search",
        description: "Windows search stops including results from your Microsoft account \
                      (OneDrive, Outlook and other Microsoft services) and from a work or school \
                      account. Files on this PC are still found. Microsoft says Bing can \
                      personalize your web searches while the Microsoft account option is on. \
                      This is a policy, so Windows Settings will say some settings are managed by \
                      your organization. Microsoft documents this setting for Windows 11 Pro and \
                      higher; on Home, Cairn could only confirm that Windows Settings shows it as \
                      managed.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, SEARCH_SETTINGS, "IsMSACloudSearchEnabled", 0),
            dword(HKCU, SEARCH_SETTINGS, "IsAADCloudSearchEnabled", 0),
            dword(HKLM, WINDOWS_SEARCH_POLICY, "AllowCloudSearch", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.feedback_prompts",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Stop feedback prompts",
        description: "Windows no longer asks for feedback or shows feedback notifications.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                r"Software\Microsoft\Siuf\Rules",
                "NumberOfSIUFInPeriod",
                0,
            ),
            dword(HKLM, DATA_COLLECTION, "DoNotShowFeedbackNotifications", 1),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.feedback_tasks",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off the feedback survey tasks",
        description: "Turns off the two Feedback tasks (DmClient) that download feedback surveys \
                      and scenario data for Windows feedback prompts.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            disable_task(r"\Microsoft\Windows\Feedback\Siuf\DmClient"),
            disable_task(r"\Microsoft\Windows\Feedback\Siuf\DmClientOnScenarioDownload"),
        ],
        former: &[],
    },
    // ───────────────────────────── Gaming ─────────────────────────────
    Tweak {
        id: "gaming.background_recording",
        category: Category::Gaming,
        topic: Topic::Games,
        title: "Turn off background game recording",
        description: "Sets the Game DVR policy and settings that turn off game recording. \
                      Microsoft documents that Windows Game Recording isn't allowed while the \
                      policy is set, so Xbox Game Bar can no longer record gameplay, in the \
                      background or with Win+Alt+R. On a PC without the Xbox Game Bar app there \
                      is nothing to record with, so nothing visible changes.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, r"System\GameConfigStore", "GameDVR_Enabled", 0),
            dword(
                HKCU,
                r"Software\Microsoft\Windows\CurrentVersion\GameDVR",
                "AppCaptureEnabled",
                0,
            ),
            dword(
                HKLM,
                r"SOFTWARE\Policies\Microsoft\Windows\GameDVR",
                "AllowGameDVR",
                0,
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "gaming.game_mode",
        category: Category::Gaming,
        topic: Topic::Games,
        title: "Turn on Game Mode",
        description: "Lets Windows prioritise the foreground game and hold back Windows Update \
                      installs and notifications while you play.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                r"Software\Microsoft\GameBar",
                "AutoGameModeEnabled",
                1,
            ),
            dword(HKCU, r"Software\Microsoft\GameBar", "AllowAutoGameMode", 1),
        ],
        former: &[],
    },
    Tweak {
        id: "gaming.multimedia_scheduling",
        category: Category::Gaming,
        topic: Topic::Games,
        title: "Prioritise games in multimedia scheduling",
        description: "Reserves less CPU time for background multimedia work and raises the \
                      scheduling priority of the Games task. The effect is small and depends on \
                      the game.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(NO_OBSERVABLE_EFFECT),
        undo_limit: None,
        actions: &[
            dword(HKLM, MMCSS_PROFILE, "SystemResponsiveness", 10),
            dword(HKLM, MMCSS_GAMES, "GPU Priority", 8),
            dword(HKLM, MMCSS_GAMES, "Priority", 6),
            sz(HKLM, MMCSS_GAMES, "Scheduling Category", "High"),
            sz(HKLM, MMCSS_GAMES, "SFIO Priority", "High"),
        ],
        former: &[],
    },
    Tweak {
        id: "gaming.mouse_acceleration",
        category: Category::Gaming,
        topic: Topic::Input,
        title: "Turn off mouse acceleration",
        description: "Turns off 'Enhance pointer precision' so the pointer moves the same \
                      distance for the same hand movement. Changes how the mouse feels on the \
                      desktop. Takes effect at once.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            sz(HKCU, MOUSE_KEY, "MouseSpeed", "0"),
            sz(HKCU, MOUSE_KEY, "MouseThreshold1", "0"),
            sz(HKCU, MOUSE_KEY, "MouseThreshold2", "0"),
        ],
        former: &[],
    },
    // ───────────────────────────── Performance ─────────────────────────────
    Tweak {
        id: "performance.power_plan",
        category: Category::Performance,
        topic: Topic::Power,
        title: "Use the Ultimate Performance power plan",
        description: "Keeps the CPU at high clocks and disables most power saving. Raises power \
                      use, heat and fan noise; not recommended on battery.",
        risk: Risk::Medium,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[Action::Power(PowerPlan::UltimatePerformance)],
        former: &[],
    },
    Tweak {
        id: "performance.sysmain",
        category: Category::Performance,
        topic: Topic::Services,
        title: "Set SysMain (Superfetch) to manual",
        description: "Stops SysMain, which preloads frequently used apps into memory. On SSDs \
                      the benefit is small and it causes background disk and CPU activity.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[service("SysMain", StartType::Manual, true)],
        former: &[],
    },
    Tweak {
        id: "performance.maps_broker",
        category: Category::Performance,
        topic: Topic::Services,
        title: "Set Downloaded Maps Manager to manual",
        description: "Stops the service that updates offline maps in the background. Windows \
                      starts this service when an app or Settings needs offline maps.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[service("MapsBroker", StartType::Manual, true)],
        former: &[],
    },
    Tweak {
        id: "performance.link_tracking",
        category: Category::Performance,
        topic: Topic::Services,
        title: "Set Distributed Link Tracking to manual",
        description: "Stops the service that repairs shortcuts to files moved between NTFS \
                      volumes on a network domain. Rarely needed on home PCs.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[service("TrkWks", StartType::Manual, true)],
        former: &[],
    },
    Tweak {
        id: "performance.startup_delay",
        category: Category::Performance,
        topic: Topic::Power,
        title: "Remove the startup app delay",
        description: "Starts apps in the Startup folder and Run keys right after sign-in instead \
                      of after a delay. Sign-in can feel busier on slow disks.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(
            "Cairn measured no earlier start of startup programs on Windows 11, so it no longer \
             offers this.",
        ),
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Serialize",
            "StartupDelayInMSec",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "performance.background_apps",
        category: Category::Performance,
        topic: Topic::Services,
        title: "Stop Store apps running in the background",
        description: "Store apps can no longer run in the background. Notifications and syncing \
                      from apps such as Phone Link, Mail and Teams may stop until you open them.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, APP_PRIVACY_POLICY, "LetAppsRunInBackground", 2)],
        former: &[],
    },
    Tweak {
        id: "performance.search_indexer",
        category: Category::Performance,
        topic: Topic::Services,
        title: "Set Windows Search indexing to manual",
        description: "Stops the search indexer and its background disk activity. Start menu, \
                      File Explorer and Outlook search become slower and less complete.",
        risk: Risk::High,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[service("WSearch", StartType::Manual, true)],
        former: &[],
    },
    Tweak {
        id: "performance.edge_background",
        category: Category::Performance,
        topic: Topic::Edge,
        title: "Stop Edge running in the background",
        description: "Turns off Edge Startup Boost and background mode, so Edge no longer keeps \
                      processes running after you close it or preloads at sign-in. These are \
                      Edge policies, so while this is applied Edge shows 'Managed by your \
                      organization' in its menu and settings; undoing it removes the message \
                      unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "StartupBoostEnabled", 0),
            dword(HKLM, EDGE_POLICY, "BackgroundModeEnabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "performance.delivery_optimization",
        category: Category::Performance,
        topic: Topic::Updates,
        title: "Stop sharing updates with other PCs",
        description: "Windows Update downloads only from Microsoft and no longer uploads \
                      update files to other PCs, which frees upload bandwidth.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
            "DODownloadMode",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "performance.menu_delay",
        category: Category::Performance,
        topic: Topic::Visuals,
        title: "Faster menus",
        description: "Shortens the delay before menus and submenus open from 400 ms to 100 ms.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[sz(HKCU, r"Control Panel\Desktop", "MenuShowDelay", "100")],
        former: &[],
    },
    Tweak {
        id: "performance.transparency",
        category: Category::Performance,
        topic: Topic::Visuals,
        title: "Turn off transparency effects",
        description: "Replaces the blurred, see-through taskbar and Start menu with solid \
                      colours, which saves a little GPU work.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, PERSONALIZE, "EnableTransparency", 0)],
        former: &[],
    },
    Tweak {
        id: "performance.fast_startup",
        category: Category::Performance,
        topic: Topic::Power,
        title: "Turn off Fast Startup",
        description: "Shut down fully instead of hibernating the kernel. Boot takes a few \
                      seconds longer, but driver and update problems clear on every shutdown.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Power",
            "HiberbootEnabled",
            0,
        )],
        former: &[],
    },
    // ───────────────────────────── Privacy (continued) ─────────────────────────────
    Tweak {
        id: "privacy.recall",
        category: Category::Privacy,
        topic: Topic::Ai,
        title: "Turn off Recall snapshots",
        description: "Sets the policy 'Turn off saving snapshots for use with Recall' for this \
                      PC and your account, so Recall no longer saves snapshots of your screen. \
                      Windows deletes the snapshots Recall already saved when the policy takes \
                      effect. Recall exists only on Copilot+ PCs.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: Some(Requirement::RecallComponent),
        windows: WindowsRange {
            from: &[Build::new(26100, 3915)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: Some(
            "Undo removes the policy, so you can turn Recall snapshots on again in Settings, but \
             it cannot bring back the snapshots Windows deleted.",
        ),
        actions: &[
            dword(HKLM, WINDOWS_AI_POLICY, "DisableAIDataAnalysis", 1),
            dword(HKCU, WINDOWS_AI_POLICY_USER, "DisableAIDataAnalysis", 1),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.copilot",
        category: Category::Privacy,
        topic: Topic::Ai,
        title: "Hide the legacy Copilot sidebar (23H2 and earlier)",
        description: "Sets the Turn off Windows Copilot policy, which hides the Copilot sidebar \
                      and its taskbar button in Windows 11 22H2 and 23H2. Microsoft documents \
                      that it also keeps an upgrade from those versions from installing the \
                      Copilot app. Windows 11 24H2 and later ignore it: Copilot is an app there, \
                      so remove the Copilot app under Apps to turn it off.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: WindowsRange {
            from: &[Build::new(22621, 2361)],
            through: Some(22621),
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, COPILOT_POLICY_USER, "TurnOffWindowsCopilot", 1)],
        former: &[Former {
            action: reg_dword(HKLM, COPILOT_POLICY, "TurnOffWindowsCopilot", 1),
            reason: "Cairn 0.2 also set a machine-wide value that this user policy does not use, \
                     so Cairn no longer sets it.",
        }],
    },
    Tweak {
        id: "privacy.notepad_ai",
        category: Category::Privacy,
        topic: Topic::Ai,
        title: "Turn off AI features in Notepad",
        description: "Turns off the AI features in Notepad (Write, Rewrite and Summarize with \
                      Copilot) for everyone on this PC. Notepad keeps working as a text editor. \
                      Restarting Notepad removes Rewrite and Summarize; the Copilot button and \
                      Write go after Windows restarts.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: Some(Requirement::NotepadAi),
        windows: WindowsRange {
            from: &[Build::new(22621, 0)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, NOTEPAD_POLICY, "DisableAIFeatures", 1)],
        former: &[],
    },
    Tweak {
        id: "privacy.text_image_generation",
        category: Category::Privacy,
        topic: Topic::Ai,
        title: "Stop apps using Windows' text and image generation",
        description: "Apps can no longer use the text and image generation features built into \
                      Windows. Apps that are open pick this up when they restart. Rated Medium \
                      like the background apps tweak: while this is applied you cannot allow \
                      single apps. This is a policy, so Windows Settings will say some settings \
                      are managed by your organization. Microsoft documents this setting for \
                      Windows 11 Pro and higher; on Home, Cairn could only confirm that Windows \
                      Settings shows it as managed.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::SystemAiModels),
        windows: WindowsRange {
            from: &[Build::new(26100, 0)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            APP_PRIVACY_POLICY,
            "LetAppsAccessSystemAIModels",
            2,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.app_launch_tracking",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Stop tracking app launches",
        description: "Windows no longer records which apps you start. The Start menu's \
                      most-used list stops updating.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "Start_TrackProgs", 0)],
        former: &[],
    },
    Tweak {
        id: "privacy.typing_data",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Stop collecting typing and inking data",
        description: "Turns off the collection of typed text, handwriting and contacts used to \
                      personalise suggestions.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                INPUT_PERSONALIZATION,
                "RestrictImplicitTextCollection",
                1,
            ),
            dword(
                HKCU,
                INPUT_PERSONALIZATION,
                "RestrictImplicitInkCollection",
                1,
            ),
            dword(
                HKCU,
                r"Software\Microsoft\InputPersonalization\TrainedDataStore",
                "HarvestContacts",
                0,
            ),
            dword(
                HKCU,
                r"Software\Microsoft\Personalization\Settings",
                "AcceptedPrivacyPolicy",
                0,
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.inking_typing",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Stop sending inking and typing data",
        description: "Windows stops sending inking and typing data to Microsoft to improve \
                      language recognition and suggestions. This is a policy, so Windows Settings \
                      will say some settings are managed by your organization. Microsoft documents \
                      this setting for Windows 11 Pro and higher; on Home, Cairn could only \
                      confirm that Windows Settings shows it as managed.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, TEXT_INPUT_POLICY, "AllowLinguisticDataCollection", 0),
            dword(HKCU, INKING_TYPING, "Enabled", 0),
            dword(HKCU, INKING_TYPING_SETTING, "Value", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.language_list",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Don't share your language list with websites",
        description: "Windows stops giving websites and apps your list of preferred languages, \
                      which they use to show content in your languages. Some websites may then \
                      use their default language. Applies to your Windows account only.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, LANGUAGE_PROFILE, "HttpAcceptLanguageOptOut", 1)],
        former: &[],
    },
    Tweak {
        id: "privacy.online_speech",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Turn off online speech recognition",
        description: "Voice input is no longer sent to Microsoft. Voice typing (Win+H) and \
                      other cloud dictation stop working until this is undone; voice access \
                      runs on the device and keeps working.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Speech_OneCore\Settings\OnlineSpeechPrivacy",
            "HasAccepted",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.error_reporting",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off Windows Error Reporting",
        description: "Crash and hang reports are no longer sent to Microsoft.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Microsoft\Windows\Windows Error Reporting",
            "Disabled",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.error_report_task",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off the error report upload task",
        description: "Turns off the task that sends queued crash and hang reports to Microsoft. \
                      Reports already queued stay on this PC.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[disable_task(
            r"\Microsoft\Windows\Windows Error Reporting\QueueReporting",
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.ceip",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Leave the Customer Experience Improvement Program",
        description: "Stops the usage statistics collected by the Customer Experience \
                      Improvement Program.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(NO_OBSERVABLE_EFFECT),
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\SQMClient\Windows",
            "CEIPEnable",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.ceip_tasks",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off the Customer Experience Improvement Program tasks",
        description: "Turns off the scheduled tasks that collect usage, USB, disk and check-disk \
                      statistics for the Customer Experience Improvement Program. Tasks this \
                      version of Windows does not have are skipped. A run already in progress \
                      finishes; later runs no longer start.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            disable_task(
                r"\Microsoft\Windows\Customer Experience Improvement Program\Consolidator",
            ),
            disable_task(r"\Microsoft\Windows\Customer Experience Improvement Program\UsbCeip"),
            disable_task(
                r"\Microsoft\Windows\Customer Experience Improvement Program\KernelCeipTask",
            ),
            disable_task(r"\Microsoft\Windows\Autochk\Proxy"),
            disable_task(
                r"\Microsoft\Windows\DiskDiagnostic\Microsoft-Windows-DiskDiagnosticDataCollector",
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.lock_screen_ads",
        category: Category::Privacy,
        topic: Topic::Ads,
        title: "Remove lock screen tips and ads",
        description: "Turns off the fun facts, tips and promotions shown on a Picture or \
                      Slideshow lock screen. It has no effect while the lock screen uses Windows \
                      Spotlight, which shows its own tips; switch the lock screen to Picture or \
                      Slideshow in Settings to remove those too.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                CONTENT_DELIVERY,
                "RotatingLockScreenOverlayEnabled",
                0,
            ),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338387Enabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.start_suggestions",
        category: Category::Privacy,
        topic: Topic::Start,
        title: "Hide tips and app promotions in Start",
        description: "Start stops showing recommendations for tips, shortcuts, new apps and more \
                      in its Recommended section. Your recent files and recently added apps are \
                      not affected. This is the 'Show recommendations for tips, shortcuts, new \
                      apps, and more' switch in Settings › Personalization › Start.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: WindowsRange {
            from: &[Build::new(26100, 0)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            EXPLORER_ADVANCED,
            "Start_IrisRecommendations",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.setup_reminders",
        category: Category::Privacy,
        topic: Topic::Start,
        title: "Stop the finish-setup screens",
        description: "Clears 'Suggest ways to get the most out of Windows and finish setting up \
                      this device' in Settings › System › Notifications › Additional settings. \
                      The full-screen offers to finish setting up this PC (Windows Hello, phone, \
                      OneDrive, Microsoft 365) stop appearing after updates.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            USER_PROFILE_ENGAGEMENT,
            "ScoobeSystemSettingEnabled",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.lock_screen_notifications",
        category: Category::Privacy,
        topic: Topic::Start,
        title: "Hide notifications on the lock screen",
        description: "Turns off 'Show notifications on the lock screen' in Settings › System › \
                      Notifications. Notifications still arrive and wait in the notification \
                      centre after you sign in. 'Show reminders and incoming VoIP calls on the \
                      lock screen' is a separate setting and is not changed.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                NOTIFICATION_SETTINGS,
                "NOC_GLOBAL_SETTING_ALLOW_TOASTS_ABOVE_LOCK",
                0,
            ),
            dword(HKCU, PUSH_NOTIFICATIONS, "LockScreenToastEnabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.explorer_cloud_files",
        category: Category::Privacy,
        topic: Topic::Explorer,
        title: "Hide account-based files in File Explorer",
        description: "Clears 'Include account-based insights, recent, favorite, and recommended \
                      files' in File Explorer's Folder Options (General, Privacy). Microsoft says \
                      File Explorer's Home page then lists files from this PC only, not documents \
                      from OneDrive, Office.com or SharePoint suggested from your account's \
                      activity.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER, "ShowCloudFilesInQuickAccess", 0)],
        former: &[],
    },
    Tweak {
        id: "privacy.location",
        category: Category::Privacy,
        topic: Topic::Activity,
        title: "Turn off location services",
        description: "No app or Windows feature can use your location. Find my device, Maps \
                      and local weather lose location.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\LocationAndSensors",
            "DisableLocation",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.compatibility_telemetry",
        category: Category::Privacy,
        topic: Topic::Telemetry,
        title: "Turn off compatibility telemetry (Compatibility Appraiser)",
        description: "Turns off the Compatibility Appraiser tasks (CompatTelRunner.exe), which \
                      inventory installed programs and devices and send the results to \
                      Microsoft. Windows Update uses those results to hold back feature updates \
                      known to cause problems on similar PCs, so with the tasks off that \
                      information can be out of date. Windows Setup still runs its own \
                      compatibility check while it upgrades. It also turns off MareBackup, which \
                      records your installed desktop apps for Windows Backup, so a backup made \
                      while it is off may leave out desktop apps or recent installs.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            disable_task(
                r"\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser",
            ),
            disable_task(
                r"\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser Exp",
            ),
            disable_task(r"\Microsoft\Windows\Application Experience\ProgramDataUpdater"),
            disable_task(r"\Microsoft\Windows\Application Experience\MareBackup"),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.office_telemetry",
        category: Category::Privacy,
        topic: Topic::Office,
        title: "Stop Office diagnostic data, feedback and surveys",
        description: "Sets the Office policies that stop Word, Excel, Outlook and the other \
                      Microsoft 365 apps from sending diagnostic data, and turns off Send \
                      Feedback and in-app surveys. Service data that Office needs to stay \
                      licensed and up to date is still sent. The Office apps pick this up the \
                      next time they start.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::OfficeBusiness),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, OFFICE_TELEMETRY_POLICY, "SendTelemetry", 3),
            dword(HKCU, OFFICE_FEEDBACK_POLICY, "Enabled", 0),
            dword(HKCU, OFFICE_FEEDBACK_POLICY, "SurveyEnabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.office_connected_experiences",
        category: Category::Privacy,
        topic: Topic::Office,
        title: "Turn off Office's optional connected experiences",
        description: "Turns off the extras Office offers through other Microsoft online \
                      services, such as 3D Maps in Excel and inserting online pictures from \
                      Bing. Saving to OneDrive, co-authoring and basic spelling keep working. \
                      The Office apps pick this up the next time they start.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::OfficeBusiness),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            OFFICE_PRIVACY_POLICY,
            "ControllerConnectedServicesEnabled",
            2,
        )],
        former: &[],
    },
    Tweak {
        id: "privacy.office_cloud_content",
        category: Category::Privacy,
        topic: Topic::Office,
        title: "Stop Office sending documents to online services",
        description: "Turns off the Office features that send your document content to \
                      Microsoft for analysis, such as PowerPoint Designer, Translator, dictation \
                      and Editor's online suggestions, and the ones that download online \
                      content, such as online templates, stock images and icons. Some Copilot \
                      features in the Office apps may stop working as well. Saving to OneDrive \
                      and co-authoring keep working. The Office apps pick this up the next time \
                      they start.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::OfficeBusiness),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, OFFICE_PRIVACY_POLICY, "UserContentDisabled", 2),
            dword(HKCU, OFFICE_PRIVACY_POLICY, "DownloadContentDisabled", 2),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.edge_telemetry",
        category: Category::Privacy,
        topic: Topic::Edge,
        title: "Stop Edge diagnostic data and ad personalization",
        description: "Turns off Edge's diagnostic data, including crash reports (Microsoft does \
                      not recommend this, because crashes are no longer reported), and stops \
                      Edge sending your browsing history to personalise ads, search and news. \
                      The diagnostic data setting applies after Edge restarts. These are Edge \
                      policies, so while this is applied Edge shows 'Managed by your \
                      organization' in its menu and settings; undoing it removes the message \
                      unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "DiagnosticData", 0),
            dword(HKLM, EDGE_POLICY, "PersonalizationReportingEnabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.edge_shopping",
        category: Category::Privacy,
        topic: Topic::Edge,
        title: "Turn off Edge shopping, tips and promotions",
        description: "Turns off price comparison and express checkout on shopping sites, Edge's \
                      feature recommendations, and the promotional backgrounds and tips from \
                      Microsoft services. On a PC managed by an organization, Edge may ignore \
                      this in profiles signed in with a personal Microsoft account. These are \
                      Edge policies, so while this is applied Edge shows 'Managed by your \
                      organization'; undoing it removes the message unless other Edge policies \
                      are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "EdgeShoppingAssistantEnabled", 0),
            dword(HKLM, EDGE_POLICY, "ShowRecommendationsEnabled", 0),
            dword(
                HKLM,
                EDGE_POLICY,
                "SpotlightExperiencesAndRecommendationsEnabled",
                0,
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "privacy.edge_feedback",
        category: Category::Privacy,
        topic: Topic::Edge,
        title: "Turn off Edge feedback and surveys",
        description: "Turns off Edge's feedback feature, so Send feedback (Alt+Shift+I) is no \
                      longer available. Microsoft says Edge also uses this feature for in-browser \
                      surveys. Applies after Edge restarts. This is an Edge policy, so while it is \
                      applied Edge shows 'Managed by your organization'; undoing it removes the \
                      message unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, EDGE_POLICY, "UserFeedbackAllowed", 0)],
        former: &[],
    },
    // ───────────────────────────── Gaming (continued) ─────────────────────────────
    Tweak {
        id: "gaming.sticky_keys",
        category: Category::Gaming,
        topic: Topic::Input,
        title: "Turn off the Sticky, Filter and Toggle Keys shortcuts",
        description: "Pressing Shift five times, holding right Shift for eight seconds or \
                      holding Num Lock for five seconds no longer interrupts a game with the \
                      Sticky, Filter or Toggle Keys prompt. Only the shortcuts change: the \
                      features stay on or off as they are, with their other settings.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            flags_sz_clear(
                HKCU,
                r"Control Panel\Accessibility\StickyKeys",
                "Flags",
                HOTKEYACTIVE,
                510,
            ),
            flags_sz_clear(
                HKCU,
                r"Control Panel\Accessibility\Keyboard Response",
                "Flags",
                HOTKEYACTIVE,
                126,
            ),
            flags_sz_clear(
                HKCU,
                r"Control Panel\Accessibility\ToggleKeys",
                "Flags",
                HOTKEYACTIVE,
                62,
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "gaming.gpu_scheduling",
        category: Category::Gaming,
        topic: Topic::Games,
        title: "Turn on hardware-accelerated GPU scheduling",
        description: "Lets the GPU schedule its own work instead of Windows doing it on the \
                      CPU, which can lower latency. Offered only when the graphics driver \
                      supports it; on PCs with two GPUs it applies to those that do. Windows \
                      already turns it on for many recent GPUs without storing the setting; the \
                      note under this item says what the GPU uses now. Some capture software \
                      and older games work better with it off. Takes effect after Windows \
                      restarts.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: Some(Requirement::GpuScheduling),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, GRAPHICS_DRIVERS, "HwSchMode", 2)],
        former: &[],
    },
    Tweak {
        id: "gaming.network_throttling",
        category: Category::Gaming,
        topic: Topic::Games,
        title: "Turn off network throttling for multimedia",
        description: "Removes the packet-rate limit Windows applies while media plays. Can \
                      reduce latency spikes when streaming or playing music during a game; \
                      little effect otherwise.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(NO_OBSERVABLE_EFFECT),
        undo_limit: None,
        actions: &[dword(
            HKLM,
            MMCSS_PROFILE,
            "NetworkThrottlingIndex",
            0xFFFF_FFFF,
        )],
        former: &[],
    },
    // ───────────────────────────── Interface ─────────────────────────────
    Tweak {
        id: "interface.file_extensions",
        category: Category::Interface,
        topic: Topic::Explorer,
        title: "Show file extensions",
        description: "File Explorer shows extensions such as .exe and .pdf, which makes \
                      disguised files easier to spot.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "HideFileExt", 0)],
        former: &[],
    },
    Tweak {
        id: "interface.hidden_files",
        category: Category::Interface,
        topic: Topic::Explorer,
        title: "Show hidden files",
        description: "File Explorer shows hidden files and folders such as AppData.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "Hidden", 1)],
        former: &[],
    },
    Tweak {
        id: "interface.classic_context_menu",
        category: Category::Interface,
        topic: Topic::Explorer,
        title: "Classic right-click menu",
        description: "Right-click shows the full menu straight away instead of the compact \
                      Windows 11 menu with 'Show more options'.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[sz(
            HKCU,
            r"Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}\InprocServer32",
            "",
            "",
        )],
        former: &[],
    },
    Tweak {
        id: "interface.widgets",
        category: Category::Interface,
        topic: Topic::Taskbar,
        title: "Remove Widgets",
        description: "Removes the Widgets board and the news and weather button from the \
                      taskbar.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: Some(
            "Windows 11 no longer lets programs set this policy, so Cairn no longer offers it.",
        ),
        undo_limit: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Dsh",
            "AllowNewsAndInterests",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "interface.end_task",
        category: Category::Interface,
        topic: Topic::Taskbar,
        title: "Add End task to the taskbar menu",
        description: "Right-clicking an app on the taskbar offers End task, like Task Manager.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced\TaskbarDeveloperSettings",
            "TaskbarEndTask",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "interface.taskbar_left",
        category: Category::Interface,
        topic: Topic::Taskbar,
        title: "Align the taskbar to the left",
        description: "Moves the Start button and taskbar icons to the left edge, as in \
                      Windows 10.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "TaskbarAl", 0)],
        former: &[],
    },
    Tweak {
        id: "interface.task_view_button",
        category: Category::Interface,
        topic: Topic::Taskbar,
        title: "Hide the Task View button",
        description: "Removes the Task View button from the taskbar. Win+Tab still works.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "ShowTaskViewButton", 0)],
        former: &[],
    },
    Tweak {
        id: "interface.search_icon",
        category: Category::Interface,
        topic: Topic::Taskbar,
        title: "Show search as an icon",
        description: "Replaces the wide taskbar search box with a search icon.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Search",
            "SearchboxTaskbarMode",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "interface.open_this_pc",
        category: Category::Interface,
        topic: Topic::Explorer,
        title: "Open File Explorer to This PC",
        description: "File Explorer opens on This PC (drives and folders) instead of Home.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "LaunchTo", 1)],
        former: &[],
    },
    Tweak {
        id: "interface.dark_mode",
        category: Category::Interface,
        topic: Topic::Visuals,
        title: "Dark mode",
        description: "Switches Windows and apps that follow the system theme to dark mode.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKCU, PERSONALIZE, "AppsUseLightTheme", 0),
            dword(HKCU, PERSONALIZE, "SystemUsesLightTheme", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "interface.explorer_sync_ads",
        category: Category::Interface,
        topic: Topic::Ads,
        title: "Hide sync app notices in File Explorer",
        description: "Clears 'Show sync provider notifications' in File Explorer's Folder \
                      Options (View). File Explorer stops showing the notices that OneDrive and \
                      other sync apps place at the top of its windows. Syncing keeps working, and \
                      OneDrive's own notifications are not affected.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            EXPLORER_ADVANCED,
            "ShowSyncProviderNotifications",
            0,
        )],
        former: &[],
    },
    Tweak {
        id: "interface.explorer_home_gallery",
        category: Category::Interface,
        topic: Topic::Explorer,
        title: "Remove Home and Gallery from File Explorer",
        description: "Removes Home and Gallery from the left pane of File Explorer. Your files \
                      and pictures are not touched, and pinned folders stay in the pane. File \
                      Explorer still opens on Home unless 'Open File Explorer to This PC' is also \
                      applied.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: WindowsRange {
            from: &[Build::new(26100, 0)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[
            dword(
                HKCU,
                EXPLORER_HOME_CLSID,
                "System.IsPinnedToNameSpaceTree",
                0,
            ),
            dword(
                HKCU,
                EXPLORER_GALLERY_CLSID,
                "System.IsPinnedToNameSpaceTree",
                0,
            ),
        ],
        former: &[],
    },
    Tweak {
        id: "interface.explorer_ai_actions",
        category: Category::Interface,
        topic: Topic::Ai,
        title: "Hide AI actions in File Explorer",
        description: "Removes the AI actions entry (such as visual search with Bing, blur \
                      background and erase objects) from File Explorer's right-click menu for \
                      everyone on this PC. The apps that provide these actions keep working.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: WindowsRange {
            from: &[Build::new(26100, 0)],
            through: None,
            editions: Editions::All,
        },
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, EXPLORER_POLICY, "HideAIActionsMenu", 1)],
        former: &[],
    },
    Tweak {
        id: "interface.spotlight_icon",
        category: Category::Interface,
        topic: Topic::Visuals,
        title: "Hide the 'Learn about this picture' icon",
        description: "Hides the 'Learn about this picture' icon that Windows puts on the desktop \
                      while the background is Windows spotlight. The pictures keep changing; only \
                      the icon and its menu (next picture, like or dislike, learn more) go away. \
                      It has no effect with any other background.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKCU,
            HIDDEN_DESKTOP_ICONS,
            "{2cc5ca98-6485-489a-920e-b3e88a6ccce3}",
            1,
        )],
        former: &[],
    },
    Tweak {
        id: "interface.long_paths",
        category: Category::Interface,
        topic: Topic::Files,
        title: "Allow long file paths",
        description: "Turns on Windows' support for paths longer than 260 characters. Only apps \
                      that declare themselves long-path aware, such as Python and many developer \
                      tools, use it; other apps, File Explorer included, keep the old limit and \
                      may not open such files. Restart Windows so every app sees the change.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, FILE_SYSTEM, "LongPathsEnabled", 1)],
        former: &[],
    },
    Tweak {
        id: "interface.edge_sidebar",
        category: Category::Interface,
        topic: Topic::Edge,
        title: "Hide the Edge sidebar",
        description: "Sets the Edge policy that removed the sidebar on the right of the Edge \
                      window. On a PC managed by an organization, Edge may ignore this in \
                      profiles signed in with a personal Microsoft account. This is an Edge \
                      policy, so while it is applied Edge shows 'Managed by your organization'; \
                      undoing it removes the message unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: Some(
            "Microsoft retired the Edge sidebar app list in 2026; this policy no longer changes \
             anything Cairn can observe.",
        ),
        undo_limit: None,
        actions: &[dword(HKLM, EDGE_POLICY, "HubsSidebarEnabled", 0)],
        former: &[],
    },
    Tweak {
        id: "interface.edge_default_browser_prompts",
        category: Category::Interface,
        topic: Topic::Edge,
        title: "Stop Edge asking to be the default browser",
        description: "Sets the Edge policies that turn off Edge's default browser prompts. \
                      Microsoft documents that Edge then stops checking whether it is your \
                      default browser and no longer asks you to make it the default browser, the \
                      default PDF reader or Bing the default search engine. You can still choose \
                      default apps in Settings › Apps › Default apps. Applies after Edge restarts. \
                      On a PC managed by an organization, Edge may ignore this in profiles signed \
                      in with a personal Microsoft account. These are Edge policies, so while \
                      this is applied Edge shows 'Managed by your organization'; undoing it \
                      removes the message unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "DefaultBrowserSettingEnabled", 0),
            dword(
                HKLM,
                EDGE_POLICY,
                "DefaultBrowserSettingsCampaignEnabled",
                0,
            ),
            dword(HKLM, EDGE_POLICY, "ShowPDFDefaultRecommendationsEnabled", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "interface.edge_first_run",
        category: Category::Interface,
        topic: Topic::Edge,
        title: "Skip Edge's first-run setup",
        description: "Edge skips its first-run setup pages and splash screen for new profiles \
                      and new installs and uses its defaults instead: the new tab page shows \
                      Microsoft news, and Edge asks at startup whether to turn on sync. Profiles \
                      you already set up do not change. Applies after Edge restarts. This is an \
                      Edge policy, so while it is applied Edge shows 'Managed by your \
                      organization'; undoing it removes the message unless other Edge policies \
                      are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(HKLM, EDGE_POLICY, "HideFirstRunExperience", 1)],
        former: &[],
    },
    Tweak {
        id: "interface.edge_new_tab_page",
        category: Category::Interface,
        topic: Topic::Edge,
        title: "Remove news and suggested sites from Edge's new tab page",
        description: "Edge's new tab page no longer shows Microsoft content such as news, and \
                      Edge's default site tiles are hidden; sites you added yourself stay. On a \
                      PC managed by an organization, Edge may ignore this in profiles signed in \
                      with a personal Microsoft account. These are Edge policies, so while this is \
                      applied Edge shows 'Managed by your organization'; undoing it removes the \
                      message unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "NewTabPageContentEnabled", 0),
            dword(HKLM, EDGE_POLICY, "NewTabPageHideDefaultTopSites", 1),
        ],
        former: &[],
    },
    Tweak {
        id: "interface.edge_rewards",
        category: Category::Interface,
        topic: Topic::Edge,
        title: "Hide Microsoft Rewards in Edge",
        description: "Removes the Adobe Acrobat subscription button from Edge's PDF viewer. \
                      Microsoft documents that the same policies also hide Microsoft Rewards in \
                      Edge and turn off its setting, and remove the Edge Insider promotion from \
                      the About page. This only changes what Edge shows. Applies after Edge \
                      restarts. On a PC managed by an organization, Edge may ignore this in \
                      profiles signed in with a personal Microsoft account. These are Edge \
                      policies, so while this is applied Edge shows 'Managed by your \
                      organization'; undoing it removes the message unless other Edge policies \
                      are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, EDGE_POLICY, "ShowMicrosoftRewards", 0),
            dword(HKLM, EDGE_POLICY, "ShowAcrobatSubscriptionButton", 0),
            dword(HKLM, EDGE_POLICY, "MicrosoftEdgeInsiderPromotionEnabled", 0),
        ],
        former: &[],
    },
    // ───────────────────────────── Security ─────────────────────────────
    Tweak {
        id: "security.remote_assistance",
        category: Category::Security,
        topic: Topic::Security,
        title: "Turn off Remote Assistance",
        description: "Turns off Remote Assistance, so nobody can view or control this PC through \
                      an invitation, which scammers often ask people to send. Quick Assist is a \
                      separate app and is not affected. The Remote Assistance option in System \
                      Properties is turned off while this is applied. The checkbox is greyed out \
                      while this is applied.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[
            dword(HKLM, REMOTE_ASSISTANCE_SETTING, "fAllowToGetHelp", 0),
            dword(HKLM, TERMINAL_SERVICES_POLICY, "fAllowToGetHelp", 0),
        ],
        former: &[],
    },
    Tweak {
        id: "security.device_metadata",
        category: Category::Security,
        topic: Topic::Security,
        title: "Stop downloading device metadata apps and icons",
        description: "Windows stops downloading the apps and custom icons that device makers \
                      publish as device metadata. Software that a device's driver package \
                      installs is not covered. The choice in Device installation settings is \
                      greyed out while this is applied. Microsoft documents this setting for \
                      Windows 11 Pro and higher; on Home, Cairn could only confirm that Windows \
                      Settings shows it as managed.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        windows: ANY_WINDOWS,
        retired: None,
        undo_limit: None,
        actions: &[dword(
            HKLM,
            DEVICE_METADATA_POLICY,
            "PreventDeviceMetadataFromNetwork",
            1,
        )],
        former: &[],
    },
];

pub static BLOAT_PACKAGES: &[BloatPackage] = &[
    // Games and entertainment
    BloatPackage { name: "king.com.*", title: "Candy Crush and other King games", description: "Promoted games installed by Windows.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.MicrosoftSolitaireCollection", title: "Solitaire Collection", description: "Ad-supported card games.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.XboxGamingOverlay", title: "Xbox Game Bar", description: "Win+G overlay, capture and widgets. Pair with turning off background recording so games do not prompt to reinstall it.", risk: Risk::Medium, default_on: true },
    BloatPackage { name: "Microsoft.XboxSpeechToTextOverlay", title: "Xbox speech-to-text overlay", description: "Game Bar accessibility overlay.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.XboxApp", title: "Xbox Console Companion", description: "Legacy Xbox app replaced by the Xbox app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.GamingApp", title: "Xbox app", description: "Needed for PC Game Pass and Xbox PC games.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "Microsoft.ZuneVideo", title: "Movies & TV", description: "Legacy video store and player.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.ZuneMusic", title: "Media Player", description: "The default music and video player.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "Disney.*", title: "Disney+", description: "Promoted streaming app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "AmazonVideo.PrimeVideo", title: "Prime Video", description: "Promoted streaming app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "BytedancePte.Ltd.TikTok", title: "TikTok", description: "Promoted social app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Facebook.*", title: "Facebook and Instagram", description: "Promoted social apps.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "SpotifyAB.SpotifyMusic", title: "Spotify", description: "Promoted music app; keep it if you use Spotify from the Store.", risk: Risk::Medium, default_on: false },
    // Microsoft consumer apps
    BloatPackage { name: "Microsoft.BingNews", title: "News", description: "Bing news feed.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.BingWeather", title: "Weather", description: "Bing weather app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.BingSearch", title: "Microsoft Bing", description: "Bing search app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.549981C3F5F10", title: "Cortana", description: "Deprecated Cortana app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.Copilot", title: "Copilot", description: "Copilot app. On Windows 11 24H2 and later this app is Copilot, so removing it is how Copilot is turned off; the Copilot key then opens Search or another app chosen in Settings. Windows can reinstall it with updates.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.GetHelp", title: "Get Help", description: "Microsoft support chat app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.Getstarted", title: "Tips", description: "Windows tips app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.MicrosoftOfficeHub", title: "Microsoft 365 app", description: "Office launcher and upsell; installed Office apps are not affected.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.People", title: "People", description: "Legacy contacts app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.PowerAutomateDesktop", title: "Power Automate", description: "Desktop automation app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.WindowsFeedbackHub", title: "Feedback Hub", description: "Sends feedback to Microsoft.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.WindowsMaps", title: "Maps", description: "Offline maps app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.Windows.DevHome", title: "Dev Home", description: "Developer dashboard, retired by Microsoft.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Clipchamp.Clipchamp", title: "Clipchamp", description: "Video editor.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "MicrosoftTeams", title: "Teams (personal, legacy)", description: "Consumer Teams chat from early Windows 11.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.SkypeApp", title: "Skype", description: "Discontinued by Microsoft.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.MixedReality.Portal", title: "Mixed Reality Portal", description: "Windows Mixed Reality headset app.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.Microsoft3DViewer", title: "3D Viewer", description: "3D model viewer.", risk: Risk::Low, default_on: true },
    BloatPackage { name: "Microsoft.Todos", title: "Microsoft To Do", description: "Task list app.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "MSTeams", title: "Microsoft Teams", description: "Teams for work, school and home.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "Microsoft.OutlookForWindows", title: "Outlook (new)", description: "The new Outlook mail app.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "Microsoft.YourPhone", title: "Phone Link", description: "Connects an Android phone or iPhone.", risk: Risk::Medium, default_on: false },
    BloatPackage { name: "Microsoft.MicrosoftStickyNotes", title: "Sticky Notes", description: "Notes app.", risk: Risk::Medium, default_on: false },
];

/// Package names that are never removed, whatever the request. Frameworks, system apps,
/// the Store, sign-in components that games use, and media codecs. A trailing `*` is a
/// prefix match. Packages marked as frameworks, non-removable or system-signed are also
/// refused at runtime.
pub static PROTECTED_PACKAGES: &[&str] = &[
    "Microsoft.WindowsStore",
    "Microsoft.StorePurchaseApp",
    "Microsoft.DesktopAppInstaller",
    "Microsoft.Winget.*",
    "Microsoft.VCLibs.*",
    "Microsoft.UI.Xaml.*",
    "Microsoft.NET.*",
    "Microsoft.WindowsAppRuntime.*",
    "Microsoft.Services.Store.Engagement",
    "Microsoft.SecHealthUI",
    "Microsoft.Windows.*",
    "MicrosoftWindows.*",
    "Microsoft.AAD.BrokerPlugin",
    "Microsoft.AccountsControl",
    "Microsoft.XboxIdentityProvider",
    "Microsoft.Xbox.TCUI",
    "Microsoft.GamingServices",
    "Microsoft.WindowsTerminal",
    "Microsoft.WindowsNotepad",
    "Microsoft.WindowsCalculator",
    "Microsoft.Paint",
    "Microsoft.ScreenSketch",
    "Microsoft.LanguageExperiencePack*",
    "Microsoft.HEIFImageExtension",
    "Microsoft.HEVCVideoExtension",
    "Microsoft.VP9VideoExtensions",
    "Microsoft.WebMediaExtensions",
    "Microsoft.WebpImageExtension",
    "Microsoft.RawImageExtension",
    "Microsoft.AV1VideoExtension",
    "Microsoft.MPEG2VideoExtension",
    "Microsoft.AVCEncoderVideoExtension",
    "MicrosoftCorporationII.WindowsSubsystemForLinux",
];

/// `Microsoft.Windows.DevHome` sits under the protected `Microsoft.Windows.*` prefix but is
/// an ordinary app; exact names listed here override a protected prefix match.
static PROTECTED_EXCEPTIONS: &[&str] = &["Microsoft.Windows.DevHome"];

/// Case-insensitive match of a package Name against a pattern with an optional trailing `*`.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
        }
        None => pattern.eq_ignore_ascii_case(name),
    }
}

pub fn is_protected_package(name: &str) -> bool {
    if PROTECTED_EXCEPTIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(name))
    {
        return false;
    }
    PROTECTED_PACKAGES.iter().any(|p| name_matches(p, name))
}

pub fn bloat_entry_for(name: &str) -> Option<&'static BloatPackage> {
    if is_protected_package(name) {
        return None;
    }
    BLOAT_PACKAGES.iter().find(|b| name_matches(b.name, name))
}

pub fn tweak(id: &str) -> Option<&'static Tweak> {
    TWEAKS.iter().find(|t| t.id == id)
}

/// Strongest restart need among the tweaks with an action matching `owns`, or
/// [`RestartNeed::None`] when no tweak has one.
fn restart_for(owns: impl Fn(&Action) -> bool) -> RestartNeed {
    TWEAKS
        .iter()
        .filter(|t| t.actions.iter().any(&owns))
        .map(|t| t.restart)
        .max()
        .unwrap_or_default()
}

/// Restart need of the tweak that writes this registry value, or wrote it in an earlier
/// version (a `former` value), matched with the hive exact and the path and name ignoring
/// ASCII case; [`RestartNeed::None`] for values no tweak writes.
pub fn restart_for_registry(hive: Hive, key_path: &str, value_name: &str) -> RestartNeed {
    let is_target = |r: &RegistryAction| {
        r.hive == hive
            && r.path.eq_ignore_ascii_case(key_path)
            && r.name.eq_ignore_ascii_case(value_name)
    };
    TWEAKS
        .iter()
        .filter(|t| {
            t.actions
                .iter()
                .any(|a| matches!(a, Action::Registry(r) if is_target(r)))
                || t.former.iter().any(|f| is_target(&f.action))
        })
        .map(|t| t.restart)
        .max()
        .unwrap_or_default()
}

/// Restart need of the tweak that configures this service (name ignoring ASCII case).
pub fn restart_for_service(name: &str) -> RestartNeed {
    restart_for(|a| matches!(a, Action::Service(s) if s.name.eq_ignore_ascii_case(name)))
}

/// Restart need of the tweak that changes this scheduled task (path ignoring ASCII case).
pub fn restart_for_scheduled_task(path: &str) -> RestartNeed {
    restart_for(|a| matches!(a, Action::ScheduledTask(s) if s.path.eq_ignore_ascii_case(path)))
}

/// Restart need of the tweak that changes the power scheme.
pub fn restart_for_power() -> RestartNeed {
    restart_for(|a| matches!(a, Action::Power(_)))
}

pub fn tweaks_in(category: Category) -> impl Iterator<Item = &'static Tweak> {
    TWEAKS.iter().filter(move |t| t.category == category)
}

/// Engine item id of an installed bloatware package.
pub fn appx_item_id(package_name: &str) -> String {
    format!("appx.{package_name}")
}

/// Package Name encoded in an engine item id, if it is an Appx item.
pub fn appx_name_from_item_id(id: &str) -> Option<&str> {
    id.strip_prefix("appx.").filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::debloat::requirements::servicing_family;
    use crate::win::task_scheduler::is_valid_task_path;

    #[test]
    fn tweak_ids_are_unique_and_prefixed_by_category() {
        let mut seen = HashSet::new();
        for t in TWEAKS {
            assert!(seen.insert(t.id), "duplicate tweak id {}", t.id);
            assert!(
                t.id.starts_with(&format!("{}.", t.category.label())),
                "{} is not prefixed with its category",
                t.id
            );
            assert_ne!(
                t.category,
                Category::Bloatware,
                "{} uses the Appx category",
                t.id
            );
            assert!(!t.actions.is_empty(), "{} has no actions", t.id);
            assert!(
                !(t.risk == Risk::High && t.default_on),
                "{} is high risk but default",
                t.id
            );
            assert!(
                t.retired.is_none() || !t.default_on,
                "{} is retired but default",
                t.id
            );
            if t.undo_limit.is_some() {
                assert!(
                    !t.default_on && t.risk != Risk::Low,
                    "{} has an undo limit but is default or low risk",
                    t.id
                );
            }
        }
        for category in Category::ALL {
            let prefix = format!("{}.", category.label());
            assert_eq!(
                TWEAKS.iter().any(|t| t.id.starts_with(&prefix)),
                category != Category::Bloatware,
                "{prefix}"
            );
        }
    }

    #[test]
    fn every_target_belongs_to_exactly_one_tweak() {
        let mut registry = HashSet::new();
        let mut services = HashSet::new();
        let mut tasks = HashSet::new();
        let mut power = 0;
        for t in TWEAKS {
            for a in t.actions {
                match a {
                    Action::Registry(r) => {
                        let key = (
                            r.hive,
                            r.path.to_ascii_lowercase(),
                            r.name.to_ascii_lowercase(),
                        );
                        assert!(
                            registry.insert(key),
                            "{} repeats {}\\{}",
                            t.id,
                            r.path,
                            r.name
                        );
                        assert!(!r.path.starts_with('\\') && !r.path.ends_with('\\'));
                        // A key's default value is only written as an empty string, the
                        // idiom that overrides a COM class registration.
                        assert!(
                            !r.name.is_empty() || r.data == RegData::Sz(""),
                            "{} writes a non-empty default value",
                            t.id
                        );
                    }
                    Action::Service(s) => {
                        assert!(
                            services.insert(s.name.to_ascii_lowercase()),
                            "{} repeats {}",
                            t.id,
                            s.name
                        );
                    }
                    Action::ScheduledTask(s) => {
                        assert!(
                            tasks.insert(s.path.to_ascii_lowercase()),
                            "{} repeats {}",
                            t.id,
                            s.path
                        );
                        assert!(is_valid_task_path(s.path), "{} names {}", t.id, s.path);
                        assert!(
                            s.path.starts_with(r"\Microsoft\Windows\"),
                            "{} names {} outside \\Microsoft\\Windows",
                            t.id,
                            s.path
                        );
                        assert!(!s.enabled, "{} enables {}", t.id, s.path);
                    }
                    Action::Power(_) => power += 1,
                }
            }
            // A value an earlier version set still belongs to its tweak alone: its records
            // undo with that tweak.
            for f in t.former {
                let r = &f.action;
                let key = (
                    r.hive,
                    r.path.to_ascii_lowercase(),
                    r.name.to_ascii_lowercase(),
                );
                assert!(
                    registry.insert(key),
                    "{} repeats the former value {}\\{}",
                    t.id,
                    r.path,
                    r.name
                );
                assert!(!r.path.starts_with('\\') && !r.path.ends_with('\\'));
                assert!(!r.name.is_empty(), "{} has a former default value", t.id);
            }
        }
        assert!(power <= 1, "more than one tweak changes the power plan");
    }

    #[test]
    fn bloat_patterns_never_match_protected_names() {
        for b in BLOAT_PACKAGES {
            for p in PROTECTED_PACKAGES {
                let probe = p.trim_end_matches('*');
                assert!(
                    !name_matches(b.name, probe) || PROTECTED_EXCEPTIONS.contains(&probe),
                    "bloat pattern {} matches protected {}",
                    b.name,
                    p
                );
            }
            assert!(
                !(b.risk == Risk::High && b.default_on),
                "{} is high risk but default",
                b.name
            );
            if !b.name.ends_with('*') {
                assert!(
                    !is_protected_package(b.name),
                    "bloat entry {} is protected",
                    b.name
                );
            }
        }
    }

    #[test]
    fn matching_and_protection() {
        assert!(name_matches("king.com.*", "king.com.CandyCrushSaga"));
        assert!(name_matches("King.Com.*", "king.com.CandyCrushSodaSaga"));
        assert!(!name_matches("king.com.*", "king.co"));
        assert!(name_matches("Microsoft.BingNews", "microsoft.bingnews"));
        assert!(!name_matches("Microsoft.BingNews", "Microsoft.BingNewsX"));

        assert!(is_protected_package("Microsoft.WindowsStore"));
        assert!(is_protected_package("Microsoft.VCLibs.140.00"));
        assert!(is_protected_package("Microsoft.Windows.Photos"));
        assert!(is_protected_package("Microsoft.XboxIdentityProvider"));
        assert!(!is_protected_package("Microsoft.Windows.DevHome"));

        assert!(bloat_entry_for("king.com.CandyCrushSaga").is_some());
        assert!(bloat_entry_for("Microsoft.Windows.DevHome").is_some());
        assert!(bloat_entry_for("Microsoft.WindowsStore").is_none());
        assert!(bloat_entry_for("Microsoft.WindowsCalculator").is_none());
    }

    #[test]
    fn restart_lookups_map_targets_to_their_tweak() {
        assert_eq!(
            restart_for_registry(
                Hive::CurrentUser,
                r"software\classes\clsid\{86CA1AA0-34AA-4E8B-A509-50C905BAE2A2}\InprocServer32",
                "",
            ),
            RestartNeed::Explorer
        );
        assert_eq!(
            restart_for_registry(Hive::LocalMachine, DATA_COLLECTION, "allowtelemetry"),
            RestartNeed::Restart
        );
        assert_eq!(
            restart_for_registry(Hive::CurrentUser, DATA_COLLECTION, "AllowTelemetry"),
            RestartNeed::None,
            "the hive must match"
        );
        assert_eq!(
            restart_for_registry(Hive::CurrentUser, r"Software\PCOptimizer\SelfTest", "Probe"),
            RestartNeed::None
        );
        assert_eq!(
            restart_for_registry(Hive::CurrentUser, r"control panel\mouse", "mousespeed"),
            RestartNeed::None
        );
        assert_eq!(
            restart_for_registry(Hive::LocalMachine, GRAPHICS_DRIVERS, "HwSchMode"),
            RestartNeed::Restart
        );
        // A value an earlier version set is undone with its tweak's restart need.
        assert_eq!(
            restart_for_registry(
                Hive::LocalMachine,
                &COPILOT_POLICY.to_ascii_lowercase(),
                "turnoffwindowscopilot"
            ),
            RestartNeed::SignOut
        );
        assert_eq!(
            restart_for_registry(Hive::LocalMachine, FILE_SYSTEM, "LongPathsEnabled"),
            RestartNeed::Restart
        );
        assert_eq!(
            restart_for_registry(Hive::LocalMachine, EXPLORER_POLICY, "HideAIActionsMenu"),
            RestartNeed::Restart
        );
        assert_eq!(restart_for_service("diagtrack"), RestartNeed::None);
        assert_eq!(
            restart_for_service("PCOptimizerNoSuchService"),
            RestartNeed::None
        );
        assert_eq!(restart_for_power(), RestartNeed::None);
        assert_eq!(
            restart_for_scheduled_task(r"\microsoft\windows\autochk\proxy"),
            RestartNeed::None
        );
        assert_eq!(
            restart_for_scheduled_task(r"\PCOptimizerSelfTest\NoSuchTask"),
            RestartNeed::None
        );
        assert_eq!(RestartNeed::default(), RestartNeed::None);
    }

    #[test]
    fn scheduled_task_tweaks_are_privacy_and_only_the_appraiser_is_optional() {
        let with_tasks: Vec<&Tweak> = TWEAKS
            .iter()
            .filter(|t| {
                t.actions
                    .iter()
                    .any(|a| matches!(a, Action::ScheduledTask(_)))
            })
            .collect();
        let ids: Vec<&str> = with_tasks.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            [
                "privacy.feedback_tasks",
                "privacy.error_report_task",
                "privacy.ceip_tasks",
                "privacy.compatibility_telemetry",
            ]
        );
        for t in &with_tasks {
            assert_eq!(t.category, Category::Privacy, "{}", t.id);
            assert_eq!(t.restart, RestartNeed::None, "{}", t.id);
            assert!(
                t.actions
                    .iter()
                    .all(|a| matches!(a, Action::ScheduledTask(_))),
                "{} mixes scheduled tasks with other targets",
                t.id
            );
            let optional = t.id == "privacy.compatibility_telemetry";
            assert_eq!(t.default_on, !optional, "{}", t.id);
            let risk = if optional { Risk::Medium } else { Risk::Low };
            assert_eq!(t.risk, risk, "{}", t.id);
        }
        let task_count: usize = with_tasks.iter().map(|t| t.actions.len()).sum();
        assert_eq!(task_count, 12);

        // Each task tweak sits right after the related setting.
        let position = |id: &str| TWEAKS.iter().position(|t| t.id == id).unwrap();
        for (before, after) in [
            ("privacy.error_reporting", "privacy.error_report_task"),
            ("privacy.ceip", "privacy.ceip_tasks"),
            ("privacy.feedback_prompts", "privacy.feedback_tasks"),
            ("privacy.location", "privacy.compatibility_telemetry"),
        ] {
            assert_eq!(position(before) + 1, position(after), "{after}");
        }

        // Deliberate tripwires: every tweak added, removed, retired or (de)recommended moves
        // one of these numbers.
        let count = |category| tweaks_in(category).count();
        assert_eq!(count(Category::Privacy), 39);
        assert_eq!(count(Category::Gaming), 7);
        assert_eq!(count(Category::Performance), 12);
        assert_eq!(count(Category::Interface), 20);
        assert_eq!(count(Category::Security), 2);
        assert_eq!(count(Category::Bloatware), 0);
        assert_eq!(TWEAKS.len(), 80);
        assert_eq!(TWEAKS.iter().filter(|t| t.default_on).count(), 29);
        assert_eq!(TWEAKS.iter().filter(|t| t.retired.is_some()).count(), 9);
        assert_eq!(TWEAKS.iter().filter(|t| t.requires.is_some()).count(), 16);
        assert_eq!(
            TWEAKS.iter().filter(|t| t.windows != ANY_WINDOWS).count(),
            7
        );
        assert_eq!(TWEAKS.iter().filter(|t| !t.former.is_empty()).count(), 1);
        assert_eq!(TWEAKS.iter().filter(|t| t.undo_limit.is_some()).count(), 2);
    }

    fn registry_actions(t: &Tweak) -> impl Iterator<Item = &RegistryAction> {
        t.actions.iter().filter_map(|a| match a {
            Action::Registry(r) => Some(r),
            Action::Service(_) | Action::ScheduledTask(_) | Action::Power(_) => None,
        })
    }

    fn is_office_policy(path: &str) -> bool {
        path.to_ascii_lowercase()
            .starts_with(&r"Software\Policies\Microsoft\office".to_ascii_lowercase())
    }

    /// Value names written only by tweaks with the requirement next to them. A value-name
    /// rule, because some of these keys also hold values of tweaks without a requirement
    /// (`AppPrivacy` holds `performance.background_apps`'s value).
    const REQUIREMENT_VALUES: &[(&str, Requirement)] = &[
        ("DisableAIDataAnalysis", Requirement::RecallComponent),
        ("DisableAIFeatures", Requirement::NotepadAi),
        ("DisableImageCreator", Requirement::Paint),
        ("DisableCocreator", Requirement::Paint),
        ("DisableGenerativeFill", Requirement::Paint),
        ("LetAppsAccessSystemAIModels", Requirement::SystemAiModels),
    ];

    #[test]
    fn app_policy_tweaks_declare_their_requirement() {
        for t in TWEAKS {
            let edge = registry_actions(t).any(|r| r.path.eq_ignore_ascii_case(EDGE_POLICY));
            let office = registry_actions(t).any(|r| is_office_policy(r.path));
            let gpu = registry_actions(t).any(|r| {
                r.path.eq_ignore_ascii_case(GRAPHICS_DRIVERS)
                    && r.name.eq_ignore_ascii_case("HwSchMode")
            });
            assert_eq!(t.requires == Some(Requirement::Edge), edge, "{}", t.id);
            // Office policies are offered only where an Office edition honours them.
            assert_eq!(
                t.requires == Some(Requirement::OfficeBusiness),
                office,
                "{}",
                t.id
            );
            assert_ne!(t.requires, Some(Requirement::Office), "{}", t.id);
            assert_eq!(
                t.requires == Some(Requirement::GpuScheduling),
                gpu,
                "{}",
                t.id
            );
            for (name, requirement) in REQUIREMENT_VALUES {
                if registry_actions(t).any(|r| r.name.eq_ignore_ascii_case(name)) {
                    assert_eq!(t.requires, Some(*requirement), "{} writes {name}", t.id);
                }
            }
            let Some(requirement) = t.requires else {
                continue;
            };
            assert!(
                t.actions.iter().all(|a| match a {
                    Action::Registry(r) => match requirement {
                        Requirement::Edge => r.path.eq_ignore_ascii_case(EDGE_POLICY),
                        Requirement::Office | Requirement::OfficeBusiness => {
                            is_office_policy(r.path)
                        }
                        Requirement::GpuScheduling => r.path.eq_ignore_ascii_case(GRAPHICS_DRIVERS),
                        Requirement::RecallComponent
                        | Requirement::NotepadAi
                        | Requirement::Paint
                        | Requirement::SystemAiModels => REQUIREMENT_VALUES
                            .iter()
                            .any(|(name, req)| *req == requirement
                                && r.name.eq_ignore_ascii_case(name)),
                    },
                    Action::Service(_) | Action::ScheduledTask(_) | Action::Power(_) => false,
                }),
                "{} mixes its product's policies with other targets",
                t.id
            );
        }
        // The Recall policy is written for this PC and for the signed-in account, nowhere else.
        let recall = tweak("privacy.recall").unwrap();
        let recall_keys: Vec<(Hive, &str)> =
            registry_actions(recall).map(|r| (r.hive, r.path)).collect();
        assert_eq!(
            recall_keys,
            [
                (Hive::LocalMachine, WINDOWS_AI_POLICY),
                (Hive::CurrentUser, WINDOWS_AI_POLICY_USER)
            ]
        );
        let required: Vec<(&str, Requirement)> = TWEAKS
            .iter()
            .filter_map(|t| t.requires.map(|r| (t.id, r)))
            .collect();
        assert_eq!(
            required,
            [
                ("performance.edge_background", Requirement::Edge),
                ("privacy.recall", Requirement::RecallComponent),
                ("privacy.notepad_ai", Requirement::NotepadAi),
                ("privacy.text_image_generation", Requirement::SystemAiModels),
                ("privacy.office_telemetry", Requirement::OfficeBusiness),
                (
                    "privacy.office_connected_experiences",
                    Requirement::OfficeBusiness
                ),
                ("privacy.office_cloud_content", Requirement::OfficeBusiness),
                ("privacy.edge_telemetry", Requirement::Edge),
                ("privacy.edge_shopping", Requirement::Edge),
                ("privacy.edge_feedback", Requirement::Edge),
                ("gaming.gpu_scheduling", Requirement::GpuScheduling),
                ("interface.edge_sidebar", Requirement::Edge),
                ("interface.edge_default_browser_prompts", Requirement::Edge),
                ("interface.edge_first_run", Requirement::Edge),
                ("interface.edge_new_tab_page", Requirement::Edge),
                ("interface.edge_rewards", Requirement::Edge),
            ]
        );
    }

    #[test]
    fn edge_policy_tweaks_mention_the_managed_message() {
        for t in TWEAKS
            .iter()
            .filter(|t| t.requires == Some(Requirement::Edge))
        {
            assert!(
                t.description.contains("Managed by your organization"),
                "{}",
                t.id
            );
        }
    }

    /// Edge policies whose page says "Applies to a profile that is signed in with a Microsoft
    /// account: No". Microsoft defines that filter for PCs managed by an organization; an
    /// unmanaged Windows 11 Home PC applied them to its personal profile.
    const MSA_FILTERED_EDGE_POLICIES: &[&str] = &[
        "AddressBarMicrosoftSearchInBingProviderEnabled",
        "AIGenThemesEnabled",
        "AlternateErrorPagesEnabled",
        "AutofillAddressEnabled",
        "AutofillCreditCardEnabled",
        "BuiltInAIAPIsEnabled",
        "ComposeInlineEnabled",
        "ConfigureDoNotTrack",
        "CopilotAddressBarSuggestionsEnabled",
        "CopilotPageContext",
        "EdgeAssetDeliveryServiceEnabled",
        "EdgeCollectionsEnabled",
        "EdgeCopilotEnabled",
        "EdgeHistoryAISearchEnabled",
        "EdgeShoppingAssistantEnabled",
        "ExtensionInstallBlocklist",
        "HubsSidebarEnabled",
        "LocalProvidersEnabled",
        "Microsoft365CopilotChatIconEnabled",
        "MicrosoftEdgeInsiderPromotionEnabled",
        "MicrosoftEditorProofingEnabled",
        "NetworkPredictionOptions",
        "NewTabPageBingChatEnabled",
        "NewTabPageContentEnabled",
        "NewTabPageHideDefaultTopSites",
        "PasswordManagerEnabled",
        "PaymentMethodQueryEnabled",
        "ResolveNavigationErrorsUseWebService",
        "SearchSuggestEnabled",
        "ShowAcrobatSubscriptionButton",
        "ShowMicrosoftRewards",
        "ShowPDFDefaultRecommendationsEnabled",
        "ShowRecommendationsEnabled",
        "TabServicesEnabled",
        "TextPredictionEnabled",
        "VisualSearchEnabled",
        "WalletDonationEnabled",
    ];
    /// Edge policies the catalog writes whose page says they apply to profiles signed in with
    /// a Microsoft account.
    const MSA_APPLIED_EDGE_POLICIES: &[&str] = &[
        "BackgroundModeEnabled",
        "DefaultBrowserSettingEnabled",
        "DefaultBrowserSettingsCampaignEnabled",
        "DiagnosticData",
        "HideFirstRunExperience",
        "PersonalizationReportingEnabled",
        "SpotlightExperiencesAndRecommendationsEnabled",
        "StartupBoostEnabled",
        "UserFeedbackAllowed",
    ];
    const PERSONAL_ACCOUNT_SENTENCE: &str = "On a PC managed by an organization, Edge may \
                                             ignore this in profiles signed in with a personal \
                                             Microsoft account.";

    #[test]
    fn per_profile_edge_policies_mention_personal_accounts() {
        let mut with_sentence = Vec::new();
        for t in TWEAKS {
            let edge: Vec<&str> = registry_actions(t)
                .filter(|r| r.path.eq_ignore_ascii_case(EDGE_POLICY))
                .map(|r| r.name)
                .collect();
            for name in &edge {
                let filtered = MSA_FILTERED_EDGE_POLICIES.contains(name);
                let applied = MSA_APPLIED_EDGE_POLICIES.contains(name);
                assert!(
                    filtered != applied,
                    "{}: {name} must be listed as applied to or filtered from personal accounts",
                    t.id
                );
            }
            let filtered = edge.iter().any(|n| MSA_FILTERED_EDGE_POLICIES.contains(n));
            assert_eq!(
                t.description.contains(PERSONAL_ACCOUNT_SENTENCE),
                filtered,
                "{}",
                t.id
            );
            assert!(!t.description.contains("Edge ignores"), "{}", t.id);
            if filtered {
                with_sentence.push(t.id);
            }
        }
        assert_eq!(
            with_sentence,
            [
                "privacy.edge_shopping",
                "interface.edge_sidebar",
                "interface.edge_default_browser_prompts",
                "interface.edge_new_tab_page",
                "interface.edge_rewards",
            ]
        );
    }

    #[test]
    fn office_and_edge_policies_are_dwords() {
        for t in TWEAKS.iter().filter(|t| {
            matches!(
                t.requires,
                Some(Requirement::OfficeBusiness) | Some(Requirement::Edge)
            )
        }) {
            assert!(
                registry_actions(t).all(|r| matches!(r.data, RegData::Dword(_))),
                "{}",
                t.id
            );
        }
    }

    #[test]
    fn new_app_policy_tweaks_follow_the_catalog_order() {
        let position = |id: &str| {
            TWEAKS
                .iter()
                .position(|t| t.id == id)
                .unwrap_or_else(|| panic!("{id} is not in the catalog"))
        };
        // Each run of ids sits together, in this order, next to the 0.2 tweak it follows.
        let runs: [&[&str]; 6] = [
            &[
                "privacy.activity_history",
                "privacy.phone_link",
                "privacy.cortana",
            ],
            &[
                "privacy.web_search",
                "privacy.search_history",
                "privacy.search_cloud",
                "privacy.feedback_prompts",
            ],
            &[
                "privacy.copilot",
                "privacy.notepad_ai",
                "privacy.text_image_generation",
                "privacy.app_launch_tracking",
            ],
            &[
                "privacy.typing_data",
                "privacy.inking_typing",
                "privacy.language_list",
                "privacy.online_speech",
            ],
            &[
                "privacy.lock_screen_ads",
                "privacy.start_suggestions",
                "privacy.setup_reminders",
                "privacy.lock_screen_notifications",
                "privacy.explorer_cloud_files",
                "privacy.location",
                "privacy.compatibility_telemetry",
                "privacy.office_telemetry",
                "privacy.office_connected_experiences",
                "privacy.office_cloud_content",
                "privacy.edge_telemetry",
                "privacy.edge_shopping",
                "privacy.edge_feedback",
                "gaming.sticky_keys",
            ],
            &[
                "interface.dark_mode",
                "interface.explorer_sync_ads",
                "interface.explorer_home_gallery",
                "interface.explorer_ai_actions",
                "interface.spotlight_icon",
                "interface.long_paths",
                "interface.edge_sidebar",
                "interface.edge_default_browser_prompts",
                "interface.edge_first_run",
                "interface.edge_new_tab_page",
                "interface.edge_rewards",
            ],
        ];
        for run in runs {
            for pair in run.windows(2) {
                assert_eq!(position(pair[0]) + 1, position(pair[1]), "{}", pair[1]);
            }
        }
        let last_interface = TWEAKS
            .iter()
            .rposition(|t| t.category == Category::Interface)
            .unwrap();
        assert_eq!(TWEAKS[last_interface].id, "interface.edge_rewards");
        // The Security block comes last.
        let first_security = TWEAKS
            .iter()
            .position(|t| t.category == Category::Security)
            .unwrap();
        assert!(TWEAKS[first_security..]
            .iter()
            .all(|t| t.category == Category::Security));
        let security: Vec<&str> = TWEAKS[first_security..].iter().map(|t| t.id).collect();
        assert_eq!(
            security,
            ["security.remote_assistance", "security.device_metadata"]
        );

        let defaults: Vec<&str> = [
            "privacy.office_telemetry",
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
            "privacy.edge_telemetry",
            "privacy.edge_shopping",
            "privacy.edge_feedback",
            "interface.edge_sidebar",
            "interface.edge_default_browser_prompts",
            "interface.edge_first_run",
            "interface.edge_new_tab_page",
            "interface.edge_rewards",
        ]
        .into_iter()
        .filter(|id| tweak(id).unwrap().default_on)
        .collect();
        assert_eq!(
            defaults,
            [
                "privacy.edge_telemetry",
                "privacy.edge_feedback",
                "interface.edge_new_tab_page",
            ]
        );
        assert_eq!(
            tweak("privacy.office_cloud_content").unwrap().risk,
            Risk::Medium
        );
        let mouse = tweak("gaming.mouse_acceleration").unwrap();
        assert_eq!(mouse.restart, RestartNeed::None);
        assert!(mouse.description.ends_with("Takes effect at once."));
        assert!(mouse
            .actions
            .iter()
            .all(|a| matches!(a, Action::Registry(r) if r.path == MOUSE_KEY && r.data == RegData::Sz("0"))));
    }

    #[test]
    fn requirement_texts() {
        assert_eq!(
            Requirement::Office.missing_text(),
            "Microsoft Office (2016 or later) is not installed."
        );
        assert_eq!(
            Requirement::Edge.missing_text(),
            "Microsoft Edge is not installed."
        );
        assert_eq!(
            Requirement::GpuScheduling.missing_text(),
            "No graphics driver on this PC supports hardware-accelerated GPU scheduling."
        );
        assert_eq!(
            Requirement::Office.subject(),
            "Microsoft Office is installed"
        );
        assert_eq!(Requirement::Edge.subject(), "Microsoft Edge is installed");
        assert_eq!(
            Requirement::GpuScheduling.subject(),
            "the graphics driver supports GPU scheduling"
        );
        assert_eq!(
            serde_json::to_value(Requirement::GpuScheduling).unwrap(),
            "gpu_scheduling"
        );
        for (requirement, name, missing, subject) in [
            (
                Requirement::RecallComponent,
                "recall_component",
                "Cairn found no sign of Recall on this PC.",
                "Recall is installed",
            ),
            (
                Requirement::NotepadAi,
                "notepad_ai",
                "Notepad 11.2503.16 or later is not installed.",
                "Notepad 11.2503.16 or later is installed",
            ),
            (
                Requirement::Paint,
                "paint",
                "Paint is not installed.",
                "Paint is installed",
            ),
            (
                Requirement::SystemAiModels,
                "system_ai_models",
                "This version of Windows has no text and image generation features for apps.",
                "Windows has text and image generation for apps",
            ),
            (
                Requirement::OfficeBusiness,
                "office_business",
                "Microsoft 365 Home ignores Office policies.",
                "Office for business or enterprise is installed",
            ),
        ] {
            assert_eq!(requirement.missing_text(), missing);
            assert_eq!(requirement.subject(), subject);
            let json = serde_json::to_value(requirement).unwrap();
            assert_eq!(json, name);
            assert_eq!(
                serde_json::from_value::<Requirement>(json).unwrap(),
                requirement
            );
        }
    }

    #[test]
    fn compatibility_telemetry_names_the_windows_backup_task() {
        let t = tweak("privacy.compatibility_telemetry").expect("compatibility telemetry tweak");
        let mare_backup = r"\Microsoft\Windows\Application Experience\MareBackup";
        assert!(
            t.actions
                .iter()
                .any(|a| matches!(a, Action::ScheduledTask(s) if s.path == mare_backup)),
            "the tweak turns off MareBackup"
        );
        for phrase in [
            "MareBackup",
            "desktop apps for Windows Backup",
            "may leave out desktop apps",
        ] {
            assert!(
                t.description.contains(phrase),
                "the description does not say {phrase:?}"
            );
        }
    }

    #[test]
    fn accessibility_shortcuts_clear_only_the_hotkey_bit() {
        let t = tweak("gaming.sticky_keys").expect("sticky keys tweak");
        assert_eq!(t.actions.len(), 3);
        for a in t.actions {
            let Action::Registry(r) = a else {
                panic!("unexpected action {a:?}")
            };
            assert_eq!(r.hive, Hive::CurrentUser);
            assert_eq!(r.name, "Flags");
            match r.data {
                RegData::FlagsSzClear { clear, default } => {
                    assert_eq!(clear, HOTKEYACTIVE);
                    assert_ne!(
                        default & HOTKEYACTIVE,
                        0,
                        "Windows defaults have the shortcut on"
                    );
                }
                other => panic!("{} uses {other:?}", r.path),
            }
        }
    }

    #[test]
    fn appx_item_ids_round_trip() {
        let id = appx_item_id("Microsoft.BingNews");
        assert_eq!(id, "appx.Microsoft.BingNews");
        assert_eq!(appx_name_from_item_id(&id), Some("Microsoft.BingNews"));
        assert_eq!(appx_name_from_item_id("privacy.cortana"), None);
        assert_eq!(appx_name_from_item_id("appx."), None);
    }

    #[test]
    fn security_is_a_category_of_its_own() {
        assert_eq!(
            Category::ALL,
            [
                Category::Privacy,
                Category::Gaming,
                Category::Performance,
                Category::Interface,
                Category::Security,
                Category::Bloatware,
            ]
        );
        assert_eq!(Category::Security.label(), "security");
        assert_eq!(Category::parse(" Security "), Some(Category::Security));
        assert_eq!(
            serde_json::to_value(Category::Security).unwrap(),
            "security"
        );
        for category in Category::ALL {
            assert_eq!(Category::parse(category.label()), Some(category));
        }
    }

    #[test]
    fn remote_assistance_writes_the_keys_the_checkup_reads() {
        let t = tweak(REMOTE_ASSISTANCE_TWEAK).expect("Remote Assistance tweak");
        assert_eq!(t.category, Category::Security);
        assert_eq!(t.topic, Topic::Security);
        assert_eq!(t.title, "Turn off Remote Assistance");
        assert_eq!(
            t.description,
            "Turns off Remote Assistance, so nobody can view or control this PC through an \
             invitation, which scammers often ask people to send. Quick Assist is a separate app \
             and is not affected. The Remote Assistance option in System Properties is turned \
             off while this is applied. The checkbox is greyed out while this is applied."
        );
        assert_eq!(t.risk, Risk::Low);
        assert!(t.default_on);
        assert_eq!(t.restart, RestartNeed::None);
        assert_eq!(t.requires, None);
        assert_eq!(t.windows, ANY_WINDOWS);
        assert!(t.retired.is_none() && t.undo_limit.is_none() && t.former.is_empty());
        let targets: Vec<RegistryAction> = registry_actions(t).copied().collect();
        assert_eq!(
            targets,
            [
                reg_dword(
                    Hive::LocalMachine,
                    REMOTE_ASSISTANCE_SETTING,
                    "fAllowToGetHelp",
                    0
                ),
                reg_dword(
                    Hive::LocalMachine,
                    TERMINAL_SERVICES_POLICY,
                    "fAllowToGetHelp",
                    0
                ),
            ]
        );
        assert_eq!(t.actions.len(), targets.len());
    }

    /// The tweaks Cairn 0.2 recommended (its `default_on` set).
    const RECOMMENDED_IN_0_2: [&str; 36] = [
        "privacy.diagnostic_data",
        "privacy.diagtrack_service",
        "privacy.activity_history",
        "privacy.cortana",
        "privacy.advertising_id",
        "privacy.tailored_experiences",
        "privacy.suggested_content",
        "privacy.web_search",
        "privacy.feedback_prompts",
        "privacy.feedback_tasks",
        "privacy.recall",
        "privacy.app_launch_tracking",
        "privacy.typing_data",
        "privacy.online_speech",
        "privacy.error_reporting",
        "privacy.error_report_task",
        "privacy.ceip",
        "privacy.ceip_tasks",
        "privacy.lock_screen_ads",
        "privacy.office_telemetry",
        "privacy.edge_telemetry",
        "gaming.background_recording",
        "gaming.game_mode",
        "gaming.multimedia_scheduling",
        "gaming.sticky_keys",
        "performance.power_plan",
        "performance.sysmain",
        "performance.maps_broker",
        "performance.link_tracking",
        "performance.startup_delay",
        "performance.edge_background",
        "performance.delivery_optimization",
        "performance.menu_delay",
        "interface.file_extensions",
        "interface.widgets",
        "interface.end_task",
    ];

    #[test]
    fn formerly_recommended_ids_exist_and_are_not_recommended() {
        for id in FORMERLY_RECOMMENDED {
            let t = tweak(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
            assert!(!t.default_on, "{id} is still recommended");
            assert!(
                RECOMMENDED_IN_0_2.contains(id),
                "{id} was not recommended before"
            );
        }
        // Every tweak that lost its recommendation is listed, so turning its mode off still
        // restores what an earlier version applied.
        for id in RECOMMENDED_IN_0_2 {
            let t = tweak(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
            assert_eq!(!t.default_on, FORMERLY_RECOMMENDED.contains(&id), "{id}");
        }
    }

    #[test]
    fn retired_tweaks_say_why_and_are_not_recommended() {
        let mut retired = Vec::new();
        for t in TWEAKS {
            let Some(why) = t.retired else {
                continue;
            };
            retired.push(t.id);
            assert!(!t.default_on, "{} is retired but recommended", t.id);
            assert!(
                why.len() > 20 && why.ends_with('.') && !why.contains("Undo"),
                "{}: {why}",
                t.id
            );
            // A retired tweak is never applied, so a value it no longer sets stays an action:
            // its earlier records undo through the tweak.
            assert!(
                t.former.is_empty(),
                "{} is retired and has former values",
                t.id
            );
        }
        assert_eq!(
            retired,
            [
                "privacy.activity_history",
                "privacy.cortana",
                "privacy.web_search",
                "gaming.multimedia_scheduling",
                "performance.startup_delay",
                "privacy.ceip",
                "gaming.network_throttling",
                "interface.widgets",
                "interface.edge_sidebar",
            ]
        );
    }

    #[test]
    fn undo_limit_means_medium_or_high_and_not_recommended() {
        let mut limited = Vec::new();
        for t in TWEAKS {
            let Some(limit) = t.undo_limit else {
                continue;
            };
            limited.push(t.id);
            assert!(
                !t.default_on,
                "{} has an undo limit but is recommended",
                t.id
            );
            assert_ne!(
                t.risk,
                Risk::Low,
                "{} has an undo limit but is Low risk",
                t.id
            );
            assert!(
                limit.ends_with('.') && limit.contains("Undo"),
                "{}: {limit}",
                t.id
            );
        }
        assert_eq!(limited, ["privacy.phone_link", "privacy.recall"]);
    }

    #[test]
    fn former_actions_are_registry_values_with_a_reason() {
        let mut with_former = Vec::new();
        for t in TWEAKS {
            for f in t.former {
                with_former.push((t.id, f.action.hive, f.action.path, f.action.name));
                assert!(
                    f.reason.starts_with("Cairn 0.") && f.reason.ends_with('.'),
                    "{}: {}",
                    t.id,
                    f.reason
                );
                assert!(
                    matches!(f.action.data, RegData::Dword(_)),
                    "{}: {} is not a DWORD",
                    t.id,
                    f.action.name
                );
                assert!(
                    !registry_actions(t).any(|r| r.hive == f.action.hive
                        && r.path.eq_ignore_ascii_case(f.action.path)
                        && r.name.eq_ignore_ascii_case(f.action.name)),
                    "{} still sets its former value {}",
                    t.id,
                    f.action.name
                );
            }
        }
        assert_eq!(
            with_former,
            [(
                "privacy.copilot",
                Hive::LocalMachine,
                COPILOT_POLICY,
                "TurnOffWindowsCopilot"
            )]
        );
    }

    #[test]
    fn windows_ranges_name_known_builds() {
        for t in TWEAKS {
            let range = &t.windows;
            let mut previous: Option<u32> = None;
            for floor in range.from {
                // Floors on enablement builds (26200, 26300) are allowed: they are met only
                // on that build or a newer one of the same family.
                assert!(
                    version_label(floor.number).is_some(),
                    "{}: build {floor} has no Windows 11 version",
                    t.id
                );
                let family = servicing_family(floor.number);
                if let Some(previous) = previous {
                    assert!(
                        previous < family,
                        "{}: floors must name distinct families, oldest first",
                        t.id
                    );
                }
                previous = Some(family);
                if let Some(last) = range.through {
                    assert!(family <= last, "{}: floor {floor} is after {last}", t.id);
                }
            }
            if let Some(last) = range.through {
                assert_eq!(
                    servicing_family(last),
                    last,
                    "{}: {last} is not a servicing family",
                    t.id
                );
                assert!(family_label(last).is_some(), "{}: {last}", t.id);
            }
            assert_eq!(
                range.describe().is_some(),
                *range != ANY_WINDOWS,
                "{}",
                t.id
            );
        }
    }

    #[test]
    fn nothing_touches_secure_boot_servicing() {
        const SECURE_BOOT: &str = r"system\currentcontrolset\control\secureboot";
        const UPDATE_TASK: &str = r"\Microsoft\Windows\PI\Secure-Boot-Update";
        for t in TWEAKS {
            for r in registry_actions(t).chain(t.former.iter().map(|f| &f.action)) {
                let path = r.path.to_ascii_lowercase();
                assert!(
                    path != SECURE_BOOT && !path.starts_with(&format!("{SECURE_BOOT}\\")),
                    "{} writes under {}",
                    t.id,
                    r.path
                );
            }
            for a in t.actions {
                if let Action::ScheduledTask(task) = a {
                    assert!(
                        !task.path.eq_ignore_ascii_case(UPDATE_TASK),
                        "{} changes the Secure Boot update task",
                        t.id
                    );
                }
            }
        }
    }

    #[test]
    fn security_tweaks_are_in_the_security_category() {
        for t in TWEAKS {
            assert_eq!(
                t.id.starts_with("security."),
                t.category == Category::Security,
                "{}",
                t.id
            );
            if t.category == Category::Security {
                assert_eq!(t.topic, Topic::Security, "{}", t.id);
                assert!(t.retired.is_none(), "{}", t.id);
            }
        }
        // Only one recommended Security tweak: the category has no mode of its own.
        let recommended: Vec<&str> = tweaks_in(Category::Security)
            .filter(|t| t.default_on)
            .map(|t| t.id)
            .collect();
        assert_eq!(recommended, [REMOTE_ASSISTANCE_TWEAK]);
    }

    /// Every sentence a tweak shows: title, description, retired reason, undo limit and the
    /// reasons of its former values.
    fn texts(t: &Tweak) -> Vec<&'static str> {
        let mut texts = vec![t.title, t.description];
        texts.extend(t.retired);
        texts.extend(t.undo_limit);
        texts.extend(t.former.iter().map(|f| f.reason));
        texts
    }

    #[test]
    fn catalog_text_uses_settings_path_arrows() {
        for t in TWEAKS {
            for text in texts(t) {
                assert!(!text.contains("Settings under"), "{}: {text}", t.id);
                assert!(!text.contains(" > "), "{}: {text}", t.id);
                for (i, _) in text.match_indices('›') {
                    let before = &text[..i];
                    let after = &text[i + '›'.len_utf8()..];
                    assert!(
                        before.ends_with(' ') && after.starts_with(' '),
                        "{}: a path arrow without spaces in {text}",
                        t.id
                    );
                }
                assert!(!text.contains("  "), "{}: double space in {text}", t.id);
                assert!(!text.contains("..."), "{}: {text}", t.id);
            }
            assert!(
                t.description.ends_with('.') && !t.title.ends_with('.'),
                "{}",
                t.id
            );
        }
    }

    #[test]
    fn wp0_changes_follow_the_checks() {
        let cortana = tweak("privacy.cortana").unwrap();
        assert_eq!(
            cortana.retired,
            Some("Cairn found no effect of this policy on Windows 11.")
        );
        for id in ["privacy.activity_history", "privacy.web_search"] {
            assert_eq!(
                tweak(id).unwrap().retired,
                Some(NO_OBSERVABLE_EFFECT),
                "{id}"
            );
        }
        // All three Activity History policies stay, so 0.2 records of each still undo.
        let activity: Vec<&str> = registry_actions(tweak("privacy.activity_history").unwrap())
            .map(|r| r.name)
            .collect();
        assert_eq!(
            activity,
            [
                "EnableActivityFeed",
                "PublishUserActivities",
                "UploadUserActivities"
            ]
        );

        let recall = tweak("privacy.recall").unwrap();
        assert_eq!(recall.risk, Risk::Medium);
        assert!(!recall.default_on);
        assert_eq!(recall.requires, Some(Requirement::RecallComponent));
        assert_eq!(recall.windows.from, [Build::new(26100, 3915)]);
        assert_eq!(recall.windows.through, None);
        assert_eq!(recall.windows.editions, Editions::All);
        assert!(recall.description.contains("Copilot+ PCs"));
        assert!(recall
            .undo_limit
            .unwrap()
            .contains("cannot bring back the snapshots"));

        let copilot = tweak("privacy.copilot").unwrap();
        assert_eq!(copilot.windows.from, [Build::new(22621, 2361)]);
        assert_eq!(copilot.windows.through, Some(22621));
        assert_eq!(copilot.windows.editions, Editions::All);
        let copilot_targets: Vec<(Hive, &str)> = registry_actions(copilot)
            .map(|r| (r.hive, r.path))
            .collect();
        assert_eq!(copilot_targets, [(Hive::CurrentUser, COPILOT_POLICY_USER)]);
        assert!(!copilot.default_on);

        // Honoured on Home: no edition gate, and the effect appears without signing out.
        let background = tweak("performance.background_apps").unwrap();
        assert_eq!(background.windows, ANY_WINDOWS);
        assert_eq!(background.restart, RestartNeed::None);
        let location = tweak("privacy.location").unwrap();
        assert_eq!(location.windows, ANY_WINDOWS);
        assert!(location.retired.is_none());
        assert_eq!(location.restart, RestartNeed::None);

        let maps = tweak("performance.maps_broker").unwrap();
        assert!(maps
            .description
            .ends_with("Windows starts this service when an app or Settings needs offline maps."));
        assert!(!maps.description.contains("Maps app"));
        assert!(!tweak("privacy.edge_shopping")
            .unwrap()
            .description
            .contains("coupon"));
        assert_eq!(
            tweak("interface.edge_sidebar").unwrap().retired,
            Some(
                "Microsoft retired the Edge sidebar app list in 2026; this policy no longer \
                 changes anything Cairn can observe."
            )
        );
    }

    #[test]
    fn verified_settings_mappings_are_written_whole() {
        let values = |id: &str| -> Vec<(Hive, &'static str, &'static str)> {
            registry_actions(tweak(id).unwrap())
                .map(|r| (r.hive, r.path, r.name))
                .collect()
        };
        // 'Improve inking and typing' writes two per-user values; the policy is the documented
        // half.
        assert_eq!(
            values("privacy.inking_typing"),
            [
                (
                    Hive::LocalMachine,
                    TEXT_INPUT_POLICY,
                    "AllowLinguisticDataCollection"
                ),
                (Hive::CurrentUser, INKING_TYPING, "Enabled"),
                (Hive::CurrentUser, INKING_TYPING_SETTING, "Value"),
            ]
        );
        // 'Show notifications on the lock screen' writes two values.
        assert_eq!(
            values("privacy.lock_screen_notifications"),
            [
                (
                    Hive::CurrentUser,
                    NOTIFICATION_SETTINGS,
                    "NOC_GLOBAL_SETTING_ALLOW_TOASTS_ABOVE_LOCK"
                ),
                (
                    Hive::CurrentUser,
                    PUSH_NOTIFICATIONS,
                    "LockScreenToastEnabled"
                ),
            ]
        );
        assert_eq!(
            values("privacy.setup_reminders"),
            [(
                Hive::CurrentUser,
                USER_PROFILE_ENGAGEMENT,
                "ScoobeSystemSettingEnabled"
            )]
        );
        assert_eq!(
            values("privacy.search_cloud"),
            [
                (
                    Hive::CurrentUser,
                    SEARCH_SETTINGS,
                    "IsMSACloudSearchEnabled"
                ),
                (
                    Hive::CurrentUser,
                    SEARCH_SETTINGS,
                    "IsAADCloudSearchEnabled"
                ),
                (
                    Hive::LocalMachine,
                    WINDOWS_SEARCH_POLICY,
                    "AllowCloudSearch"
                ),
            ]
        );
        // Values without a Settings switch on Windows 11 26H2, or that the switch does not
        // write there, are in no tweak.
        for name in [
            "IsDynamicSearchBoxEnabled",
            "SubscribedContent-310093Enabled",
            "SoftLandingEnabled",
            "Start_AccountNotifications",
            "EnableAccountNotifications",
        ] {
            for t in TWEAKS {
                assert!(
                    !registry_actions(t).any(|r| r.name.eq_ignore_ascii_case(name)),
                    "{} writes {name}",
                    t.id
                );
            }
        }
        // Candidates that left 0.3: no second device or account to check them with, no switch
        // on Windows 11 26H2, or nothing to observe on the test PC.
        for id in [
            "privacy.cloud_clipboard",
            "privacy.settings_sync",
            "privacy.account_notifications",
            "privacy.recent_files",
            "privacy.search_highlights",
            "security.password_reveal",
            "privacy.paint_ai",
        ] {
            assert!(tweak(id).is_none(), "{id}");
        }
    }

    /// The standard sentence of a policy Microsoft documents for Pro and higher whose effect on
    /// Home could only be seen in Windows Settings.
    const HOME_RULE_SENTENCE: &str = "Microsoft documents this setting for Windows 11 Pro and \
                                      higher; on Home, Cairn could only confirm that Windows \
                                      Settings shows it as managed.";

    #[test]
    fn checked_tweaks_carry_their_results() {
        // 0.2 tweaks whose effect could not be observed are retired and leave the
        // recommendations; their records still undo.
        for id in [
            "privacy.ceip",
            "gaming.multimedia_scheduling",
            "gaming.network_throttling",
        ] {
            let t = tweak(id).unwrap();
            assert_eq!(t.retired, Some(NO_OBSERVABLE_EFFECT), "{id}");
            assert!(!t.default_on, "{id}");
        }
        assert_eq!(
            tweak("performance.startup_delay").unwrap().retired,
            Some(
                "Cairn measured no earlier start of startup programs on Windows 11, so it no \
                 longer offers this."
            )
        );
        assert_eq!(
            tweak("interface.widgets").unwrap().retired,
            Some(
                "Windows 11 no longer lets programs set this policy, so Cairn no longer offers it."
            )
        );

        // Restart needs: the level at which the effect appeared on the test PC.
        use RestartNeed::{Restart, SignOut};
        for (id, restart) in [
            ("privacy.diagnostic_data", Restart),
            ("interface.explorer_ai_actions", Restart),
            ("privacy.notepad_ai", Restart),
            ("privacy.phone_link", Restart),
            ("interface.long_paths", Restart),
            ("performance.menu_delay", SignOut),
            ("privacy.lock_screen_notifications", RestartNeed::None),
            ("privacy.search_history", RestartNeed::None),
            ("privacy.search_cloud", RestartNeed::None),
            ("privacy.inking_typing", RestartNeed::None),
            ("privacy.language_list", RestartNeed::None),
            ("privacy.text_image_generation", RestartNeed::None),
            ("security.device_metadata", RestartNeed::None),
            ("security.remote_assistance", RestartNeed::None),
            ("privacy.error_reporting", RestartNeed::None),
        ] {
            assert_eq!(tweak(id).unwrap().restart, restart, "{id}");
        }

        // A policy Home honoured only as far as Windows Settings shows: the standard sentence,
        // and never recommended.
        for id in [
            "privacy.inking_typing",
            "privacy.text_image_generation",
            "privacy.search_cloud",
            "security.device_metadata",
        ] {
            let t = tweak(id).unwrap();
            assert!(t.description.ends_with(HOME_RULE_SENTENCE), "{id}");
            assert!(!t.default_on, "{id}");
        }
        for t in TWEAKS {
            assert_eq!(
                t.description.contains("Cairn could only confirm"),
                t.description.ends_with(HOME_RULE_SENTENCE),
                "{}",
                t.id
            );
        }

        // What was not observed on the test PC is stated as Microsoft's documentation; what was
        // observed is described as seen.
        for (id, phrase) in [
            (
                "interface.edge_rewards",
                "Microsoft documents that the same policies also hide Microsoft Rewards",
            ),
            (
                "interface.edge_default_browser_prompts",
                "Microsoft documents that Edge then stops checking",
            ),
            (
                "privacy.explorer_cloud_files",
                "Microsoft says File Explorer's Home page",
            ),
            (
                "gaming.background_recording",
                "Microsoft documents that Windows Game Recording isn't allowed",
            ),
            (
                "privacy.diagnostic_data",
                "also shows 'Improve inking and typing' off",
            ),
            (
                "privacy.lock_screen_notifications",
                "'Show reminders and incoming VoIP calls on the lock screen' is a separate \
                 setting and is not changed.",
            ),
            (
                "security.remote_assistance",
                "The checkbox is greyed out while this is applied.",
            ),
            (
                "privacy.notepad_ai",
                "the Copilot button and Write go after Windows restarts.",
            ),
            ("privacy.tailored_experiences", "'Personalized offers'"),
            ("privacy.advertising_id", "Recommendations & offers"),
        ] {
            assert!(
                tweak(id).unwrap().description.contains(phrase),
                "{id}: {phrase}"
            );
        }
        assert!(tweak("interface.edge_rewards")
            .unwrap()
            .description
            .starts_with("Removes the Adobe Acrobat subscription button"));
        for t in TWEAKS {
            assert!(
                !t.description.contains("Feedback frequency")
                    && !t.description.contains("Tailored experiences")
                    && !t.description.contains("Privacy & security › General"),
                "{}: names a Settings page Windows 11 26H2 no longer has",
                t.id
            );
        }

        // Tweaks whose effect no PC available for checks could show (the feature was absent,
        // had no control or was already in the tweak's state there) stay offered and are never
        // recommended.
        for id in [
            "privacy.feedback_prompts",
            "privacy.typing_data",
            "privacy.online_speech",
            "gaming.background_recording",
            "gaming.sticky_keys",
            "gaming.gpu_scheduling",
            "performance.fast_startup",
            "interface.edge_default_browser_prompts",
        ] {
            let t = tweak(id).unwrap();
            assert!(t.retired.is_none(), "{id}");
            assert!(!t.default_on, "{id}");
        }

        // The Office policies are kept for business and enterprise Office, which consumer
        // Microsoft 365 ignores: gated, never recommended.
        for id in [
            "privacy.office_telemetry",
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
        ] {
            let t = tweak(id).unwrap();
            assert!(!t.default_on, "{id}");
            assert_eq!(t.requires, Some(Requirement::OfficeBusiness), "{id}");
        }
    }

    #[test]
    fn new_tweaks_have_their_designed_fields() {
        const ANY: &[Build] = &[];
        const FROM_22H2: &[Build] = &[Build::new(22621, 0)];
        const FROM_24H2: &[Build] = &[Build::new(26100, 0)];
        use Requirement::{Edge, NotepadAi, SystemAiModels};
        use RestartNeed::{Explorer, Restart};
        let rows = [
            (
                "privacy.search_history",
                Topic::Search,
                Risk::Low,
                false,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "privacy.search_cloud",
                Topic::Search,
                Risk::Low,
                false,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "security.remote_assistance",
                Topic::Security,
                Risk::Low,
                true,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "privacy.notepad_ai",
                Topic::Ai,
                Risk::Low,
                false,
                Restart,
                Some(NotepadAi),
                FROM_22H2,
            ),
            (
                "interface.explorer_ai_actions",
                Topic::Ai,
                Risk::Low,
                false,
                Restart,
                None,
                FROM_24H2,
            ),
            (
                "privacy.text_image_generation",
                Topic::Ai,
                Risk::Medium,
                false,
                RestartNeed::None,
                Some(SystemAiModels),
                FROM_24H2,
            ),
            (
                "privacy.inking_typing",
                Topic::Telemetry,
                Risk::Low,
                false,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "privacy.language_list",
                Topic::Activity,
                Risk::Low,
                false,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "privacy.phone_link",
                Topic::Sync,
                Risk::Medium,
                false,
                Restart,
                None,
                ANY,
            ),
            (
                "security.device_metadata",
                Topic::Security,
                Risk::Low,
                false,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "privacy.start_suggestions",
                Topic::Start,
                Risk::Low,
                true,
                Explorer,
                None,
                FROM_24H2,
            ),
            (
                "privacy.setup_reminders",
                Topic::Start,
                Risk::Low,
                true,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "interface.explorer_sync_ads",
                Topic::Ads,
                Risk::Low,
                true,
                Explorer,
                None,
                ANY,
            ),
            (
                "privacy.lock_screen_notifications",
                Topic::Start,
                Risk::Low,
                true,
                RestartNeed::None,
                None,
                ANY,
            ),
            (
                "interface.explorer_home_gallery",
                Topic::Explorer,
                Risk::Low,
                false,
                Explorer,
                None,
                FROM_24H2,
            ),
            (
                "interface.long_paths",
                Topic::Files,
                Risk::Low,
                false,
                Restart,
                None,
                ANY,
            ),
            (
                "privacy.explorer_cloud_files",
                Topic::Explorer,
                Risk::Low,
                false,
                Explorer,
                None,
                ANY,
            ),
            (
                "interface.spotlight_icon",
                Topic::Visuals,
                Risk::Low,
                false,
                Explorer,
                None,
                ANY,
            ),
            (
                "interface.edge_default_browser_prompts",
                Topic::Edge,
                Risk::Low,
                false,
                RestartNeed::None,
                Some(Edge),
                ANY,
            ),
            (
                "privacy.edge_feedback",
                Topic::Edge,
                Risk::Low,
                true,
                RestartNeed::None,
                Some(Edge),
                ANY,
            ),
            (
                "interface.edge_first_run",
                Topic::Edge,
                Risk::Low,
                false,
                RestartNeed::None,
                Some(Edge),
                ANY,
            ),
            (
                "interface.edge_new_tab_page",
                Topic::Edge,
                Risk::Low,
                true,
                RestartNeed::None,
                Some(Edge),
                ANY,
            ),
            (
                "interface.edge_rewards",
                Topic::Edge,
                Risk::Low,
                false,
                RestartNeed::None,
                Some(Edge),
                ANY,
            ),
        ];
        for (id, topic, risk, recommended, restart, requires, from) in rows {
            let t = tweak(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
            assert_eq!(t.topic, topic, "{id}");
            assert_eq!(t.risk, risk, "{id}");
            assert_eq!(t.default_on, recommended, "{id}");
            assert_eq!(t.restart, restart, "{id}");
            assert_eq!(t.requires, requires, "{id}");
            assert_eq!(t.windows.from, from, "{id}");
            assert_eq!(t.windows.through, None, "{id}");
            assert_eq!(t.windows.editions, Editions::All, "{id}");
            assert!(t.retired.is_none() && t.former.is_empty(), "{id}");
            assert!(
                registry_actions(t).all(|r| matches!(r.data, RegData::Dword(_))),
                "{id}"
            );
        }
        for t in TWEAKS
            .iter()
            .filter(|t| t.requires == Some(Requirement::Edge))
        {
            let restart_policy = registry_actions(t).any(|r| {
                [
                    "ShowPDFDefaultRecommendationsEnabled",
                    "UserFeedbackAllowed",
                    "HideFirstRunExperience",
                    "ShowMicrosoftRewards",
                ]
                .contains(&r.name)
            });
            if restart_policy {
                assert!(
                    t.description.contains("Applies after Edge restarts."),
                    "{}",
                    t.id
                );
            }
        }
        assert!(tweak("privacy.text_image_generation")
            .unwrap()
            .description
            .contains(
                "Rated Medium like the background apps tweak: while this is applied you \
                 cannot allow single apps. This is a policy"
            ));
    }

    #[test]
    fn topic_labels_are_unique() {
        let labels: HashSet<&str> = Topic::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(labels.len(), Topic::ALL.len());
        let ids: HashSet<Topic> = Topic::ALL.into_iter().collect();
        assert_eq!(ids.len(), Topic::ALL.len());
        assert!(labels.iter().all(|l| !l.is_empty() && !l.ends_with('.')));
        assert_eq!(Topic::Files.label(), "Files and storage");
        assert_eq!(serde_json::to_value(Topic::Ai).unwrap(), "ai");
        assert_eq!(serde_json::to_value(Topic::Files).unwrap(), "files");
    }

    #[test]
    fn every_0_2_tweak_has_its_topic() {
        let table: [(Topic, &[&str]); 15] = [
            (
                Topic::Telemetry,
                &[
                    "privacy.diagnostic_data",
                    "privacy.diagtrack_service",
                    "privacy.feedback_prompts",
                    "privacy.feedback_tasks",
                    "privacy.error_reporting",
                    "privacy.error_report_task",
                    "privacy.ceip",
                    "privacy.ceip_tasks",
                    "privacy.compatibility_telemetry",
                ],
            ),
            (
                Topic::Ads,
                &[
                    "privacy.advertising_id",
                    "privacy.tailored_experiences",
                    "privacy.suggested_content",
                    "privacy.lock_screen_ads",
                ],
            ),
            (
                Topic::Activity,
                &[
                    "privacy.activity_history",
                    "privacy.app_launch_tracking",
                    "privacy.typing_data",
                    "privacy.online_speech",
                    "privacy.location",
                ],
            ),
            (Topic::Ai, &["privacy.recall", "privacy.copilot"]),
            (Topic::Search, &["privacy.cortana", "privacy.web_search"]),
            (
                Topic::Office,
                &[
                    "privacy.office_telemetry",
                    "privacy.office_connected_experiences",
                    "privacy.office_cloud_content",
                ],
            ),
            (
                Topic::Edge,
                &[
                    "privacy.edge_telemetry",
                    "privacy.edge_shopping",
                    "performance.edge_background",
                    "interface.edge_sidebar",
                ],
            ),
            (
                Topic::Games,
                &[
                    "gaming.background_recording",
                    "gaming.game_mode",
                    "gaming.multimedia_scheduling",
                    "gaming.gpu_scheduling",
                    "gaming.network_throttling",
                ],
            ),
            (
                Topic::Input,
                &["gaming.mouse_acceleration", "gaming.sticky_keys"],
            ),
            (
                Topic::Power,
                &[
                    "performance.power_plan",
                    "performance.startup_delay",
                    "performance.fast_startup",
                ],
            ),
            (
                Topic::Services,
                &[
                    "performance.sysmain",
                    "performance.maps_broker",
                    "performance.link_tracking",
                    "performance.background_apps",
                    "performance.search_indexer",
                ],
            ),
            (Topic::Updates, &["performance.delivery_optimization"]),
            (
                Topic::Visuals,
                &[
                    "performance.menu_delay",
                    "performance.transparency",
                    "interface.dark_mode",
                ],
            ),
            (
                Topic::Explorer,
                &[
                    "interface.file_extensions",
                    "interface.hidden_files",
                    "interface.classic_context_menu",
                    "interface.open_this_pc",
                ],
            ),
            (
                Topic::Taskbar,
                &[
                    "interface.widgets",
                    "interface.end_task",
                    "interface.taskbar_left",
                    "interface.task_view_button",
                    "interface.search_icon",
                ],
            ),
        ];
        let mut listed = HashSet::new();
        for (topic, ids) in table {
            for id in ids {
                assert!(listed.insert(*id), "{id} is listed twice");
                let t = tweak(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
                assert_eq!(t.topic, topic, "{id}");
            }
        }
        assert_eq!(listed.len(), 57, "every tweak of Cairn 0.2");
    }

    #[test]
    fn windows_ranges_describe_their_builds_and_editions() {
        const ONE: &[Build] = &[Build::new(26100, 3915)];
        const LEGACY: &[Build] = &[Build::new(22621, 2361)];
        const TWO: &[Build] = &[Build::new(22621, 4870), Build::new(26100, 3360)];
        const THREE: &[Build] = &[
            Build::new(22000, 1),
            Build::new(22621, 2),
            Build::new(26100, 3),
        ];
        const ENABLEMENT: &[Build] = &[Build::new(26200, 0)];
        let range = |from: &'static [Build], through: Option<u32>, editions: Editions| {
            WindowsRange {
                from,
                through,
                editions,
            }
            .describe()
            .unwrap()
        };
        assert_eq!(ANY_WINDOWS.describe(), None);
        assert_eq!(
            range(ONE, None, Editions::All),
            "Windows 11 24H2 (build 26100.3915) or later"
        );
        assert_eq!(
            range(LEGACY, Some(22621), Editions::All),
            "Windows 11 22H2 (build 22621.2361) or later through 23H2"
        );
        assert_eq!(
            range(TWO, None, Editions::All),
            "Windows 11 22H2 (build 22621.4870 or later) or 24H2 (build 26100.3360 or later)"
        );
        assert_eq!(
            range(THREE, None, Editions::All),
            "Windows 11 21H2 (build 22000.1 or later), 22H2 (build 22621.2 or later) or \
             24H2 (build 26100.3 or later)"
        );
        assert_eq!(
            range(ENABLEMENT, None, Editions::ProOrHigher),
            "Windows 11 25H2 (build 26200.0) or later; Pro, Enterprise or Education"
        );
        assert_eq!(
            range(&[], Some(22621), Editions::All),
            "Windows 11 23H2 and earlier"
        );
        assert_eq!(
            range(&[], None, Editions::EnterpriseOrEducation),
            "Windows 11 Enterprise or Education"
        );
        assert_eq!(Build::new(26300, 9457).to_string(), "26300.9457");
        assert!(Build::new(26100, 9550) < Build::new(26300, 1));
        assert_eq!(
            serde_json::to_value(Editions::ProOrHigher).unwrap(),
            "pro_or_higher"
        );
    }
}
