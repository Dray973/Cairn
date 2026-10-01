//! Profiles through the public API against the live system, read-only: a plan and the export
//! candidates only read the catalog state, startup entries and adapters, and never open a
//! journal session or write an audit row. Applying is covered by the unit tests with a fake
//! system and never runs here.

use std::sync::Arc;

use optimizer_core::profiles::{self, Section};
use optimizer_core::safety::state_log::Journal;

fn temp_journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
    (dir, journal)
}

#[test]
fn starter_plan_is_read_only() {
    let (_dir, journal) = temp_journal();
    let gaming = profiles::starter("gaming").unwrap();
    let plan = profiles::plan(journal.clone(), &gaming).unwrap();
    assert!(plan.dry_run);
    assert_eq!(plan.name, "Gaming");
    assert_eq!(plan.rows.len(), gaming.tweaks.len());
    for row in &plan.rows {
        assert!(row.key.starts_with("tweak:"), "{}", row.key);
        assert_eq!(row.section, Section::Tweaks);
        assert_eq!(
            row.selected,
            row.caution.is_none() && row.status == profiles::step::StepStatus::Change
        );
    }
    assert_eq!(plan.changes + plan.already + plan.skipped, plan.rows.len());
    assert!(journal.sessions().unwrap().is_empty());
    assert!(journal.ops(10).unwrap().is_empty());
}

#[test]
fn export_candidates_are_read_only() {
    let (_dir, journal) = temp_journal();
    let candidates = profiles::candidates(journal.clone()).unwrap();
    for row in &candidates.rows {
        let prefix = row.key.split(':').next().unwrap();
        assert!(
            [
                "tweak",
                "app",
                "startup",
                "dns",
                "windows_update",
                "maintenance"
            ]
            .contains(&prefix),
            "{}",
            row.key
        );
    }
    assert!(journal.sessions().unwrap().is_empty());
    assert!(journal.ops(10).unwrap().is_empty());
}

#[test]
fn starter_texts_check_and_round_trip() {
    for summary in profiles::starters() {
        let parsed = profiles::parse(&summary.text).unwrap();
        let again = profiles::summary(&parsed);
        assert_eq!(again.text, summary.text);
        assert_eq!(again.counts, summary.counts);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Gaming.json");
    profiles::write_file(&path, &profiles::starters()[0].text).unwrap();
    let loaded = profiles::load(&path).unwrap().unwrap();
    assert_eq!(loaded.name, "Gaming");
    std::fs::write(&path, "[]").unwrap();
    let invalid = profiles::load(&path).unwrap().unwrap_err();
    assert_eq!(invalid.to_string(), "This file is not a Cairn profile.");
    assert!(profiles::load(&dir.path().join("missing.json")).is_err());
}
