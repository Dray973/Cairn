//! Plan, apply and export against the fake system. No test touches the live system: every
//! read and change goes through `fake::FakeProfiles`, and journals live in temporary folders.

use serde_json::{json, Value};

use super::fake::{
    adapter, appx_item, automatic, dns_plan, network_report, preset_config, scan_item, scan_report,
    startup_entry, step, FakeProfiles,
};
use super::format::{parse, to_text, Profile, FORMAT};
use super::plan::{
    apply_with, build_with, candidates_with, merge_filter, plan_or_apply_with, plan_with,
    session_label, PlanOrApply, PlanRow, ProfileApplyReport, ProfilePlan, Section, EARLIER_CHANGE,
};
use super::step::{
    ActiveHoursChoice, MaintenanceChoice, StepOutcome, StepReason, StepStatus, WindowsUpdateChoice,
};
use super::system::ProfileSystem;
use super::OP_APPLY_PROFILE;
use crate::debloat::catalog::RestartNeed;
use crate::debloat::ItemState;
use crate::maintenance::config::ScheduleDay;
use crate::network::{AdapterKind, ChangeOutcome, DnsConfig, DnsMode, IpFamily, LinkStatus};
use crate::safety::rollback::{
    rollback_filtered, rollback_journal, RegistryTarget, RollbackFilter,
};
use crate::safety::state_log::{NewDnsRecord, NewRegistryRecord};
use crate::safety::{MutationOutcome, Safety};
use crate::startup::{self, StartupSource};
use crate::win::registry::Hive;
use crate::{Error, Result, VERSION};

const OTHER: &str = "this window runs as a different account than the signed-in user";

fn profile(fields: Value) -> Profile {
    let mut value = json!({"format": FORMAT, "schema": 1, "name": "Test"});
    for (key, field) in fields.as_object().unwrap() {
        value[key] = field.clone();
    }
    parse(&value.to_string()).unwrap()
}

fn plan(fake: &FakeProfiles, p: &Profile) -> ProfilePlan {
    plan_with(fake, p).unwrap()
}

fn row<'a>(plan: &'a ProfilePlan, key: &str) -> &'a PlanRow {
    plan.rows
        .iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("no row {key} in {:?}", keys(plan)))
}

fn keys(plan: &ProfilePlan) -> Vec<&str> {
    plan.rows.iter().map(|r| r.key.as_str()).collect()
}

fn skipped(r: &PlanRow) -> StepReason {
    assert_eq!(r.status, StepStatus::Skipped, "{r:?}");
    r.reason.unwrap()
}

fn apply(fake: &FakeProfiles, p: &Profile, keys: Option<&[String]>) -> Result<ProfileApplyReport> {
    let label = session_label(&p.name);
    apply_with(fake, || Ok(fake.session(&label)), p, keys)
}

fn never_begin() -> Result<Safety> {
    panic!("no session may be opened here")
}

fn outcome(report: &ProfileApplyReport, key: &str) -> StepOutcome {
    report
        .results
        .iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("no result {key}: {:?}", report.results))
        .outcome
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

fn calls(fake: &FakeProfiles) -> Vec<String> {
    fake.calls.borrow().clone()
}

fn guid(n: u32) -> String {
    crate::network::tests::guid(n)
}

/// A scan item recorded by Cairn earlier: applied or not, with an active journal record.
fn revertible(mut item: crate::debloat::ScanItem) -> crate::debloat::ScanItem {
    item.revertible = true;
    item
}

/// One adapter per kind of a mixed PC, with DNS plans that change both families.
fn dns_fake() -> FakeProfiles {
    let mut fake = FakeProfiles::new();
    let wired = adapter(1, "Ethernet", AdapterKind::Ethernet);
    let mut unplugged = adapter(2, "Ethernet 2", AdapterKind::Ethernet);
    unplugged.status = LinkStatus::Disconnected;
    let virtual_switch = adapter(3, "vEthernet (Default Switch)", AdapterKind::Virtual);
    let mut usb = adapter(4, "USB Ethernet", AdapterKind::Ethernet);
    usb.status = LinkStatus::NotPresent;
    let bluetooth = adapter(5, "Bluetooth Network Connection", AdapterKind::Bluetooth);
    let wifi = adapter(6, "Wi-Fi", AdapterKind::Wifi);
    let both = [
        (IpFamily::Ipv4, ChangeOutcome::Planned),
        (IpFamily::Ipv6, ChangeOutcome::Planned),
    ];
    for a in [&wired, &unplugged, &wifi] {
        fake.dns_plans
            .insert(a.id.clone(), dns_plan(a, "cloudflare", &both));
    }
    fake.network = Ok(network_report(vec![
        wired,
        unplugged,
        virtual_switch,
        usb,
        bluetooth,
        wifi,
    ]));
    fake
}

// ───────────────────────────── plan ─────────────────────────────

#[test]
fn plan_reads_only_needed_sources() {
    let only = |p: Profile, fake: &FakeProfiles| -> Vec<&'static str> {
        fake.reads.borrow_mut().clear();
        plan(fake, &p);
        let mut reads: Vec<&'static str> = fake
            .reads
            .borrow()
            .iter()
            .copied()
            .filter(|r| *r != "per_user_allowed")
            .collect();
        reads.dedup();
        reads
    };
    let mut fake = dns_fake();
    fake.scan = Ok(scan_report(vec![scan_item(
        "performance.sysmain",
        ItemState::NotApplied,
    )]));
    fake.wu_steps = Ok(vec![step(
        "windows_update:active_hours",
        "Active hours",
        StepStatus::Already,
        None,
    )]);

    assert_eq!(
        only(profile(json!({"tweaks": ["performance.sysmain"]})), &fake),
        ["scan"]
    );
    assert_eq!(
        only(profile(json!({"apps": ["Microsoft.BingNews"]})), &fake),
        ["scan"]
    );
    assert_eq!(
        only(
            profile(json!({"startup": [{"id": "user_run:Discord"}]})),
            &fake
        ),
        ["startup"]
    );
    assert_eq!(
        only(profile(json!({"dns": {"wifi": {"ipv4": "quad9"}}})), &fake),
        ["network", "plan_dns"]
    );
    assert_eq!(
        only(
            profile(json!({"windows_update": {"active_hours": {"start": 8, "end": 20}}})),
            &fake
        ),
        ["wu_plan"]
    );
    assert_eq!(
        only(
            profile(
                json!({"maintenance": {"enabled": true, "day": "sunday", "time": "12:00",
                "sfc_verify": true}})
            ),
            &fake
        ),
        ["maintenance_plan"]
    );
    // Machine-wide rows never ask about the account.
    fake.reads.borrow_mut().clear();
    plan(&fake, &profile(json!({"tweaks": ["performance.sysmain"]})));
    assert!(!fake.reads.borrow().contains(&"per_user_allowed"));
    assert!(fake.calls.borrow().is_empty());
}

#[test]
fn unknown_tweak_is_skipped_unknown_id() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![scan_item(
        "performance.sysmain",
        ItemState::NotApplied,
    )]));
    let p = plan(
        &fake,
        &profile(json!({"tweaks": ["privacy.new_future_id", "performance.sysmain"]})),
    );
    let r = row(&p, "tweak:privacy.new_future_id");
    assert_eq!(skipped(r), StepReason::UnknownId);
    assert_eq!(r.title, "privacy.new_future_id");
    assert_eq!(
        r.detail,
        format!("Cairn {VERSION} doesn't know this setting; a newer version may.")
    );
    assert!(!r.selected);
    assert_eq!(
        keys(&p),
        ["tweak:privacy.new_future_id", "tweak:performance.sysmain"]
    );
    assert_eq!((p.changes, p.already, p.skipped), (1, 0, 1));
}

#[test]
fn tweak_states_map_to_rows() {
    let mut fake = FakeProfiles::new();
    let mut unavailable = scan_item("performance.edge_background", ItemState::Unavailable);
    unavailable.note = Some("Microsoft Edge is not installed.".into());
    fake.scan = Ok(scan_report(vec![
        revertible(scan_item("gaming.game_mode", ItemState::Applied)),
        scan_item("privacy.cortana", ItemState::Applied),
        scan_item("performance.sysmain", ItemState::NotApplied),
        scan_item("privacy.activity_history", ItemState::Partial),
        unavailable,
        scan_item("performance.power_plan", ItemState::NotApplied),
        scan_item("interface.file_extensions", ItemState::NotApplied),
    ]));
    let p = plan(
        &fake,
        &profile(
            json!({"tweaks": ["gaming.game_mode", "privacy.cortana", "performance.sysmain",
            "privacy.activity_history", "performance.edge_background", "performance.power_plan",
            "interface.file_extensions"]}),
        ),
    );
    let r = row(&p, "tweak:gaming.game_mode");
    assert_eq!(
        (r.status, r.detail.as_str()),
        (StepStatus::Already, "Already applied")
    );
    assert!(r.per_user && !r.selected);
    let r = row(&p, "tweak:privacy.cortana");
    assert_eq!(r.detail, "Already set on this PC");
    let r = row(&p, "tweak:performance.sysmain");
    assert_eq!(r.status, StepStatus::Change);
    assert_eq!(r.detail, "Performance · not applied");
    assert!(r.selected && !r.per_user);
    assert_eq!(
        r.title,
        crate::debloat::catalog::tweak("performance.sysmain")
            .unwrap()
            .title
    );
    let r = row(&p, "tweak:privacy.activity_history");
    assert_eq!(
        r.detail,
        "Privacy · partly applied · takes effect after signing out"
    );
    assert_eq!(r.restart, RestartNeed::SignOut);
    let r = row(&p, "tweak:performance.edge_background");
    assert_eq!(skipped(r), StepReason::NotOnThisPc);
    assert_eq!(r.detail, "Microsoft Edge is not installed.");
    let r = row(&p, "tweak:performance.power_plan");
    assert_eq!(r.detail, "Performance · not applied · Medium risk");
    assert!(r.caution.is_none() && r.selected);
    assert_eq!(r.risk, Some(crate::debloat::Risk::Medium));
    let r = row(&p, "tweak:interface.file_extensions");
    assert_eq!(r.detail, "Interface · not applied · File Explorer restarts");
    assert!(r.per_user);
    assert_eq!((p.changes, p.already, p.skipped), (4, 2, 1));
    assert_eq!(p.restart, RestartNeed::SignOut);
    assert!(p.dry_run && p.elevated && p.other_account.is_none());
}

#[test]
fn high_risk_and_battery_power_start_unselected() {
    let mut fake = FakeProfiles::new();
    let mut scan = scan_report(vec![
        scan_item("performance.search_indexer", ItemState::NotApplied),
        scan_item("performance.power_plan", ItemState::NotApplied),
        scan_item("performance.sysmain", ItemState::NotApplied),
    ]);
    scan.has_battery = true;
    fake.scan = Ok(scan);
    let p = profile(
        json!({"tweaks": ["performance.search_indexer", "performance.power_plan",
        "performance.sysmain"]}),
    );
    let planned = plan(&fake, &p);
    let high = row(&planned, "tweak:performance.search_indexer");
    assert_eq!(high.status, StepStatus::Change);
    assert!(high.caution.as_deref().unwrap().starts_with("High risk: "));
    assert!(!high.selected);
    let power = row(&planned, "tweak:performance.power_plan");
    assert!(power
        .caution
        .as_deref()
        .unwrap()
        .starts_with("This PC has a battery"));
    assert!(!power.selected);
    assert!(row(&planned, "tweak:performance.sysmain").selected);

    if let Ok(scan) = &mut fake.scan {
        scan.has_battery = false;
    }
    let planned = plan(&fake, &p);
    let power = row(&planned, "tweak:performance.power_plan");
    assert!(power.caution.is_none() && power.selected);
}

#[test]
fn app_patterns_resolve_to_installed_names() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        scan_item("performance.sysmain", ItemState::NotApplied),
        appx_item("king.com.CandyCrushSaga", ItemState::NotApplied),
        appx_item("king.com.BubbleWitch3Saga", ItemState::Applied),
        appx_item("Microsoft.BingWeather", ItemState::NotApplied),
        appx_item("Microsoft.XboxGamingOverlay", ItemState::NotApplied),
    ]));
    let p = plan(
        &fake,
        &profile(
            json!({"apps": ["king.com.*", "Disney.*", "microsoft.bingweather",
            "Microsoft.BingNews", "Microsoft.XboxGamingOverlay"],
            "tweaks": ["performance.sysmain"]}),
        ),
    );
    assert_eq!(
        keys(&p),
        [
            "tweak:performance.sysmain",
            "app:king.com.CandyCrushSaga",
            "app:king.com.BubbleWitch3Saga",
            "app:Disney.*",
            "app:Microsoft.BingWeather",
            "app:Microsoft.BingNews",
            "app:Microsoft.XboxGamingOverlay",
        ]
    );
    let r = row(&p, "app:king.com.CandyCrushSaga");
    assert_eq!(r.status, StepStatus::Change);
    assert_eq!(
        r.title,
        "Candy Crush and other King games (king.com.CandyCrushSaga)"
    );
    assert_eq!(
        r.detail,
        "Store app king.com.CandyCrushSaga · removed for your account"
    );
    assert!(r.per_user && r.selected);
    assert_eq!(
        row(&p, "app:king.com.BubbleWitch3Saga").detail,
        "Removed by Cairn"
    );
    let r = row(&p, "app:Disney.*");
    assert_eq!(
        (r.status, r.detail.as_str()),
        (StepStatus::Already, "Not installed for your account.")
    );
    assert_eq!(r.title, "Disney+");
    assert_eq!(
        row(&p, "app:Microsoft.BingWeather").status,
        StepStatus::Change
    );
    assert_eq!(
        row(&p, "app:Microsoft.BingNews").detail,
        "Not installed for your account."
    );
    let r = row(&p, "app:Microsoft.XboxGamingOverlay");
    assert_eq!(
        r.detail,
        "Store app Microsoft.XboxGamingOverlay · removed for your account · Medium risk"
    );
    assert!(r.caution.is_none());
}

#[test]
fn protected_and_non_catalog_apps_are_skipped() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(Vec::new()));
    let p = plan(
        &fake,
        &profile(
            json!({"apps": ["Microsoft.WindowsStore", "Microsoft.Windows.Photos",
            "Contoso.Notes", "Fabrikam.*", "Microsoft.Windows.DevHome"]}),
        ),
    );
    let r = row(&p, "app:Microsoft.WindowsStore");
    assert_eq!(skipped(r), StepReason::CannotChange);
    assert_eq!(r.detail, "Cairn never removes this app.");
    assert_eq!(
        skipped(row(&p, "app:Microsoft.Windows.Photos")),
        StepReason::CannotChange
    );
    let r = row(&p, "app:Contoso.Notes");
    assert_eq!(skipped(r), StepReason::UnknownId);
    assert_eq!(r.detail, "Cairn doesn't remove this app.");
    let r = row(&p, "app:Fabrikam.*");
    assert_eq!(skipped(r), StepReason::UnknownId);
    assert_eq!(r.detail, "Cairn doesn't remove these apps.");
    // Dev Home sits under a protected prefix but is a catalog app.
    assert_eq!(
        row(&p, "app:Microsoft.Windows.DevHome").status,
        StepStatus::Already
    );
}

#[test]
fn appx_unavailable_skips_apps_as_unreadable() {
    let mut fake = FakeProfiles::new();
    let mut scan = scan_report(vec![scan_item(
        "performance.sysmain",
        ItemState::NotApplied,
    )]);
    scan.appx_unavailable = Some("the Appx module could not be loaded".into());
    fake.scan = Ok(scan);
    let p = plan(
        &fake,
        &profile(json!({"apps": ["Microsoft.BingNews", "king.com.*"],
            "tweaks": ["performance.sysmain"]})),
    );
    for key in ["app:Microsoft.BingNews", "app:king.com.*"] {
        let r = row(&p, key);
        assert_eq!(skipped(r), StepReason::Unreadable);
        assert!(
            r.detail.contains("the Appx module could not be loaded"),
            "{}",
            r.detail
        );
    }
    assert_eq!(
        row(&p, "tweak:performance.sysmain").status,
        StepStatus::Change
    );
}

#[test]
fn startup_matches_ignoring_case_and_one_sibling() {
    let mut fake = FakeProfiles::new();
    fake.startup = Ok(vec![
        startup_entry(StartupSource::UserRun, "Discord", "Discord", true),
        startup_entry(StartupSource::MachineRun, "Steam", "Steam", true),
        startup_entry(StartupSource::UserFolder, "Notes.lnk", "Notes", false),
        startup_entry(StartupSource::MachineRun, "Tool", "Tool", true),
        startup_entry(StartupSource::MachineRun32, "Tool", "Tool", true),
    ]);
    let p = plan(
        &fake,
        &profile(json!({"startup": [
            {"id": "user_run:DISCORD"},
            {"id": "user_run:Steam", "name": "Steam"},
            {"id": "common_folder:Notes.lnk"},
            {"id": "packaged_task:Contoso.App_aaaaaaaaaaaaa\\Task"},
            {"id": "user_run:Tool", "name": "Tool"}
        ]})),
    );
    let r = row(&p, "startup:user_run:Discord");
    assert_eq!(r.status, StepStatus::Change);
    assert_eq!(r.detail, "HKCU Run · Contoso Ltd. · turned off at sign-in");
    assert!(r.per_user && r.selected);
    let r = row(&p, "startup:machine_run:Steam");
    assert_eq!(r.status, StepStatus::Change);
    assert_eq!(
        r.detail,
        "HKLM Run · Contoso Ltd. · turned off at sign-in · Listed under HKLM Run; the profile \
         has it under HKCU Run"
    );
    assert!(!r.per_user);
    let r = row(&p, "startup:user_folder:Notes.lnk");
    assert_eq!(r.status, StepStatus::Already);
    assert_eq!(
        r.detail,
        "Already turned off · Listed under Startup folder; the profile has it under Startup \
         folder (all users)"
    );
    let r = row(&p, "startup:packaged_task:Contoso.App_aaaaaaaaaaaaa\\Task");
    assert_eq!(skipped(r), StepReason::NotOnThisPc);
    assert_eq!(r.title, "Contoso.App_aaaaaaaaaaaaa\\Task");
    // Two siblings match: none is chosen.
    let r = row(&p, "startup:user_run:Tool");
    assert_eq!(skipped(r), StepReason::NotOnThisPc);
    assert_eq!(r.title, "Tool");
}

#[test]
fn policy_unknown_source_and_untoggleable_startup_are_skipped() {
    let mut fake = FakeProfiles::new();
    let mut locked = startup_entry(StartupSource::MachineRun, "Locked", "Locked", true);
    locked.can_toggle = false;
    locked.note = "This entry is in an unrecognized state.".into();
    fake.startup = Ok(vec![
        locked,
        startup_entry(StartupSource::PolicyMachineRun, "Agent", "Agent", true),
    ]);
    let p = plan(
        &fake,
        &profile(json!({"startup": [
            {"id": "policy_machine_run:Agent", "name": "Agent"},
            {"id": "future_source:X", "name": "Future"},
            {"id": "machine_run:Locked"}
        ]})),
    );
    let r = row(&p, "startup:policy_machine_run:Agent");
    assert_eq!(skipped(r), StepReason::CannotChange);
    assert_eq!(r.detail, "Set by Group Policy.");
    let r = row(&p, "startup:future_source:X");
    assert_eq!(skipped(r), StepReason::NotOnThisPc);
    assert_eq!(r.title, "Future");
    let r = row(&p, "startup:machine_run:Locked");
    assert_eq!(skipped(r), StepReason::CannotChange);
    assert_eq!(r.detail, "This entry is in an unrecognized state.");
}

#[test]
fn dns_rows_cover_every_hardware_adapter_of_the_kind() {
    let fake = dns_fake();
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "cloudflare"}}})),
    );
    assert_eq!(
        keys(&p),
        [format!("dns:{}", guid(1)), format!("dns:{}", guid(2))]
    );
    let r = row(&p, &format!("dns:{}", guid(1)));
    assert_eq!(r.status, StepStatus::Change);
    assert_eq!(r.title, "DNS servers of Ethernet (Ethernet)");
    assert_eq!(
        r.detail,
        "IPv4: Automatic → Cloudflare · IPv6: Automatic → Cloudflare"
    );
    assert!(!r.per_user && r.selected && r.risk.is_none());

    let mut fake = dns_fake();
    let wifi = adapter(6, "Wi-Fi", AdapterKind::Wifi);
    fake.dns_plans.insert(
        wifi.id.clone(),
        dns_plan(
            &wifi,
            "quad9",
            &[
                (IpFamily::Ipv4, ChangeOutcome::AlreadySet),
                (IpFamily::Ipv6, ChangeOutcome::AlreadySet),
            ],
        ),
    );
    let p = plan(
        &fake,
        &profile(json!({"dns": {"wifi": {"ipv4": "quad9", "ipv6": "quad9"}}})),
    );
    let r = row(&p, &format!("dns:{}", guid(6)));
    assert_eq!(r.status, StepStatus::Already);
    assert_eq!(r.title, "DNS servers of Wi-Fi (Wi-Fi)");
    assert!(
        r.detail.contains("Quad9 (blocks malware) (already set)"),
        "{}",
        r.detail
    );
}

#[test]
fn dns_rows_skip_vpn_profile_and_policy() {
    let mut fake = dns_fake();
    if let Ok(report) = &mut fake.network {
        report.adapters[0].can_change_dns = false;
        report.adapters[0].note =
            Some("Disconnect the VPN to change DNS servers; while it is connected it may be managing them.".into());
        report.adapters[1].can_change_dns = false;
        report.adapters[1].note = None;
    }
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv4": "google"}}})),
    );
    let r = row(&p, &format!("dns:{}", guid(1)));
    assert_eq!(skipped(r), StepReason::CannotChange);
    assert!(r.detail.starts_with("Disconnect the VPN"));
    let r = row(&p, &format!("dns:{}", guid(2)));
    assert_eq!(skipped(r), StepReason::CannotChange);
    assert_eq!(r.detail, "DNS servers can't be changed on this adapter.");

    let mut fake = dns_fake();
    if let Ok(report) = &mut fake.network {
        report.dns_policy = vec!["198.51.100.53".into()];
    }
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv4": "google"}, "wifi": {"ipv4": "google"}}})),
    );
    assert_eq!(p.rows.len(), 3);
    for r in &p.rows {
        assert_eq!(skipped(r), StepReason::CannotChange);
        assert!(r.detail.contains("Group Policy"));
    }
    assert!(!fake.reads.borrow().contains(&"plan_dns"));
}

#[test]
fn dns_missing_kind_is_not_on_this_pc() {
    let mut fake = FakeProfiles::new();
    fake.network = Ok(network_report(vec![adapter(
        1,
        "Ethernet",
        AdapterKind::Ethernet,
    )]));
    let p = plan(&fake, &profile(json!({"dns": {"wifi": {"ipv4": "quad9"}}})));
    let r = row(&p, "dns:wifi");
    assert_eq!(skipped(r), StepReason::NotOnThisPc);
    assert_eq!(r.detail, "This PC has no Wi-Fi adapter.");
    assert_eq!(r.title, "DNS servers (Wi-Fi)");
    let mut fake = FakeProfiles::new();
    fake.network = Ok(network_report(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]));
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv6": "quad9"}}})),
    );
    assert_eq!(
        row(&p, "dns:ethernet").detail,
        "This PC has no Ethernet adapter."
    );
}

#[test]
fn unknown_preset_is_skipped() {
    let fake = dns_fake();
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "future_dns"}}})),
    );
    assert_eq!(p.rows.len(), 2);
    for r in &p.rows {
        assert_eq!(skipped(r), StepReason::UnknownId);
        assert!(r.detail.contains("“future_dns”"), "{}", r.detail);
        assert!(r.detail.contains("automatic, cloudflare"), "{}", r.detail);
    }
    assert!(!fake.reads.borrow().contains(&"plan_dns"));
}

#[test]
fn per_user_rows_are_skipped_for_another_account_machine_rows_stay() {
    let mut fake = FakeProfiles::new();
    fake.other_account = Some(OTHER.into());
    fake.scan = Ok(scan_report(vec![
        scan_item("interface.file_extensions", ItemState::NotApplied),
        scan_item("performance.sysmain", ItemState::NotApplied),
        scan_item("gaming.game_mode", ItemState::Applied),
        appx_item("Microsoft.BingNews", ItemState::NotApplied),
    ]));
    fake.startup = Ok(vec![
        startup_entry(StartupSource::UserRun, "Discord", "Discord", true),
        startup_entry(StartupSource::MachineRun, "Steam", "Steam", true),
    ]);
    let p = plan(
        &fake,
        &profile(json!({
            "tweaks": ["interface.file_extensions", "performance.sysmain", "gaming.game_mode"],
            "apps": ["Microsoft.BingNews"],
            "startup": [{"id": "user_run:Discord"}, {"id": "machine_run:Steam"}]
        })),
    );
    for key in [
        "tweak:interface.file_extensions",
        "tweak:gaming.game_mode",
        "app:Microsoft.BingNews",
        "startup:user_run:Discord",
    ] {
        let r = row(&p, key);
        assert_eq!(skipped(r), StepReason::OtherAccount, "{key}");
        assert_eq!(
            r.detail,
            "Belongs to your user account, but Cairn is running as another account."
        );
    }
    assert_eq!(
        row(&p, "tweak:performance.sysmain").status,
        StepStatus::Change
    );
    assert_eq!(
        row(&p, "startup:machine_run:Steam").status,
        StepStatus::Change
    );
    assert!(p
        .other_account
        .as_deref()
        .unwrap()
        .starts_with("Cairn is running as a different account"));
    let probes = fake
        .reads
        .borrow()
        .iter()
        .filter(|r| **r == "per_user_allowed")
        .count();
    assert_eq!(probes, 1);
}

#[test]
fn source_failure_skips_section_with_warning() {
    let mut fake = FakeProfiles::new();
    fake.scan = Err("the registry could not be read".into());
    fake.startup = Err("access denied".into());
    fake.network = Err("IP Helper failed".into());
    fake.wu_steps = Err("policy key unreadable".into());
    fake.maintenance_steps = Err("Task Scheduler unavailable".into());
    let p = plan(
        &fake,
        &profile(json!({
            "tweaks": ["performance.sysmain"],
            "apps": ["Microsoft.BingNews"],
            "startup": [{"id": "user_run:Discord", "name": "Discord"}],
            "dns": {"wifi": {"ipv4": "quad9"}},
            "windows_update": {"exclude_drivers": true, "restart_notify": true},
            "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "dism_check": true}
        })),
    );
    for r in &p.rows {
        assert_eq!(skipped(r), StepReason::Unreadable, "{}", r.key);
        assert!(r.detail.starts_with("Couldn't read: "), "{}", r.detail);
    }
    assert_eq!(
        keys(&p),
        [
            "tweak:performance.sysmain",
            "startup:user_run:Discord",
            "dns:wifi",
            "windows_update:restart_notify",
            "windows_update:exclude_drivers",
            "maintenance",
            "app:Microsoft.BingNews",
        ]
    );
    assert_eq!(row(&p, "startup:user_run:Discord").title, "Discord");
    assert_eq!(
        p.warnings,
        [
            "Optimize settings and Store apps could not be read: the registry could not be read",
            "Startup apps could not be read: access denied",
            "Network adapters could not be read: IP Helper failed",
            "Windows Update settings could not be read: policy key unreadable",
            "Scheduled maintenance could not be read: Task Scheduler unavailable",
        ]
    );
}

#[test]
fn wu_and_maintenance_steps_are_forwarded_with_selection_rule() {
    let mut fake = FakeProfiles::new();
    fake.wu_steps = Ok(vec![
        step(
            "windows_update:active_hours",
            "Active hours",
            StepStatus::Change,
            None,
        ),
        step(
            "windows_update:defer_feature",
            "Defer feature updates",
            StepStatus::Change,
            Some("Windows Update waits 180 days after each new Windows version is released."),
        ),
        step(
            "windows_update:exclude_drivers",
            "Exclude drivers",
            StepStatus::Already,
            None,
        ),
        step(
            "windows_update:restart_notify",
            "Restart notifications",
            StepStatus::Skipped,
            None,
        ),
    ]);
    fake.maintenance_steps = Ok(vec![step(
        "maintenance",
        "Scheduled maintenance",
        StepStatus::Change,
        None,
    )]);
    let p = plan(
        &fake,
        &profile(json!({
            "windows_update": {"active_hours": {"start": 8, "end": 20}, "defer_feature_days": 180,
                "exclude_drivers": true, "restart_notify": true},
            "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "sfc_verify": true}
        })),
    );
    let r = row(&p, "windows_update:active_hours");
    assert_eq!(
        (r.section, r.status, r.selected),
        (Section::WindowsUpdate, StepStatus::Change, true)
    );
    assert_eq!(r.detail, "Active hours detail");
    let r = row(&p, "windows_update:defer_feature");
    assert!(!r.selected && r.caution.is_some());
    assert_eq!(
        row(&p, "windows_update:exclude_drivers").status,
        StepStatus::Already
    );
    assert_eq!(
        skipped(row(&p, "windows_update:restart_notify")),
        StepReason::Edition
    );
    let r = row(&p, "maintenance");
    assert_eq!(
        (r.section, r.selected, r.per_user),
        (Section::Maintenance, true, false)
    );
}

#[test]
fn profile_maintenance_row_is_opt_in() {
    let caution = "Runs every Sunday at 12:00 with administrator rights and permanently deletes \
                   files in: Temporary files. Undo removes the task; files already deleted stay \
                   deleted.";
    let mut fake = FakeProfiles::new();
    fake.maintenance_steps = Ok(vec![step(
        "maintenance",
        "Scheduled maintenance",
        StepStatus::Change,
        Some(caution),
    )]);
    fake.maintenance_filter = RollbackFilter {
        task_definitions: vec![
            r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001".into(),
        ],
        ..Default::default()
    };
    let p = profile(
        json!({"maintenance": {"enabled": true, "day": "sunday", "time": "12:00",
        "clean": ["user_temp"]}}),
    );
    let planned = plan(&fake, &p);
    let r = row(&planned, "maintenance");
    assert_eq!(r.caution.as_deref(), Some(caution));
    assert!(!r.selected);

    // The default selection leaves it out, and no session is opened for nothing.
    let report = apply_with(&fake, never_begin, &p, None).unwrap();
    assert!(report.session_id.is_none() && report.results.is_empty());
    assert!(calls(&fake).is_empty());

    // Chosen by key, it applies and its filter joins the undo.
    let report = apply(&fake, &p, Some(&strings(&["maintenance"]))).unwrap();
    assert_eq!(outcome(&report, "maintenance"), StepOutcome::Applied);
    assert_eq!(calls(&fake), ["maintenance_apply"]);
    assert_eq!(report.undo.task_definitions.len(), 1);
}

#[test]
fn earlier_change_rows_say_undo_goes_to_the_first_baseline() {
    let mut fake = dns_fake();
    fake.scan = Ok(scan_report(vec![
        revertible(scan_item("performance.sysmain", ItemState::NotApplied)),
        scan_item("privacy.cortana", ItemState::NotApplied),
    ]));
    fake.startup = Ok(vec![
        startup_entry(StartupSource::UserRun, "Discord", "Discord", true),
        startup_entry(StartupSource::UserRun, "Steam", "Steam", true),
    ]);
    fake.wu_steps = Ok(vec![
        step(
            "windows_update:active_hours",
            "Active hours",
            StepStatus::Change,
            None,
        ),
        step(
            "windows_update:restart_notify",
            "Restart notifications",
            StepStatus::Change,
            None,
        ),
    ]);
    fake.wu_recorded = Ok(vec![
        "windows_update:active_hours".into(),
        "windows_update:defer_feature".into(),
    ]);
    let session = fake.journal.begin_session("earlier", VERSION).unwrap();
    let discord = startup::registry_target("user_run:Discord").unwrap();
    fake.journal
        .record_registry(
            session,
            &NewRegistryRecord {
                hive: discord.hive,
                key_path: discord.key_path.to_ascii_lowercase(),
                value_name: discord.value_name.clone(),
                key_existed: true,
                value_existed: false,
                original: None,
                created_root: None,
            },
        )
        .unwrap();
    fake.journal
        .record_dns(
            session,
            &NewDnsRecord {
                interface_guid: guid(1).to_ascii_uppercase(),
                family: IpFamily::Ipv4,
                adapter_name: "Ethernet".into(),
                previous_servers: String::new(),
                target_servers: "1.1.1.1,1.0.0.1".into(),
            },
        )
        .unwrap();

    let p = plan(
        &fake,
        &profile(json!({
            "tweaks": ["performance.sysmain", "privacy.cortana"],
            "startup": [{"id": "user_run:Discord"}, {"id": "user_run:Steam"}],
            "dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "cloudflare"}},
            "windows_update": {"active_hours": {"start": 8, "end": 20}, "restart_notify": true}
        })),
    );
    let earlier = |p: &ProfilePlan, key: &str| row(p, key).detail.ends_with(EARLIER_CHANGE);
    assert!(earlier(&p, "tweak:performance.sysmain"));
    assert!(!earlier(&p, "tweak:privacy.cortana"));
    assert!(earlier(&p, "startup:user_run:Discord"));
    assert!(!earlier(&p, "startup:user_run:Steam"));
    assert!(earlier(&p, &format!("dns:{}", guid(1))));
    assert!(!earlier(&p, &format!("dns:{}", guid(2))));
    assert!(earlier(&p, "windows_update:active_hours"));
    assert!(!earlier(&p, "windows_update:restart_notify"));
    assert_eq!(
        EARLIER_CHANGE,
        " · Undo returns it to how it was before Cairn first changed it"
    );

    // Undo selects an adapter's records of both address families, so a row that changes only
    // IPv6 on the adapter whose IPv4 servers were recorded earlier carries the note too.
    let wired = adapter(1, "Ethernet", AdapterKind::Ethernet);
    fake.dns_plans.insert(
        wired.id.clone(),
        dns_plan(
            &wired,
            "cloudflare",
            &[(IpFamily::Ipv6, ChangeOutcome::Planned)],
        ),
    );
    let p = plan(
        &fake,
        &profile(json!({"dns": {"ethernet": {"ipv6": "cloudflare"}}})),
    );
    let wired_row = row(&p, &format!("dns:{}", guid(1)));
    assert_eq!(wired_row.status, StepStatus::Change);
    assert!(wired_row.detail.starts_with("IPv6: "), "{wired_row:?}");
    assert!(earlier(&p, &format!("dns:{}", guid(1))));
    assert!(!earlier(&p, &format!("dns:{}", guid(2))));
}

// ───────────────────────────── apply ─────────────────────────────

/// A PC where one row of every section would change.
fn full_fake() -> (FakeProfiles, Profile) {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        scan_item("performance.sysmain", ItemState::NotApplied),
        scan_item("gaming.game_mode", ItemState::Applied),
        appx_item("Microsoft.BingNews", ItemState::NotApplied),
    ]));
    fake.startup = Ok(vec![startup_entry(
        StartupSource::UserRun,
        "Discord",
        "Discord",
        true,
    )]);
    let wired = adapter(1, "Ethernet", AdapterKind::Ethernet);
    fake.dns_plans.insert(
        wired.id.clone(),
        dns_plan(
            &wired,
            "cloudflare",
            &[
                (IpFamily::Ipv4, ChangeOutcome::Planned),
                (IpFamily::Ipv6, ChangeOutcome::Planned),
            ],
        ),
    );
    fake.network = Ok(network_report(vec![wired]));
    fake.wu_steps = Ok(vec![step(
        "windows_update:active_hours",
        "Active hours",
        StepStatus::Change,
        None,
    )]);
    fake.maintenance_steps = Ok(vec![step(
        "maintenance",
        "Scheduled maintenance",
        StepStatus::Change,
        None,
    )]);
    let p = profile(json!({
        "name": "Everything",
        "tweaks": ["performance.sysmain", "gaming.game_mode"],
        "apps": ["Microsoft.BingNews"],
        "startup": [{"id": "user_run:Discord"}],
        "dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "cloudflare"}},
        "windows_update": {"active_hours": {"start": 8, "end": 20}},
        "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "sfc_verify": true}
    }));
    (fake, p)
}

#[test]
fn dry_run_path_never_calls_begin() {
    let (fake, p) = full_fake();
    let keys = strings(&["tweak:performance.sysmain"]);
    let result = plan_or_apply_with(&fake, true, never_begin, &p, Some(&keys)).unwrap();
    let PlanOrApply::Plan(planned) = &result else {
        panic!("a dry run returned an apply report");
    };
    assert_eq!(planned.changes, 6);
    assert!(fake.journal.sessions().unwrap().is_empty());
    assert!(fake.journal.ops(10).unwrap().is_empty());
    assert!(calls(&fake).is_empty());
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["dry_run"], true);
    assert!(json.get("rows").is_some() && json.get("undo").is_none());

    let result = plan_or_apply_with(
        &fake,
        false,
        || Ok(fake.session("profile: Everything")),
        &p,
        Some(&keys),
    )
    .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["dry_run"], false);
    assert!(json.get("undo").is_some());
}

#[test]
fn nothing_chosen_opens_no_session() {
    let (fake, p) = full_fake();
    let report = apply_with(&fake, never_begin, &p, Some(&[])).unwrap();
    assert!(report.session_id.is_none());
    assert!(report.results.is_empty() && report.undo.is_empty());
    let report = apply_with(
        &fake,
        never_begin,
        &p,
        Some(&strings(&[
            "tweak:gaming.game_mode",
            "startup:user_run:Nobody",
        ])),
    )
    .unwrap();
    assert!(report.session_id.is_none());
    assert_eq!(
        outcome(&report, "tweak:gaming.game_mode"),
        StepOutcome::AlreadySet
    );
    assert_eq!(
        outcome(&report, "startup:user_run:Nobody"),
        StepOutcome::Skipped
    );
    assert_eq!((report.applied, report.already, report.skipped), (0, 1, 1));
    assert!(fake.journal.sessions().unwrap().is_empty());
    assert!(fake.journal.ops(10).unwrap().is_empty());
    assert!(calls(&fake).is_empty());
}

#[test]
fn not_elevated_refuses_before_begin() {
    let (mut fake, p) = full_fake();
    fake.elevated = false;
    fake.reads.borrow_mut().clear();
    let e = apply_with(&fake, never_begin, &p, None).unwrap_err();
    assert!(matches!(e, Error::NotElevated), "{e}");
    // Refused before planning: no scan, startup, network or plan read ran.
    assert!(fake.reads.borrow().is_empty(), "{:?}", fake.reads.borrow());
    // Also when the chosen rows would change nothing.
    let e = apply_with(
        &fake,
        never_begin,
        &p,
        Some(&strings(&["tweak:gaming.game_mode"])),
    )
    .unwrap_err();
    assert!(matches!(e, Error::NotElevated), "{e}");
    assert!(fake.reads.borrow().is_empty(), "{:?}", fake.reads.borrow());
    assert!(fake.journal.sessions().unwrap().is_empty());
    assert!(fake.journal.ops(10).unwrap().is_empty());
    assert!(calls(&fake).is_empty());
    // Plans still work without elevation.
    assert!(!plan(&fake, &p).elevated);
}

#[test]
fn other_account_withholds_per_user_rows_before_begin() {
    let mut fake = FakeProfiles::new();
    fake.other_account = Some(OTHER.into());
    // The plan's probe passes; the check before the session refuses.
    fake.other_account_from_call = 2;
    fake.scan = Ok(scan_report(vec![
        scan_item("interface.file_extensions", ItemState::NotApplied),
        scan_item("performance.sysmain", ItemState::NotApplied),
    ]));
    let p = profile(json!({"tweaks": ["interface.file_extensions", "performance.sysmain"]}));
    let report = apply(&fake, &p, None).unwrap();
    assert_eq!(
        outcome(&report, "tweak:interface.file_extensions"),
        StepOutcome::Skipped
    );
    let withheld = report
        .results
        .iter()
        .find(|r| r.key == "tweak:interface.file_extensions")
        .unwrap();
    assert_eq!(withheld.details, [OTHER]);
    assert_eq!(
        outcome(&report, "tweak:performance.sysmain"),
        StepOutcome::Applied
    );
    assert_eq!(calls(&fake), ["apply_items performance.sysmain"]);

    // Only per-user rows: no session at all.
    let mut fake = FakeProfiles::new();
    fake.other_account = Some(OTHER.into());
    fake.other_account_from_call = 2;
    fake.scan = Ok(scan_report(vec![scan_item(
        "interface.file_extensions",
        ItemState::NotApplied,
    )]));
    let p = profile(json!({"tweaks": ["interface.file_extensions"]}));
    let report = apply_with(&fake, never_begin, &p, None).unwrap();
    assert!(report.session_id.is_none());
    assert_eq!(report.skipped, 1);
    assert!(calls(&fake).is_empty());
}

#[test]
fn sections_run_in_order_under_one_session() {
    let (fake, p) = full_fake();
    let report = apply(&fake, &p, None).unwrap();
    assert_eq!(
        calls(&fake),
        [
            "apply_items performance.sysmain".to_string(),
            "set_startup user_run:Discord false".to_string(),
            format!("set_dns {}", guid(1)),
            "wu_apply windows_update:active_hours".to_string(),
            "maintenance_apply".to_string(),
            "apply_items appx.Microsoft.BingNews".to_string(),
        ]
    );
    let sessions = fake.journal.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].label, "profile: Everything");
    assert_eq!(report.session_id, Some(sessions[0].id));
    assert_eq!(session_label("  Everything "), "profile: Everything");
    let sections: Vec<Section> = report.results.iter().map(|r| r.section).collect();
    assert_eq!(
        sections,
        [
            Section::Tweaks,
            Section::Startup,
            Section::Dns,
            Section::WindowsUpdate,
            Section::Maintenance,
            Section::Apps,
        ]
    );
    assert_eq!((report.applied, report.failed), (6, 0));
    assert!(!report.dry_run);
    assert_eq!(report.name, "Everything");
}

#[test]
fn started_row_precedes_first_change() {
    let (fake, p) = full_fake();
    // Every fake mutator asserts that the started row is already written.
    apply(&fake, &p, None).unwrap();
    let ops = fake.journal.ops(100).unwrap();
    let rows: Vec<(&str, &str)> = ops
        .iter()
        .rev()
        .filter(|o| o.op == OP_APPLY_PROFILE)
        .map(|o| (o.outcome.as_str(), o.target.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("started", "profile \"Everything\""),
            ("applied", "profile \"Everything\"")
        ]
    );
    let started = ops.iter().find(|o| o.outcome == "started").unwrap();
    assert_eq!(
        started.detail.as_deref(),
        Some(
            "6 changes: 1 setting, 1 startup app, 1 DNS, 1 Windows Update setting, scheduled \
             maintenance, 1 app"
        )
    );
    let session = fake.journal.sessions().unwrap()[0].id;
    assert!(ops
        .iter()
        .filter(|o| o.op == OP_APPLY_PROFILE)
        .all(|o| o.session_id == Some(session)));
}

#[test]
fn final_row_counts_and_failed_outcome() {
    let (mut fake, p) = full_fake();
    fake.fail.insert("startup:user_run:Discord".into());
    let report = apply(
        &fake,
        &p,
        Some(&strings(&[
            "tweak:performance.sysmain",
            "startup:user_run:Discord",
            "tweak:gaming.game_mode",
            "app:Contoso.Unknown",
        ])),
    )
    .unwrap();
    assert_eq!(
        (
            report.applied,
            report.already,
            report.skipped,
            report.failed
        ),
        (1, 1, 1, 1)
    );
    let last = &fake.journal.ops(1).unwrap()[0];
    assert_eq!(
        (last.op.as_str(), last.outcome.as_str()),
        (OP_APPLY_PROFILE, "failed")
    );
    assert_eq!(
        last.detail.as_deref(),
        Some("1 applied, 1 already set, 1 skipped, 1 failed")
    );
}

#[test]
fn keys_choose_rows_and_report_unknown_already_and_skipped_keys() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        scan_item("performance.sysmain", ItemState::NotApplied),
        scan_item("gaming.game_mode", ItemState::Applied),
        scan_item("performance.search_indexer", ItemState::NotApplied),
    ]));
    let p = profile(json!({"tweaks": ["performance.sysmain", "gaming.game_mode",
        "privacy.new_future_id", "performance.search_indexer"]}));
    let chosen = strings(&[
        "tweak:performance.search_indexer",
        "tweak:gaming.game_mode",
        "tweak:privacy.new_future_id",
        "startup:user_run:Nobody",
        "tweak:performance.search_indexer",
    ]);
    let report = apply(&fake, &p, Some(&chosen)).unwrap();
    // A cautioned change row is applied when its key is given; sysmain was not chosen.
    assert_eq!(calls(&fake), ["apply_items performance.search_indexer"]);
    assert_eq!(
        outcome(&report, "tweak:performance.search_indexer"),
        StepOutcome::Applied
    );
    let already = report
        .results
        .iter()
        .find(|r| r.key == "tweak:gaming.game_mode")
        .unwrap();
    assert_eq!(already.outcome, StepOutcome::AlreadySet);
    assert_eq!(already.details, ["Already set on this PC"]);
    let unknown = report
        .results
        .iter()
        .find(|r| r.key == "tweak:privacy.new_future_id")
        .unwrap();
    assert_eq!(unknown.outcome, StepOutcome::Skipped);
    assert!(unknown.details[0].starts_with("Unknown to this version of Cairn: Cairn "));
    let missing = report
        .results
        .iter()
        .find(|r| r.key == "startup:user_run:Nobody")
        .unwrap();
    assert_eq!(missing.outcome, StepOutcome::Skipped);
    assert_eq!(missing.section, Section::Startup);
    assert_eq!(missing.title, "startup:user_run:Nobody");
    assert_eq!(
        missing.details,
        ["Not part of this profile's plan on this PC."]
    );
    assert_eq!(report.results.len(), 4);
    assert!(!report
        .results
        .iter()
        .any(|r| r.key == "tweak:performance.sysmain"));
}

#[test]
fn default_selection_leaves_cautioned_rows_out() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        scan_item("performance.sysmain", ItemState::NotApplied),
        scan_item("performance.search_indexer", ItemState::NotApplied),
    ]));
    fake.wu_steps = Ok(vec![step(
        "windows_update:defer_feature",
        "Defer feature updates",
        StepStatus::Change,
        Some("Windows Update waits 30 days."),
    )]);
    let p = profile(
        json!({"tweaks": ["performance.sysmain", "performance.search_indexer"],
        "windows_update": {"defer_feature_days": 30}}),
    );
    let report = apply(&fake, &p, None).unwrap();
    assert_eq!(calls(&fake), ["apply_items performance.sysmain"]);
    assert_eq!(report.results.len(), 1);
}

#[test]
fn row_failure_continues_with_next_section() {
    let (mut fake, p) = full_fake();
    fake.fail.insert("tweak:performance.sysmain".into());
    fake.fail.insert(format!("dns:{}", guid(1)));
    let report = apply(&fake, &p, None).unwrap();
    assert_eq!(
        outcome(&report, "tweak:performance.sysmain"),
        StepOutcome::Failed
    );
    assert_eq!(
        outcome(&report, "startup:user_run:Discord"),
        StepOutcome::Applied
    );
    assert_eq!(
        outcome(&report, &format!("dns:{}", guid(1))),
        StepOutcome::Failed
    );
    assert_eq!(
        outcome(&report, "windows_update:active_hours"),
        StepOutcome::Applied
    );
    assert_eq!(outcome(&report, "maintenance"), StepOutcome::Applied);
    assert_eq!(
        outcome(&report, "app:Microsoft.BingNews"),
        StepOutcome::Applied
    );
    assert_eq!(report.failed, 2);
    assert_eq!(calls(&fake).len(), 6);
    // A failed DNS write or tweak may have recorded a baseline, so the adapter and the
    // tweak's targets are in the undo.
    assert_eq!(report.undo.dns, [guid(1)]);
    assert_eq!(report.undo.services, ["SysMain"]);
    assert_eq!(fake.journal.ops(1).unwrap()[0].outcome, "failed");
}

#[test]
fn failed_rows_that_recorded_changes_are_undone_too() {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        scan_item("privacy.office_telemetry", ItemState::NotApplied),
        scan_item("performance.sysmain", ItemState::NotApplied),
        appx_item("Microsoft.BingNews", ItemState::NotApplied),
    ]));
    // Each of these writes and records its first change, then fails.
    fake.partial.insert("tweak:privacy.office_telemetry".into());
    fake.partial.insert("app:Microsoft.BingNews".into());
    let p = profile(json!({
        "tweaks": ["privacy.office_telemetry", "performance.sysmain"],
        "apps": ["Microsoft.BingNews"]
    }));
    let report = apply(&fake, &p, None).unwrap();
    assert_eq!(
        outcome(&report, "tweak:privacy.office_telemetry"),
        StepOutcome::Failed
    );
    assert_eq!(
        outcome(&report, "tweak:performance.sysmain"),
        StepOutcome::Applied
    );
    assert_eq!(
        outcome(&report, "app:Microsoft.BingNews"),
        StepOutcome::Failed
    );
    assert_eq!(fake.journal.active_registry().unwrap().len(), 1);
    assert_eq!(fake.journal.active_appx().unwrap().len(), 1);

    let office = fake
        .revert_filter(&strings(&["privacy.office_telemetry"]))
        .unwrap();
    assert_eq!(office.registry.len(), 3);
    assert_eq!(report.undo.registry, office.registry);
    assert_eq!(report.undo.services, ["SysMain"]);
    assert_eq!(
        report.undo.appx_families,
        ["Microsoft.BingNews_8wekyb3d8bbwe"]
    );
    // "Undo these changes" restores every record the session left, as Revert All would.
    let undo = rollback_filtered(&fake.journal, &report.undo, true).unwrap();
    let all = rollback_journal(&fake.journal, true).unwrap();
    assert_eq!(all.actions.len(), 2, "{:?}", all.actions);
    assert_eq!(undo.actions, all.actions);
}

#[test]
fn undo_selects_only_applied_rows() {
    let (mut fake, p) = full_fake();
    fake.startup = Ok(vec![
        startup_entry(StartupSource::UserRun, "Discord", "Discord", true),
        startup_entry(StartupSource::UserRun, "Steam", "Steam", true),
    ]);
    fake.startup_outcomes.insert(
        "user_run:Steam".into(),
        MutationOutcome::AlreadyInDesiredState,
    );
    let mut p = p;
    p.startup.push(super::format::StartupChoice {
        id: "user_run:Steam".into(),
        name: String::new(),
    });
    let report = apply(
        &fake,
        &p,
        Some(&strings(&[
            "tweak:performance.sysmain",
            "tweak:gaming.game_mode",
            "startup:user_run:Discord",
            "startup:user_run:Steam",
            &format!("dns:{}", guid(1)),
            "app:Microsoft.BingNews",
        ])),
    )
    .unwrap();
    assert_eq!(
        outcome(&report, "startup:user_run:Steam"),
        StepOutcome::AlreadySet
    );
    let undo = &report.undo;
    assert_eq!(undo.services, ["SysMain"]);
    assert_eq!(
        undo.registry,
        [startup::registry_target("user_run:Discord").unwrap()]
    );
    assert_eq!(undo.dns, [guid(1)]);
    assert_eq!(undo.appx_families, ["Microsoft.BingNews_8wekyb3d8bbwe"]);
    assert!(!undo.power && undo.scheduled_tasks.is_empty() && undo.task_definitions.is_empty());
}

#[test]
fn undo_merges_wu_and_maintenance_filters() {
    let (mut fake, p) = full_fake();
    let wu_target = RegistryTarget {
        hive: Hive::LocalMachine,
        key_path: r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings".into(),
        value_name: "ActiveHoursStart".into(),
    };
    fake.wu_filter = RollbackFilter {
        registry: vec![wu_target.clone()],
        ..Default::default()
    };
    let task = r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001".to_string();
    fake.maintenance_filter = RollbackFilter {
        task_definitions: vec![task.clone()],
        ..Default::default()
    };
    let report = apply(
        &fake,
        &p,
        Some(&strings(&["windows_update:active_hours", "maintenance"])),
    )
    .unwrap();
    assert_eq!(report.undo.registry, [wu_target]);
    assert_eq!(report.undo.task_definitions, [task]);
    let json = serde_json::to_value(&report.undo).unwrap();
    for field in [
        "registry",
        "services",
        "appx_families",
        "power",
        "scheduled_tasks",
        "dns",
        "task_definitions",
    ] {
        assert!(json.get(field).is_some(), "{field}");
    }
}

#[test]
fn merge_filter_dedupes_ignoring_case() {
    let target = |key: &str, name: &str| RegistryTarget {
        hive: Hive::CurrentUser,
        key_path: key.into(),
        value_name: name.into(),
    };
    let mut into = RollbackFilter {
        registry: vec![target(r"Software\Contoso", "Value")],
        services: vec!["SysMain".into()],
        dns: vec![guid(1)],
        ..Default::default()
    };
    merge_filter(
        &mut into,
        RollbackFilter {
            registry: vec![
                target(r"SOFTWARE\CONTOSO", "value"),
                target(r"Software\Fabrikam", "Value"),
            ],
            services: vec!["sysmain".into(), "WSearch".into()],
            appx_families: vec!["Microsoft.BingNews_8wekyb3d8bbwe".into()],
            power: true,
            scheduled_tasks: vec![r"\Microsoft\Windows\Autochk\Proxy".into()],
            dns: vec![
                guid(1)
                    .to_ascii_uppercase()
                    .trim_matches(['{', '}'])
                    .to_string(),
                guid(2),
            ],
            task_definitions: vec![r"\Cairn\Maintenance-S-1-5-18".into()],
        },
    );
    merge_filter(
        &mut into,
        RollbackFilter {
            appx_families: vec!["MICROSOFT.BINGNEWS_8WEKYB3D8BBWE".into()],
            scheduled_tasks: vec![r"\microsoft\windows\autochk\proxy".into()],
            task_definitions: vec![r"\CAIRN\MAINTENANCE-S-1-5-18".into()],
            ..Default::default()
        },
    );
    assert_eq!(into.registry.len(), 2);
    assert_eq!(into.services, ["SysMain", "WSearch"]);
    assert_eq!(into.appx_families.len(), 1);
    assert!(into.power);
    assert_eq!(into.scheduled_tasks.len(), 1);
    assert_eq!(into.dns, [guid(1), guid(2)]);
    assert_eq!(into.task_definitions.len(), 1);
}

#[test]
fn hostile_profile_reaches_no_mutator() {
    let mut fake = dns_fake();
    fake.scan = Ok(scan_report(vec![appx_item(
        "Microsoft.BingNews",
        ItemState::NotApplied,
    )]));
    fake.startup = Ok(vec![startup_entry(
        StartupSource::UserRun,
        "Discord",
        "Discord",
        true,
    )]);
    fake.wu_steps = Ok(vec![step(
        "windows_update:exclude_drivers",
        "Exclude drivers",
        StepStatus::Skipped,
        None,
    )]);
    let p = profile(json!({
        "tweaks": ["privacy.no_such_setting", "system.format_disk"],
        "apps": ["Microsoft.WindowsStore", "Microsoft.Windows.ShellExperienceHost", "Contoso.Tool",
            "Fabrikam.*", "Microsoft.*"],
        "startup": [
            {"id": "user_run:C:\\Windows\\System32\\cmd.exe /c del C:\\"},
            {"id": "machine_run:powershell -enc AAAA"},
            {"id": "policy_user_run:Discord"},
            {"id": "future_source:Discord"}
        ],
        "dns": {"ethernet": {"ipv4": "evil_resolver"}, "wifi": {"ipv6": "attacker"}},
        "windows_update": {"exclude_drivers": true}
    }));
    let planned = plan(&fake, &p);
    assert!(
        planned.rows.iter().all(|r| r.status != StepStatus::Change),
        "{:?}",
        planned.rows
    );
    let every_key: Vec<String> = planned.rows.iter().map(|r| r.key.clone()).collect();
    let report = apply_with(&fake, never_begin, &p, Some(&every_key)).unwrap();
    assert!(report.session_id.is_none());
    let report = apply_with(&fake, never_begin, &p, None).unwrap();
    assert!(report.session_id.is_none());
    assert!(calls(&fake).is_empty());
    assert!(fake.journal.sessions().unwrap().is_empty());
    assert!(!fake.reads.borrow().contains(&"plan_dns"));
}

// ───────────────────────────── export ─────────────────────────────

fn export_fake() -> FakeProfiles {
    let mut fake = FakeProfiles::new();
    fake.scan = Ok(scan_report(vec![
        revertible(scan_item("performance.sysmain", ItemState::Applied)),
        scan_item("interface.file_extensions", ItemState::Applied),
        scan_item("privacy.cortana", ItemState::NotApplied),
        appx_item("Microsoft.BingNews", ItemState::Applied),
        appx_item("king.com.CandyCrushSaga", ItemState::NotApplied),
    ]));
    let mut policy = startup_entry(StartupSource::PolicyMachineRun, "Agent", "Agent", false);
    policy.can_toggle = false;
    fake.startup = Ok(vec![
        startup_entry(StartupSource::UserRun, "Discord", "Discord", false),
        startup_entry(StartupSource::UserRun, "Steam", "Steam", true),
        policy,
    ]);
    let mut wired = adapter(2, "Ethernet 2", AdapterKind::Ethernet);
    wired.primary = true;
    wired.dns_ipv4 = preset_config("cloudflare", IpFamily::Ipv4);
    wired.dns_ipv6 = preset_config("cloudflare", IpFamily::Ipv6);
    let wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
    fake.network = Ok(network_report(vec![wired, wifi]));
    fake.wu_current = Ok(WindowsUpdateChoice {
        active_hours: Some(ActiveHoursChoice {
            automatic: false,
            start: Some(8),
            end: Some(23),
        }),
        exclude_drivers: true,
        ..Default::default()
    });
    fake.maintenance_current = Ok(Some(MaintenanceChoice {
        enabled: true,
        day: Some(ScheduleDay::Sunday),
        time: Some("12:00".into()),
        clean: vec!["user_temp".into(), "windows_temp".into()],
        sfc_verify: true,
        dism_check: false,
    }));
    fake
}

#[test]
fn candidates_list_applied_tweaks_removed_apps_disabled_startup_preset_dns() {
    let fake = export_fake();
    let c = candidates_with(&fake).unwrap();
    let keys: Vec<&str> = c.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "tweak:performance.sysmain",
            "tweak:interface.file_extensions",
            "app:Microsoft.BingNews",
            "startup:user_run:Discord",
            "dns:ethernet",
            "windows_update:active_hours",
            "windows_update:exclude_drivers",
            "maintenance",
        ]
    );
    let by_key = |key: &str| c.rows.iter().find(|r| r.key == key).unwrap();
    assert_eq!(
        by_key("tweak:performance.sysmain").detail,
        "Changed by Cairn"
    );
    assert_eq!(
        by_key("tweak:interface.file_extensions").detail,
        "Already set on this PC"
    );
    assert!(by_key("tweak:interface.file_extensions").per_user);
    assert_eq!(by_key("app:Microsoft.BingNews").detail, "Removed by Cairn");
    assert_eq!(
        by_key("startup:user_run:Discord").detail,
        "HKCU Run · turned off"
    );
    let dns = by_key("dns:ethernet");
    assert_eq!(dns.title, "DNS servers (Ethernet)");
    assert_eq!(
        dns.detail,
        "IPv4: Cloudflare · IPv6: Cloudflare (from Ethernet 2)"
    );
    assert_eq!(
        by_key("windows_update:active_hours").detail,
        "08:00 to 23:00"
    );
    assert_eq!(
        by_key("maintenance").detail,
        "Every Sunday at 12:00 · 2 cleanup targets · system file check"
    );
    assert!(c.rows.iter().all(|r| r.selected && r.caution.is_none()));
    assert!(c.other_account.is_none() && c.warnings.is_empty());
    assert!(calls(&fake).is_empty());

    let (built, missing) = build_with(&fake, " Desk PC ", " Mine ", None, "2026-09-28").unwrap();
    assert!(missing.is_empty());
    assert_eq!(built.name, "Desk PC");
    assert_eq!(built.description, "Mine");
    assert_eq!(built.created.as_deref(), Some("2026-09-28"));
    assert_eq!(built.created_with, Some(format!("Cairn {VERSION}")));
    assert_eq!(
        built.tweaks,
        ["performance.sysmain", "interface.file_extensions"]
    );
    assert_eq!(built.apps, ["Microsoft.BingNews"]);
    assert_eq!(built.startup[0].id, "user_run:Discord");
    assert_eq!(built.startup[0].name, "Discord");
    let ethernet = built.dns.ethernet.clone().unwrap();
    assert_eq!(
        (ethernet.ipv4.as_deref(), ethernet.ipv6.as_deref()),
        (Some("cloudflare"), Some("cloudflare"))
    );
    assert!(built.dns.wifi.is_none());
    let wu = built.windows_update.clone().unwrap();
    assert!(wu.exclude_drivers && wu.active_hours.is_some() && wu.restart_notify.is_none());
    assert!(built.maintenance.is_some());
    assert_eq!(parse(&to_text(&built)).unwrap(), built);
}

#[test]
fn custom_profile_and_policy_dns_are_never_exported() {
    let mut fake = export_fake();
    if let Ok(report) = &mut fake.network {
        report.adapters[0].dns_ipv4 = DnsConfig {
            mode: DnsMode::Manual,
            servers: vec!["192.0.2.53".into()],
            preset: None,
            profile_servers: Vec::new(),
        };
        report.adapters[0].dns_ipv6 = automatic();
        report.adapters[1].dns_ipv4 = DnsConfig {
            mode: DnsMode::Profile,
            servers: vec!["192.0.2.54".into()],
            preset: None,
            profile_servers: vec!["192.0.2.54".into()],
        };
    }
    let c = candidates_with(&fake).unwrap();
    assert!(!c.rows.iter().any(|r| r.section == Section::Dns));
    assert_eq!(
        c.warnings,
        ["Custom DNS servers on Ethernet are not saved in profiles."]
    );

    let mut fake = export_fake();
    if let Ok(report) = &mut fake.network {
        report.dns_policy = vec!["198.51.100.53".into()];
    }
    let c = candidates_with(&fake).unwrap();
    assert!(!c.rows.iter().any(|r| r.section == Section::Dns));
}

#[test]
fn automatic_family_exported_only_beside_a_preset() {
    let mut fake = export_fake();
    if let Ok(report) = &mut fake.network {
        report.adapters[0].dns_ipv6 = automatic();
    }
    let (built, _) = build_with(&fake, "x", "", None, "2026-09-28").unwrap();
    let ethernet = built.dns.ethernet.unwrap();
    assert_eq!(ethernet.ipv4.as_deref(), Some("cloudflare"));
    assert_eq!(ethernet.ipv6.as_deref(), Some("automatic"));
    let c = candidates_with(&fake).unwrap();
    let dns = c.rows.iter().find(|r| r.key == "dns:ethernet").unwrap();
    assert_eq!(
        dns.detail,
        "IPv4: Cloudflare · IPv6: Automatic (from Ethernet 2)"
    );

    if let Ok(report) = &mut fake.network {
        report.adapters[0].dns_ipv4 = automatic();
    }
    let c = candidates_with(&fake).unwrap();
    assert!(!c.rows.iter().any(|r| r.section == Section::Dns));

    // Another adapter of the kind with other settings is named in a warning.
    let mut fake = export_fake();
    if let Ok(report) = &mut fake.network {
        report
            .adapters
            .push(adapter(3, "Ethernet 3", AdapterKind::Ethernet));
    }
    let c = candidates_with(&fake).unwrap();
    assert_eq!(
        c.warnings,
        ["Ethernet 3 uses other DNS servers; the profile keeps these."]
    );
}

#[test]
fn per_user_candidates_unselected_for_another_account() {
    let mut fake = export_fake();
    fake.other_account = Some(OTHER.into());
    let c = candidates_with(&fake).unwrap();
    assert!(c
        .other_account
        .as_deref()
        .unwrap()
        .contains("were read from that account"));
    for r in &c.rows {
        if r.per_user {
            assert!(!r.selected, "{}", r.key);
            assert_eq!(
                r.caution.as_deref(),
                Some("Read from the account Cairn runs as, not yours.")
            );
        } else {
            assert!(r.selected && r.caution.is_none(), "{}", r.key);
        }
    }
    let per_user: Vec<&str> = c
        .rows
        .iter()
        .filter(|r| r.per_user)
        .map(|r| r.key.as_str())
        .collect();
    assert_eq!(
        per_user,
        [
            "tweak:interface.file_extensions",
            "app:Microsoft.BingNews",
            "startup:user_run:Discord"
        ]
    );
    let (built, _) = build_with(&fake, "x", "", None, "2026-09-28").unwrap();
    assert_eq!(built.tweaks, ["performance.sysmain"]);
    assert!(built.apps.is_empty() && built.startup.is_empty());
}

#[test]
fn build_reports_missing_keys_and_refuses_empty() {
    let fake = export_fake();
    let (built, missing) = build_with(
        &fake,
        "x",
        "",
        Some(&strings(&[
            "tweak:performance.sysmain",
            "tweak:gaming.gone",
            "tweak:gaming.gone",
        ])),
        "2026-09-28",
    )
    .unwrap();
    assert_eq!(built.tweaks, ["performance.sysmain"]);
    assert!(built.apps.is_empty() && built.maintenance.is_none());
    assert_eq!(missing, ["tweak:gaming.gone"]);
    let nothing = "Nothing was selected, so no profile was written.";
    for keys in [strings(&[]), strings(&["tweak:gaming.gone"])] {
        let e = build_with(&fake, "x", "", Some(&keys), "2026-09-28").unwrap_err();
        assert_eq!(e.to_string(), nothing);
    }
    let e = build_with(&fake, "  ", "", None, "2026-09-28").unwrap_err();
    assert_eq!(e.to_string(), "The profile needs a name.");
    let e = build_with(&fake, &"n".repeat(61), "", None, "2026-09-28").unwrap_err();
    assert!(e.to_string().contains("longer than 60"));
}

#[test]
fn export_contains_no_machine_identifiers() {
    let mut fake = export_fake();
    if let Ok(report) = &mut fake.network {
        report.adapters[1].dns_ipv4 = DnsConfig {
            mode: DnsMode::Manual,
            servers: vec!["192.0.2.53".into()],
            preset: None,
            profile_servers: Vec::new(),
        };
        report.adapters[0].description = "Contoso 2.5GbE Controller on TEST-PC".into();
    }
    if let Ok(entries) = &mut fake.startup {
        entries[0].command = r"\\TEST-PC\share\Discord.exe --user C:\Users\Test".into();
        entries[0].path = r"C:\Users\Test\AppData\Local\Discord\Discord.exe".into();
    }
    if let Ok(scan) = &mut fake.scan {
        scan.items[3].actions[0].detail =
            "removed: Microsoft.BingNews_4.55.62231.0_x64__8wekyb3d8bbwe".into();
    }
    let (built, _) = build_with(&fake, "Desk PC", "", None, "2026-09-28").unwrap();
    let text = to_text(&built);
    for forbidden in [
        "TEST-PC",
        "Ethernet 2",
        "aaaaaaaa",
        r"Users\Test",
        r"Users\\Test",
        "4.55.62231.0",
        "8wekyb3d8bbwe",
        "192.0.2.53",
        "00-11-22",
        "192.168.0",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    assert!(text.contains("\"cloudflare\""));
}

#[test]
fn startup_names_with_an_identifier_are_not_exported() {
    // A browser's auto-start entry is named after a hash of its profile folder, whose path
    // holds the user name.
    let edge = "MicrosoftEdgeAutoLaunch_0123456789ABCDEF0123456789ABCDEF";
    let mut fake = export_fake();
    if let Ok(entries) = &mut fake.startup {
        for name in [
            edge,
            "Fabrikam_0123456789abcdef",
            "Contoso_Updater",
            "Northwind_123456789ABCDEF",
        ] {
            entries.push(startup_entry(StartupSource::UserRun, name, name, false));
        }
    }
    let c = candidates_with(&fake).unwrap();
    let startup: Vec<&str> = c
        .rows
        .iter()
        .filter(|r| r.section == Section::Startup)
        .map(|r| r.key.as_str())
        .collect();
    assert_eq!(
        startup,
        [
            "startup:user_run:Discord",
            "startup:user_run:Contoso_Updater",
            "startup:user_run:Northwind_123456789ABCDEF",
        ]
    );
    let note =
        "can't be saved in a profile: its name contains an identifier of this PC or account.";
    assert_eq!(
        c.warnings,
        [
            format!("The startup app MicrosoftEdgeAutoLaunch {note}"),
            format!("The startup app Fabrikam {note}"),
        ]
    );

    let hashed_key = format!("startup:user_run:{edge}");
    let (chosen, missing) = build_with(
        &fake,
        "Desk PC",
        "",
        Some(&[hashed_key.clone(), "startup:user_run:Discord".into()]),
        "2026-09-28",
    )
    .unwrap();
    assert_eq!(missing, [hashed_key]);
    let (every, _) = build_with(&fake, "Desk PC", "", None, "2026-09-28").unwrap();
    assert_eq!(every.startup.len(), 3);
    for built in [chosen, every] {
        let text = to_text(&built);
        assert!(
            !text.to_ascii_lowercase().contains("0123456789abcdef"),
            "{text}"
        );
    }
}
