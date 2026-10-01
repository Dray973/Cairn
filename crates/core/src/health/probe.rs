//! The readers behind the checkup: the `Probe` seam, the raw readings it returns, the
//! context computed before the reads start, where per-user values are read, and the live
//! probe of this PC.

use crate::sysinfo::SecurityInfo;
use crate::win::registry::{Hive, Key, RegValue};
use crate::win::session::{self, SessionUser};
use crate::Result;

use super::checkup::{Source, SourceError};
use super::{accounts, apps, device, network, protection, updates};

/// Facts about this process and session, read once before the source threads start.
#[derive(Debug, Clone)]
pub(crate) struct Context {
    pub(crate) elevated: bool,
    /// `session::elevated_as_other_user`, `None` when it could not be decided.
    pub(crate) other_user: Option<bool>,
    pub(crate) has_battery: bool,
    /// The user signed in to this session; `Ok(None)` when there is none.
    pub(crate) session: std::result::Result<Option<SessionUser>, String>,
    /// Where the signed-in user's own settings are read.
    pub(crate) hive: UserHive,
}

impl Context {
    /// A context with `hive` resolved from the other inputs.
    pub(crate) fn new(
        elevated: bool,
        other_user: Option<bool>,
        has_battery: bool,
        session: std::result::Result<Option<SessionUser>, String>,
    ) -> Context {
        let hive = UserHive::resolve(other_user, &session);
        Context {
            elevated,
            other_user,
            has_battery,
            session,
            hive,
        }
    }

    /// The context of this process.
    pub(crate) fn live() -> Context {
        Context::new(
            crate::is_elevated(),
            session::elevated_as_other_user().ok(),
            crate::debloat::power::has_system_battery(),
            session::session_user().map_err(|e| e.to_string()),
        )
    }
}

/// Where per-user values are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserHive {
    /// HKEY_CURRENT_USER: Cairn runs as the signed-in user.
    Current,
    /// `HKEY_USERS\<sid>`: the signed-in user's hive, read from another account.
    Users(String),
    /// Per-user values cannot be read; the note says why.
    Unavailable(String),
}

pub(crate) const NO_SESSION_NOTE: &str =
    "No user is signed in to this session; per-user settings were not checked.";
pub(crate) const UNKNOWN_ACCOUNT_NOTE: &str =
    "Could not tell which account is signed in; per-user settings were not checked.";

impl UserHive {
    /// HKCU when Cairn runs as the signed-in user, the signed-in user's hive when it runs as
    /// another account or that cannot be decided, otherwise unavailable.
    pub(crate) fn resolve(
        other_user: Option<bool>,
        session: &std::result::Result<Option<SessionUser>, String>,
    ) -> UserHive {
        match (session, other_user) {
            (Ok(None), _) => UserHive::Unavailable(NO_SESSION_NOTE.into()),
            (_, Some(false)) => UserHive::Current,
            (Ok(Some(user)), _) => UserHive::Users(user.sid.clone()),
            (Err(_), _) => UserHive::Unavailable(UNKNOWN_ACCOUNT_NOTE.into()),
        }
    }

    /// Opens the per-user key `path` (relative to the user's hive) for reading.
    pub(crate) fn open(&self, path: &str) -> Result<Option<Key>> {
        match self {
            UserHive::Current => Key::open(Hive::CurrentUser, path, false),
            UserHive::Users(sid) => Key::open(Hive::Users, &format!(r"{sid}\{path}"), false),
            UserHive::Unavailable(note) => Err(crate::Error::Other(note.clone())),
        }
    }
}

/// Windows edition and version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OsRaw {
    /// `EditionID`: "Core" and its variants are Home editions.
    pub(crate) edition_id: String,
    pub(crate) build: u32,
    pub(crate) fast_startup: Option<bool>,
}

impl OsRaw {
    pub(crate) fn home(&self) -> bool {
        self.edition_id.to_ascii_lowercase().starts_with("core")
    }
}

/// Every source's reading, or the message of why it failed.
#[derive(Debug, Clone)]
pub(crate) struct CheckupRaw {
    pub(crate) os: std::result::Result<OsRaw, String>,
    pub(crate) device: std::result::Result<SecurityInfo, String>,
    pub(crate) security_center: std::result::Result<protection::WscRaw, String>,
    pub(crate) defender: std::result::Result<protection::DefenderRead, String>,
    pub(crate) firewall: std::result::Result<network::FirewallRaw, String>,
    pub(crate) windows_update: std::result::Result<updates::WuRaw, String>,
    pub(crate) encryption: std::result::Result<device::EncryptionRaw, String>,
    pub(crate) accounts: std::result::Result<accounts::AccountsRaw, String>,
    pub(crate) remote: std::result::Result<network::RemoteRaw, String>,
    pub(crate) apps: std::result::Result<apps::AppsRaw, String>,
}

impl CheckupRaw {
    /// The failed sources, in `Source::ALL` order.
    pub(crate) fn errors(&self) -> Vec<SourceError> {
        fn err<T>(source: Source, r: &std::result::Result<T, String>) -> Option<SourceError> {
            r.as_ref().err().map(|message| SourceError {
                source,
                message: message.clone(),
            })
        }
        [
            err(Source::Os, &self.os),
            err(Source::Device, &self.device),
            err(Source::SecurityCenter, &self.security_center),
            err(Source::Defender, &self.defender),
            err(Source::Firewall, &self.firewall),
            err(Source::WindowsUpdate, &self.windows_update),
            err(Source::Encryption, &self.encryption),
            err(Source::Accounts, &self.accounts),
            err(Source::Remote, &self.remote),
            err(Source::Apps, &self.apps),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// The readers of the checkup; `Live` reads this PC. Each method runs on its own thread,
/// which is in the multithreaded COM apartment.
pub(crate) trait Probe: Send + Sync {
    fn context(&self) -> Context;
    fn os(&self) -> Result<OsRaw>;
    fn device(&self) -> Result<SecurityInfo>;
    fn security_center(&self) -> Result<protection::WscRaw>;
    fn defender(&self) -> Result<protection::DefenderRead>;
    fn firewall(&self) -> Result<network::FirewallRaw>;
    fn windows_update(&self) -> Result<updates::WuRaw>;
    /// Reads nothing when `elevated` is false.
    fn encryption(&self, elevated: bool) -> Result<device::EncryptionRaw>;
    fn accounts(&self, ctx: &Context) -> Result<accounts::AccountsRaw>;
    fn remote(&self) -> Result<network::RemoteRaw>;
    fn apps(&self, hive: &UserHive) -> Result<apps::AppsRaw>;
}

/// The probe of this PC.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Live;

impl Probe for Live {
    fn context(&self) -> Context {
        Context::live()
    }

    fn os(&self) -> Result<OsRaw> {
        let os = crate::sysinfo::os_info()?;
        Ok(OsRaw {
            edition_id: os.edition_id,
            build: os.build,
            fast_startup: os.fast_startup,
        })
    }

    fn device(&self) -> Result<SecurityInfo> {
        crate::sysinfo::security_info()
    }

    fn security_center(&self) -> Result<protection::WscRaw> {
        protection::read_security_center()
    }

    fn defender(&self) -> Result<protection::DefenderRead> {
        protection::read_defender()
    }

    fn firewall(&self) -> Result<network::FirewallRaw> {
        network::read_firewall()
    }

    fn windows_update(&self) -> Result<updates::WuRaw> {
        updates::read_windows_update()
    }

    fn encryption(&self, elevated: bool) -> Result<device::EncryptionRaw> {
        device::read_encryption(elevated)
    }

    fn accounts(&self, ctx: &Context) -> Result<accounts::AccountsRaw> {
        accounts::read_accounts(ctx)
    }

    fn remote(&self) -> Result<network::RemoteRaw> {
        network::read_remote()
    }

    fn apps(&self, hive: &UserHive) -> Result<apps::AppsRaw> {
        apps::read_apps(hive)
    }
}

// ───────────────────────────── Registry helpers ─────────────────────────────

/// A DWORD value of an open key; any other type, a missing value or a read error is `None`.
pub(crate) fn key_dword(key: Option<&Key>, name: &str) -> Option<u32> {
    match key?.query(name).ok()?? {
        RegValue::Dword(v) => Some(v),
        _ => None,
    }
}

/// A string value of an open key, trimmed; any other type, a missing value or a read error
/// is `None`.
pub(crate) fn key_text(key: Option<&Key>, name: &str) -> Option<String> {
    match key?.query(name).ok()?? {
        RegValue::Sz(s) | RegValue::ExpandSz(s) => Some(s.trim().to_string()),
        _ => None,
    }
}

/// A read-only HKLM key; `None` when it is missing or cannot be opened.
pub(crate) fn hklm(path: &str) -> Option<Key> {
    Key::open(Hive::LocalMachine, path, false).ok().flatten()
}

/// A DWORD under HKLM; `None` when the key or value is missing.
pub(crate) fn hklm_dword(path: &str, name: &str) -> Option<u32> {
    key_dword(hklm(path).as_ref(), name)
}

/// Whether an HKLM key exists (an unreadable key counts as existing).
pub(crate) fn hklm_exists(path: &str) -> bool {
    !matches!(Key::open(Hive::LocalMachine, path, false), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> SessionUser {
        SessionUser {
            domain: "TEST-PC".into(),
            name: "Test".into(),
            sid: "S-1-5-21-1111111111-2222222222-3333333333-1001".into(),
        }
    }

    #[test]
    fn per_user_values_come_from_the_signed_in_user() {
        assert_eq!(
            UserHive::resolve(Some(false), &Ok(Some(user()))),
            UserHive::Current
        );
        let other = UserHive::Users("S-1-5-21-1111111111-2222222222-3333333333-1001".into());
        assert_eq!(UserHive::resolve(Some(true), &Ok(Some(user()))), other);
        assert_eq!(UserHive::resolve(None, &Ok(Some(user()))), other);
        assert_eq!(
            UserHive::resolve(Some(false), &Ok(None)),
            UserHive::Unavailable(NO_SESSION_NOTE.into())
        );
        assert_eq!(
            UserHive::resolve(Some(true), &Err("no session".into())),
            UserHive::Unavailable(UNKNOWN_ACCOUNT_NOTE.into())
        );
        assert_eq!(
            UserHive::resolve(Some(false), &Err("no session".into())),
            UserHive::Current
        );
    }

    #[test]
    fn an_unavailable_hive_reads_nothing() {
        let hive = UserHive::Unavailable(NO_SESSION_NOTE.into());
        let err = hive.open(r"Software\Microsoft").unwrap_err();
        assert_eq!(err.to_string(), NO_SESSION_NOTE);
    }

    #[test]
    fn home_editions_are_recognized() {
        let os = |edition: &str| OsRaw {
            edition_id: edition.into(),
            build: 26200,
            fast_startup: None,
        };
        assert!(os("Core").home());
        assert!(os("CoreSingleLanguage").home());
        assert!(os("CoreN").home());
        assert!(!os("Professional").home());
        assert!(!os("").home());
    }

    #[test]
    fn registry_helpers_tolerate_missing_keys_and_values() {
        assert_eq!(hklm_dword(r"SOFTWARE\PCOptimizerNoSuchKey", "X"), None);
        assert!(!hklm_exists(r"SOFTWARE\PCOptimizerNoSuchKey"));
        assert!(hklm_exists("SOFTWARE"));
        let key = hklm(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        assert!(key_text(key.as_ref(), "ProductName").is_some());
        assert_eq!(key_dword(key.as_ref(), "ProductName"), None);
        assert_eq!(key_text(None, "ProductName"), None);
    }
}

/// Readings of a well-protected PC and helpers to vary them, for the rule tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use chrono::{DateTime, Duration, Utc};

    use super::super::accounts::AccountsRaw;
    use super::super::apps::{AppsRaw, UserApps};
    use super::super::device::{EncryptedVolume, EncryptionRaw};
    use super::super::network::{FirewallRaw, RemoteRaw};
    use super::super::protection::{DefenderMode, DefenderRaw, DefenderRead, WscRaw};
    use super::super::updates::{ScanState, UpdateScanView, WuRaw};
    use super::*;
    use crate::sysinfo::{FeatureState, FirmwareKind, SecureBoot, Virtualization};
    use crate::win::accounts::{ElevationType, LocalAccount};
    use crate::win::firewall::{FirewallState, ProfileState};
    use crate::win::scm::StartType;
    use crate::win::security_center::{ProductState, ProviderHealth, SecurityProduct};
    use crate::win::update_agent::{HistoryEntry, UpdateStatus};

    /// The time every fixture is read at.
    pub(crate) fn now() -> DateTime<Utc> {
        "2026-09-28T12:00:00Z".parse().unwrap()
    }

    pub(crate) fn days_ago(days: i64) -> DateTime<Utc> {
        now() - Duration::days(days)
    }

    pub(crate) fn user() -> SessionUser {
        SessionUser {
            domain: "TEST-PC".into(),
            name: "Test".into(),
            sid: "S-1-5-21-1111111111-2222222222-3333333333-1001".into(),
        }
    }

    /// Cairn runs unelevated as the signed-in user of a desktop PC.
    pub(crate) fn ctx() -> Context {
        Context::new(false, Some(false), false, Ok(Some(user())))
    }

    pub(crate) fn product(name: &str, state: ProductState, up_to_date: bool) -> SecurityProduct {
        SecurityProduct {
            name: name.into(),
            state,
            signatures_up_to_date: up_to_date,
            remediation: String::new(),
        }
    }

    /// Windows' own firewall as Security Center lists it among the firewall products.
    pub(crate) fn windows_firewall(state: ProductState) -> SecurityProduct {
        SecurityProduct {
            remediation: r"%windir%\system32\firewall.cpl".into(),
            ..product("Windows Firewall", state, true)
        }
    }

    pub(crate) fn defender() -> DefenderRaw {
        DefenderRaw {
            mode: Some(DefenderMode::Active),
            antivirus_enabled: Some(true),
            realtime: Some(true),
            tamper_protected: Some(true),
            tamper_source: None,
            signatures_out_of_date: Some(false),
            signature_age_days: Some(0),
            signature_version: Some("1.459.440.0".into()),
            signature_updated: Some(now() - Duration::hours(17)),
            computer_state: Some(0),
        }
    }

    pub(crate) fn profile(enabled: bool) -> ProfileState {
        ProfileState {
            enabled,
            block_all_inbound: false,
            inbound_allowed: false,
        }
    }

    pub(crate) fn firewall() -> FirewallRaw {
        FirewallRaw {
            state: FirewallState {
                current_profiles: 4,
                domain: profile(true),
                private: profile(true),
                public: profile(true),
                policy_managed: false,
                remote_desktop_group: Some([false; 3]),
            },
            service_start: Some(StartType::Automatic),
            domain_joined: Some(false),
        }
    }

    /// A successful installation recorded with the Security Updates category.
    pub(crate) fn installed(title: &str, days: i64) -> HistoryEntry {
        HistoryEntry {
            title: title.into(),
            date: Some(days_ago(days)),
            installation: true,
            succeeded: true,
            hresult: 0,
            category_ids: vec!["{0FA1201D-4330-4FA8-8AE9-B877473B6441}".into()],
            service_id: String::new(),
            support_url: String::new(),
        }
    }

    pub(crate) fn windows_update() -> WuRaw {
        WuRaw {
            service_start: Some(StartType::Manual),
            status: Ok(UpdateStatus {
                last_search: Some(days_ago(1)),
                last_install: Some(days_ago(10)),
                reboot_required: Some(false),
                history: vec![installed("2026-09 Cumulative Update for Windows 11", 10)],
            }),
            last_scan_event: Some(now() - Duration::hours(2)),
            ..WuRaw::default()
        }
    }

    pub(crate) fn volume(
        letter: &str,
        kind: u32,
        protection: u32,
        conversion: u32,
    ) -> EncryptedVolume {
        EncryptedVolume {
            letter: letter.into(),
            volume_type: Some(kind),
            protection: Some(protection),
            conversion: Some(conversion),
        }
    }

    pub(crate) fn account(rid: u32, name: &str, enabled: bool) -> LocalAccount {
        LocalAccount {
            rid,
            name: name.into(),
            enabled,
        }
    }

    pub(crate) fn accounts() -> AccountsRaw {
        AccountsRaw {
            enable_lua: Some(1),
            consent_prompt: Some(5),
            secure_desktop: Some(1),
            filter_admin_token: None,
            admin_approval_mode: None,
            token: Ok(ElevationType::Default),
            session_admin: None,
            local_accounts: Ok(vec![
                account(500, "Administrator", false),
                account(501, "Guest", false),
                account(1001, "Test", true),
            ]),
            auto_logon: None,
            auto_logon_count: None,
            default_password: false,
        }
    }

    pub(crate) fn remote() -> RemoteRaw {
        RemoteRaw {
            deny_connections: Some(1),
            nla: Some(1),
            port: Some(3389),
            assistance: Some(0),
            ..RemoteRaw::default()
        }
    }

    pub(crate) fn apps() -> AppsRaw {
        AppsRaw {
            smartscreen_policy: None,
            smartscreen_policy_level: None,
            smartscreen: None,
            edge_installed: true,
            edge_policy: None,
            user: Ok(UserApps {
                edge_policy: None,
                edge: Some(1),
                hide_file_ext: Some(0),
            }),
            smart_app_control: Some(1),
        }
    }

    pub(crate) fn device() -> SecurityInfo {
        SecurityInfo {
            firmware: FirmwareKind::Uefi,
            secure_boot: SecureBoot::On,
            tpm_found: Some(true),
            tpm_version: Some("2.0".into()),
            virtualization: Virtualization::InUse,
            hypervisor: Some("Microsoft Hv".into()),
            memory_integrity: FeatureState::Running,
            vbs: FeatureState::Running,
        }
    }

    /// Every source read, and every check passes.
    pub(crate) fn raw() -> CheckupRaw {
        CheckupRaw {
            os: Ok(OsRaw {
                edition_id: "Professional".into(),
                build: 26200,
                fast_startup: Some(true),
            }),
            device: Ok(device()),
            security_center: Ok(WscRaw {
                antivirus: vec![product("Windows Defender", ProductState::On, true)],
                antivirus_health: Some(ProviderHealth::Good),
                firewall: Vec::new(),
            }),
            defender: Ok(DefenderRead::Present(defender())),
            firewall: Ok(firewall()),
            windows_update: Ok(windows_update()),
            encryption: Ok(EncryptionRaw::Volumes {
                system_drive: "C:".into(),
                volumes: vec![volume("C:", 0, 1, 1)],
            }),
            accounts: Ok(accounts()),
            remote: Ok(remote()),
            apps: Ok(apps()),
        }
    }

    /// A finished offline search that found nothing.
    pub(crate) fn scan_done() -> UpdateScanView {
        UpdateScanView {
            state: ScanState::Done,
            online: false,
            started_at: Some(now() - Duration::minutes(5)),
            finished_at: Some(now() - Duration::minutes(4)),
            elapsed_ms: 60_000,
            updates: Vec::new(),
            error: None,
        }
    }

    /// Changes the Defender reading of `raw`.
    pub(crate) fn with_defender(raw: &mut CheckupRaw, change: impl FnOnce(&mut DefenderRaw)) {
        if let Ok(DefenderRead::Present(d)) = &mut raw.defender {
            change(d);
        }
    }
}
