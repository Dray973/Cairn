//! Read-only checks of the startup scan against this machine's real configuration.
//! Nothing here writes to the registry or the Startup folders; changes are exercised only
//! against the HKCU sandbox in the library's unit tests.

use std::collections::HashSet;
use std::path::Path;

use optimizer_core::startup::{self, StartupEntry, StartupSource};
use optimizer_core::win::registry::{read_value, Hive, RegValue};
use optimizer_core::win::session;

const APPROVED_ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const PACKAGED_ROOT: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";

/// Hive and key where Explorer keeps the enabled state of entries of `source`; `None` for
/// sources StartupApproved does not cover.
fn approved_key(source: StartupSource) -> Option<(Hive, String)> {
    let (hive, subkey) = match source {
        StartupSource::UserRun => (Hive::CurrentUser, "Run"),
        StartupSource::MachineRun => (Hive::LocalMachine, "Run"),
        StartupSource::MachineRun32 => (Hive::LocalMachine, "Run32"),
        StartupSource::UserFolder => (Hive::CurrentUser, "StartupFolder"),
        StartupSource::CommonFolder => (Hive::LocalMachine, "StartupFolder"),
        StartupSource::PackagedTask
        | StartupSource::PolicyUserRun
        | StartupSource::PolicyMachineRun => return None,
    };
    Some((hive, format!(r"{APPROVED_ROOT}\{subkey}")))
}

/// `<family>\<task>` part of a packaged task id.
fn packaged_key(entry: &StartupEntry) -> &str {
    entry
        .id
        .strip_prefix("packaged_task:")
        .expect("packaged task id")
}

/// Raw state behind an entry: the StartupApproved bytes, or the packaged task's State.
fn state_bytes(entry: &StartupEntry) -> Option<Vec<u8>> {
    if entry.source == StartupSource::PackagedTask {
        let path = format!(r"{PACKAGED_ROOT}\{}", packaged_key(entry));
        return read_value(Hive::CurrentUser, &path, "State")
            .unwrap()
            .map(|value| value.to_bytes());
    }
    let (hive, key) = approved_key(entry.source)?;
    read_value(hive, &key, &entry.name)
        .unwrap()
        .map(|value| value.to_bytes())
}

fn source_name(source: StartupSource) -> String {
    serde_json::to_value(source)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn list_reports_unique_ids_in_name_order() {
    let entries = startup::list().expect("list startup entries");
    println!("{} startup entries", entries.len());
    for e in &entries {
        println!(
            "{}\n    name={:?} location={} enabled={} requires_admin={} can_toggle={} exists={} publisher={:?}\n    path={:?}\n    command={:?}\n    note={:?}",
            e.id,
            e.name,
            e.location,
            e.enabled,
            e.requires_admin,
            e.can_toggle,
            e.exists,
            e.publisher,
            e.path,
            e.command,
            e.note
        );
    }

    let mut ids = HashSet::new();
    for e in &entries {
        assert!(ids.insert(e.id.as_str()), "duplicate id {}", e.id);
        assert!(
            e.id.starts_with(&format!("{}:", source_name(e.source))),
            "{}",
            e.id
        );
        assert_eq!(StartupSource::of_id(&e.id), Some(e.source));
        if e.source == StartupSource::PackagedTask {
            let (family, task) = packaged_key(e).split_once('\\').expect("<family>\\<task>");
            assert!(!family.is_empty() && !task.is_empty(), "{}", e.id);
        } else {
            assert_eq!(e.id, format!("{}:{}", source_name(e.source), e.name));
        }
        let machine = matches!(
            e.source,
            StartupSource::MachineRun
                | StartupSource::MachineRun32
                | StartupSource::CommonFolder
                | StartupSource::PolicyMachineRun
        );
        assert_eq!(e.requires_admin, machine, "{}", e.id);
        assert!(!e.location.is_empty(), "{}", e.id);
        assert!(!e.name.is_empty(), "{}", e.id);
        assert!(
            !e.name.eq_ignore_ascii_case("desktop.ini"),
            "desktop.ini is not an entry"
        );
        assert_eq!(
            e.exists,
            !e.path.is_empty() && Path::new(&e.path).exists(),
            "{}",
            e.id
        );
        if !e.publisher.is_empty() && e.source != StartupSource::PackagedTask {
            assert!(e.exists, "publisher without a file: {}", e.id);
        }
        assert_eq!(e.can_toggle, e.note.is_empty(), "{}", e.id);
        if e.source.is_policy() {
            assert!(e.enabled && !e.can_toggle, "{}", e.id);
        }
    }

    let names: Vec<String> = entries.iter().map(|e| e.name.to_lowercase()).collect();
    assert!(
        names.windows(2).all(|pair| pair[0] <= pair[1]),
        "not sorted by name: {names:?}"
    );
}

#[test]
fn per_user_entries_are_toggleable_only_for_the_signed_in_user() {
    let signed_in = matches!(session::elevated_as_other_user(), Ok(false));
    let entries = startup::list().expect("list startup entries");
    for e in entries
        .iter()
        .filter(|e| matches!(e.source, StartupSource::UserRun | StartupSource::UserFolder))
    {
        assert_eq!(e.can_toggle, signed_in, "{}: {:?}", e.id, e.note);
    }
    assert_eq!(
        startup::ensure_per_user_changes_allowed().is_ok(),
        signed_in
    );
}

#[test]
fn enabled_matches_the_recorded_state() {
    let entries = startup::list().expect("list startup entries");
    let before: Vec<Option<Vec<u8>>> = entries.iter().map(state_bytes).collect();
    for (e, bytes) in entries.iter().zip(&before) {
        let expected = match e.source {
            StartupSource::PolicyUserRun | StartupSource::PolicyMachineRun => true,
            StartupSource::PackagedTask => {
                let raw = bytes.as_deref().expect("packaged tasks have a State value");
                let state = RegValue::from_raw(4, raw);
                matches!(state, RegValue::Dword(2) | RegValue::Dword(4))
            }
            _ => match bytes.as_deref().and_then(|b| b.first()) {
                Some(flags) => flags & 1 == 0,
                None => true,
            },
        };
        println!("{}: state={bytes:02x?} enabled={}", e.id, e.enabled);
        assert_eq!(e.enabled, expected, "{}", e.id);
    }

    // Scanning again changes neither the result nor any recorded state.
    let again = startup::list().expect("list startup entries");
    let ids: Vec<(&str, bool)> = entries.iter().map(|e| (e.id.as_str(), e.enabled)).collect();
    let ids_again: Vec<(&str, bool)> = again.iter().map(|e| (e.id.as_str(), e.enabled)).collect();
    assert_eq!(ids, ids_again);
    let after: Vec<Option<Vec<u8>>> = entries.iter().map(state_bytes).collect();
    assert_eq!(before, after);
}
