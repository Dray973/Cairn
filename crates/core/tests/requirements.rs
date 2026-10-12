//! The Windows facts, the requirement probes and the Windows ranges of the catalog against
//! this PC, read-only: the probes read `CurrentVersion`, the Recall servicing record, the
//! consent store and the installed packages; the scans use a temporary journal and open no
//! session.

use std::sync::Arc;

use optimizer_core::debloat::catalog::{ANY_WINDOWS, TWEAKS};
use optimizer_core::debloat::requirements::{self, servicing_family, Unavailable, WindowsFacts};
use optimizer_core::debloat::{ActionState, Engine, ItemState};
use optimizer_core::safety::state_log::Journal;

fn temp_journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let journal = Journal::open(dir.path().join("journal.db")).expect("open journal");
    (dir, Arc::new(journal))
}

/// Client editions name themselves "Windows 11 …"; a Windows Server build (a CI runner) keeps
/// its own name.
fn assert_windows_11_name(facts: &WindowsFacts) {
    if facts.name.starts_with("Windows Server") {
        println!("{}: a server edition keeps its own name", facts.name);
    } else {
        assert!(facts.name.starts_with("Windows 11"), "{facts:?}");
    }
}

#[test]
fn scan_reports_the_windows_facts() {
    let (_dir, journal) = temp_journal();
    let report = Engine::new(journal.clone()).scan().expect("scan");
    let facts = report
        .windows
        .clone()
        .expect("the scan reports the running Windows");
    println!("{facts:?}");
    assert!(facts.family >= 22000, "{facts:?}");
    assert_eq!(facts.family, servicing_family(facts.build.number));
    assert!(facts.build.number >= facts.family, "{facts:?}");
    assert_windows_11_name(&facts);
    assert_eq!(Some(&facts), requirements::windows_facts().ok().as_ref());
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.starts_with("Cannot read the Windows version")),
        "{:?}",
        report.warnings
    );
    let json = serde_json::to_value(&report).expect("scan report serialises");
    assert_eq!(
        json["windows"]["build"]["number"].as_u64(),
        Some(u64::from(facts.build.number))
    );
    assert_eq!(
        json["windows"]["family"].as_u64(),
        Some(u64::from(facts.family))
    );
    assert_eq!(journal.summary().unwrap().sessions, 0, "read-only");
}

#[test]
fn windows_range_items_are_unavailable_with_a_reason_or_read() {
    let (_dir, journal) = temp_journal();
    let report = Engine::new(journal.clone()).scan().expect("scan");
    let facts = report.windows.clone().expect("the running Windows");
    let mut ranged = 0;
    for t in TWEAKS {
        let item = report
            .item(t.id)
            .unwrap_or_else(|| panic!("{} missing from scan", t.id));
        let range_kind = matches!(
            item.unavailable,
            Some(Unavailable::WindowsVersion | Unavailable::Edition)
        );
        if t.windows == ANY_WINDOWS {
            assert!(
                !range_kind,
                "{} has no Windows range but scans as {:?}",
                t.id, item.unavailable
            );
            continue;
        }
        ranged += 1;
        if t.retired.is_some() {
            assert_eq!(item.unavailable, Some(Unavailable::Retired), "{}", t.id);
            continue;
        }
        if range_kind {
            // Nothing is recorded in this journal, so nothing is read.
            let note = item.note.as_deref().unwrap_or_default();
            assert!(
                note.contains("; this PC runs Windows") && note.ends_with('.'),
                "{}: {note}",
                t.id
            );
            assert_eq!(item.state, ItemState::Unavailable, "{}", t.id);
            assert!(
                item.actions
                    .iter()
                    .all(|a| a.state == ActionState::Unavailable && a.detail.ends_with(note)),
                "{}: {:?}",
                t.id,
                item.actions
            );
            assert!(!item.revertible && !item.recommended, "{}", t.id);
            println!("{}: {:?}, {note}", t.id, item.unavailable);
        } else {
            // Inside its range: read, or not on this PC for its requirement, or a warning.
            let warned = report
                .warnings
                .iter()
                .any(|w| w.starts_with(&format!("{}:", t.id)));
            assert!(
                item.state != ItemState::Unavailable
                    || item.unavailable == Some(Unavailable::NotOnThisPc)
                    || warned,
                "{} is unavailable without a reason or a warning",
                t.id
            );
            println!("{}: {:?}, note {:?}", t.id, item.state, item.note);
        }
    }
    let expected = TWEAKS.iter().filter(|t| t.windows != ANY_WINDOWS).count();
    assert_eq!(ranged, expected);
    println!(
        "{ranged} tweaks with a Windows range on {} {:?} (build {})",
        facts.name, facts.version, facts.build
    );
    assert_eq!(journal.summary().unwrap().sessions, 0, "read-only");
}

#[test]
fn system_probes_are_read_only() {
    let lines = requirements::probe_lines();
    for line in &lines {
        println!("{line}");
    }
    assert_eq!(lines.len(), 9);
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("not available in this build")),
        "{lines:?}"
    );
    // The Office product probe reads only the Click-to-Run and Windows Installer registration.
    assert!(
        lines.iter().any(|l| l.starts_with("office product: Ok(")),
        "{lines:?}"
    );
    let facts = requirements::windows_facts().expect("the Windows facts of this PC");
    println!(
        "edition {:?} ({}), version {:?}, build {}, family {}",
        facts.edition, facts.edition_id, facts.version, facts.build, facts.family
    );
    assert!(facts.family >= 22000, "{facts:?}");
    assert_windows_11_name(&facts);
}
