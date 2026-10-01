//! Tests of the permissions guide against a sandbox layout (every key lives under
//! HKCU\Software\PCOptimizer\SelfTest and is deleted when the test ends), of the refusal of
//! every change, and of the undo of the permission records earlier builds left in a journal,
//! written by a test session to sandbox keys. The real consent store is never written.

use std::sync::Arc;

use chrono::DateTime;

use super::store::{Layout, NON_PACKAGED, SYSTEM, USER_STORE};
use super::*;
use crate::safety::rollback::{
    rollback_filtered, rollback_journal, RegistryTarget, RollbackFilter,
};
use crate::safety::state_log::Journal;
use crate::safety::test_safety;
use crate::win::filetime::UNIX_EPOCH_FILETIME;
use crate::win::registry::{delete_sandbox_tree, exists, read_value, Hive, RegValue};

const SELF_TEST: &str = r"Software\PCOptimizer\SelfTest";
const HKCU: Hive = Hive::CurrentUser;
const MEET_EXE: &str = "C:#Program Files#Contoso#meet.exe";
const RECORDER_EXE: &str = "C:#Tools#Northwind#recorder.exe";
const CONTOSO: &str = "Contoso.Meet_a";
/// Below the sandbox root: where the location sensor's override stands in for
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Sensor\Overrides\{BFA794E4-…}`.
const SENSOR: &str = r"Sensor\Overrides\{BFA794E4-F964-4FDB-90F6-51056BFE4B44}";

/// The keys of one test: `<root>\User` stands in for the per-user consent store and
/// `<root>\Device` for the one of the whole PC. The root is a direct child of the self-test
/// key and is deleted when the sandbox is dropped.
struct Sandbox {
    root: String,
    user: String,
}

impl Sandbox {
    fn new(test: &str) -> Sandbox {
        let root = format!(r"{SELF_TEST}\Permissions{test}{}", std::process::id());
        delete_sandbox_tree(&root).unwrap();
        Sandbox {
            user: format!(r"{root}\User"),
            root,
        }
    }

    fn layout(&self) -> Layout<'_> {
        Layout {
            user: (HKCU, &self.user),
        }
    }

    /// The full path of `rel` below the root.
    fn path(&self, rel: &str) -> String {
        format!(r"{}\{rel}", self.root)
    }

    /// Writes `name` = `value` at `rel`, creating the key.
    fn put(&self, rel: &str, name: &str, value: RegValue) {
        let (key, _) = Key::create(HKCU, &self.path(rel)).unwrap();
        key.set(name, &value).unwrap();
    }

    fn get(&self, rel: &str, name: &str) -> Option<RegValue> {
        read_value(HKCU, &self.path(rel), name).unwrap()
    }

    fn has_key(&self, rel: &str) -> bool {
        exists(HKCU, &self.path(rel)).unwrap()
    }

    /// The usage record Windows keeps of a desktop program under `<store key>\NonPackaged`.
    fn used(&self, store_key: &str, program: &str, start: u64, stop: u64) {
        let rel = format!(r"User\{store_key}\{NON_PACKAGED}\{program}");
        self.put(&rel, "LastUsedTimeStart", RegValue::Qword(start));
        self.put(&rel, "LastUsedTimeStop", RegValue::Qword(stop));
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = delete_sandbox_tree(&self.root);
    }
}

/// A time Settings stored with a consent value before any change.
const LAST_SET: u64 = UNIX_EPOCH_FILETIME + 1_700_000_000 * 10_000_000;

fn sz(s: &str) -> RegValue {
    RegValue::Sz(s.to_string())
}

/// FILETIME of an RFC 3339 time.
fn at(rfc3339: &str) -> u64 {
    let secs = DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp();
    UNIX_EPOCH_FILETIME + secs as u64 * 10_000_000
}

fn temp_journal(dir: &tempfile::TempDir) -> Arc<Journal> {
    Arc::new(Journal::open(dir.path().join("journal.db")).unwrap())
}

/// (path, last used, in use) of each recent desktop program of `cap`.
fn recent(guide: &PermissionsGuide, cap: Capability) -> Vec<(String, Option<String>, bool)> {
    guide
        .capabilities
        .iter()
        .find(|c| c.capability == cap)
        .unwrap()
        .recent_desktop_apps
        .iter()
        .map(|r| (r.path.clone(), r.last_used.clone(), r.in_use))
        .collect()
}

#[test]
fn every_capability_names_its_settings_page() {
    assert_eq!(
        Capability::ALL.map(Capability::key),
        ["camera", "microphone", "location"]
    );
    assert_eq!(
        Capability::ALL.map(Capability::store_key),
        ["webcam", "microphone", "location"]
    );
    assert_eq!(
        Capability::ALL.map(Capability::label),
        ["Camera", "Microphone", "Location"]
    );
    assert_eq!(
        Capability::ALL.map(Capability::settings_uri),
        [
            "ms-settings:privacy-webcam",
            "ms-settings:privacy-microphone",
            "ms-settings:privacy-location"
        ]
    );
    assert_eq!(
        serde_json::to_value(Capability::Microphone).unwrap(),
        "microphone"
    );
    assert_eq!(
        SYSTEM.user,
        (
            Hive::CurrentUser,
            r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore"
        )
    );
    assert_eq!(
        SYSTEM.desktop_apps(Capability::Camera),
        (
            Hive::CurrentUser,
            format!(r"{USER_STORE}\webcam\NonPackaged")
        )
    );
}

#[test]
fn the_guide_lists_each_settings_page_and_the_desktop_programs_newest_first() {
    let sandbox = Sandbox::new("Guide");
    sandbox.used("webcam", MEET_EXE, at("2026-09-27T19:14:02Z"), 0);
    sandbox.used(
        "webcam",
        RECORDER_EXE,
        at("2026-09-01T12:00:00Z"),
        at("2026-09-01T12:05:00Z"),
    );
    // A program key without a stamp is not a use.
    sandbox.used("webcam", "C:#Tools#Fabrikam#idle.exe", 0, 0);
    sandbox.used("location", MEET_EXE, 0, at("2026-09-20T08:00:00Z"));
    // A Store app's usage record is not a desktop program's.
    sandbox.put(
        &format!(r"User\webcam\{CONTOSO}"),
        "LastUsedTimeStart",
        RegValue::Qword(at("2026-09-28T10:00:00Z")),
    );

    let guide = sandbox.layout().guide();
    assert!(guide.warnings.is_empty(), "{:?}", guide.warnings);
    let pages: Vec<(Capability, &str, &str)> = guide
        .capabilities
        .iter()
        .map(|c| (c.capability, c.label.as_str(), c.settings_uri.as_str()))
        .collect();
    assert_eq!(
        pages,
        [
            (Capability::Camera, "Camera", "ms-settings:privacy-webcam"),
            (
                Capability::Microphone,
                "Microphone",
                "ms-settings:privacy-microphone"
            ),
            (
                Capability::Location,
                "Location",
                "ms-settings:privacy-location"
            ),
        ]
    );
    assert_eq!(
        recent(&guide, Capability::Camera),
        [
            (
                r"C:\Program Files\Contoso\meet.exe".to_string(),
                Some("2026-09-27T19:14:02Z".to_string()),
                true
            ),
            (
                r"C:\Tools\Northwind\recorder.exe".to_string(),
                Some("2026-09-01T12:05:00Z".to_string()),
                false
            ),
        ]
    );
    assert!(recent(&guide, Capability::Microphone).is_empty());
    assert_eq!(
        recent(&guide, Capability::Location),
        [(
            r"C:\Program Files\Contoso\meet.exe".to_string(),
            Some("2026-09-20T08:00:00Z".to_string()),
            false
        )]
    );
}

#[test]
fn the_guide_holds_no_permission_state_and_reads_no_consent_value() {
    let sandbox = Sandbox::new("NoState");
    // Consent values of every kind: none of them is part of the guide.
    sandbox.put(r"User\webcam", "Value", sz("Deny"));
    sandbox.put(r"User\webcam", "LastSetTime", RegValue::Qword(LAST_SET));
    sandbox.put(r"User\webcam\NonPackaged", "Value", sz("Deny"));
    sandbox.put(&format!(r"User\webcam\{CONTOSO}"), "Value", sz("Prompt"));
    sandbox.used("webcam", MEET_EXE, at("2026-09-27T19:14:02Z"), 0);

    let json = serde_json::to_value(sandbox.layout().guide()).unwrap();
    let keys = |value: &serde_json::Value| {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&json), ["capabilities", "warnings"]);
    let capabilities = json["capabilities"].as_array().unwrap();
    assert_eq!(capabilities.len(), 3);
    for capability in capabilities {
        assert_eq!(
            keys(capability),
            ["capability", "label", "recent_desktop_apps", "settings_uri"]
        );
        for program in capability["recent_desktop_apps"].as_array().unwrap() {
            assert_eq!(keys(program), ["in_use", "last_used", "path"]);
        }
    }
    let text = json.to_string();
    for state in ["Deny", "Prompt", "deny", "prompt", "allow", "not_set"] {
        assert!(!text.contains(state), "{state} in {text}");
    }

    // Reading changed nothing.
    assert_eq!(sandbox.get(r"User\webcam", "Value"), Some(sz("Deny")));
    assert_eq!(
        sandbox.get(r"User\webcam", "LastSetTime"),
        Some(RegValue::Qword(LAST_SET))
    );
    assert_eq!(
        sandbox.get(&format!(r"User\webcam\{CONTOSO}"), "Value"),
        Some(sz("Prompt"))
    );
}

#[test]
fn a_missing_consent_store_gives_empty_lists_and_creates_nothing() {
    let sandbox = Sandbox::new("Missing");
    let guide = sandbox.layout().guide();
    assert!(guide.warnings.is_empty(), "{:?}", guide.warnings);
    assert_eq!(guide.capabilities.len(), 3);
    assert!(guide
        .capabilities
        .iter()
        .all(|c| c.recent_desktop_apps.is_empty()));
    assert!(!sandbox.has_key("User"), "reading created no key");
}

#[test]
fn at_most_fifty_desktop_programs_are_listed_newest_first() {
    let sandbox = Sandbox::new("Limit");
    let start = at("2026-09-01T00:00:00Z");
    for n in 0..55u64 {
        let minute = start + n * 60 * 10_000_000;
        sandbox.used(
            "microphone",
            &format!("C:#Apps#app{n:02}.exe"),
            minute,
            minute + 10_000_000,
        );
    }
    let guide = sandbox.layout().guide();
    let listed = recent(&guide, Capability::Microphone);
    assert_eq!(listed.len(), RECENT_LIMIT);
    assert_eq!(listed[0].0, r"C:\Apps\app54.exe");
    assert_eq!(listed[49].0, r"C:\Apps\app05.exe");
    assert!(listed.iter().all(|(_, _, in_use)| !in_use));
}

#[test]
fn every_change_is_refused_with_one_message() {
    let refused = change_refused();
    assert!(matches!(refused, Error::Other(_)), "{refused:?}");
    assert_eq!(
        refused.to_string(),
        "Cairn does not change app permissions: Windows 11 manages camera, microphone and \
         location permissions itself, in Settings › Privacy & security, and on this version of \
         Windows an app like Cairn cannot change them. Nothing was changed."
    );
    assert_eq!(change_refused().to_string(), refused.to_string());
}

/// What earlier builds recorded and wrote for a Store app's camera permission
/// (`camera:app:Contoso.Meet_a`) and for the location switch for the whole PC
/// (`location:device`, with the location sensor's override), in two sessions of a temp
/// journal: each value's baseline first, then the value.
fn record_earlier_changes(sandbox: &Sandbox, journal: &Arc<Journal>) {
    let app = sandbox.path(&format!(r"User\webcam\{CONTOSO}"));
    let safety = test_safety(
        journal.clone(),
        &format!("permission: camera:app:{CONTOSO}"),
        false,
    );
    safety
        .set_registry_value(HKCU, &app, "Value", &sz("Deny"))
        .unwrap();
    safety
        .set_registry_value(HKCU, &app, "LastSetTime", &RegValue::Qword(LAST_SET + 1))
        .unwrap();

    let location = sandbox.path(r"Device\location");
    let safety = test_safety(journal.clone(), "permission: location:device", false);
    safety
        .set_registry_value(HKCU, &location, "Value", &sz("Deny"))
        .unwrap();
    safety
        .set_registry_value(
            HKCU,
            &location,
            "LastSetTime",
            &RegValue::Qword(LAST_SET + 2),
        )
        .unwrap();
    safety
        .set_registry_value(
            HKCU,
            &sandbox.path(SENSOR),
            "SensorPermissionState",
            &RegValue::Dword(0),
        )
        .unwrap();
}

/// The sandbox as it was before the earlier changes: the Store app asked first, location was
/// on, and the location sensor had no override.
fn with_original_values(sandbox: Sandbox) -> Sandbox {
    let app = format!(r"User\webcam\{CONTOSO}");
    sandbox.put(&app, "Value", sz("Prompt"));
    sandbox.put(&app, "LastSetTime", RegValue::Qword(LAST_SET));
    sandbox.put(r"Device\location", "Value", sz("Allow"));
    sandbox.put(r"Device\location", "LastSetTime", RegValue::Qword(LAST_SET));
    sandbox
}

#[test]
fn records_of_earlier_permission_changes_undo_one_entry_at_a_time() {
    let sandbox = with_original_values(Sandbox::new("UndoEntry"));
    let dir = tempfile::tempdir().unwrap();
    let journal = temp_journal(&dir);
    record_earlier_changes(&sandbox, &journal);
    assert_eq!(
        sandbox.get(SENSOR, "SensorPermissionState"),
        Some(RegValue::Dword(0))
    );
    assert_eq!(journal.active_registry().unwrap().len(), 5);

    // History's Undo of "Location services on this PC": the three values of that entry.
    let location = sandbox.path(r"Device\location");
    let target = |key_path: &str, value_name: &str| RegistryTarget {
        hive: HKCU,
        key_path: key_path.to_string(),
        value_name: value_name.to_string(),
    };
    let filter = RollbackFilter {
        registry: vec![
            target(&location, "Value"),
            target(&location, "LastSetTime"),
            target(&sandbox.path(SENSOR), "SensorPermissionState"),
        ],
        ..Default::default()
    };
    let report = rollback_filtered(&journal, &filter, false).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!((report.registry_restored, report.registry_deleted), (2, 1));
    assert_eq!(sandbox.get(r"Device\location", "Value"), Some(sz("Allow")));
    assert_eq!(
        sandbox.get(r"Device\location", "LastSetTime"),
        Some(RegValue::Qword(LAST_SET))
    );
    assert!(
        !sandbox.has_key("Sensor"),
        "the override did not exist before: its value and the keys created for it are gone"
    );
    // The Store app's records are still active and its values unchanged.
    let active: Vec<String> = journal
        .active_registry()
        .unwrap()
        .into_iter()
        .map(|r| r.value_name)
        .collect();
    assert_eq!(active.len(), 2, "{active:?}");
    assert_eq!(
        sandbox.get(&format!(r"User\webcam\{CONTOSO}"), "Value"),
        Some(sz("Deny"))
    );
}

#[test]
fn revert_all_undoes_every_record_of_earlier_permission_changes() {
    let sandbox = with_original_values(Sandbox::new("UndoAll"));
    let dir = tempfile::tempdir().unwrap();
    let journal = temp_journal(&dir);
    record_earlier_changes(&sandbox, &journal);

    let planned = rollback_journal(&journal, true).unwrap();
    assert_eq!(planned.actions.len(), 5, "{:?}", planned.actions);
    assert!(planned
        .actions
        .iter()
        .any(|a| a.contains("SensorPermissionState")));
    assert_eq!(
        sandbox.get(r"Device\location", "Value"),
        Some(sz("Deny")),
        "a dry run changes nothing"
    );

    let report = rollback_journal(&journal, false).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!((report.registry_restored, report.registry_deleted), (4, 1));
    let app = format!(r"User\webcam\{CONTOSO}");
    assert_eq!(sandbox.get(&app, "Value"), Some(sz("Prompt")));
    assert_eq!(
        sandbox.get(&app, "LastSetTime"),
        Some(RegValue::Qword(LAST_SET))
    );
    assert_eq!(sandbox.get(r"Device\location", "Value"), Some(sz("Allow")));
    assert_eq!(
        sandbox.get(r"Device\location", "LastSetTime"),
        Some(RegValue::Qword(LAST_SET))
    );
    assert!(!sandbox.has_key("Sensor"));
    assert!(journal.active_registry().unwrap().is_empty());
}
