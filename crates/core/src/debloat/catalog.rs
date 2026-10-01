//! The fixed catalog of tweaks and bloatware packages.
//!
//! Every tweak is a list of primitive actions (a registry value, a service start type, a
//! scheduled task's enabled state or the power scheme). The engine never changes anything
//! that is not described here, and every target appears in at most one tweak, so reverting
//! one tweak never touches another.
//!
//! Policy values under `SOFTWARE\Policies` are read by the Windows components themselves
//! on every edition. Where an edition limits a policy, the description says so.

use serde::{Deserialize, Serialize};

use crate::win::registry::Hive;
use crate::win::scm::StartType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Privacy,
    Gaming,
    Performance,
    Interface,
    Bloatware,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Privacy,
        Category::Gaming,
        Category::Performance,
        Category::Interface,
        Category::Bloatware,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Privacy => "privacy",
            Category::Gaming => "gaming",
            Category::Performance => "performance",
            Category::Interface => "interface",
            Category::Bloatware => "bloatware",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        match s.trim().to_ascii_lowercase().as_str() {
            "privacy" => Some(Category::Privacy),
            "gaming" => Some(Category::Gaming),
            "performance" => Some(Category::Performance),
            "interface" | "tweaks" => Some(Category::Interface),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// Microsoft Office 2016 or later (version 16.0; Click-to-Run or Windows Installer).
    Office,
    /// Microsoft Edge.
    Edge,
    /// A graphics driver that supports hardware-accelerated GPU scheduling.
    GpuScheduling,
}

impl Requirement {
    /// Why a tweak with this requirement is not offered on this PC.
    pub fn missing_text(self) -> &'static str {
        match self {
            Requirement::Office => "Microsoft Office (2016 or later) is not installed.",
            Requirement::Edge => "Microsoft Edge is not installed.",
            Requirement::GpuScheduling => {
                "No graphics driver on this PC supports hardware-accelerated GPU scheduling."
            }
        }
    }

    /// Subject of a failed check, as in "cannot check whether <subject>".
    pub fn subject(self) -> &'static str {
        match self {
            Requirement::Office => "Microsoft Office is installed",
            Requirement::Edge => "Microsoft Edge is installed",
            Requirement::GpuScheduling => "the graphics driver supports GPU scheduling",
        }
    }
}

#[derive(Debug)]
pub struct Tweak {
    pub id: &'static str,
    pub category: Category,
    pub title: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    /// Selected when the user enables the whole category.
    pub default_on: bool,
    pub restart: RestartNeed,
    /// What this PC needs for the tweak to have any effect; without it the tweak is not
    /// offered.
    pub requires: Option<Requirement>,
    pub actions: &'static [Action],
}

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

const fn dword(hive: Hive, path: &'static str, name: &'static str, value: u32) -> Action {
    Action::Registry(RegistryAction {
        hive,
        path,
        name,
        data: RegData::Dword(value),
    })
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
/// `HOTKEYACTIVE` in the StickyKeys, FilterKeys (Keyboard Response) and ToggleKeys flags:
/// the keyboard shortcut that turns the feature on and opens its prompt.
const HOTKEYACTIVE: u32 = 0x4;

pub static TWEAKS: &[Tweak] = &[
    // ───────────────────────────── Privacy ─────────────────────────────
    Tweak {
        id: "privacy.diagnostic_data",
        category: Category::Privacy,
        title: "Limit diagnostic data",
        description:
            "Sets the AllowTelemetry policy to its lowest value. Enterprise and Education \
                      stop sending diagnostic data; Home and Pro treat the value as 'Required', \
                      the minimum those editions allow.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Restart,
        requires: None,
        actions: &[dword(HKLM, DATA_COLLECTION, "AllowTelemetry", 0)],
    },
    Tweak {
        id: "privacy.diagtrack_service",
        category: Category::Privacy,
        title: "Turn off the telemetry service",
        description: "Stops and disables DiagTrack (Connected User Experiences and Telemetry), \
                      the service that uploads diagnostic data.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[service("DiagTrack", StartType::Disabled, true)],
    },
    Tweak {
        id: "privacy.activity_history",
        category: Category::Privacy,
        title: "Turn off Activity History",
        description: "Stops Windows from collecting, publishing and uploading the list of apps, \
                      files and sites you use.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[
            dword(HKLM, POLICY_SYSTEM, "EnableActivityFeed", 0),
            dword(HKLM, POLICY_SYSTEM, "PublishUserActivities", 0),
            dword(HKLM, POLICY_SYSTEM, "UploadUserActivities", 0),
        ],
    },
    Tweak {
        id: "privacy.cortana",
        category: Category::Privacy,
        title: "Disable Cortana",
        description: "Sets the AllowCortana policy to off. The Cortana app itself is listed \
                      separately under bloatware.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\Windows Search",
            "AllowCortana",
            0,
        )],
    },
    Tweak {
        id: "privacy.advertising_id",
        category: Category::Privacy,
        title: "Disable the advertising ID",
        description: "Stops apps from using a per-user advertising identifier for targeted ads.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\AdvertisingInfo",
            "DisabledByGroupPolicy",
            1,
        )],
    },
    Tweak {
        id: "privacy.tailored_experiences",
        category: Category::Privacy,
        title: "Disable tailored experiences",
        description: "Stops Microsoft from using diagnostic data to personalise tips, ads and \
                      recommendations.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Policies\Microsoft\Windows\CloudContent",
            "DisableTailoredExperiencesWithDiagnosticData",
            1,
        )],
    },
    Tweak {
        id: "privacy.suggested_content",
        category: Category::Privacy,
        title: "Stop suggested apps and ads",
        description: "Turns off silent installation of promoted apps (such as Candy Crush), Start \
                      menu suggestions, tips and suggested content in Settings.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[
            dword(HKCU, CONTENT_DELIVERY, "SilentInstalledAppsEnabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SystemPaneSuggestionsEnabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338388Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338389Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338393Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-353694Enabled", 0),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-353696Enabled", 0),
        ],
    },
    Tweak {
        id: "privacy.web_search",
        category: Category::Privacy,
        title: "Remove web results from Start search",
        description: "Start menu search only shows local results and no longer sends what you \
                      type to Bing.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Policies\Microsoft\Windows\Explorer",
            "DisableSearchBoxSuggestions",
            1,
        )],
    },
    Tweak {
        id: "privacy.feedback_prompts",
        category: Category::Privacy,
        title: "Stop feedback prompts",
        description: "Windows no longer asks for feedback or shows feedback notifications.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            dword(
                HKCU,
                r"Software\Microsoft\Siuf\Rules",
                "NumberOfSIUFInPeriod",
                0,
            ),
            dword(HKLM, DATA_COLLECTION, "DoNotShowFeedbackNotifications", 1),
        ],
    },
    Tweak {
        id: "privacy.feedback_tasks",
        category: Category::Privacy,
        title: "Turn off the feedback survey tasks",
        description: "Turns off the two Feedback tasks (DmClient) that download feedback surveys \
                      and scenario data for Windows feedback prompts.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            disable_task(r"\Microsoft\Windows\Feedback\Siuf\DmClient"),
            disable_task(r"\Microsoft\Windows\Feedback\Siuf\DmClientOnScenarioDownload"),
        ],
    },
    // ───────────────────────────── Gaming ─────────────────────────────
    Tweak {
        id: "gaming.background_recording",
        category: Category::Gaming,
        title: "Turn off background game recording",
        description: "Disables Game DVR capture, which records gameplay in the background and \
                      uses the GPU encoder. Win+Alt+R clip recording stops working.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
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
    },
    Tweak {
        id: "gaming.game_mode",
        category: Category::Gaming,
        title: "Turn on Game Mode",
        description: "Lets Windows prioritise the foreground game and hold back Windows Update \
                      installs and notifications while you play.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            dword(
                HKCU,
                r"Software\Microsoft\GameBar",
                "AutoGameModeEnabled",
                1,
            ),
            dword(HKCU, r"Software\Microsoft\GameBar", "AllowAutoGameMode", 1),
        ],
    },
    Tweak {
        id: "gaming.multimedia_scheduling",
        category: Category::Gaming,
        title: "Prioritise games in multimedia scheduling",
        description: "Reserves less CPU time for background multimedia work and raises the \
                      scheduling priority of the Games task. The effect is small and depends on \
                      the game.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Restart,
        requires: None,
        actions: &[
            dword(HKLM, MMCSS_PROFILE, "SystemResponsiveness", 10),
            dword(HKLM, MMCSS_GAMES, "GPU Priority", 8),
            dword(HKLM, MMCSS_GAMES, "Priority", 6),
            sz(HKLM, MMCSS_GAMES, "Scheduling Category", "High"),
            sz(HKLM, MMCSS_GAMES, "SFIO Priority", "High"),
        ],
    },
    Tweak {
        id: "gaming.mouse_acceleration",
        category: Category::Gaming,
        title: "Turn off mouse acceleration",
        description: "Turns off 'Enhance pointer precision' so the pointer moves the same \
                      distance for the same hand movement. Changes how the mouse feels on the \
                      desktop. Takes effect at once.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            sz(HKCU, MOUSE_KEY, "MouseSpeed", "0"),
            sz(HKCU, MOUSE_KEY, "MouseThreshold1", "0"),
            sz(HKCU, MOUSE_KEY, "MouseThreshold2", "0"),
        ],
    },
    // ───────────────────────────── Performance ─────────────────────────────
    Tweak {
        id: "performance.power_plan",
        category: Category::Performance,
        title: "Use the Ultimate Performance power plan",
        description: "Keeps the CPU at high clocks and disables most power saving. Raises power \
                      use, heat and fan noise; not recommended on battery.",
        risk: Risk::Medium,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[Action::Power(PowerPlan::UltimatePerformance)],
    },
    Tweak {
        id: "performance.sysmain",
        category: Category::Performance,
        title: "Set SysMain (Superfetch) to manual",
        description: "Stops SysMain, which preloads frequently used apps into memory. On SSDs \
                      the benefit is small and it causes background disk and CPU activity.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[service("SysMain", StartType::Manual, true)],
    },
    Tweak {
        id: "performance.maps_broker",
        category: Category::Performance,
        title: "Set Downloaded Maps Manager to manual",
        description: "Stops the service that updates offline maps in the background. The Maps \
                      app starts it when needed.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[service("MapsBroker", StartType::Manual, true)],
    },
    Tweak {
        id: "performance.link_tracking",
        category: Category::Performance,
        title: "Set Distributed Link Tracking to manual",
        description: "Stops the service that repairs shortcuts to files moved between NTFS \
                      volumes on a network domain. Rarely needed on home PCs.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[service("TrkWks", StartType::Manual, true)],
    },
    Tweak {
        id: "performance.startup_delay",
        category: Category::Performance,
        title: "Remove the startup app delay",
        description: "Starts apps in the Startup folder and Run keys right after sign-in instead \
                      of after a delay. Sign-in can feel busier on slow disks.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Serialize",
            "StartupDelayInMSec",
            0,
        )],
    },
    Tweak {
        id: "performance.background_apps",
        category: Category::Performance,
        title: "Stop Store apps running in the background",
        description: "Store apps can no longer run in the background. Notifications and syncing \
                      from apps such as Phone Link, Mail and Teams may stop until you open them.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\AppPrivacy",
            "LetAppsRunInBackground",
            2,
        )],
    },
    Tweak {
        id: "performance.search_indexer",
        category: Category::Performance,
        title: "Set Windows Search indexing to manual",
        description: "Stops the search indexer and its background disk activity. Start menu, \
                      File Explorer and Outlook search become slower and less complete.",
        risk: Risk::High,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        actions: &[service("WSearch", StartType::Manual, true)],
    },
    Tweak {
        id: "performance.edge_background",
        category: Category::Performance,
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
        actions: &[
            dword(HKLM, EDGE_POLICY, "StartupBoostEnabled", 0),
            dword(HKLM, EDGE_POLICY, "BackgroundModeEnabled", 0),
        ],
    },
    Tweak {
        id: "performance.delivery_optimization",
        category: Category::Performance,
        title: "Stop sharing updates with other PCs",
        description: "Windows Update downloads only from Microsoft and no longer uploads \
                      update files to other PCs, which frees upload bandwidth.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
            "DODownloadMode",
            0,
        )],
    },
    Tweak {
        id: "performance.menu_delay",
        category: Category::Performance,
        title: "Faster menus",
        description: "Shortens the delay before menus and submenus open from 400 ms to 100 ms.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[sz(HKCU, r"Control Panel\Desktop", "MenuShowDelay", "100")],
    },
    Tweak {
        id: "performance.transparency",
        category: Category::Performance,
        title: "Turn off transparency effects",
        description: "Replaces the blurred, see-through taskbar and Start menu with solid \
                      colours, which saves a little GPU work.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(HKCU, PERSONALIZE, "EnableTransparency", 0)],
    },
    Tweak {
        id: "performance.fast_startup",
        category: Category::Performance,
        title: "Turn off Fast Startup",
        description: "Shut down fully instead of hibernating the kernel. Boot takes a few \
                      seconds longer, but driver and update problems clear on every shutdown.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Power",
            "HiberbootEnabled",
            0,
        )],
    },
    // ───────────────────────────── Privacy (continued) ─────────────────────────────
    Tweak {
        id: "privacy.recall",
        category: Category::Privacy,
        title: "Turn off Recall snapshots",
        description: "Stops Windows Recall from saving snapshots of your screen. Recall only \
                      exists on Copilot+ PCs; elsewhere the policy has no effect.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[
            dword(HKLM, WINDOWS_AI_POLICY, "DisableAIDataAnalysis", 1),
            dword(HKCU, WINDOWS_AI_POLICY_USER, "DisableAIDataAnalysis", 1),
        ],
    },
    Tweak {
        id: "privacy.copilot",
        category: Category::Privacy,
        title: "Hide the legacy Copilot sidebar (23H2 and earlier)",
        description: "Sets the Turn off Windows Copilot policy, which only hides the Copilot \
                      sidebar of Windows 11 23H2 and earlier. Windows 11 24H2 and later ignore \
                      it: Copilot is the Copilot app there, so Win+C and the Copilot key keep \
                      working, and this item still shows as applied because the policy is set. \
                      Remove the Copilot app under bloatware to turn Copilot off.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::SignOut,
        requires: None,
        actions: &[
            dword(HKLM, COPILOT_POLICY, "TurnOffWindowsCopilot", 1),
            dword(HKCU, COPILOT_POLICY_USER, "TurnOffWindowsCopilot", 1),
        ],
    },
    Tweak {
        id: "privacy.app_launch_tracking",
        category: Category::Privacy,
        title: "Stop tracking app launches",
        description: "Windows no longer records which apps you start. The Start menu's \
                      most-used list stops updating.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "Start_TrackProgs", 0)],
    },
    Tweak {
        id: "privacy.typing_data",
        category: Category::Privacy,
        title: "Stop collecting typing and inking data",
        description: "Turns off the collection of typed text, handwriting and contacts used to \
                      personalise suggestions.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
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
    },
    Tweak {
        id: "privacy.online_speech",
        category: Category::Privacy,
        title: "Turn off online speech recognition",
        description: "Voice input is no longer sent to Microsoft. Voice typing (Win+H) and \
                      other cloud dictation stop working until this is undone; voice access \
                      runs on the device and keeps working.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Speech_OneCore\Settings\OnlineSpeechPrivacy",
            "HasAccepted",
            0,
        )],
    },
    Tweak {
        id: "privacy.error_reporting",
        category: Category::Privacy,
        title: "Turn off Windows Error Reporting",
        description: "Crash and hang reports are no longer sent to Microsoft.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Microsoft\Windows\Windows Error Reporting",
            "Disabled",
            1,
        )],
    },
    Tweak {
        id: "privacy.error_report_task",
        category: Category::Privacy,
        title: "Turn off the error report upload task",
        description: "Turns off the task that sends queued crash and hang reports to Microsoft. \
                      Reports already queued stay on this PC.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[disable_task(
            r"\Microsoft\Windows\Windows Error Reporting\QueueReporting",
        )],
    },
    Tweak {
        id: "privacy.ceip",
        category: Category::Privacy,
        title: "Leave the Customer Experience Improvement Program",
        description: "Stops the usage statistics collected by the Customer Experience \
                      Improvement Program.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\SQMClient\Windows",
            "CEIPEnable",
            0,
        )],
    },
    Tweak {
        id: "privacy.ceip_tasks",
        category: Category::Privacy,
        title: "Turn off the Customer Experience Improvement Program tasks",
        description: "Turns off the scheduled tasks that collect usage, USB, disk and check-disk \
                      statistics for the Customer Experience Improvement Program. Tasks this \
                      version of Windows does not have are skipped. A run already in progress \
                      finishes; later runs no longer start.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
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
    },
    Tweak {
        id: "privacy.lock_screen_ads",
        category: Category::Privacy,
        title: "Remove lock screen tips and ads",
        description: "Turns off the fun facts, tips and promotions shown on a Picture or \
                      Slideshow lock screen. It has no effect while the lock screen uses Windows \
                      Spotlight, which shows its own tips; switch the lock screen to Picture or \
                      Slideshow in Settings to remove those too.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            dword(
                HKCU,
                CONTENT_DELIVERY,
                "RotatingLockScreenOverlayEnabled",
                0,
            ),
            dword(HKCU, CONTENT_DELIVERY, "SubscribedContent-338387Enabled", 0),
        ],
    },
    Tweak {
        id: "privacy.location",
        category: Category::Privacy,
        title: "Turn off location services",
        description: "No app or Windows feature can use your location. Find my device, Maps \
                      and local weather lose location.",
        risk: Risk::Medium,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Windows\LocationAndSensors",
            "DisableLocation",
            1,
        )],
    },
    Tweak {
        id: "privacy.compatibility_telemetry",
        category: Category::Privacy,
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
    },
    Tweak {
        id: "privacy.office_telemetry",
        category: Category::Privacy,
        title: "Stop Office diagnostic data, feedback and surveys",
        description: "Sets the Office policies that stop Word, Excel, Outlook and the other \
                      Microsoft 365 apps from sending diagnostic data, and turns off Send \
                      Feedback and in-app surveys. Service data that Office needs to stay \
                      licensed and up to date is still sent. The Office apps pick this up the \
                      next time they start.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
        actions: &[
            dword(HKCU, OFFICE_TELEMETRY_POLICY, "SendTelemetry", 3),
            dword(HKCU, OFFICE_FEEDBACK_POLICY, "Enabled", 0),
            dword(HKCU, OFFICE_FEEDBACK_POLICY, "SurveyEnabled", 0),
        ],
    },
    Tweak {
        id: "privacy.office_connected_experiences",
        category: Category::Privacy,
        title: "Turn off Office's optional connected experiences",
        description: "Turns off the extras Office offers through other Microsoft online \
                      services, such as 3D Maps in Excel and inserting online pictures from \
                      Bing. Saving to OneDrive, co-authoring and basic spelling keep working. \
                      The Office apps pick this up the next time they start.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Office),
        actions: &[dword(
            HKCU,
            OFFICE_PRIVACY_POLICY,
            "ControllerConnectedServicesEnabled",
            2,
        )],
    },
    Tweak {
        id: "privacy.office_cloud_content",
        category: Category::Privacy,
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
        requires: Some(Requirement::Office),
        actions: &[
            dword(HKCU, OFFICE_PRIVACY_POLICY, "UserContentDisabled", 2),
            dword(HKCU, OFFICE_PRIVACY_POLICY, "DownloadContentDisabled", 2),
        ],
    },
    Tweak {
        id: "privacy.edge_telemetry",
        category: Category::Privacy,
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
        actions: &[
            dword(HKLM, EDGE_POLICY, "DiagnosticData", 0),
            dword(HKLM, EDGE_POLICY, "PersonalizationReportingEnabled", 0),
        ],
    },
    Tweak {
        id: "privacy.edge_shopping",
        category: Category::Privacy,
        title: "Turn off Edge shopping, tips and promotions",
        description: "Turns off price comparison, coupons and express checkout on shopping \
                      sites, Edge's feature recommendations, and the promotional backgrounds and \
                      tips from Microsoft services. Edge ignores the shopping and recommendation \
                      policies in profiles signed in with a personal Microsoft account; turn \
                      those off in Edge's settings there. These are Edge policies, so while this \
                      is applied Edge shows 'Managed by your organization'; undoing it removes \
                      the message unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
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
    },
    // ───────────────────────────── Gaming (continued) ─────────────────────────────
    Tweak {
        id: "gaming.sticky_keys",
        category: Category::Gaming,
        title: "Turn off the Sticky, Filter and Toggle Keys shortcuts",
        description: "Pressing Shift five times, holding right Shift for eight seconds or \
                      holding Num Lock for five seconds no longer interrupts a game with the \
                      Sticky, Filter or Toggle Keys prompt. Only the shortcuts change: the \
                      features stay on or off as they are, with their other settings.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::SignOut,
        requires: None,
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
    },
    Tweak {
        id: "gaming.gpu_scheduling",
        category: Category::Gaming,
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
        actions: &[dword(HKLM, GRAPHICS_DRIVERS, "HwSchMode", 2)],
    },
    Tweak {
        id: "gaming.network_throttling",
        category: Category::Gaming,
        title: "Turn off network throttling for multimedia",
        description: "Removes the packet-rate limit Windows applies while media plays. Can \
                      reduce latency spikes when streaming or playing music during a game; \
                      little effect otherwise.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Restart,
        requires: None,
        actions: &[dword(
            HKLM,
            MMCSS_PROFILE,
            "NetworkThrottlingIndex",
            0xFFFF_FFFF,
        )],
    },
    // ───────────────────────────── Interface ─────────────────────────────
    Tweak {
        id: "interface.file_extensions",
        category: Category::Interface,
        title: "Show file extensions",
        description: "File Explorer shows extensions such as .exe and .pdf, which makes \
                      disguised files easier to spot.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "HideFileExt", 0)],
    },
    Tweak {
        id: "interface.hidden_files",
        category: Category::Interface,
        title: "Show hidden files",
        description: "File Explorer shows hidden files and folders such as AppData.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "Hidden", 1)],
    },
    Tweak {
        id: "interface.classic_context_menu",
        category: Category::Interface,
        title: "Classic right-click menu",
        description: "Right-click shows the full menu straight away instead of the compact \
                      Windows 11 menu with 'Show more options'.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[sz(
            HKCU,
            r"Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}\InprocServer32",
            "",
            "",
        )],
    },
    Tweak {
        id: "interface.widgets",
        category: Category::Interface,
        title: "Remove Widgets",
        description: "Removes the Widgets board and the news and weather button from the \
                      taskbar.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(
            HKLM,
            r"SOFTWARE\Policies\Microsoft\Dsh",
            "AllowNewsAndInterests",
            0,
        )],
    },
    Tweak {
        id: "interface.end_task",
        category: Category::Interface,
        title: "Add End task to the taskbar menu",
        description: "Right-clicking an app on the taskbar offers End task, like Task Manager.",
        risk: Risk::Low,
        default_on: true,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced\TaskbarDeveloperSettings",
            "TaskbarEndTask",
            1,
        )],
    },
    Tweak {
        id: "interface.taskbar_left",
        category: Category::Interface,
        title: "Align the taskbar to the left",
        description: "Moves the Start button and taskbar icons to the left edge, as in \
                      Windows 10.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "TaskbarAl", 0)],
    },
    Tweak {
        id: "interface.task_view_button",
        category: Category::Interface,
        title: "Hide the Task View button",
        description: "Removes the Task View button from the taskbar. Win+Tab still works.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "ShowTaskViewButton", 0)],
    },
    Tweak {
        id: "interface.search_icon",
        category: Category::Interface,
        title: "Show search as an icon",
        description: "Replaces the wide taskbar search box with a search icon.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(
            HKCU,
            r"Software\Microsoft\Windows\CurrentVersion\Search",
            "SearchboxTaskbarMode",
            1,
        )],
    },
    Tweak {
        id: "interface.open_this_pc",
        category: Category::Interface,
        title: "Open File Explorer to This PC",
        description: "File Explorer opens on This PC (drives and folders) instead of Home.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::Explorer,
        requires: None,
        actions: &[dword(HKCU, EXPLORER_ADVANCED, "LaunchTo", 1)],
    },
    Tweak {
        id: "interface.dark_mode",
        category: Category::Interface,
        title: "Dark mode",
        description: "Switches Windows and apps that follow the system theme to dark mode.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: None,
        actions: &[
            dword(HKCU, PERSONALIZE, "AppsUseLightTheme", 0),
            dword(HKCU, PERSONALIZE, "SystemUsesLightTheme", 0),
        ],
    },
    Tweak {
        id: "interface.edge_sidebar",
        category: Category::Interface,
        title: "Hide the Edge sidebar",
        description: "Removes the sidebar on the right of the Edge window. Edge ignores this \
                      policy in profiles signed in with a personal Microsoft account; turn off \
                      the sidebar in Edge's settings there. The Copilot button has its own \
                      setting in Edge. This is an Edge policy, so while it is applied Edge \
                      shows 'Managed by your organization'; undoing it removes the message \
                      unless other Edge policies are set.",
        risk: Risk::Low,
        default_on: false,
        restart: RestartNeed::None,
        requires: Some(Requirement::Edge),
        actions: &[dword(HKLM, EDGE_POLICY, "HubsSidebarEnabled", 0)],
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

/// Restart need of the tweak that writes this registry value (hive exact, path and name
/// ignoring ASCII case); [`RestartNeed::None`] for values no tweak writes.
pub fn restart_for_registry(hive: Hive, key_path: &str, value_name: &str) -> RestartNeed {
    restart_for(|a| {
        matches!(a, Action::Registry(r) if r.hive == hive
            && r.path.eq_ignore_ascii_case(key_path)
            && r.name.eq_ignore_ascii_case(value_name))
    })
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

        assert_eq!(tweaks_in(Category::Privacy).count(), 27);
        assert_eq!(TWEAKS.len(), 57);
        assert_eq!(TWEAKS.iter().filter(|t| t.default_on).count(), 36);
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
            assert_eq!(t.requires == Some(Requirement::Office), office, "{}", t.id);
            assert_eq!(
                t.requires == Some(Requirement::GpuScheduling),
                gpu,
                "{}",
                t.id
            );
            let Some(requirement) = t.requires else {
                continue;
            };
            assert!(
                t.actions.iter().all(|a| match a {
                    Action::Registry(r) => match requirement {
                        Requirement::Edge => r.path.eq_ignore_ascii_case(EDGE_POLICY),
                        Requirement::Office => is_office_policy(r.path),
                        Requirement::GpuScheduling => r.path.eq_ignore_ascii_case(GRAPHICS_DRIVERS),
                    },
                    Action::Service(_) | Action::ScheduledTask(_) | Action::Power(_) => false,
                }),
                "{} mixes its product's policies with other targets",
                t.id
            );
        }
        let required: Vec<(&str, Requirement)> = TWEAKS
            .iter()
            .filter_map(|t| t.requires.map(|r| (t.id, r)))
            .collect();
        assert_eq!(
            required,
            [
                ("performance.edge_background", Requirement::Edge),
                ("privacy.office_telemetry", Requirement::Office),
                ("privacy.office_connected_experiences", Requirement::Office),
                ("privacy.office_cloud_content", Requirement::Office),
                ("privacy.edge_telemetry", Requirement::Edge),
                ("privacy.edge_shopping", Requirement::Edge),
                ("gaming.gpu_scheduling", Requirement::GpuScheduling),
                ("interface.edge_sidebar", Requirement::Edge),
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

    #[test]
    fn per_profile_edge_policies_mention_personal_accounts() {
        for id in ["privacy.edge_shopping", "interface.edge_sidebar"] {
            assert!(
                tweak(id)
                    .unwrap()
                    .description
                    .contains("personal Microsoft account"),
                "{id}"
            );
        }
    }

    #[test]
    fn office_and_edge_policies_are_dwords() {
        for t in TWEAKS.iter().filter(|t| {
            matches!(
                t.requires,
                Some(Requirement::Office) | Some(Requirement::Edge)
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
        let position = |id: &str| TWEAKS.iter().position(|t| t.id == id).unwrap();
        let privacy = [
            "privacy.compatibility_telemetry",
            "privacy.office_telemetry",
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
            "privacy.edge_telemetry",
            "privacy.edge_shopping",
        ];
        for pair in privacy.windows(2) {
            assert_eq!(position(pair[0]) + 1, position(pair[1]), "{}", pair[1]);
        }
        assert_eq!(
            TWEAKS[position("privacy.edge_shopping") + 1].id,
            "gaming.sticky_keys"
        );
        assert_eq!(
            position("interface.dark_mode") + 1,
            position("interface.edge_sidebar")
        );
        let last_interface = TWEAKS
            .iter()
            .rposition(|t| t.category == Category::Interface)
            .unwrap();
        assert_eq!(TWEAKS[last_interface].id, "interface.edge_sidebar");

        let defaults: Vec<&str> = [
            "privacy.office_telemetry",
            "privacy.office_connected_experiences",
            "privacy.office_cloud_content",
            "privacy.edge_telemetry",
            "privacy.edge_shopping",
            "interface.edge_sidebar",
        ]
        .into_iter()
        .filter(|id| tweak(id).unwrap().default_on)
        .collect();
        assert_eq!(
            defaults,
            ["privacy.office_telemetry", "privacy.edge_telemetry"]
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
}
