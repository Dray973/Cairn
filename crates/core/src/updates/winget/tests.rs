//! Tests of the winget lane: requests, plans, and the work with scripted processes. Nothing
//! here starts a process: every launcher is a [`ScriptedLauncher`] or a [`PanicLauncher`].

use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::SystemTime;

use parking_lot::Mutex;

use super::*;
use crate::jobs::testing::{Gate, PanicLauncher, ScriptedLauncher, Step};
use crate::jobs::{HostJobId, HostShutdownAction, JobState};
use crate::tools::launch::{CommandSpec, Launched};

const WINGET: &str = r"C:\Program Files\WindowsApps\Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe\winget.exe";
const EN_US: &str = include_str!("fixtures/en_us_basic.txt");
const EXPORT_JSON: &str = r#"{
  "Sources": [
    { "Packages": [
        { "PackageIdentifier": "Contoso.Editor", "Version": "1.2.0" },
        { "PackageIdentifier": "Fabrikam.MediaPlayer", "Version": "2.0.11.6" },
        { "PackageIdentifier": "Northwind.Tools.x64", "Version": "4.0" }
      ],
      "SourceDetails": { "Name": "winget" } },
    { "Packages": [ { "PackageIdentifier": "9NTAILSPIN0001", "Version": "1.0.0.0" } ],
      "SourceDetails": { "Name": "msstore" } }
  ]
}"#;

// ───────────────────────────── environments ─────────────────────────────

fn yes() -> bool {
    true
}
fn no() -> bool {
    false
}
fn same_user() -> Result<bool> {
    Ok(false)
}
fn other_user() -> Result<bool> {
    Ok(true)
}
fn user_unknown() -> Result<bool> {
    Err(Error::Other("no session".into()))
}
fn fake_location() -> WingetLocation {
    WingetLocation {
        path: PathBuf::from(WINGET),
        package_full_name: "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe".into(),
        package_version: "1.29.380.0".into(),
    }
}
fn located() -> Result<Option<WingetLocation>> {
    Ok(Some(fake_location()))
}
fn not_installed() -> Result<Option<WingetLocation>> {
    Ok(None)
}
fn no_processes() -> Option<HashSet<String>> {
    Some(HashSet::new())
}
fn winget_running() -> Option<HashSet<String>> {
    Some(["winget.exe".to_string()].into_iter().collect())
}
fn idle() -> Option<bool> {
    Some(false)
}
fn busy() -> Option<bool> {
    Some(true)
}
fn no_tool() -> Option<String> {
    None
}
fn sfc_running() -> Option<String> {
    Some("Repair system files".into())
}
fn fixed_now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-25T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// An elevated signed-in user with winget installed, on AC power, nothing else running.
fn env() -> WingetEnv {
    WingetEnv {
        elevated: yes,
        other_user: same_user,
        locate: located,
        processes: no_processes,
        msi_busy: idle,
        on_battery: idle,
        servicing_tool: no_tool,
        now: fixed_now,
        installs_forbidden: no,
    }
}

fn host(dir: &Path) -> JobHost {
    JobHost::new(HostConfig {
        tick: Duration::from_millis(5),
        settle_wait: Duration::from_secs(1),
        stop_wait: Duration::from_secs(2),
        ..host_config(dir.join("jobs").join("winget"))
    })
}

fn deps(env: WingetEnv, launcher: Arc<dyn Launcher>) -> WingetDeps {
    WingetDeps {
        env,
        launcher: Ok(launcher),
        limits: Limits {
            version: Duration::from_secs(5),
            list: Duration::from_secs(5),
            item: Duration::from_secs(5),
        },
    }
}

fn journal(dir: &Path) -> Arc<Journal> {
    Arc::new(Journal::open(dir.join("journal.db")).unwrap())
}

fn no_journal() -> Result<Arc<Journal>> {
    panic!("this job needs no journal")
}

fn item(id: &str) -> UpdateItem {
    UpdateItem {
        id: id.into(),
        source: "winget".into(),
        name: format!("{id} app"),
        from: Some("1.0".into()),
        to: Some("2.0".into()),
    }
}

fn request(kind: UpdatesKind, ids: &[&str]) -> UpdatesRequest {
    UpdatesRequest::new(kind, ids.iter().map(|id| item(id)).collect()).unwrap()
}

/// `(op, target, outcome)` of every ops_log row, oldest first.
fn rows(journal: &Journal) -> Vec<(String, String, String)> {
    let mut ops: Vec<_> = journal
        .ops(1000)
        .unwrap()
        .into_iter()
        .map(|o| (o.op, o.target, o.outcome))
        .collect();
    ops.reverse();
    ops
}

fn row(op: &str, target: &str, outcome: &str) -> (String, String, String) {
    (op.to_string(), target.to_string(), outcome.to_string())
}

fn result_json(host: &JobHost, id: HostJobId) -> serde_json::Value {
    host.result(id, 0)
        .expect("a published result")
        .1
        .as_ref()
        .clone()
}

fn states(value: &serde_json::Value) -> Vec<String> {
    value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["state"].as_str().unwrap().to_string())
        .collect()
}

/// Every launched argv holds no forbidden flag.
fn assert_no_forbidden_flags(launched: &[CommandSpec]) {
    for command in launched {
        assert_eq!(forbidden_flag(&command.args), None, "{:?}", command.args);
        assert_eq!(command.program, PathBuf::from(WINGET));
    }
}

/// A launcher whose export launches write `export` (when given) to their `--output` path,
/// and which records for every app launch whether the journal already held its "started"
/// row.
struct Scripted {
    launcher: Arc<ScriptedLauncher>,
    seen_started: Arc<Mutex<Vec<(String, bool)>>>,
}

fn scripted(export: Option<&'static str>, journal: Option<Arc<Journal>>) -> Scripted {
    scripted_with(ScriptedLauncher::new(), export, journal)
}

fn scripted_with(
    mut launcher: ScriptedLauncher,
    export: Option<&'static str>,
    journal: Option<Arc<Journal>>,
) -> Scripted {
    let launches = Arc::clone(&launcher.launches);
    let seen_started = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_started);
    launcher.before_launch = Some(Box::new(move || {
        let command = launches.lock().last().cloned().unwrap();
        let args = &command.args;
        if args.first().map(String::as_str) == Some("export") {
            if let Some(json) = export {
                std::fs::write(&args[2], json).unwrap();
            }
        }
        if matches!(
            args.first().map(String::as_str),
            Some("upgrade" | "install")
        ) && args.get(1).map(String::as_str) == Some("--id")
        {
            let id = args[2].clone();
            let op = if args[0] == "install" {
                OP_INSTALL
            } else {
                OP_UPGRADE
            };
            let present = journal.as_ref().is_some_and(|j| {
                j.ops(1000)
                    .unwrap()
                    .iter()
                    .any(|o| o.op == op && o.target == id && o.outcome == "started")
            });
            seen.lock().push((id, present));
        }
    }));
    Scripted {
        launcher: Arc::new(launcher),
        seen_started,
    }
}

fn exit_with(script: &mpsc::Sender<Step>, text: &str, code: i32) {
    if !text.is_empty() {
        script.send(Step::Write(text.as_bytes().to_vec())).unwrap();
    }
    script.send(Step::Exit(code)).unwrap();
}

fn hr(raw: u32) -> i32 {
    raw as i32
}

fn wait(host: &JobHost, id: HostJobId) -> HostJobSnapshot {
    let snapshot = host.wait(id, Duration::from_secs(20)).unwrap();
    assert!(snapshot.state.is_finished(), "{snapshot:?}");
    snapshot
}

/// Waits until `launcher` has launched `count` commands.
fn wait_launches(launcher: &ScriptedLauncher, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while launcher.launched().len() < count {
        assert!(
            Instant::now() < deadline,
            "only {} launches",
            launcher.launched().len()
        );
        thread::sleep(Duration::from_millis(2));
    }
}

// ───────────────────────────── requests and arguments ─────────────────────────────

#[test]
fn winget_versions_parse() {
    let v = |text: &str| WingetVersion::parse(text).map(|v| v.to_string());
    assert_eq!(v("v1.29.380").as_deref(), Some("1.29.380"));
    assert_eq!(v("1.30.0-preview").as_deref(), Some("1.30.0"));
    assert_eq!(v("V1.6").as_deref(), Some("1.6.0"));
    assert_eq!(v("version"), None);
    assert_eq!(v("v1"), None);
    assert_eq!(v(""), None);
    assert!(WingetVersion::parse("v1.5.9").unwrap() < MIN_VERSION);
    assert!(WingetVersion::parse("v1.6.0").unwrap() >= MIN_VERSION);
    assert!(outdated_text("1.5.0").contains("Cairn needs 1.6.0 or newer"));
}

#[test]
fn requests_validate_ids_and_keep_the_first_of_each() {
    assert!(UpdatesRequest::new(UpdatesKind::Scan, vec![item("Contoso.Editor")]).is_err());
    assert_eq!(
        UpdatesRequest::new(UpdatesKind::Scan, vec![]).unwrap(),
        UpdatesRequest::scan()
    );
    assert!(UpdatesRequest::new(UpdatesKind::Upgrade, vec![]).is_err());
    let too_many: Vec<UpdateItem> = (0..=MAX_BATCH_ITEMS)
        .map(|n| item(&format!("Contoso.App{n}")))
        .collect();
    assert!(UpdatesRequest::new(UpdatesKind::Install, too_many).is_err());
    for bad in ["-h", "--force", "Contoso", "Contoso Editor.x", "a\"b.c"] {
        assert!(
            UpdatesRequest::new(UpdatesKind::Upgrade, vec![item(bad)]).is_err(),
            "{bad}"
        );
    }
    let mut bad_source = item("Contoso.Editor");
    bad_source.source = "win get".into();
    assert!(UpdatesRequest::new(UpdatesKind::Upgrade, vec![bad_source]).is_err());

    let mut unnamed = item("Fabrikam.Player");
    unnamed.name = "  ".into();
    let request = UpdatesRequest::new(
        UpdatesKind::Upgrade,
        vec![item("Contoso.Editor"), item("contoso.EDITOR"), unnamed],
    )
    .unwrap();
    let ids: Vec<&str> = request.items().iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["Contoso.Editor", "Fabrikam.Player"]);
    assert_eq!(request.items()[1].name, "Fabrikam.Player");
    assert_eq!(request.kind(), UpdatesKind::Upgrade);
}

#[test]
fn arguments_are_fixed_and_never_forbidden() {
    let upgrade = request(UpdatesKind::Upgrade, &["Contoso.Editor"]);
    assert_eq!(
        upgrade.item_args(&upgrade.items()[0]),
        [
            "upgrade",
            "--id",
            "Contoso.Editor",
            "--exact",
            "--source",
            "winget",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity"
        ]
    );
    let install = request(UpdatesKind::Install, &["Contoso.Editor"]);
    let args = install.item_args(&install.items()[0]);
    assert_eq!(&args[..2], ["install", "--id"]);
    assert!(args.contains(&"--no-upgrade".to_string()));
    assert_eq!(
        export_args(Path::new(r"C:\x\inventory.json")),
        [
            "export",
            "--output",
            r"C:\x\inventory.json",
            "--include-versions",
            "--accept-source-agreements",
            "--disable-interactivity"
        ]
    );
    assert_eq!(version_args(), ["--version"]);
    assert_eq!(
        upgrade_list_args(),
        [
            "upgrade",
            "--accept-source-agreements",
            "--disable-interactivity"
        ]
    );
    let all = [
        upgrade.item_args(&upgrade.items()[0]),
        args,
        export_args(Path::new(r"C:\x.json")),
        version_args(),
        upgrade_list_args(),
    ];
    for args in all {
        assert_eq!(forbidden_flag(&args), None, "{args:?}");
    }
    assert!(item_args(UpdatesKind::Scan, &item("Contoso.Editor")).is_empty());
    let with = |extra: &str| {
        let mut args = upgrade.item_args(&upgrade.items()[0]);
        args.push(extra.to_string());
        forbidden_flag(&args).map(str::to_string)
    };
    for flag in FORBIDDEN_FLAGS {
        assert_eq!(with(flag).as_deref(), Some(*flag));
    }
    assert!(with("--override=/S").is_some());
    assert_eq!(
        forbidden_flag(&["install".into(), "--version".into(), "1.0".into()]),
        Some("--version")
    );
}

#[test]
fn job_command_lines_name_the_batch_and_its_apps_within_the_hosts_limit() {
    assert_eq!(
        job_command_line(&UpdatesRequest::scan()),
        "winget upgrade --accept-source-agreements --disable-interactivity"
    );
    assert_eq!(
        job_command_line(&request(
            UpdatesKind::Upgrade,
            &["Contoso.Editor", "Fabrikam.Player"]
        )),
        "winget upgrade --id <id> --exact … for 2 apps: Contoso.Editor, Fabrikam.Player"
    );
    assert_eq!(
        job_command_line(&request(UpdatesKind::Install, &["Contoso.Editor"])),
        "winget install --id <id> --exact … for 1 app: Contoso.Editor"
    );
    // A long batch names as many apps as fit and counts the rest, so the host keeps it whole.
    let ids: Vec<String> = (0..MAX_BATCH_ITEMS)
        .map(|n| format!("Contoso.App{n:03}"))
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let line = job_command_line(&request(UpdatesKind::Upgrade, &refs));
    assert!(line.chars().count() <= MAX_JOB_COMMAND_LINE, "{line}");
    assert!(
        line.starts_with(
            "winget upgrade --id <id> --exact … for 200 apps: Contoso.App000, Contoso.App001, "
        ),
        "{line}"
    );
    let named = line.matches("Contoso.App").count();
    assert!(named > 2, "{line}");
    assert!(
        line.ends_with(&format!(
            "Contoso.App{:03} and {} more",
            named - 1,
            MAX_BATCH_ITEMS - named
        )),
        "{line}"
    );
}

#[test]
fn update_all_says_why_it_takes_none_of_the_updates_found() {
    let listed =
        |id: &str, name: &str, source: &str, explicit_only: bool, selectable: bool| UpgradeRow {
            id: id.into(),
            name: name.into(),
            installed: "1.0".into(),
            available: "2.0".into(),
            source: source.into(),
            explicit_only,
            selectable,
            note: None,
        };
    let mut installer = listed(
        "Microsoft.AppInstaller",
        "App Installer",
        "winget",
        false,
        false,
    );
    installer.note = Some(APP_INSTALLER_NOTE.to_string());
    let rows = [
        listed("9NTAILSPIN0001", "Tailspin Notes", "msstore", false, true),
        listed("Fabrikam.Tool", "Fabrikam Tool", "winget", true, true),
        installer,
    ];
    let reasons: Vec<Option<LeftOut>> = rows
        .iter()
        .map(|row| left_out_of_update_all(row, false))
        .collect();
    assert_eq!(
        reasons,
        [
            Some(LeftOut::Store),
            Some(LeftOut::NamedOnly),
            Some(LeftOut::NotSelectable)
        ]
    );
    assert_eq!(left_out_of_update_all(&rows[0], true), None);
    let editor = listed("Contoso.Editor", "Contoso Editor", "winget", false, true);
    assert_eq!(left_out_of_update_all(&editor, false), None);

    let lines = update_all_takes_none(&rows, false);
    assert_ne!(
        lines,
        ["All apps are up to date."],
        "the check found updates"
    );
    assert_eq!(
        lines,
        [
            "The check found 3 updates, but --all leaves them out:",
            "  Tailspin Notes (9NTAILSPIN0001): a Microsoft Store app; add --include-store to \
             update it with --all",
            "  Fabrikam Tool (Fabrikam.Tool): winget updates it only when it is named, as in: \
             optctl updates upgrade Fabrikam.Tool",
            "  App Installer (Microsoft.AppInstaller): Cairn can't update it: Updated by the \
             Microsoft Store.",
            "Nothing was started.",
        ]
    );
    assert_eq!(
        update_all_takes_none(&rows[1..2], false)[0],
        "The check found 1 update, but --all leaves it out:"
    );
    assert_eq!(
        update_all_takes_none(&[], false),
        ["All apps are up to date."]
    );
}

#[test]
fn app_installs_are_forbidden_under_test() {
    assert!(app_installs_forbidden());
    assert_eq!(FORBID_ENV, "OPTIMIZER_FORBID_APP_INSTALLS");
}

// ───────────────────────────── status and plans ─────────────────────────────

#[test]
fn status_reports_the_account_before_the_package() {
    let status = winget_status_with(&env());
    assert_eq!(status.availability, Availability::Ready);
    assert_eq!(status.location, Some(fake_location()));
    assert_eq!(status.min_version, "1.6.0");
    assert_eq!(status.store_uri, APP_INSTALLER_STORE_URI);

    let missing = winget_status_with(&WingetEnv {
        locate: not_installed,
        ..env()
    });
    assert_eq!(missing.availability, Availability::Missing);
    assert_eq!(missing.message.as_deref(), Some(WINGET_MISSING_TEXT));

    let other = winget_status_with(&WingetEnv {
        other_user,
        ..env()
    });
    assert_eq!(other.availability, Availability::OtherUser);
    assert_eq!(other.message.as_deref(), Some(OTHER_USER_TEXT));
    assert_eq!(other.location, None);

    let unknown = winget_status_with(&WingetEnv {
        other_user: user_unknown,
        ..env()
    });
    assert_eq!(unknown.availability, Availability::UserUnknown);
    assert_eq!(unknown.message.as_deref(), Some(USER_UNKNOWN_TEXT));
}

#[test]
fn plans_are_read_only_and_explain_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let panic_deps = |env: WingetEnv| deps(env, Arc::new(PanicLauncher));
    let upgrade = request(UpdatesKind::Upgrade, &["Contoso.Editor", "Fabrikam.Player"]);

    let outcome =
        plan_or_start_with(&host, &panic_deps(env()), &upgrade, true, no_journal).unwrap();
    assert!(outcome.job.is_none());
    let plan = outcome.plan;
    assert_eq!(plan.title, "Update 2 apps");
    assert_eq!(plan.blocked_reason, None);
    assert!(plan.requires_admin && plan.irreversible && plan.cancellable);
    assert_eq!(plan.program.as_deref(), Some(WINGET));
    assert_eq!(plan.command_lines.len(), 2);
    assert!(plan.command_lines[0].starts_with("winget upgrade --id Contoso.Editor --exact"));
    assert_eq!(plan.notes, [ELEVATED_NOTE]);

    let scan = plan_with(&host, &panic_deps(env()), &UpdatesRequest::scan()).0;
    assert_eq!(scan.title, "Check for app updates");
    assert!(!scan.requires_admin && !scan.irreversible);
    assert!(scan.notes.is_empty());
    assert_eq!(scan.command_lines.len(), 3);

    let install = request(UpdatesKind::Install, &["Contoso.Editor"]);
    assert_eq!(
        plan_with(&host, &panic_deps(env()), &install).0.title,
        "Install 1 app"
    );

    let blocked = |env: WingetEnv, request: &UpdatesRequest| {
        plan_with(&host, &panic_deps(env), request).0.blocked_reason
    };
    assert_eq!(
        blocked(
            WingetEnv {
                locate: not_installed,
                ..env()
            },
            &upgrade
        )
        .as_deref(),
        Some(WINGET_MISSING_TEXT)
    );
    for request in [&upgrade, &UpdatesRequest::scan()] {
        assert_eq!(
            blocked(
                WingetEnv {
                    other_user,
                    ..env()
                },
                request
            )
            .as_deref(),
            Some(OTHER_USER_TEXT)
        );
        assert_eq!(
            blocked(
                WingetEnv {
                    other_user: user_unknown,
                    ..env()
                },
                request
            )
            .as_deref(),
            Some(USER_UNKNOWN_TEXT)
        );
    }
    let sfc = WingetEnv {
        servicing_tool: sfc_running,
        ..env()
    };
    assert_eq!(
        blocked(sfc, &upgrade).as_deref(),
        Some(
            "Wait for Repair system files to finish; Windows can't install apps safely while it \
             repairs itself."
        )
    );
    assert_eq!(blocked(sfc, &UpdatesRequest::scan()), None);

    let no_folder = WingetDeps {
        launcher: Err("winget's folder could not be found: gone".into()),
        ..panic_deps(env())
    };
    assert_eq!(
        plan_with(&host, &no_folder, &upgrade)
            .0
            .blocked_reason
            .as_deref(),
        Some("winget's folder could not be found: gone")
    );

    let noisy = WingetEnv {
        on_battery: busy,
        msi_busy: busy,
        processes: winget_running,
        elevated: no,
        ..env()
    };
    assert_eq!(
        plan_with(&host, &panic_deps(noisy), &upgrade).0.notes,
        [BATTERY_NOTE, MSI_BUSY_NOTE, WINGET_RUNNING_NOTE]
    );
    assert!(!dir.path().join("jobs").exists(), "a plan creates no files");
}

#[test]
fn the_install_guard_wins_over_every_other_block() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let everything = WingetEnv {
        installs_forbidden: yes,
        other_user,
        locate: not_installed,
        servicing_tool: sfc_running,
        ..env()
    };
    let deps = deps(everything, Arc::new(PanicLauncher));
    for kind in [UpdatesKind::Upgrade, UpdatesKind::Install] {
        let plan = plan_with(&host, &deps, &request(kind, &["Contoso.Editor"])).0;
        assert_eq!(
            plan.blocked_reason.as_deref(),
            Some(INSTALLS_FORBIDDEN_TEXT)
        );
    }
    let scan = plan_with(
        &host,
        &WingetDeps {
            env: WingetEnv {
                installs_forbidden: yes,
                ..env()
            },
            ..deps.clone()
        },
        &UpdatesRequest::scan(),
    )
    .0;
    assert_eq!(scan.blocked_reason, None, "the check is not an install");
}

#[test]
fn a_forbidden_batch_start_writes_nothing_and_launches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let launcher = Arc::new(ScriptedLauncher::new());
    let deps = deps(
        WingetEnv {
            installs_forbidden: yes,
            ..env()
        },
        launcher.clone(),
    );
    for kind in [UpdatesKind::Upgrade, UpdatesKind::Install] {
        let err = plan_or_start_with(
            &host,
            &deps,
            &request(kind, &["Contoso.Editor"]),
            false,
            || Ok(Arc::clone(&journal)),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), INSTALLS_FORBIDDEN_TEXT);
    }
    assert!(launcher.launched().is_empty());
    assert!(rows(&journal).is_empty());
    assert!(host.jobs().is_empty());
    assert!(!dir.path().join("jobs").exists(), "no transcript");
}

#[test]
fn a_standard_user_cannot_start_a_batch() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let deps = deps(
        WingetEnv {
            elevated: no,
            ..env()
        },
        Arc::new(PanicLauncher),
    );
    let err = plan_or_start_with(
        &host,
        &deps,
        &request(UpdatesKind::Upgrade, &["Contoso.Editor"]),
        false,
        no_journal,
    )
    .unwrap_err();
    assert!(matches!(err, Error::NotElevated));
    assert!(host.jobs().is_empty());
    assert!(!dir.path().join("jobs").exists());
}

#[test]
fn a_running_job_blocks_the_next_plan() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let (release, released) = mpsc::channel::<()>();
    let spec = JobSpec {
        kind: "winget_scan",
        title: "Check for app updates".into(),
        command_line: "winget upgrade".into(),
        cancellable: true,
        audit: None,
        needs_journal: false,
        log: false,
    };
    let job = host
        .start(
            spec,
            no_journal,
            Box::new(move |_| {
                let _ = released.recv();
                crate::jobs::WorkEnd {
                    state: JobState::Succeeded,
                    summary: String::new(),
                    hint: None,
                    restart_required: false,
                    audit_detail: None,
                }
            }),
        )
        .unwrap();
    let deps = deps(env(), Arc::new(PanicLauncher));
    for kind in [UpdatesKind::Scan, UpdatesKind::Upgrade] {
        let request = if kind == UpdatesKind::Scan {
            UpdatesRequest::scan()
        } else {
            request(kind, &["Contoso.Editor"])
        };
        assert_eq!(
            plan_with(&host, &deps, &request)
                .0
                .blocked_reason
                .as_deref(),
            Some("Cairn is already checking for app updates.")
        );
    }
    release.send(()).unwrap();
    wait(&host, job.id);
    host.shutdown();
    assert_eq!(
        plan_with(&host, &deps, &UpdatesRequest::scan())
            .0
            .blocked_reason
            .as_deref(),
        Some(CLOSING_TEXT)
    );
}

// ───────────────────────────── the update check ─────────────────────────────

/// Starts a check with the three steps scripted: version text, export (the hook writes the
/// JSON), listing text and its exit code.
fn run_scan(
    version: &str,
    export: Option<&'static str>,
    export_code: i32,
    listing: &str,
    listing_code: i32,
) -> (tempfile::TempDir, JobHost, Scripted, HostJobId) {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let scripted = scripted(export, None);
    let version_script = scripted.launcher.script();
    let export_script = scripted.launcher.script();
    let listing_script = scripted.launcher.script();
    exit_with(&version_script, version, 0);
    exit_with(&export_script, "", export_code);
    exit_with(&listing_script, listing, listing_code);
    let deps = deps(env(), scripted.launcher.clone());
    let outcome =
        plan_or_start_with(&host, &deps, &UpdatesRequest::scan(), false, no_journal).unwrap();
    let id = outcome.job.unwrap().id;
    wait(&host, id);
    (dir, host, scripted, id)
}

#[test]
fn a_check_reads_versions_installed_apps_and_updates() {
    let (dir, host, scripted, id) = run_scan("v1.29.380\r\n", Some(EXPORT_JSON), 0, EN_US, 0);
    let snapshot = host.snapshot(id).unwrap();
    assert_eq!(snapshot.state, JobState::Succeeded, "{snapshot:?}");
    assert_eq!(snapshot.summary.as_deref(), Some("4 updates available"));
    assert_eq!(snapshot.kind, "winget_scan");
    let result = result_json(&host, id);
    assert_eq!(result["kind"], "scan");
    assert_eq!(result["winget_version"], "1.29.380");
    assert_eq!(result["checked_at"], "2026-09-25T10:00:00+00:00");
    assert_eq!(result["inventory_complete"], true);
    assert_eq!(result["unparsed_rows"], 0);
    assert_eq!(result["error"], serde_json::Value::Null);
    assert_eq!(result["installed"].as_array().unwrap().len(), 4);
    let upgrades = result["upgrades"].as_array().unwrap();
    assert_eq!(upgrades.len(), 4);
    assert_eq!(upgrades[0]["id"], "Contoso.Editor");
    assert_eq!(upgrades[0]["selectable"], true);
    assert_eq!(upgrades[2]["installed"], "< 4.0");
    assert_eq!(upgrades[3]["source"], "msstore");
    assert_eq!(upgrades[3]["note"], STORE_NOTE);

    let launched = scripted.launcher.launched();
    assert_eq!(launched.len(), 3);
    assert_eq!(launched[0].args, version_args());
    assert_eq!(launched[1].args[0], "export");
    assert_eq!(launched[2].args, upgrade_list_args());
    assert_no_forbidden_flags(&launched);
    // Step files and the export are gone; the transcript stays.
    let steps = dir.path().join(r"jobs\winget\steps");
    assert_eq!(std::fs::read_dir(&steps).unwrap().count(), 0);
    let log = std::fs::read_to_string(snapshot.log_path.unwrap()).unwrap();
    assert!(log.contains("Contoso Editor"), "{log}");
    // Each step is marked where it begins, before its own output.
    let at = |text: &str| {
        log.find(text)
            .unwrap_or_else(|| panic!("{text:?} is missing: {log}"))
    };
    assert!(at("== winget --version ==") < at("v1.29.380"), "{log}");
    assert!(at("v1.29.380") < at("== winget export =="), "{log}");
    assert!(
        at("== winget export ==") < at("== winget upgrade =="),
        "{log}"
    );
    assert!(at("== winget upgrade ==") < at("Contoso Editor"), "{log}");
}

#[test]
fn an_outdated_winget_is_reported_and_nothing_else_runs() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let scripted = scripted(None, None);
    exit_with(&scripted.launcher.script(), "v1.5.2\r\n", 0);
    let deps = deps(env(), scripted.launcher.clone());
    let id = plan_or_start_with(&host, &deps, &UpdatesRequest::scan(), false, no_journal)
        .unwrap()
        .job
        .unwrap()
        .id;
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Failed);
    let result = result_json(&host, id);
    assert_eq!(result["error"]["outdated"], true);
    assert_eq!(result["error"]["message"], outdated_text("1.5.2"));
    assert_eq!(scripted.launcher.launched().len(), 1);
}

#[test]
fn a_failed_export_still_lists_updates() {
    let (_dir, host, _scripted, id) = run_scan("v1.29.380\r\n", None, 1, EN_US, 0);
    assert_eq!(host.snapshot(id).unwrap().state, JobState::Succeeded);
    let result = result_json(&host, id);
    assert_eq!(result["inventory_complete"], false);
    assert_eq!(result["installed"].as_array().unwrap().len(), 0);
    assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(result["upgrades"].as_array().unwrap().len(), 4);
}

#[test]
fn unreachable_sources_warn_with_rows_and_fail_without() {
    let (_dir, host, _s, id) = run_scan(
        "v1.29.380\r\n",
        Some(EXPORT_JSON),
        0,
        EN_US,
        hr(0x8A15_004B),
    );
    assert_eq!(host.snapshot(id).unwrap().state, JobState::Succeeded);
    let result = result_json(&host, id);
    assert_eq!(result["warnings"][0], codes::UNREACHABLE_TEXT);
    assert_eq!(result["upgrades"].as_array().unwrap().len(), 4);

    let (_dir, host, _s, id) = run_scan("v1.29.380\r\n", Some(EXPORT_JSON), 0, "", hr(0x8A15_004B));
    assert_eq!(host.snapshot(id).unwrap().state, JobState::Failed);
    let result = result_json(&host, id);
    assert_eq!(result["error"]["message"], codes::UNREACHABLE_TEXT);
    assert_eq!(result["error"]["code"], "0x8A15004B");

    let (_dir, host, _s, id) = run_scan("v1.29.380\r\n", Some(EXPORT_JSON), 0, "", hr(0x8A15_0014));
    assert_eq!(host.snapshot(id).unwrap().state, JobState::Succeeded);
    assert_eq!(
        host.snapshot(id).unwrap().summary.as_deref(),
        Some("All apps are up to date")
    );
}

#[test]
fn stopping_a_check_ends_its_winget() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let scripted = scripted(None, None);
    let version = scripted.launcher.script();
    let deps = deps(env(), scripted.launcher.clone());
    let id = plan_or_start_with(&host, &deps, &UpdatesRequest::scan(), false, no_journal)
        .unwrap()
        .job
        .unwrap()
        .id;
    wait_launches(&scripted.launcher, 1);
    assert!(host.cancel(id).unwrap());
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Cancelled);
    assert_eq!(snapshot.summary.as_deref(), Some("The check was stopped."));
    assert_eq!(
        scripted.launcher.launched().len(),
        1,
        "no later step starts"
    );
    drop(version);
}

// ───────────────────────────── batches ─────────────────────────────

fn start_batch(
    host: &JobHost,
    deps: &WingetDeps,
    kind: UpdatesKind,
    ids: &[&str],
    journal: &Arc<Journal>,
) -> HostJobId {
    let journal = Arc::clone(journal);
    plan_or_start_with(host, deps, &request(kind, ids), false, move || Ok(journal))
        .unwrap()
        .job
        .unwrap()
        .id
}

#[test]
fn an_update_batch_writes_a_started_and_a_final_row_per_app() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(None, Some(Arc::clone(&journal)));
    exit_with(&scripted.launcher.script(), "Successfully installed\r\n", 0);
    exit_with(&scripted.launcher.script(), "", hr(0x8A15_0109));
    exit_with(&scripted.launcher.script(), "", hr(0x8A15_0101));
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B", "Contoso.C"],
        &journal,
    );
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Attention, "{snapshot:?}");
    assert!(snapshot.restart_required);
    assert_eq!(
        snapshot.summary.as_deref(),
        Some("1 updated · 1 failed · 1 needs a restart")
    );
    assert_eq!(
        rows(&journal),
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "succeeded"),
            row(OP_UPGRADE, "Contoso.B", "started"),
            row(OP_UPGRADE, "Contoso.B", "restart_required"),
            row(OP_UPGRADE, "Contoso.C", "started"),
            row(OP_UPGRADE, "Contoso.C", "failed"),
        ]
    );
    let ops = journal.ops(10).unwrap();
    assert!(ops.iter().all(|o| o.session_id.is_none()));
    let failed = ops.iter().find(|o| o.outcome == "failed").unwrap();
    assert!(
        failed
            .detail
            .as_deref()
            .unwrap()
            .starts_with("exit 0x8A150101 (")
            && failed
                .detail
                .as_deref()
                .unwrap()
                .ends_with("The app is open. Close it and try again."),
        "{failed:?}"
    );
    let started = ops.iter().find(|o| o.outcome == "started").unwrap();
    assert!(started
        .detail
        .as_deref()
        .unwrap()
        .contains("1.0 → 2.0  ·  source winget  ·  log "));
    assert_eq!(
        *scripted.seen_started.lock(),
        [
            ("Contoso.A".to_string(), true),
            ("Contoso.B".to_string(), true),
            ("Contoso.C".to_string(), true)
        ],
        "each started row precedes its launch"
    );
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["succeeded", "restart_required", "failed"]);
    assert_eq!(result["done"], 3);
    assert_eq!(result["total"], 3);
    assert_eq!(result["restart_required"], true);
    assert_eq!(result["items"][1]["exit_code_hex"], "0x8A150109");
    assert_no_forbidden_flags(&scripted.launcher.launched());
}

#[test]
fn a_batch_says_which_failures_a_retry_cannot_change_and_how_each_app_ended() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(None, Some(Arc::clone(&journal)));
    exit_with(
        &scripted.launcher.script(),
        "A newer version was found, but the install technology is different.\r\n",
        hr(0x8A15_008E),
    );
    exit_with(
        &scripted.launcher.script(),
        "Installer failed with exit code: 1603\r\n",
        hr(0x8A15_0049),
    );
    exit_with(&scripted.launcher.script(), "Successfully installed\r\n", 0);
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B", "Contoso.C"],
        &journal,
    );
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Attention, "{snapshot:?}");
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["failed", "failed", "succeeded"]);
    let retries: Vec<bool> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["retry"].as_bool().unwrap())
        .collect();
    assert_eq!(
        retries,
        [false, true, true],
        "only the install technology mismatch can't change"
    );
    assert_eq!(
        result["items"][0]["message"],
        "winget can't update this copy because it was installed another way. It may update \
         itself; otherwise get the update from its publisher."
    );
    assert_eq!(
        result["items"][1]["message"],
        "The app's own installer failed. If the app is open, close it and try again."
    );
    // A result published without the flag reads as one a retry can change.
    let mut older = result["items"][0].clone();
    older.as_object_mut().unwrap().remove("retry");
    let item: ItemResult = serde_json::from_value(older).unwrap();
    assert!(item.retry);

    // The transcript names the batch and says how each app ended, after the app's output.
    let log = std::fs::read_to_string(snapshot.log_path.as_deref().unwrap()).unwrap();
    let header = log.lines().next().unwrap();
    assert!(
        header.ends_with(
            "  ·  winget upgrade --id <id> --exact … for 3 apps: Contoso.A, Contoso.B, Contoso.C"
        ),
        "{header}"
    );
    let ends: Vec<&str> = log.lines().filter(|l| l.starts_with("→ ")).collect();
    assert_eq!(ends.len(), 3, "{log}");
    assert!(
        ends[0].starts_with("→ failed: exit 0x8A15008E (")
            && ends[0].ends_with("otherwise get the update from its publisher."),
        "{}",
        ends[0]
    );
    assert!(
        ends[1].starts_with("→ failed: exit 0x8A150049 (")
            && ends[1].ends_with("If the app is open, close it and try again."),
        "{}",
        ends[1]
    );
    assert!(
        ends[2].starts_with("→ succeeded: exit 0x00000000 (0) · "),
        "{}",
        ends[2]
    );
    let at = |text: &str| {
        log.find(text)
            .unwrap_or_else(|| panic!("{text:?} is missing: {log}"))
    };
    assert!(at("== Contoso.A app (Contoso.A)") < at("install technology is different"));
    assert!(at("install technology is different") < at("→ failed: exit 0x8A15008E"));
    assert!(at("→ failed: exit 0x8A15008E") < at("== Contoso.B app (Contoso.B)"));
    // Each line carries the detail of the app's final row.
    let ops = journal.ops(10).unwrap();
    let edge = ops
        .iter()
        .find(|o| o.target == "Contoso.A" && o.outcome == "failed")
        .unwrap();
    assert_eq!(
        ends[0],
        format!("→ failed: {}", edge.detail.as_deref().unwrap())
    );
}

#[test]
fn stopping_a_batch_ends_it_after_the_current_app() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let mut launcher = ScriptedLauncher::new();
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    launcher.gate = parking_lot::Mutex::new(Some(Gate {
        entered: entered_tx,
        release: release_rx,
    }));
    let scripted = scripted_with(launcher, None, Some(Arc::clone(&journal)));
    let first = scripted.launcher.script();
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B", "Contoso.C"],
        &journal,
    );
    entered.recv().unwrap();
    assert!(host.cancel(id).unwrap());
    release.send(()).unwrap();
    exit_with(&first, "", 0);
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Cancelled);
    assert_eq!(
        rows(&journal),
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "succeeded"),
        ]
    );
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["succeeded", "not_started", "not_started"]);
    assert_eq!(result["stopping"], true);
    assert_eq!(scripted.launcher.launched().len(), 1);
}

#[test]
fn closing_mid_app_records_it_as_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(None, Some(Arc::clone(&journal)));
    let first = scripted.launcher.script();
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B"],
        &journal,
    );
    wait_launches(&scripted.launcher, 1);
    let begun = Instant::now();
    let outcomes = host.shutdown();
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "it returns within the stop wait"
    );
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].action, HostShutdownAction::Stopped);
    let records = rows(&journal);
    assert_eq!(
        records,
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "left_running"),
        ]
    );
    let detail = journal.ops(1).unwrap()[0].detail.clone().unwrap();
    assert!(
        detail.starts_with("Cairn closed while this was running"),
        "{detail}"
    );
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["left_running", "not_started"]);
    // The left-running app keeps its output file for the record.
    let capture = detail.rsplit("Its output: ").next().unwrap();
    assert!(Path::new(capture).is_file(), "{capture}");
    drop(first);
}

#[test]
fn a_running_app_says_whether_its_winget_outlives_cairn() {
    for detached in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let journal = journal(dir.path());
        let mut launcher = ScriptedLauncher::new();
        launcher.detached = detached;
        let scripted = scripted_with(launcher, None, Some(Arc::clone(&journal)));
        let first = scripted.launcher.script();
        let second = scripted.launcher.script();
        let deps = deps(env(), scripted.launcher.clone());
        let id = start_batch(
            &host,
            &deps,
            UpdatesKind::Upgrade,
            &["Contoso.A", "Contoso.B"],
            &journal,
        );
        wait_launches(&scripted.launcher, 1);
        // Published while the app still runs, for the close dialog.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let result = result_json(&host, id);
            let running = &result["items"][0];
            if running["state"] == "running" && running["detached"] == detached {
                assert_eq!(result["items"][1]["detached"], false, "not launched yet");
                break;
            }
            assert!(Instant::now() < deadline, "{result}");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(host.snapshot(id).unwrap().detached, detached);
        exit_with(&first, "", 0);
        exit_with(&second, "", 0);
        let snapshot = wait(&host, id);
        assert_eq!(snapshot.state, JobState::Succeeded, "{snapshot:?}");
        assert!(!snapshot.detached, "nothing runs any more");
        let result = result_json(&host, id);
        assert_eq!(states(&result), ["succeeded", "succeeded"]);
        assert_eq!(result["items"][0]["detached"], detached);
        assert_eq!(result["items"][1]["detached"], detached);
    }
}

/// Waits until the job's progress line is `line` and returns its snapshot.
fn wait_progress_line(host: &JobHost, id: HostJobId, line: &str) -> HostJobSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = host.snapshot(id).unwrap();
        if snapshot.progress_line.as_deref() == Some(line) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {line:?}: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_running_app_shows_its_download_sizes_or_its_percentage() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(None, Some(Arc::clone(&journal)));
    let first = scripted.launcher.script();
    let second = scripted.launcher.script();
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B"],
        &journal,
    );
    wait_launches(&scripted.launcher, 1);
    let snapshot = wait_progress_line(&host, id, "Updating Contoso.A app (1 of 2)");
    assert_eq!(snapshot.progress, Some(0.0));
    // The detail says which app runs and what its display shows, for that app's row.
    let detail =
        |progress: Option<&str>| Some(serde_json::json!({"item": 0, "progress": progress}));
    assert_eq!(snapshot.detail, detail(None));

    // winget redraws a download as a bar followed by the sizes.
    let write = |text: &str| first.send(Step::Write(text.as_bytes().to_vec())).unwrap();
    write("Downloading https://contoso.example/a.msi\r\n  ██▒▒▒▒▒▒  6.0 MB / 32.5 MB\r");
    write("  ███▒▒▒▒▒  12.0 MB / 32.5 MB");
    let snapshot = wait_progress_line(
        &host,
        id,
        "Updating Contoso.A app (1 of 2)  ·  12.0 MB / 32.5 MB",
    );
    // Half the batch is this app, and 12.0 of its 32.5 MB are there.
    let progress = snapshot.progress.unwrap();
    assert!((progress - 18.46).abs() < 0.1, "{progress}");
    assert_eq!(snapshot.detail, detail(Some("12.0 MB / 32.5 MB")));

    // A display without sizes shows its percentage.
    write("\r  ████████  45%");
    let snapshot = wait_progress_line(&host, id, "Updating Contoso.A app (1 of 2)  ·  45%");
    let progress = snapshot.progress.unwrap();
    assert!((progress - 22.5).abs() < 0.1, "{progress}");
    assert_eq!(snapshot.detail, detail(Some("45%")));

    // Anything else leaves the line as it was started.
    write("\r  Starting package install...");
    let snapshot = wait_progress_line(&host, id, "Updating Contoso.A app (1 of 2)");
    assert_eq!(snapshot.detail, detail(None));

    exit_with(&first, "\r\nSuccessfully installed\r\n", 0);
    let snapshot = wait_progress_line(&host, id, "Updating Contoso.B app (2 of 2)");
    assert_eq!(
        snapshot.detail,
        Some(serde_json::json!({"item": 1, "progress": null}))
    );
    exit_with(&second, "", 0);
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Succeeded, "{snapshot:?}");
    assert_eq!(
        snapshot.detail,
        Some(serde_json::Value::Null),
        "no app runs any more"
    );
}

#[test]
fn an_app_past_its_deadline_is_recorded_and_the_rest_do_not_start() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(None, Some(Arc::clone(&journal)));
    let first = scripted.launcher.script();
    let mut deps = deps(env(), scripted.launcher.clone());
    deps.limits.item = Duration::from_millis(50);
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B"],
        &journal,
    );
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Attention);
    assert_eq!(
        rows(&journal),
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "timeout"),
        ]
    );
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["timed_out", "not_started"]);
    assert_eq!(scripted.launcher.launched().len(), 1);
    drop(first);
}

#[test]
fn no_started_row_means_no_launch() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let blocker = rusqlite::Connection::open(dir.path().join("journal.db")).unwrap();
    blocker
        .execute_batch(
            "CREATE TRIGGER refuse_started BEFORE INSERT ON ops_log WHEN NEW.outcome = 'started' \
             BEGIN SELECT RAISE(ABORT, 'the journal refuses new rows'); END;",
        )
        .unwrap();
    let launcher = Arc::new(ScriptedLauncher::new());
    let deps = deps(env(), launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Upgrade,
        &["Contoso.A", "Contoso.B"],
        &journal,
    );
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Failed);
    assert_eq!(
        snapshot.detail,
        Some(serde_json::Value::Null),
        "no app runs"
    );
    assert!(snapshot.hint.is_some());
    assert!(launcher.launched().is_empty(), "nothing was launched");
    assert!(rows(&journal).is_empty());
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["failed", "not_started"]);
    assert_eq!(
        result["items"][0]["message"],
        "Couldn't write the audit row, so it wasn't started."
    );
}

#[test]
fn an_install_batch_skips_installed_apps() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let scripted = scripted(Some(EXPORT_JSON), Some(Arc::clone(&journal)));
    exit_with(&scripted.launcher.script(), "", 0); // export
    exit_with(&scripted.launcher.script(), "", 0); // Fabrikam.Chat
    let deps = deps(env(), scripted.launcher.clone());
    let id = start_batch(
        &host,
        &deps,
        UpdatesKind::Install,
        &["contoso.editor", "Fabrikam.Chat"],
        &journal,
    );
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Succeeded, "{snapshot:?}");
    assert_eq!(
        snapshot.summary.as_deref(),
        Some("1 installed · 1 already installed")
    );
    let launched = scripted.launcher.launched();
    assert_eq!(launched.len(), 2);
    assert_eq!(launched[0].args[0], "export");
    assert_eq!(&launched[1].args[..3], ["install", "--id", "Fabrikam.Chat"]);
    assert!(launched[1].args.contains(&"--no-upgrade".to_string()));
    assert_eq!(
        rows(&journal),
        [
            row(OP_INSTALL, "Fabrikam.Chat", "started"),
            row(OP_INSTALL, "Fabrikam.Chat", "succeeded"),
        ]
    );
    let result = result_json(&host, id);
    assert_eq!(states(&result), ["already_installed", "succeeded"]);
    assert_no_forbidden_flags(&launched);
    // The export that reads the installed apps is marked as a step of its own.
    let log = std::fs::read_to_string(snapshot.log_path.as_deref().unwrap()).unwrap();
    let at = |text: &str| {
        log.find(text)
            .unwrap_or_else(|| panic!("{text:?} is missing: {log}"))
    };
    assert!(
        at("== winget export ==")
            < at("== Fabrikam.Chat app (Fabrikam.Chat): installing (2 of 2) =="),
        "{log}"
    );
    assert!(
        at("== Fabrikam.Chat app (Fabrikam.Chat)") < at("→ succeeded: exit 0x00000000 (0) · "),
        "{log}"
    );
}

const WINGET_MOVED: &str = r"C:\Program Files\WindowsApps\Microsoft.DesktopAppInstaller_1.30.0.0_x64__8wekyb3d8bbwe\winget.exe";

/// Calls of [`moving`]; only `a_missing_program_is_located_again_once` uses it.
static LOCATE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// winget where the plan finds it first, and in a newer package folder afterwards.
fn moving() -> Result<Option<WingetLocation>> {
    let mut location = fake_location();
    if LOCATE_CALLS.fetch_add(1, Ordering::SeqCst) > 0 {
        location.path = PathBuf::from(WINGET_MOVED);
    }
    Ok(Some(location))
}

/// Fails the first launch as a missing program does, then launches scripted processes.
#[derive(Debug)]
struct MovedOnce {
    inner: ScriptedLauncher,
    failed: AtomicBool,
}

impl Launcher for MovedOnce {
    fn launch(&self, command: &CommandSpec, output: File) -> Result<Launched> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            self.inner.launches.lock().push(command.clone());
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )));
        }
        self.inner.launch(command, output)
    }
}

#[test]
fn a_missing_program_is_located_again_once() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let launcher = Arc::new(MovedOnce {
        inner: ScriptedLauncher::new(),
        failed: AtomicBool::new(false),
    });
    exit_with(&launcher.inner.script(), "", 0);
    let deps = deps(
        WingetEnv {
            locate: moving,
            ..env()
        },
        launcher.clone(),
    );
    let id = start_batch(&host, &deps, UpdatesKind::Upgrade, &["Contoso.A"], &journal);
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Succeeded, "{snapshot:?}");
    let programs: Vec<PathBuf> = launcher
        .inner
        .launched()
        .into_iter()
        .map(|c| c.program)
        .collect();
    assert_eq!(
        programs,
        [PathBuf::from(WINGET), PathBuf::from(WINGET_MOVED)]
    );
    assert_eq!(
        rows(&journal),
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "succeeded"),
        ],
        "one started row covers the retry"
    );
}

#[test]
fn other_launch_errors_are_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let journal = journal(dir.path());
    let mut launcher = ScriptedLauncher::new();
    launcher.fail = Some("access is denied".into());
    let launcher = Arc::new(launcher);
    let deps = deps(env(), launcher.clone());
    let id = start_batch(&host, &deps, UpdatesKind::Upgrade, &["Contoso.A"], &journal);
    let snapshot = wait(&host, id);
    assert_eq!(snapshot.state, JobState::Failed);
    assert_eq!(
        rows(&journal),
        [
            row(OP_UPGRADE, "Contoso.A", "started"),
            row(OP_UPGRADE, "Contoso.A", "failed"),
        ]
    );
    assert_eq!(launcher.launched().len(), 1);
    let result = result_json(&host, id);
    assert_eq!(
        result["items"][0]["message"],
        "winget couldn't be started: access is denied"
    );
}

#[test]
fn stale_step_folders_are_swept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("steps");
    std::fs::create_dir_all(root.join("20260101-000000-winget_scan")).unwrap();
    std::fs::write(
        root.join("20260101-000000-winget_scan").join("s1.out"),
        b"x",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("20260102-000000-winget_upgrade-2")).unwrap();
    std::fs::write(root.join("notes.txt"), b"keep").unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();

    work::sweep(&root, SystemTime::now());
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        4,
        "fresh folders stay"
    );

    work::sweep(&root, SystemTime::now() + Duration::from_secs(48 * 3600));
    let mut left: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["notes.txt", "other"]);
}
