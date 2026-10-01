use std::path::Path;
use std::sync::Arc;

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

use super::command::{executable_path, expand_env, quote};
use super::folder::{items_in, FolderItem};
use super::publisher::company_name;
use super::*;
use crate::safety::rollback::rollback_journal;
use crate::safety::{Journal, RestorePointPolicy, SafetyOptions};
use crate::win::registry::{delete_key_if_empty, exists, read_value};
use crate::win::wide;

fn system_root() -> String {
    std::env::var("SystemRoot").expect("SystemRoot is set")
}

// ───────────────────────────── StartupApproved values ─────────────────────────────

#[test]
fn even_first_byte_or_missing_value_is_enabled() {
    assert!(approved_is_enabled(None));
    assert!(approved_is_enabled(Some(&[])));
    assert!(approved_is_enabled(Some(&[0x02])));
    assert!(approved_is_enabled(Some(&[
        0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
    ])));
    assert!(approved_is_enabled(Some(&[
        0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
    ])));
    assert!(approved_is_enabled(Some(&[0x04, 0, 0, 0])));
}

#[test]
fn odd_first_byte_is_disabled() {
    assert!(!approved_is_enabled(Some(&[0x03])));
    assert!(!approved_is_enabled(Some(&[
        0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8
    ])));
    assert!(!approved_is_enabled(Some(&[
        0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8
    ])));
    assert!(!approved_is_enabled(Some(&[0x01, 0xFF])));
}

#[test]
fn enabled_value_is_flag_and_eleven_zero_bytes() {
    let data = approved_enabled_value();
    assert_eq!(data, [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(approved_is_enabled(Some(&data)));
}

#[test]
fn disabled_value_is_flag_then_little_endian_filetime() {
    assert_eq!(
        approved_disabled_value(0x0102_0304_0506_0708),
        [0x03, 0, 0, 0, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
    );

    let before = filetime_now();
    let data = approved_disabled_value(filetime_now());
    let after = filetime_now();
    assert_eq!(data.len(), 12);
    assert_eq!(&data[..4], &[0x03, 0, 0, 0]);
    let stamp = u64::from_le_bytes(data[4..].try_into().unwrap());
    assert!(
        before <= stamp && stamp <= after,
        "{before} {stamp} {after}"
    );
    assert!(!approved_is_enabled(Some(&data)));
}

#[test]
fn filetime_counts_from_1601() {
    // 2024-01-01T00:00:00Z and 2100-01-01T00:00:00Z as FILETIME.
    let now = filetime_now();
    assert!(now > 133_485_408_000_000_000, "{now}");
    assert!(now < 157_469_184_000_000_000, "{now}");
}

// ───────────────────────────── ids ─────────────────────────────

#[test]
fn ids_are_source_key_and_name() {
    assert_eq!(
        entry_id(StartupSource::UserRun, "Discord"),
        "user_run:Discord"
    );
    assert_eq!(
        entry_id(StartupSource::UserFolder, "Spotify.lnk"),
        "user_folder:Spotify.lnk"
    );
    assert_eq!(
        entry_id(StartupSource::MachineRun32, "A:B"),
        "machine_run32:A:B"
    );
    for source in StartupSource::ALL {
        let serde_name = serde_json::to_value(source).unwrap();
        assert_eq!(serde_name.as_str(), Some(source.key()));
        let id = entry_id(source, "Name With Spaces");
        assert_eq!(parse_id(&id), Some((source, "Name With Spaces")));
        assert_eq!(StartupSource::of_id(&id), Some(source));
    }
}

#[test]
fn malformed_ids_do_not_parse() {
    assert_eq!(
        parse_id("machine_run32:A:B"),
        Some((StartupSource::MachineRun32, "A:B"))
    );
    assert_eq!(parse_id("user_run:"), None);
    assert_eq!(parse_id("user_run"), None);
    assert_eq!(parse_id("User_Run:Discord"), None);
    assert_eq!(parse_id("services:Discord"), None);
    assert_eq!(parse_id(":Discord"), None);
    assert_eq!(StartupSource::of_id("bogus"), None);
}

#[test]
fn machine_sources_require_admin_and_use_hklm() {
    for source in StartupSource::ALL {
        let machine = matches!(
            source,
            StartupSource::MachineRun
                | StartupSource::MachineRun32
                | StartupSource::CommonFolder
                | StartupSource::PolicyMachineRun
        );
        assert_eq!(source.requires_admin(), machine, "{source:?}");
        assert_eq!(source.is_per_user(), !machine, "{source:?}");
        let hive = if machine {
            Hive::LocalMachine
        } else {
            Hive::CurrentUser
        };
        assert_eq!(source.approved_hive(), hive, "{source:?}");
        let policy = matches!(
            source,
            StartupSource::PolicyUserRun | StartupSource::PolicyMachineRun
        );
        assert_eq!(source.is_policy(), policy, "{source:?}");
        let approved = !policy && source != StartupSource::PackagedTask;
        assert_eq!(
            SYSTEM.approved_key(source).is_some(),
            approved,
            "{source:?}"
        );
        assert!(!source.location().is_empty());
    }
    assert_eq!(
        SYSTEM.approved_key(StartupSource::MachineRun32).as_deref(),
        Some(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32")
    );
    assert_eq!(
        SYSTEM.approved_key(StartupSource::UserFolder).as_deref(),
        Some(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder")
    );
}

fn target(hive: Hive, key_path: &str, value_name: &str) -> Option<RegistryTarget> {
    Some(RegistryTarget {
        hive,
        key_path: key_path.to_string(),
        value_name: value_name.to_string(),
    })
}

#[test]
fn registry_targets_name_the_value_each_source_is_switched_by() {
    let approved = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
    let cases = [
        (
            "user_run:Contoso Sync",
            target(
                Hive::CurrentUser,
                &format!(r"{approved}\Run"),
                "Contoso Sync",
            ),
        ),
        (
            "machine_run:Fabrikam",
            target(Hive::LocalMachine, &format!(r"{approved}\Run"), "Fabrikam"),
        ),
        (
            "machine_run32:A:B",
            target(Hive::LocalMachine, &format!(r"{approved}\Run32"), "A:B"),
        ),
        (
            "user_folder:Northwind.lnk",
            target(
                Hive::CurrentUser,
                &format!(r"{approved}\StartupFolder"),
                "Northwind.lnk",
            ),
        ),
        (
            "common_folder:Contoso.lnk",
            target(
                Hive::LocalMachine,
                &format!(r"{approved}\StartupFolder"),
                "Contoso.lnk",
            ),
        ),
        (
            r"packaged_task:Contoso.App_0000000000000\StartOnLogin",
            target(
                Hive::CurrentUser,
                &format!(
                    r"{}\Contoso.App_0000000000000\StartOnLogin",
                    packaged::TASKS_ROOT
                ),
                "State",
            ),
        ),
        ("policy_user_run:Contoso", None),
        ("policy_machine_run:Contoso", None),
        ("user_run:", None),
        ("user_run", None),
        ("services:Contoso", None),
        ("packaged_task:Contoso.App_0000000000000", None),
        (r"packaged_task:\StartOnLogin", None),
        (r"packaged_task:Contoso.App_0000000000000\", None),
    ];
    for (id, expected) in cases {
        assert_eq!(registry_target(id), expected, "{id}");
    }
}

#[test]
fn new_sources_use_snake_case_names() {
    for (source, name) in [
        (StartupSource::PackagedTask, "packaged_task"),
        (StartupSource::PolicyUserRun, "policy_user_run"),
        (StartupSource::PolicyMachineRun, "policy_machine_run"),
    ] {
        assert_eq!(serde_json::to_value(source).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<StartupSource>(name.into()).unwrap(),
            source
        );
    }
    let id = entry_id(
        StartupSource::PackagedTask,
        &packaged_key("Contoso.App_0000000000000", "Start"),
    );
    assert_eq!(id, r"packaged_task:Contoso.App_0000000000000\Start");
    assert_eq!(
        parse_id(&id),
        Some((
            StartupSource::PackagedTask,
            r"Contoso.App_0000000000000\Start"
        ))
    );
}

#[test]
fn entries_without_the_new_fields_deserialize_as_toggleable() {
    let entry: StartupEntry = serde_json::from_value(serde_json::json!({
        "id": "user_run:A", "name": "A", "source": "user_run", "location": "HKCU Run",
        "command": "", "path": "", "publisher": "", "exists": false, "enabled": true,
        "requires_admin": false
    }))
    .unwrap();
    assert!(entry.can_toggle);
    assert!(entry.note.is_empty());
}

// ───────────────────────────── command lines ─────────────────────────────

#[test]
fn quoted_command_uses_the_quoted_token() {
    assert_eq!(
        executable_path(r#""C:\Program Files\App\app.exe" --flag "x y""#),
        r"C:\Program Files\App\app.exe"
    );
    assert_eq!(
        executable_path(r#"  "C:\Unterminated Quote\app.exe --x"#),
        r"C:\Unterminated Quote\app.exe --x"
    );
    assert_eq!(executable_path(""), "");
    assert_eq!(executable_path("   "), "");
    assert_eq!(executable_path(r#""""#), "");
}

#[test]
fn unquoted_command_with_spaces_resolves_to_the_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path().join("My App");
    std::fs::create_dir(&app_dir).unwrap();
    let exe = app_dir.join("app.exe");
    std::fs::write(&exe, b"").unwrap();
    let expected = exe.display().to_string();

    let cmd = format!("{expected} --flag --name value");
    assert_eq!(executable_path(&cmd), expected);
    assert_eq!(executable_path(&expected), expected);
    assert_eq!(executable_path(&format!("{expected}   ")), expected);
    assert_eq!(executable_path(&format!("{expected}\t--flag")), expected);

    // CreateProcess appends ".exe" when the name as written does not exist.
    let without_ext = format!("{} --flag", app_dir.join("app").display());
    assert_eq!(executable_path(&without_ext), expected);
}

#[test]
fn unquoted_command_resolves_shortest_prefix_first() {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path().join("My App");
    std::fs::create_dir(&app_dir).unwrap();
    std::fs::write(app_dir.join("app.exe"), b"").unwrap();
    let cmd = format!("{} --flag", app_dir.join("app.exe").display());

    // A shorter "<prefix>.exe" runs instead of the longer path.
    let shorter_exe = dir.path().join("My.exe");
    std::fs::write(&shorter_exe, b"").unwrap();
    assert_eq!(executable_path(&cmd), shorter_exe.display().to_string());

    // The prefix as written is tried before "<prefix>.exe", extension or not.
    let shorter_bare = dir.path().join("My");
    std::fs::write(&shorter_bare, b"").unwrap();
    assert_eq!(executable_path(&cmd), shorter_bare.display().to_string());

    let dotted_dir = dir.path().join("Tool.v2 Dir");
    std::fs::create_dir(&dotted_dir).unwrap();
    std::fs::write(dotted_dir.join("tool.exe"), b"").unwrap();
    let dotted_exe = dir.path().join("Tool.v2.exe");
    std::fs::write(&dotted_exe, b"").unwrap();
    let dotted_cmd = dotted_dir.join("tool.exe").display().to_string();
    assert_eq!(
        executable_path(&dotted_cmd),
        dotted_exe.display().to_string()
    );
}

#[test]
fn unquoted_command_skips_directory_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let tool_dir = dir.path().join("Sub Dir");
    std::fs::create_dir(&tool_dir).unwrap();
    let tool = tool_dir.join("tool.exe");
    std::fs::write(&tool, b"").unwrap();
    // "Sub" is a directory, so "Sub.exe" is not tried for that prefix.
    std::fs::create_dir(dir.path().join("Sub")).unwrap();
    std::fs::write(dir.path().join("Sub.exe"), b"").unwrap();

    let cmd = format!("{} -x", tool.display());
    assert_eq!(executable_path(&cmd), tool.display().to_string());
}

#[test]
fn environment_variables_are_expanded() {
    let root = system_root();
    assert_eq!(
        expand_env(r"%SystemRoot%\System32"),
        format!(r"{root}\System32")
    );
    assert_eq!(expand_env("no variables"), "no variables");
    assert_eq!(
        expand_env(r"%PCOPTIMIZER_UNDEFINED_VARIABLE%\x"),
        r"%PCOPTIMIZER_UNDEFINED_VARIABLE%\x"
    );

    let cmd_exe = format!(r"{root}\System32\cmd.exe");
    assert_eq!(
        executable_path(r"%SystemRoot%\System32\cmd.exe /c exit"),
        cmd_exe
    );
    assert_eq!(
        executable_path(r#""%SystemRoot%\System32\cmd.exe" /c exit"#),
        cmd_exe
    );
}

#[test]
fn expanded_variable_with_spaces_resolves_unquoted() {
    let temp = std::env::var("TEMP").expect("TEMP is set");
    let dir = tempfile::tempdir_in(&temp).unwrap();
    let rel = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let tool_dir = dir.path().join("Sub Dir");
    std::fs::create_dir(&tool_dir).unwrap();
    std::fs::write(tool_dir.join("tool.exe"), b"").unwrap();

    let cmd = format!(r"%TEMP%\{rel}\Sub Dir\tool.exe -x --y=z");
    let expected = Path::new(&temp).join(&rel).join("Sub Dir").join("tool.exe");
    assert_eq!(executable_path(&cmd), expected.display().to_string());
}

#[test]
fn bare_names_resolve_through_system_directories() {
    for cmd in [
        "rundll32.exe shell32.dll,Control_RunDLL",
        "rundll32 shell32.dll,Control_RunDLL",
        r#""rundll32.exe" shell32.dll,Control_RunDLL"#,
    ] {
        let path = executable_path(cmd);
        assert!(
            path.to_ascii_lowercase()
                .ends_with(r"\system32\rundll32.exe"),
            "{cmd}: {path}"
        );
        assert!(Path::new(&path).is_file(), "{path}");
    }
}

#[test]
fn missing_files_fall_back_to_the_exe_token_or_first_word() {
    assert_eq!(
        executable_path(r"C:\PCOptimizer Missing Dir\app.exe --flag value"),
        r"C:\PCOptimizer Missing Dir\app.exe"
    );
    assert_eq!(
        executable_path(r"C:\PCOptimizer Missing Dir\app.exe"),
        r"C:\PCOptimizer Missing Dir\app.exe"
    );
    assert_eq!(
        executable_path(r"C:\PCOptimizerMissing\my.exe.tool.exe -q"),
        r"C:\PCOptimizerMissing\my.exe.tool.exe"
    );
    assert_eq!(
        executable_path(r"C:\PCOptimizerMissing\tool --flag"),
        r"C:\PCOptimizerMissing\tool"
    );
}

#[test]
fn paths_with_spaces_are_quoted() {
    assert_eq!(quote(r"C:\A B\c.exe"), r#""C:\A B\c.exe""#);
    assert_eq!(quote(r"C:\AB\c.exe"), r"C:\AB\c.exe");
    assert_eq!(quote(r#""C:\A B\c.exe""#), r#""C:\A B\c.exe""#);
}

// ───────────────────────────── publisher ─────────────────────────────

#[test]
fn publisher_comes_from_the_version_resource() {
    let kernel32 = format!(r"{}\System32\kernel32.dll", system_root());
    assert_eq!(company_name(&kernel32), "Microsoft Corporation");
    assert_eq!(company_name(""), "");
    assert_eq!(company_name(r"C:\PCOptimizerMissing\app.exe"), "");

    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.exe");
    std::fs::write(&plain, b"not a PE file").unwrap();
    assert_eq!(company_name(&plain.display().to_string()), "");
}

// ───────────────────────────── Startup folders ─────────────────────────────

/// Writes a `.lnk` with the given target and arguments.
fn create_shortcut(lnk: &Path, target: &str, args: &str) {
    let target = wide(target);
    let args = wide(args);
    let file = wide(&lnk.display().to_string());
    // SAFETY: COM is initialized on this thread for the duration of the calls and every
    // string outlives the call using it.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(PCWSTR(target.as_ptr())).unwrap();
            link.SetArguments(PCWSTR(args.as_ptr())).unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            persist.Save(PCWSTR(file.as_ptr()), true).unwrap();
        }
        if hr.is_ok() {
            CoUninitialize();
        }
    }
}

#[test]
fn folder_items_resolve_shortcuts_urls_and_plain_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = system_root();
    let cmd_exe = format!(r"{root}\System32\cmd.exe");

    create_shortcut(&dir.path().join("Shell.lnk"), &cmd_exe, "/c exit 0");
    std::fs::write(dir.path().join("desktop.ini"), b"[.ShellClassInfo]\r\n").unwrap();
    std::fs::write(
        dir.path().join("Site.url"),
        b"[InternetShortcut]\r\nURL=https://example.com/start\r\n",
    )
    .unwrap();
    let script = dir.path().join("Run Me.bat");
    std::fs::write(&script, b"@echo off\r\n").unwrap();
    std::fs::create_dir(dir.path().join("Subfolder")).unwrap();

    let mut items = items_in(dir.path()).unwrap();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Run Me.bat", "Shell.lnk", "Site.url"]);

    let script = script.display().to_string();
    assert_eq!(
        items[0],
        FolderItem {
            name: "Run Me.bat".to_string(),
            command: format!("\"{script}\""),
            path: script.clone(),
        }
    );

    // The shell may normalise the case of the stored target (C:\WINDOWS → C:\Windows).
    let shortcut = &items[1];
    assert!(shortcut.path.eq_ignore_ascii_case(&cmd_exe), "{shortcut:?}");
    assert!(
        shortcut
            .command
            .eq_ignore_ascii_case(&format!("{cmd_exe} /c exit 0")),
        "{shortcut:?}"
    );

    assert_eq!(
        items[2],
        FolderItem {
            name: "Site.url".to_string(),
            command: "https://example.com/start".to_string(),
            path: String::new(),
        }
    );
}

#[test]
fn missing_folder_has_no_items() {
    let dir = tempfile::tempdir().unwrap();
    assert!(items_in(&dir.path().join("absent")).unwrap().is_empty());
}

// ───────────────────────────── sandboxed set_enabled ─────────────────────────────

const SANDBOX_ROOT: &str = r"Software\PCOptimizer\SelfTest\Startup";
const SANDBOX_RUN: &str = r"Software\PCOptimizer\SelfTest\Startup\Run";
const SANDBOX_POLICY_RUN: &str = r"Software\PCOptimizer\SelfTest\Startup\PolicyRun";
const SANDBOX_APPROVED: &str = r"Software\PCOptimizer\SelfTest\Startup\StartupApproved";
const SANDBOX_APPROVED_RUN: &str = r"Software\PCOptimizer\SelfTest\Startup\StartupApproved\Run";
const SANDBOX_PACKAGED: &str = r"Software\PCOptimizer\SelfTest\Startup\Packaged";
const PROBE: &str = "PCOptimizerProbe";
const OTHER: &str = "PCOptimizerOther";
const NOT_A_STRING: &str = "PCOptimizerDword";
const PROBE_COMMAND: &str = r#""%SystemRoot%\System32\cmd.exe" /c exit"#;

/// Package family the sandbox resolver reports as installed, without a readable manifest.
const INSTALLED_FAMILY: &str = "PCOptimizer.SelfTest_0000000000000";
/// Package family the sandbox resolver reports as not installed.
const REMOVED_FAMILY: &str = "PCOptimizer.Removed_0000000000000";
/// Package family whose installation the sandbox resolver cannot determine.
const UNRESOLVED_FAMILY: &str = "PCOptimizer.Unresolved_0000000000000";
/// Sandbox startup tasks as `(family, task id, State)`.
const SANDBOX_TASKS: [(&str, &str, u32); 7] = [
    (INSTALLED_FAMILY, "Enabled", 2),
    (INSTALLED_FAMILY, "AppDisabled", 0),
    (INSTALLED_FAMILY, "PolicyOn", 4),
    (INSTALLED_FAMILY, "PolicyOff", 3),
    (INSTALLED_FAMILY, "Unknown", 9),
    (REMOVED_FAMILY, "Leftover", 2),
    (UNRESOLVED_FAMILY, "Start", 1),
];
/// Task key of [`INSTALLED_FAMILY`] without a State value.
const NO_STATE_TASK: &str = "NoState";

fn sandbox_packages(family: &str) -> Option<Vec<Package>> {
    match family {
        INSTALLED_FAMILY => Some(vec![Package {
            full_name: "PCOptimizer.SelfTest_1.0.0.0_x64__0000000000000".to_string(),
            install_dir: None,
        }]),
        UNRESOLVED_FAMILY => None,
        _ => Some(Vec::new()),
    }
}

/// The real logic pointed at Run keys, a StartupApproved root and a packaged-task root
/// under the HKCU sandbox.
const SANDBOX: Layout<'static> = Layout {
    run_keys: &[
        (StartupSource::UserRun, Hive::CurrentUser, SANDBOX_RUN),
        (
            StartupSource::PolicyUserRun,
            Hive::CurrentUser,
            SANDBOX_POLICY_RUN,
        ),
    ],
    approved_root: SANDBOX_APPROVED,
    folders: false,
    packaged_root: Some(SANDBOX_PACKAGED),
    packages: sandbox_packages,
};

fn same_user() -> Result<bool> {
    Ok(false)
}

fn clean_sandbox() {
    for path in [SANDBOX_RUN, SANDBOX_APPROVED_RUN, SANDBOX_POLICY_RUN] {
        if let Ok(Some(key)) = Key::open(Hive::CurrentUser, path, true) {
            for name in [PROBE, OTHER, NOT_A_STRING] {
                let _ = key.delete_value(name);
            }
        }
    }
    let tasks = SANDBOX_TASKS
        .iter()
        .map(|&(family, task, _)| (family, task))
        .chain([(INSTALLED_FAMILY, NO_STATE_TASK)]);
    for (family, task) in tasks {
        let family_path = format!(r"{SANDBOX_PACKAGED}\{family}");
        let task_path = format!(r"{family_path}\{task}");
        if let Ok(Some(key)) = Key::open(Hive::CurrentUser, &task_path, true) {
            let _ = key.delete_value(packaged::STATE_VALUE);
        }
        let _ = delete_key_if_empty(Hive::CurrentUser, &task_path);
        let _ = delete_key_if_empty(Hive::CurrentUser, &family_path);
    }
    for path in [
        SANDBOX_APPROVED_RUN,
        SANDBOX_APPROVED,
        SANDBOX_RUN,
        SANDBOX_POLICY_RUN,
        SANDBOX_PACKAGED,
        SANDBOX_ROOT,
    ] {
        let _ = delete_key_if_empty(Hive::CurrentUser, path);
    }
}

fn sandbox_session(journal: &Arc<Journal>) -> Safety {
    Safety::begin(
        journal.clone(),
        SafetyOptions {
            label: "startup-selftest".to_string(),
            restore_point: RestorePointPolicy::Skip,
            require_elevation: false,
            ..Default::default()
        },
    )
    .unwrap()
}

fn sandbox_approved(name: &str) -> Option<Vec<u8>> {
    match read_value(Hive::CurrentUser, SANDBOX_APPROVED_RUN, name).unwrap() {
        Some(RegValue::Binary(data)) => Some(data),
        None => None,
        Some(other) => panic!("unexpected StartupApproved value {other:?}"),
    }
}

fn sandbox_entry(name: &str) -> StartupEntry {
    SANDBOX
        .list(&Account::SignedIn)
        .unwrap()
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{name} not listed"))
}

fn is_unknown_entry(err: &Error) -> bool {
    matches!(err, Error::Other(message) if message.starts_with("unknown startup entry"))
}

/// The value `set_enabled` recorded in `journal` for `id` is the one the layout's
/// `registry_target` names.
fn assert_recorded_target(journal: &Journal, id: &str) {
    let expected = SANDBOX.registry_target(id).expect("a target");
    let recorded: Vec<RegistryTarget> = journal
        .active_registry()
        .unwrap()
        .into_iter()
        .map(|r| RegistryTarget {
            hive: r.hive,
            key_path: r.key_path,
            value_name: r.value_name,
        })
        .collect();
    assert!(
        recorded.contains(&expected),
        "{id}: {expected:?} not in {recorded:?}"
    );
}

/// Disables and re-enables an entry that has no StartupApproved value, then rolls back.
fn absent_value_round_trip() {
    let (run, _) = Key::create(Hive::CurrentUser, SANDBOX_RUN).unwrap();
    run.set(PROBE, &RegValue::ExpandSz(PROBE_COMMAND.to_string()))
        .unwrap();
    run.set(NOT_A_STRING, &RegValue::Dword(1)).unwrap();
    drop(run);
    assert!(!exists(Hive::CurrentUser, SANDBOX_APPROVED).unwrap());

    let entries = SANDBOX.list(&Account::SignedIn).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "non-string values are skipped: {entries:?}"
    );
    let entry = &entries[0];
    let id = format!("user_run:{PROBE}");
    assert_eq!(entry.id, id);
    assert_eq!(entry.name, PROBE);
    assert_eq!(entry.source, StartupSource::UserRun);
    assert_eq!(entry.location, "HKCU Run");
    assert_eq!(entry.command, PROBE_COMMAND);
    assert_eq!(entry.path, format!(r"{}\System32\cmd.exe", system_root()));
    assert!(entry.exists);
    assert_eq!(entry.publisher, "Microsoft Corporation");
    assert!(entry.enabled);
    assert!(!entry.requires_admin);
    assert!(entry.can_toggle);
    assert_eq!(entry.note, "");

    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            SANDBOX.set_enabled(&safety, &id, true, &same_user).unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );
        assert!(
            !exists(Hive::CurrentUser, SANDBOX_APPROVED).unwrap(),
            "nothing is written for an entry already in the desired state"
        );

        let before = filetime_now();
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &id, false, &same_user)
                .unwrap(),
            MutationOutcome::Applied
        );
        let after = filetime_now();
        let data = sandbox_approved(PROBE).expect("disabled value written");
        assert_eq!(data.len(), 12);
        assert_eq!(&data[..4], &[0x03, 0, 0, 0]);
        let stamp = u64::from_le_bytes(data[4..].try_into().unwrap());
        assert!(
            before <= stamp && stamp <= after,
            "{before} {stamp} {after}"
        );
        assert!(!sandbox_entry(PROBE).enabled);
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &id, false, &same_user)
                .unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );

        assert_eq!(
            SANDBOX.set_enabled(&safety, &id, true, &same_user).unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(sandbox_approved(PROBE), Some(approved_enabled_value()));
        assert!(sandbox_entry(PROBE).enabled);

        for bad in [
            format!("user_run:{OTHER}"),
            format!("user_run:{NOT_A_STRING}"),
            format!("machine_run:{PROBE}"),
            format!("user_folder:{PROBE}"),
            format!("services:{PROBE}"),
            PROBE.to_string(),
        ] {
            let err = SANDBOX
                .set_enabled(&safety, &bad, false, &same_user)
                .unwrap_err();
            assert!(is_unknown_entry(&err), "{bad}: {err}");
        }
    }

    assert_eq!(journal.summary().unwrap().registry_active, 1);
    assert_recorded_target(&journal, &id);
    let report = rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_deleted, 1);
    assert_eq!(sandbox_approved(PROBE), None);
    assert!(
        !exists(Hive::CurrentUser, SANDBOX_APPROVED_RUN).unwrap(),
        "the StartupApproved key created by the change is removed"
    );
    assert!(sandbox_entry(PROBE).enabled);
}

/// Changes entries that already have StartupApproved values; rollback restores the bytes.
fn existing_values_round_trip() {
    let probe_original = vec![0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let other_original = vec![0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    let (run, _) = Key::create(Hive::CurrentUser, SANDBOX_RUN).unwrap();
    run.set(PROBE, &RegValue::Sz(PROBE_COMMAND.to_string()))
        .unwrap();
    run.set(
        OTHER,
        &RegValue::Sz(r"C:\PCOptimizer Missing\other.exe -q".into()),
    )
    .unwrap();
    drop(run);
    let (approved, _) = Key::create(Hive::CurrentUser, SANDBOX_APPROVED_RUN).unwrap();
    approved
        .set(PROBE, &RegValue::Binary(probe_original.clone()))
        .unwrap();
    approved
        .set(OTHER, &RegValue::Binary(other_original.clone()))
        .unwrap();
    drop(approved);

    let entries = SANDBOX.list(&Account::SignedIn).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, [OTHER, PROBE]);
    let other = &entries[0];
    assert!(!other.enabled);
    assert!(!other.exists);
    assert_eq!(other.path, r"C:\PCOptimizer Missing\other.exe");
    assert_eq!(other.publisher, "");
    assert!(entries[1].enabled);

    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    {
        let safety = sandbox_session(&journal);
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &format!("user_run:{OTHER}"), true, &same_user)
                .unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &format!("user_run:{PROBE}"), false, &same_user)
                .unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(sandbox_approved(OTHER), Some(approved_enabled_value()));
        assert_eq!(sandbox_approved(PROBE).unwrap()[0], 0x03);
    }

    let report = rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_restored, 2);
    assert_eq!(sandbox_approved(PROBE), Some(probe_original));
    assert_eq!(sandbox_approved(OTHER), Some(other_original));
}

/// Per-user entries are listed but refused, before anything is written, while the process
/// runs as a different account than the signed-in user or the accounts cannot be compared.
fn other_user_refusal() {
    let (run, _) = Key::create(Hive::CurrentUser, SANDBOX_RUN).unwrap();
    run.set(PROBE, &RegValue::Sz(PROBE_COMMAND.to_string()))
        .unwrap();
    drop(run);
    create_sandbox_tasks();

    let unknown = Account::Unknown("identity check failed".to_string());
    for (account, note) in [
        (Account::Other, OTHER_USER_NOTE),
        (unknown.clone(), UNKNOWN_USER_NOTE),
    ] {
        let entries = SANDBOX.list(&account).unwrap();
        let probe = entries.iter().find(|e| e.name == PROBE).unwrap();
        assert!(probe.enabled);
        assert!(!probe.can_toggle, "{account:?}");
        assert_eq!(probe.note, note);
        let task = entries
            .iter()
            .find(|e| e.id == sandbox_task_id(INSTALLED_FAMILY, "Enabled"))
            .unwrap();
        assert!(!task.can_toggle, "{account:?}");
        assert_eq!(task.note, note);
        let policy_task = entries
            .iter()
            .find(|e| e.id == sandbox_task_id(INSTALLED_FAMILY, "PolicyOn"))
            .unwrap();
        assert_eq!(policy_task.note, POLICY_NOTE, "the policy reason is kept");
        assert!(entries.iter().all(|e| !e.can_toggle && !e.note.is_empty()));
    }
    assert_eq!(
        Account::from_check(Err(Error::Other("identity check failed".to_string()))),
        unknown
    );
    assert_eq!(Account::from_check(Ok(true)), Account::Other);
    assert_eq!(Account::from_check(Ok(false)), Account::SignedIn);
    assert!(Account::SignedIn.ensure_signed_in().is_ok());

    let other_user = || -> Result<bool> { Ok(true) };
    let check_failed =
        || -> Result<bool> { Err(Error::Other("identity check failed".to_string())) };
    let not_consulted = || -> Result<bool> { panic!("per-user check used for a machine entry") };
    let other_error = Account::Other.ensure_signed_in().unwrap_err().to_string();
    assert!(
        other_error.contains("without another account's credentials"),
        "{other_error}"
    );
    let unknown_error = unknown.ensure_signed_in().unwrap_err().to_string();
    assert!(
        unknown_error.starts_with("cannot confirm")
            && unknown_error.contains("identity check failed")
            && unknown_error.contains("nothing was changed"),
        "{unknown_error}"
    );
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    {
        let safety = sandbox_session(&journal);
        for id in [
            format!("user_run:{PROBE}"),
            sandbox_task_id(INSTALLED_FAMILY, "Enabled"),
            format!("user_folder:{PROBE}"),
        ] {
            let err = SANDBOX
                .set_enabled(&safety, &id, false, &other_user)
                .unwrap_err();
            assert_eq!(err.to_string(), other_error, "{id}");
            let err = SANDBOX
                .set_enabled(&safety, &id, false, &check_failed)
                .unwrap_err();
            assert_eq!(err.to_string(), unknown_error, "{id}");
        }
        let err = SANDBOX
            .set_enabled(
                &safety,
                &format!("machine_run:{PROBE}"),
                false,
                &not_consulted,
            )
            .unwrap_err();
        assert!(is_unknown_entry(&err), "{err}");
    }
    assert!(!exists(Hive::CurrentUser, SANDBOX_APPROVED).unwrap());
    assert_eq!(
        sandbox_task_state(INSTALLED_FAMILY, "Enabled"),
        Some(2),
        "nothing is written"
    );
    assert_eq!(journal.summary().unwrap().registry_active, 0);
}

fn sandbox_task_id(family: &str, task: &str) -> String {
    format!(r"packaged_task:{family}\{task}")
}

fn sandbox_task_state(family: &str, task: &str) -> Option<u32> {
    let path = format!(r"{SANDBOX_PACKAGED}\{family}\{task}");
    match read_value(Hive::CurrentUser, &path, packaged::STATE_VALUE).unwrap() {
        Some(RegValue::Dword(state)) => Some(state),
        None => None,
        Some(other) => panic!("unexpected State value {other:?}"),
    }
}

fn create_sandbox_tasks() {
    for (family, task, state) in SANDBOX_TASKS {
        let path = format!(r"{SANDBOX_PACKAGED}\{family}\{task}");
        let (key, _) = Key::create(Hive::CurrentUser, &path).unwrap();
        key.set(packaged::STATE_VALUE, &RegValue::Dword(state))
            .unwrap();
    }
    // A task key without a State value is not a startup task.
    Key::create(
        Hive::CurrentUser,
        &format!(r"{SANDBOX_PACKAGED}\{INSTALLED_FAMILY}\{NO_STATE_TASK}"),
    )
    .unwrap();
}

/// Lists packaged tasks, toggles them through their State value, and rolls back.
fn packaged_round_trip() {
    create_sandbox_tasks();
    let tasks: Vec<StartupEntry> = SANDBOX
        .list(&Account::SignedIn)
        .unwrap()
        .into_iter()
        .filter(|e| e.source == StartupSource::PackagedTask)
        .collect();
    let ids: Vec<&str> = tasks.iter().map(|e| e.id.as_str()).collect();
    let expected_ids = [
        sandbox_task_id(INSTALLED_FAMILY, "AppDisabled"),
        sandbox_task_id(INSTALLED_FAMILY, "Enabled"),
        sandbox_task_id(INSTALLED_FAMILY, "PolicyOff"),
        sandbox_task_id(INSTALLED_FAMILY, "PolicyOn"),
        sandbox_task_id(INSTALLED_FAMILY, "Unknown"),
        sandbox_task_id(UNRESOLVED_FAMILY, "Start"),
    ];
    assert_eq!(
        ids, expected_ids,
        "sorted by name; uninstalled packages and keys without State are left out"
    );
    let by_task = |task: &str| {
        tasks
            .iter()
            .find(|e| e.id.ends_with(&format!(r"\{task}")))
            .unwrap()
    };
    for (task, enabled, can_toggle) in [
        ("Enabled", true, true),
        ("AppDisabled", false, true),
        ("PolicyOn", true, false),
        ("PolicyOff", false, false),
        ("Unknown", false, false),
        ("Start", false, true),
    ] {
        let entry = by_task(task);
        assert_eq!(entry.enabled, enabled, "{task}");
        assert_eq!(entry.can_toggle, can_toggle, "{task}");
        assert_eq!(entry.note.is_empty(), can_toggle, "{task}");
        assert!(!entry.requires_admin);
        assert_eq!(entry.location, "Packaged app");
        assert_eq!(entry.path, "");
        assert_eq!(entry.command, "");
        assert!(!entry.exists);
    }
    assert_eq!(by_task("Enabled").name, "PCOptimizer.SelfTest (Enabled)");
    assert_eq!(by_task("Start").name, "PCOptimizer.Unresolved (Start)");
    assert_eq!(by_task("PolicyOn").note, POLICY_NOTE);

    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    {
        let safety = sandbox_session(&journal);
        let enabled_id = sandbox_task_id(INSTALLED_FAMILY, "Enabled");
        let app_disabled_id = sandbox_task_id(INSTALLED_FAMILY, "AppDisabled");
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &enabled_id, true, &same_user)
                .unwrap(),
            MutationOutcome::AlreadyInDesiredState
        );
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &enabled_id, false, &same_user)
                .unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(
            sandbox_task_state(INSTALLED_FAMILY, "Enabled"),
            Some(TaskState::DISABLE_RAW)
        );
        assert_eq!(
            SANDBOX
                .set_enabled(&safety, &app_disabled_id, true, &same_user)
                .unwrap(),
            MutationOutcome::Applied
        );
        assert_eq!(
            sandbox_task_state(INSTALLED_FAMILY, "AppDisabled"),
            Some(TaskState::ENABLE_RAW)
        );

        for task in ["PolicyOn", "PolicyOff", "Unknown"] {
            let id = sandbox_task_id(INSTALLED_FAMILY, task);
            let err = SANDBOX
                .set_enabled(&safety, &id, false, &same_user)
                .unwrap_err();
            assert!(
                err.to_string().contains("cannot be turned on or off"),
                "{err}"
            );
            let err = SANDBOX
                .set_enabled(&safety, &id, true, &same_user)
                .unwrap_err();
            assert!(
                err.to_string().contains("cannot be turned on or off"),
                "{err}"
            );
        }
        for bad in [
            sandbox_task_id(REMOVED_FAMILY, "Leftover"),
            sandbox_task_id(INSTALLED_FAMILY, NO_STATE_TASK),
            sandbox_task_id(INSTALLED_FAMILY, "Missing"),
            format!("packaged_task:{INSTALLED_FAMILY}"),
        ] {
            let err = SANDBOX
                .set_enabled(&safety, &bad, false, &same_user)
                .unwrap_err();
            assert!(is_unknown_entry(&err), "{bad}: {err}");
        }
    }
    assert_eq!(sandbox_task_state(INSTALLED_FAMILY, "PolicyOn"), Some(4));
    assert_eq!(sandbox_task_state(INSTALLED_FAMILY, "PolicyOff"), Some(3));
    assert_eq!(sandbox_task_state(INSTALLED_FAMILY, "Unknown"), Some(9));
    assert_eq!(sandbox_task_state(REMOVED_FAMILY, "Leftover"), Some(2));

    assert_eq!(journal.summary().unwrap().registry_active, 2);
    assert_recorded_target(&journal, &sandbox_task_id(INSTALLED_FAMILY, "Enabled"));
    assert_recorded_target(&journal, &sandbox_task_id(INSTALLED_FAMILY, "AppDisabled"));
    let report = rollback_journal(&journal, false).unwrap();
    assert!(report.is_clean(), "{:?}", report.failures);
    assert_eq!(report.registry_restored, 2);
    assert_eq!(sandbox_task_state(INSTALLED_FAMILY, "Enabled"), Some(2));
    assert_eq!(sandbox_task_state(INSTALLED_FAMILY, "AppDisabled"), Some(0));
}

/// Group Policy Run entries are listed as enabled and are never toggled.
fn policy_entries_are_read_only() {
    let (policy, _) = Key::create(Hive::CurrentUser, SANDBOX_POLICY_RUN).unwrap();
    policy
        .set(PROBE, &RegValue::Sz(PROBE_COMMAND.to_string()))
        .unwrap();
    drop(policy);
    // A StartupApproved value of the same name does not apply to policy entries.
    let (approved, _) = Key::create(Hive::CurrentUser, SANDBOX_APPROVED_RUN).unwrap();
    approved
        .set(PROBE, &RegValue::Binary(approved_disabled_value(1)))
        .unwrap();
    drop(approved);

    let entries = SANDBOX.list(&Account::SignedIn).unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.id, format!("policy_user_run:{PROBE}"));
    assert_eq!(entry.source, StartupSource::PolicyUserRun);
    assert_eq!(entry.location, "HKCU Run (Group Policy)");
    assert_eq!(entry.path, format!(r"{}\System32\cmd.exe", system_root()));
    assert!(entry.enabled);
    assert!(!entry.can_toggle);
    assert!(!entry.requires_admin);
    assert_eq!(entry.note, POLICY_NOTE);

    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    {
        let safety = sandbox_session(&journal);
        for id in [
            format!("policy_user_run:{PROBE}"),
            format!("policy_machine_run:{PROBE}"),
            format!("policy_user_run:{OTHER}"),
        ] {
            for enabled in [false, true] {
                let err = SANDBOX
                    .set_enabled(&safety, &id, enabled, &same_user)
                    .unwrap_err();
                assert!(err.to_string().contains("Group Policy"), "{id}: {err}");
            }
        }
    }
    assert_eq!(journal.summary().unwrap().registry_active, 0);
    assert_eq!(
        sandbox_approved(PROBE),
        Some(approved_disabled_value(1)),
        "nothing is written"
    );
}

#[test]
fn set_enabled_writes_task_manager_values_and_rolls_back() {
    clean_sandbox();
    absent_value_round_trip();
    clean_sandbox();
    existing_values_round_trip();
    clean_sandbox();
    other_user_refusal();
    clean_sandbox();
    packaged_round_trip();
    clean_sandbox();
    policy_entries_are_read_only();
    clean_sandbox();
    assert!(!exists(Hive::CurrentUser, SANDBOX_ROOT).unwrap());
}
