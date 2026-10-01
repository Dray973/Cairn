//! Integration tests of the permissions guide. Read-only: the guide reads Windows' usage
//! records in the live per-user consent store and never writes it, and every change is
//! refused before anything is read.

use optimizer_core::permissions::{self, Capability};

#[test]
fn the_guide_covers_three_capabilities_with_their_settings_pages() {
    let guide = permissions::list();
    let capabilities: Vec<Capability> = guide.capabilities.iter().map(|c| c.capability).collect();
    assert_eq!(capabilities, Capability::ALL);
    for capability in &guide.capabilities {
        assert_eq!(capability.label, capability.capability.label());
        assert_eq!(
            capability.settings_uri,
            capability.capability.settings_uri()
        );
        assert!(capability.settings_uri.starts_with("ms-settings:privacy-"));
        assert!(capability.recent_desktop_apps.len() <= 50);
        assert!(capability
            .recent_desktop_apps
            .iter()
            .all(|r| !r.path.is_empty()));
        println!(
            "{}: {} desktop programs",
            capability.label,
            capability.recent_desktop_apps.len()
        );
    }
    let json = serde_json::to_value(&guide).expect("the guide serializes");
    assert!(json["capabilities"][0].get("switches").is_none());
    assert!(json["capabilities"][0].get("settings_uri").is_some());
}

#[test]
fn neither_the_guide_nor_a_refusal_opens_a_journal() {
    // The default journal lives in OPTIMIZER_DATA_DIR; no other test of this binary reads it.
    let dir = tempfile::tempdir().expect("temp dir");
    std::env::set_var("OPTIMIZER_DATA_DIR", dir.path());
    let guide = permissions::list();
    let refused = permissions::change_refused();
    assert_eq!(guide.capabilities.len(), 3);
    assert!(refused.to_string().ends_with("Nothing was changed."));
    let created: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read the data folder")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(created.is_empty(), "{created:?}");
}

#[test]
fn every_change_is_refused() {
    let text = permissions::change_refused().to_string();
    assert!(
        text.starts_with("Cairn does not change app permissions"),
        "{text}"
    );
    assert!(text.contains("Settings › Privacy & security"), "{text}");
    assert!(text.ends_with("Nothing was changed."), "{text}");
}
