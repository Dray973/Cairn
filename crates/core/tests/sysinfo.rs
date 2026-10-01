//! System information read from this PC.
//!
//! Read-only: every call only queries the system; nothing is written.

use std::time::{Duration, Instant};

use optimizer_core::sysinfo::{self, ReportSection, Section, SystemInfo};

#[test]
fn snapshot_reads_the_machine_without_changing_it() {
    let started = Instant::now();
    let snap = sysinfo::snapshot().expect("a snapshot never fails as a whole");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
    let info = &snap.info;
    assert!(info.errors.is_empty(), "sections failed: {:?}", info.errors);

    let os = info.os.as_ref().expect("the Windows section");
    assert!(os.build >= 10240, "build {}", os.build);
    assert!(!os.product_name.is_empty());

    let cpu = info.cpu.as_ref().expect("the processor section");
    assert!(cpu.cores >= 1);
    assert!(cpu.logical_processors >= cpu.cores);
    if let (Some(p), Some(e)) = (cpu.performance_cores, cpu.efficiency_cores) {
        assert_eq!(p + e, cpu.cores);
    }

    let memory = info.memory.as_ref().expect("the memory section");
    assert!(memory.usable_bytes > 0);
    assert!(memory.available_bytes <= memory.usable_bytes);

    let windows = info
        .volumes
        .iter()
        .find(|v| v.system)
        .expect("the Windows volume is listed and marked");
    assert!(windows.ready, "{windows:?}");
    assert!(
        windows.size_bytes.is_some_and(|size| size > 0),
        "{windows:?}"
    );
    // The disk holding Windows answers well within the deadline.
    for number in &windows.disk_numbers {
        let disk = info
            .disks
            .iter()
            .find(|d| d.number == *number)
            .unwrap_or_else(|| panic!("disk {number} is listed: {:?}", info.disks));
        assert!(disk.system, "{disk:?}");
        assert!(disk.size_bytes.is_some_and(|size| size > 0), "{disk:?}");
    }

    let ids: Vec<Section> = snap.sections.iter().map(|s| s.id).collect();
    assert_eq!(ids, Section::ALL);
    for section in &snap.sections {
        assert_eq!(section.title, section.id.title());
    }
    // Every memory row, one per module among them, can be told apart by its label.
    let memory_rows = &snap
        .sections
        .iter()
        .find(|s| s.id == Section::Memory)
        .expect("the memory section")
        .rows;
    let mut labels: Vec<&str> = memory_rows.iter().map(|r| r.label.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), memory_rows.len(), "{memory_rows:?}");

    assert!(snap.text.starts_with("Cairn "), "{}", snap.text);
    assert!(snap.text.ends_with('\n'));
    if !os.computer_name.is_empty() {
        assert!(
            !snap.text.contains(&os.computer_name),
            "the copied text must leave out the computer name"
        );
    }
    assert!(!snap.summary.is_empty());

    let json = serde_json::to_string(&snap).expect("the snapshot serializes");
    let back: sysinfo::Snapshot = serde_json::from_str(&json).expect("and reads back");
    assert_eq!(back.text, snap.text);
    // serde_json's default float parser can read a value back one unit in the last place
    // off, so fractions are compared with a tolerance and everything else exactly.
    let (sections, fractions) = split_fractions(&snap.sections);
    let (back_sections, back_fractions) = split_fractions(&back.sections);
    assert_eq!(back_sections, sections);
    for (read, written) in back_fractions.iter().zip(&fractions) {
        assert!((read - written).abs() < 1e-12, "{read} != {written}");
    }
}

/// The sections with every fraction set to 0.0, and the fractions in row order.
fn split_fractions(sections: &[ReportSection]) -> (Vec<ReportSection>, Vec<f64>) {
    let mut sections = sections.to_vec();
    let mut fractions = Vec::new();
    for section in &mut sections {
        let group_rows = section.groups.iter_mut().flat_map(|g| g.rows.iter_mut());
        for row in section.rows.iter_mut().chain(group_rows) {
            if let Some(fraction) = row.fraction.as_mut() {
                fractions.push(*fraction);
                *fraction = 0.0;
            }
        }
    }
    (sections, fractions)
}

/// The parts of a snapshot that only change when the hardware or the drivers change.
fn static_parts(info: &SystemInfo) -> String {
    let cpu = info.cpu.as_ref().map(|c| {
        (
            c.name.clone(),
            c.cores,
            c.logical_processors,
            c.performance_cores,
            c.cache.clone(),
        )
    });
    let memory = info.memory.as_ref().map(|m| {
        (
            m.installed_bytes,
            m.usable_bytes,
            m.slots,
            m.modules.clone(),
        )
    });
    let gpus: Vec<_> = info
        .gpus
        .iter()
        .map(|g| {
            (
                g.name.clone(),
                g.vendor_id,
                g.device_id,
                g.driver_version.clone(),
            )
        })
        .collect();
    let disks: Vec<_> = info
        .disks
        .iter()
        .map(|d| {
            (
                d.number,
                d.model.clone(),
                d.bus.clone(),
                d.media,
                d.size_bytes,
            )
        })
        .collect();
    let os = info
        .os
        .as_ref()
        .map(|o| (o.product_name.clone(), o.build, o.architecture));
    format!(
        "{cpu:?}\n{memory:?}\n{gpus:?}\n{disks:?}\n{os:?}\n{:?}\n{:?}",
        info.board, info.security
    )
}

#[test]
fn static_hardware_is_stable_between_snapshots() {
    let first = sysinfo::collect();
    let second = sysinfo::collect();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(static_parts(&first), static_parts(&second));
    let letters = |info: &SystemInfo| -> Vec<String> {
        info.volumes
            .iter()
            .filter(|v| v.kind == sysinfo::DriveKind::Fixed)
            .map(|v| v.letter.clone())
            .collect()
    };
    assert_eq!(letters(&first), letters(&second));
}

#[test]
fn single_sections_read_like_the_snapshot() {
    let os = sysinfo::os_info().expect("the Windows section");
    assert!(os.build >= 10240, "build {}", os.build);
    assert!(!os.product_name.is_empty());

    sysinfo::security_info().expect("the security section");

    let started = Instant::now();
    let (disks, volumes) = sysinfo::storage_devices().expect("the storage section");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    let windows = volumes
        .iter()
        .find(|v| v.system)
        .expect("the Windows volume is listed and marked");
    assert!(windows.ready, "{windows:?}");
    assert!(
        windows
            .disk_numbers
            .iter()
            .all(|n| disks.iter().any(|d| d.number == *n)),
        "{windows:?} {disks:?}"
    );
}
