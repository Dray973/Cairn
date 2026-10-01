//! Read-only security checkup and boot history.
//!
//! - [`security_checkup`] reads Windows Security Center, Microsoft Defender, Windows Firewall,
//!   Windows Update, BitLocker (administrators only), account and app settings, each source on
//!   its own thread under an 8 s deadline, and rates 24 checks with a 0-100 score.
//! - [`start_update_scan`] searches Windows Update for waiting updates on its own engine thread;
//!   [`update_scan`] and [`cancel_update_scan`] only touch in-memory state.
//! - [`boot_history`] reads start and shutdown durations from the Diagnostics-Performance log
//!   (administrators only), labels Fast Startup starts and finds what slowed them.
//!
//! Nothing here changes the PC: no session, no journal record, no `ops_log` row, no external
//! program. Fixes the window offers reuse the journaled apply and startup flows, or open
//! Settings and Windows Security pages and built-in Windows tools.

pub mod boot;
pub mod checkup;
pub mod text;
pub mod updates;

mod accounts;
mod apps;
mod device;
mod network;
mod probe;
mod protection;

use std::sync::Arc;

use chrono::Utc;

use crate::Result;

pub use boot::{
    BootHistory, BootPhases, BootRecord, BootStats, BootType, LogAccess, Phase, ShutdownRecord,
    SlowEvent, SlowItem, SlowKind, Trend, MAX_BOOT_LIMIT,
};
pub use checkup::{
    allowed_uri, Check, CheckGroup, CheckId, CheckState, Checkup, Fact, Fix, FixAction, Grade,
    Score, Severity, Source, SourceError,
};
pub use updates::{PendingUpdate, ScanState, UpdateScanView};

/// Runs the security checkup. Never fails as a whole: a source that cannot be read makes its
/// checks "could not check". Takes up to about 8 s.
pub fn security_checkup() -> Checkup {
    checkup::checkup_with(
        Arc::new(probe::Live),
        updates::scanner(),
        Utc::now(),
        checkup::CHECKUP_DEADLINE,
    )
}

/// Starts a search for waiting updates (Windows Update's cached data, or online) on an engine
/// thread and returns its view at once. Refused while a search runs.
pub fn start_update_scan(online: bool) -> Result<UpdateScanView> {
    updates::scanner().start(online)
}

/// State of the latest update search; reads in-memory state only.
pub fn update_scan() -> UpdateScanView {
    updates::scanner().view()
}

/// Asks a running update search to stop; false when none runs.
pub fn cancel_update_scan() -> bool {
    updates::scanner().cancel()
}

/// Start and shutdown history with at most `limit` starts (1 to 500).
pub fn boot_history(limit: usize) -> Result<BootHistory> {
    boot::live_history(limit)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::probe::{Context, Live, Probe, UserHive};
    use super::updates::{UpdateAgent, UpdateScanner, WuRaw};
    use super::*;
    use crate::win::update_agent::{SearchOutcome, UpdateStatus};

    /// Every file of this module and of the readers it uses; none may change anything.
    const SOURCES: [(&str, &str); 17] = [
        ("health/mod.rs", include_str!("mod.rs")),
        ("health/checkup.rs", include_str!("checkup.rs")),
        ("health/probe.rs", include_str!("probe.rs")),
        ("health/protection.rs", include_str!("protection.rs")),
        ("health/network.rs", include_str!("network.rs")),
        ("health/updates.rs", include_str!("updates.rs")),
        ("health/device.rs", include_str!("device.rs")),
        ("health/accounts.rs", include_str!("accounts.rs")),
        ("health/apps.rs", include_str!("apps.rs")),
        ("health/boot.rs", include_str!("boot.rs")),
        ("health/text.rs", include_str!("text.rs")),
        ("win/firewall.rs", include_str!("../win/firewall.rs")),
        ("win/wmi.rs", include_str!("../win/wmi.rs")),
        (
            "win/update_agent.rs",
            include_str!("../win/update_agent.rs"),
        ),
        ("win/event_log.rs", include_str!("../win/event_log.rs")),
        (
            "win/security_center.rs",
            include_str!("../win/security_center.rs"),
        ),
        ("win/accounts.rs", include_str!("../win/accounts.rs")),
    ];

    /// Splits a source file into its production code and its module-level test code. Test
    /// code starts at the first `#[cfg(test)]` at column 0; an indented one marks a single
    /// test-only item, and production code follows it.
    fn split_tests(source: &str) -> (&str, &str) {
        match source.find("\n#[cfg(test)]") {
            Some(at) => source.split_at(at + 1),
            None => (source, ""),
        }
    }

    /// Top-level items in the test part of a file that are not marked test-only, so they
    /// are production code the scan would miss.
    fn unmarked_items(tests: &str) -> Vec<&str> {
        const ITEM_STARTS: [&str; 14] = [
            "fn ", "pub ", "pub(", "impl ", "impl<", "struct ", "enum ", "const ", "static ",
            "use ", "mod ", "type ", "trait ", "unsafe ",
        ];
        let mut previous = "";
        let mut found = Vec::new();
        for line in tests.lines() {
            let item = ITEM_STARTS.iter().any(|start| line.starts_with(start));
            if item && previous.trim_end() != "#[cfg(test)]" {
                found.push(line);
            }
            previous = line;
        }
        found
    }

    #[test]
    fn the_scan_covers_everything_but_test_code() {
        let source = "impl A {\n    #[cfg(test)]\n    fn probe(&self) {}\n}\n\nfn live() {}\n\n\
                      #[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        let (body, tests) = split_tests(source);
        assert!(body.contains("fn probe") && body.contains("fn live()"));
        assert!(!body.contains("mod tests"));
        assert!(tests.starts_with("#[cfg(test)]\nmod tests"));
        assert!(unmarked_items(tests).is_empty());
        let crlf = source.replace('\n', "\r\n");
        let (body, tests) = split_tests(&crlf);
        assert!(body.contains("fn live()") && tests.starts_with("#[cfg(test)]\r\nmod tests"));
        assert_eq!(split_tests("fn live() {}\n"), ("fn live() {}\n", ""));
        let late = "#[cfg(test)]\nmod tests {}\n\npub(crate) fn late() {}\n";
        assert_eq!(unmarked_items(late), ["pub(crate) fn late() {}"]);
    }

    #[test]
    fn health_code_only_reads() {
        // The names are split so this test's own source does not contain them.
        let forbidden = [
            concat!("put", "_"),
            concat!("Enable", "RuleGroup"),
            concat!("Restore", "LocalFirewallDefaults"),
            concat!("Exec", "Method"),
            concat!("Put", "Instance"),
            concat!("Delete", "Instance"),
            concat!("CreateUpdate", "Installer"),
            concat!("CreateUpdate", "Downloader"),
            concat!("EvtClear", "Log"),
            concat!("EvtExport", "Log"),
            concat!("set", "_raw"),
            concat!("delete", "_value"),
            concat!("log", "_op"),
            concat!("Safety::", "begin"),
            concat!("LsaRetrieve", "PrivateData"),
            concat!("windowsdefender://", "quickscan"),
            concat!("windowsdefender://", "fullscan"),
            concat!("windowsdefender://", "enablertp"),
            concat!("windowsdefender://", "update"),
            concat!("windowsdefender://", "wdoscan"),
            concat!("windowsdefender://", "reboot"),
            concat!("windowsdefender://", "enableandupdate"),
        ];
        let mut scanner_scanned = false;
        for (name, source) in SOURCES {
            let (body, tests) = split_tests(source);
            assert!(body.contains("fn "), "{name} has no code before its tests");
            assert_eq!(
                unmarked_items(tests),
                Vec::<&str>::new(),
                "{name} has code after its tests"
            );
            for word in forbidden {
                assert!(!body.contains(word), "{name} contains {word}");
            }
            // The scanner's accessor follows a test-only method of `UpdateScanner`.
            if name == "health/updates.rs" {
                scanner_scanned = body.contains("fn scanner()");
            }
        }
        assert!(scanner_scanned);
    }

    #[derive(Debug)]
    struct NeverSearches;

    impl UpdateAgent for NeverSearches {
        fn search(
            &self,
            _online: bool,
            _cancel: &AtomicBool,
            _deadline: Duration,
        ) -> crate::Result<SearchOutcome> {
            panic!("the checkup never searches");
        }
    }

    /// The live probe with a synthetic Windows Update reading, so no Windows Update Agent
    /// object is created.
    struct LiveWithoutWindowsUpdate;

    impl Probe for LiveWithoutWindowsUpdate {
        fn context(&self) -> Context {
            Live.context()
        }
        fn os(&self) -> crate::Result<probe::OsRaw> {
            Live.os()
        }
        fn device(&self) -> crate::Result<crate::sysinfo::SecurityInfo> {
            Live.device()
        }
        fn security_center(&self) -> crate::Result<protection::WscRaw> {
            Live.security_center()
        }
        fn defender(&self) -> crate::Result<protection::DefenderRead> {
            Live.defender()
        }
        fn firewall(&self) -> crate::Result<network::FirewallRaw> {
            Live.firewall()
        }
        fn windows_update(&self) -> crate::Result<WuRaw> {
            Ok(WuRaw {
                status: Ok(UpdateStatus::default()),
                ..WuRaw::default()
            })
        }
        fn encryption(&self, elevated: bool) -> crate::Result<device::EncryptionRaw> {
            Live.encryption(elevated)
        }
        fn accounts(&self, ctx: &Context) -> crate::Result<accounts::AccountsRaw> {
            Live.accounts(ctx)
        }
        fn remote(&self) -> crate::Result<network::RemoteRaw> {
            Live.remote()
        }
        fn apps(&self, hive: &UserHive) -> crate::Result<apps::AppsRaw> {
            Live.apps(hive)
        }
    }

    #[test]
    fn live_checkup_without_windows_update_is_consistent() {
        // Read-only: Security Center, Defender and firewall getters, the registry, the account
        // list; drive encryption only when elevated. Windows Update is a synthetic reading.
        let scanner = UpdateScanner::new(Arc::new(NeverSearches));
        let checkup = checkup::checkup_with(
            Arc::new(LiveWithoutWindowsUpdate),
            &scanner,
            Utc::now(),
            checkup::CHECKUP_DEADLINE,
        );
        assert_eq!(checkup.checks.len(), 24);
        let ids: Vec<CheckId> = checkup.checks.iter().map(|c| c.id).collect();
        assert_eq!(ids, CheckId::ALL.to_vec());
        for check in &checkup.checks {
            assert!(!check.summary.is_empty(), "{:?} has no summary", check.id);
            for fix in &check.fixes {
                if let FixAction::Uri { uri } = &fix.action {
                    assert!(allowed_uri(uri), "{uri}");
                }
            }
        }
        assert!(checkup
            .errors
            .iter()
            .all(|e| e.source != Source::WindowsUpdate));
        let pending = &checkup.checks[10];
        assert_eq!(pending.id, CheckId::PendingUpdates);
        assert_eq!(pending.summary, "Not checked yet");
        assert!(checkup.update_scan_due);
        if !checkup.elevated {
            assert_eq!(checkup.checks[12].state, CheckState::Unknown);
            assert!(checkup.checks[12].needs_admin);
        }
    }

    #[test]
    fn live_boot_history_reads_or_needs_admin() {
        // Read-only: event log queries; the Diagnostics-Performance log needs administrator
        // rights, so a standard user gets needs_admin.
        let history = boot_history(5).unwrap();
        assert!(
            matches!(
                history.access,
                LogAccess::Ok | LogAccess::NeedsAdmin | LogAccess::LogDisabled
            ),
            "{:?}",
            history.access
        );
        if !crate::is_elevated() {
            assert_eq!(history.access, LogAccess::NeedsAdmin);
            assert!(history.boots.is_empty());
        }
        assert!(history.boots.len() <= 5);
    }

    #[test]
    fn boot_history_limits_are_checked() {
        assert!(boot_history(0).is_err());
        assert!(boot_history(MAX_BOOT_LIMIT + 1).is_err());
    }

    #[test]
    fn the_update_scan_view_needs_no_search() {
        let view = update_scan();
        assert!(matches!(view.state, ScanState::Idle | ScanState::Failed));
        assert!(!cancel_update_scan() || view.state == ScanState::Running);
    }
}
