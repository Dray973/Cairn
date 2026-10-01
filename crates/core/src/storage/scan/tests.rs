use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::walk::{walk, DirSource, Limits, ListError, LiveDirs, RawEntry, WalkSpec};
use super::*;
use crate::jobs::JobState;
use crate::storage::files::{
    verbatim, ATTR_COMPRESSED, ATTR_DIRECTORY, ATTR_REPARSE, ATTR_SYSTEM, TAG_WOF,
};
use crate::storage::speed::tests::test_host;

const MIB: u64 = 1 << 20;
/// A FILETIME in 2024.
const TICKS: i64 = 133_500_000_000_000_000;
const JUNCTION: u32 = 0xA000_0003;
const SYMLINK: u32 = 0xA000_000C;
const CLOUD: u32 = 0x9000_601A;

fn key(path: &Path) -> String {
    path.to_string_lossy()
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// Folders kept in memory.
#[derive(Default)]
struct MemDirs {
    dirs: HashMap<String, std::result::Result<Vec<RawEntry>, ListError>>,
    compressed: HashMap<String, u64>,
    listed: Mutex<Vec<String>>,
    /// After this many listings the flag is set.
    stop: Option<(usize, Arc<AtomicBool>)>,
}

impl MemDirs {
    fn dir(mut self, path: &str, entries: Vec<RawEntry>) -> MemDirs {
        self.dirs.insert(key(Path::new(path)), Ok(entries));
        self
    }

    fn fail(mut self, path: &str, error: ListError) -> MemDirs {
        self.dirs.insert(key(Path::new(path)), Err(error));
        self
    }

    fn packed(mut self, path: &str, size: u64) -> MemDirs {
        self.compressed.insert(key(Path::new(path)), size);
        self
    }

    fn listed(&self) -> Vec<String> {
        self.listed.lock().unwrap().clone()
    }
}

impl DirSource for MemDirs {
    fn list(
        &self,
        path: &Path,
        visit: &mut dyn FnMut(RawEntry) -> bool,
    ) -> std::result::Result<(), ListError> {
        let count = {
            let mut listed = self.listed.lock().unwrap();
            listed.push(key(path));
            listed.len()
        };
        if let Some((after, flag)) = &self.stop {
            if count >= *after {
                flag.store(true, Ordering::SeqCst);
            }
        }
        match self.dirs.get(&key(path)) {
            None => Err(ListError::Gone),
            Some(Err(e)) => Err(e.clone()),
            Some(Ok(entries)) => {
                for entry in entries {
                    if !visit(entry.clone()) {
                        break;
                    }
                }
                Ok(())
            }
        }
    }

    fn compressed_size(&self, path: &Path) -> Option<u64> {
        self.compressed.get(&key(path)).copied()
    }
}

fn file(name: &str, logical: u64, allocated: u64) -> RawEntry {
    RawEntry {
        name: name.into(),
        attrs: 0x20,
        reparse_tag: 0,
        logical,
        allocated,
        file_id: None,
        modified: TICKS,
    }
}

fn folder(name: &str) -> RawEntry {
    RawEntry {
        name: name.into(),
        attrs: ATTR_DIRECTORY,
        reparse_tag: 0,
        logical: 0,
        allocated: 0,
        file_id: None,
        modified: TICKS,
    }
}

fn id(mut entry: RawEntry, n: u8) -> RawEntry {
    entry.file_id = Some([n; 16]);
    entry
}

fn reparse(mut entry: RawEntry, tag: u32) -> RawEntry {
    entry.attrs |= ATTR_REPARSE;
    entry.reparse_tag = tag;
    entry
}

fn attrs(mut entry: RawEntry, extra: u32) -> RawEntry {
    entry.attrs |= extra;
    entry
}

/// A small C:\ with every kind of entry the scanner treats specially.
fn sample() -> MemDirs {
    MemDirs::default()
        .dir(
            r"C:\",
            vec![
                folder("Users"),
                folder("Windows"),
                folder("$Recycle.Bin"),
                folder("Denied"),
                folder("Gone"),
                id(attrs(file("sys.bin", 2 * MIB, 2 * MIB), ATTR_SYSTEM), 8),
                id(
                    attrs(file("packed.bin", 8 * MIB, 8 * MIB), ATTR_COMPRESSED),
                    9,
                ),
                id(reparse(file("wof.bin", 2 * MIB, 2 * MIB), TAG_WOF), 10),
                reparse(file("link.txt", 0, 0), SYMLINK),
            ],
        )
        .dir(r"C:\Users", vec![folder("Test")])
        .dir(
            r"C:\Users\Test",
            vec![
                folder("Videos"),
                reparse(folder("Documents"), JUNCTION),
                reparse(folder("OneDrive"), CLOUD),
            ],
        )
        .dir(
            r"C:\Users\Test\Videos",
            vec![
                id(file("holiday.mp4", 4 * MIB, 4 * MIB), 1),
                id(file("holiday-link.mp4", 4 * MIB, 4 * MIB), 1),
                id(file("clip.mp4", 2 * MIB, 2 * MIB), 2),
                id(file("notes.txt", 1000, 4096), 3),
            ],
        )
        .dir(
            r"C:\Users\Test\OneDrive",
            vec![
                id(
                    reparse(attrs(file("cloud.docx", 3 * MIB, 0), 0x40_1220), CLOUD),
                    4,
                ),
                id(file("local.bin", 2 * MIB, 2 * MIB), 5),
            ],
        )
        .dir(
            r"C:\Windows",
            vec![id(file("big.dll", 2 * MIB, 2 * MIB), 6)],
        )
        .dir(
            r"C:\$Recycle.Bin",
            vec![id(file("old.bin", 2 * MIB, 2 * MIB), 7)],
        )
        .fail(r"C:\Denied", ListError::Denied)
        .fail(r"C:\Gone", ListError::Gone)
        .packed(r"C:\packed.bin", MIB)
        .packed(r"C:\wof.bin", 512 << 10)
}

fn facts(root: &Path, threads: u32) -> ScanFacts {
    ScanFacts {
        root: root.to_path_buf(),
        volume: "C:".to_string(),
        whole_volume: true,
        media: MediaKind::Ssd,
        threads,
        volume_size: Some(100 << 30),
        volume_used: Some(50 * MIB),
        elapsed_ms: 5,
        warnings: vec!["a warning".to_string()],
    }
}

fn scan_with(
    src: &MemDirs,
    root: &str,
    threads: u32,
    limits: Limits,
    cancel: &AtomicBool,
) -> ScanResult {
    let root = PathBuf::from(root);
    let name = root.display().to_string();
    let walked = walk(
        src,
        &WalkSpec {
            root: &root,
            root_name: &name,
            threads,
            windows_dir: Some(Path::new(r"C:\Windows")),
            limits,
        },
        cancel,
        &|_: &ScanProgress| {},
    );
    ScanResult::finish(walked, facts(&root, threads))
}

fn scan(src: &MemDirs, root: &str, threads: u32) -> ScanResult {
    scan_with(
        src,
        root,
        threads,
        Limits::default(),
        &AtomicBool::new(false),
    )
}

fn names(page: &ChildrenPage) -> Vec<&str> {
    page.children.iter().map(|r| r.name.as_str()).collect()
}

fn child<'a>(page: &'a ChildrenPage, name: &str) -> &'a TreeRow {
    page.children
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("{name} in {:?}", names(page)))
}

fn node_of(result: &ScanResult, parts: &[&str]) -> u32 {
    let mut node = 0u32;
    for part in parts {
        let page = result
            .children(node, SortOrder::Allocated, CHILDREN_LIMIT)
            .unwrap();
        node = child(&page, part).node.unwrap();
    }
    node
}

#[test]
fn totals_and_subtree_sums() {
    let result = scan(&sample(), r"C:\", 2);
    let s = result.summary();
    assert!(s.completed);
    assert_eq!(s.files, 11);
    assert_eq!(s.folders, 8);
    assert_eq!(s.logical_bytes, 27 * MIB + 1000);
    assert_eq!(s.allocated_bytes, 15 * MIB + (512 << 10) + 4096);
    assert_eq!(s.online_only_bytes, 3 * MIB);
    assert_eq!(s.online_only_files, 1);
    assert_eq!(s.hard_links_counted_once, 1);
    assert_eq!(s.links_skipped, 1);
    assert_eq!(s.denied_folders, 1);
    assert_eq!(s.unreadable_folders, 1);
    assert_eq!(s.not_reached_bytes, Some(50 * MIB - s.allocated_bytes));
    assert!(!s.id_limit_reached && !s.node_limit_reached && !s.big_file_limit_reached);
    let root = result.root_row();
    assert_eq!(root.kind, TreeRowKind::Folder);
    assert_eq!(root.node, Some(0));
    assert_eq!(root.name, r"C:\");
    assert_eq!(root.allocated, s.allocated_bytes);
    assert!(root.has_children);
    let users = node_of(&result, &["Users"]);
    let users = result
        .children(users, SortOrder::Allocated, 10)
        .unwrap()
        .node;
    assert_eq!(users.allocated, 8 * MIB + 4096);
    assert_eq!(users.logical, 11 * MIB + 1000);
    assert_eq!(users.files, 5);
    assert_eq!(users.folders, 3);
    assert_eq!(result.warnings(), ["a warning"]);
}

#[test]
fn a_hard_link_is_counted_once() {
    let result = scan(&sample(), r"C:\", 1);
    let videos = node_of(&result, &["Users", "Test", "Videos"]);
    let page = result.children(videos, SortOrder::Allocated, 10).unwrap();
    assert_eq!(names(&page), ["holiday.mp4", "clip.mp4", "1 smaller file"]);
    assert_eq!(page.node.files, 3);
    assert_eq!(page.node.logical, 6 * MIB + 1000);
}

#[test]
fn links_are_not_followed_and_cloud_folders_are_entered() {
    let src = sample();
    let result = scan(&src, r"C:\", 3);
    let listed = src.listed();
    assert!(
        !listed.iter().any(|p| p.ends_with("documents")),
        "{listed:?}"
    );
    assert!(listed.iter().any(|p| p.ends_with("onedrive")), "{listed:?}");
    let test = node_of(&result, &["Users", "Test"]);
    let page = result.children(test, SortOrder::Allocated, 10).unwrap();
    let documents = child(&page, "Documents");
    assert_eq!(documents.kind, TreeRowKind::Link);
    assert!(!documents.has_children);
    assert_eq!((documents.logical, documents.allocated), (0, 0));
    let empty = result
        .children(documents.node.unwrap(), SortOrder::Allocated, 10)
        .unwrap();
    assert!(empty.children.is_empty());
    let onedrive = child(&page, "OneDrive");
    assert_eq!(onedrive.kind, TreeRowKind::Folder);
    // Online-only files count toward Size, not Size on disk.
    assert_eq!(onedrive.logical, 5 * MIB);
    assert_eq!(onedrive.allocated, 2 * MIB);
    assert_eq!(onedrive.online_only, 3 * MIB);
}

#[test]
fn packed_files_use_their_compressed_size() {
    let result = scan(&sample(), r"C:\", 2);
    let page = result.children(0, SortOrder::Logical, 20).unwrap();
    let packed = child(&page, "packed.bin");
    assert_eq!((packed.logical, packed.allocated), (8 * MIB, MIB));
    let wof = child(&page, "wof.bin");
    assert_eq!((wof.logical, wof.allocated), (2 * MIB, 512 << 10));
}

#[test]
fn denied_and_gone_folders_are_flagged() {
    let result = scan(&sample(), r"C:\", 2);
    let page = result.children(0, SortOrder::Allocated, 20).unwrap();
    let denied = child(&page, "Denied");
    assert!(denied.denied && denied.error.is_none() && !denied.has_children);
    let gone = child(&page, "Gone");
    assert!(!gone.denied);
    assert_eq!(
        gone.error.as_deref(),
        Some("it was removed while Cairn scanned")
    );
}

#[test]
fn children_are_sorted_and_cut_with_a_more_row() {
    let result = scan(&sample(), r"C:\", 4);
    let page = result
        .children(0, SortOrder::Allocated, CHILDREN_LIMIT)
        .unwrap();
    assert_eq!(page.total, 9);
    assert_eq!(page.order, SortOrder::Allocated);
    assert_eq!(
        names(&page),
        [
            "Users",
            "$Recycle.Bin",
            "Windows",
            "sys.bin",
            "packed.bin",
            "wof.bin",
            "1 smaller file",
            "Denied",
            "Gone"
        ]
    );
    let small = child(&page, "1 smaller file");
    assert_eq!(small.kind, TreeRowKind::SmallFiles);
    assert_eq!(small.count, 1);
    assert!(small.path.is_none() && small.node.is_none());

    let page = result
        .children(0, SortOrder::Logical, CHILDREN_LIMIT)
        .unwrap();
    assert_eq!(
        names(&page),
        [
            "Users",
            "packed.bin",
            "$Recycle.Bin",
            "Windows",
            "sys.bin",
            "wof.bin",
            "1 smaller file",
            "Denied",
            "Gone"
        ]
    );

    let page = result.children(0, SortOrder::Allocated, 4).unwrap();
    assert_eq!(page.total, 9);
    assert_eq!(
        names(&page),
        ["Users", "$Recycle.Bin", "Windows", "6 more items"]
    );
    let more = &page.children[3];
    assert_eq!(more.kind, TreeRowKind::More);
    assert_eq!(more.count, 6);
    assert_eq!(more.allocated, 3 * MIB + (512 << 10));
    assert_eq!(more.logical, 12 * MIB);
    assert_eq!(more.files, 4);
    assert_eq!(more.folders, 2);
    // Limits are kept within 1 and the largest page.
    assert_eq!(
        result
            .children(0, SortOrder::Allocated, 0)
            .unwrap()
            .children
            .len(),
        1
    );
    assert!(result.children(9999, SortOrder::Allocated, 10).is_none());
}

#[test]
fn file_rows_carry_their_path_and_date() {
    let result = scan(&sample(), r"C:\", 2);
    let videos = node_of(&result, &["Users", "Test", "Videos"]);
    assert_eq!(
        result.path_of(videos).unwrap(),
        PathBuf::from(r"C:\Users\Test\Videos")
    );
    assert_eq!(result.path_of(0).unwrap(), PathBuf::from(r"C:\"));
    assert!(result.path_of(9999).is_none());
    let page = result.children(videos, SortOrder::Allocated, 10).unwrap();
    let holiday = child(&page, "holiday.mp4");
    assert_eq!(holiday.kind, TreeRowKind::File);
    assert_eq!(
        holiday.path.as_deref(),
        Some(r"C:\Users\Test\Videos\holiday.mp4")
    );
    assert_eq!(
        holiday.modified,
        crate::storage::files::ticks_rfc3339(TICKS)
    );
    assert_eq!(holiday.files, 1);
    assert_eq!(page.node.path.as_deref(), Some(r"C:\Users\Test\Videos"));
}

#[test]
fn the_largest_files_by_both_orders() {
    let result = scan(&sample(), r"C:\", 2);
    let by_disk = result.largest_files(SortOrder::Allocated);
    assert_eq!(by_disk.len(), 9);
    assert_eq!(by_disk[0].name, "holiday.mp4");
    assert!(by_disk.windows(2).all(|w| w[0].allocated >= w[1].allocated));
    let by_size = result.largest_files(SortOrder::Logical);
    let top: Vec<&str> = by_size.iter().take(3).map(|r| r.name.as_str()).collect();
    assert_eq!(top, ["packed.bin", "holiday.mp4", "cloud.docx"]);
    assert_eq!(by_size[2].online_only, 3 * MIB);
}

#[test]
fn candidates_leave_out_system_windows_recycle_and_online_files() {
    let result = scan(&sample(), r"C:\", 2);
    let mut names: Vec<String> = result
        .candidates
        .iter()
        .map(|&i| result.big[i as usize].name.to_string())
        .collect();
    names.sort();
    // holiday.mp4 and packed.bin have sizes no other candidate shares.
    assert_eq!(names, ["clip.mp4", "local.bin", "wof.bin"]);
    assert_eq!(result.summary().duplicate_candidates, 3);
    let clip = result.big.iter().find(|f| &*f.name == "clip.mp4").unwrap();
    assert_eq!(
        result.file_path(clip),
        PathBuf::from(r"C:\Users\Test\Videos\clip.mp4")
    );
    assert!(clip.id_hash.is_some());
}

#[test]
fn a_stop_mid_walk_keeps_what_was_scanned() {
    let flag = Arc::new(AtomicBool::new(false));
    let mut src = MemDirs::default();
    let mut root = Vec::new();
    for i in 0..100 {
        root.push(folder(&format!("d{i}")));
        src = src.dir(&format!(r"C:\Many\d{i}"), vec![file("f.bin", 10, 4096)]);
    }
    src = src.dir(r"C:\Many", root);
    src.stop = Some((10, Arc::clone(&flag)));
    let result = scan_with(&src, r"C:\Many", 1, Limits::default(), &flag);
    let s = result.summary();
    assert!(!s.completed);
    assert!(s.files < 100, "{s:?}");
    assert!(src.listed().len() < 20);
    // Every folder is still in the tree, scanned or not.
    assert_eq!(
        result
            .children(0, SortOrder::Allocated, 5000)
            .unwrap()
            .total,
        100
    );
}

fn generated() -> MemDirs {
    let mut src = MemDirs::default();
    let mut top = Vec::new();
    for a in 0..20u64 {
        top.push(folder(&format!("a{a}")));
        let mut mid = Vec::new();
        for b in 0..10u64 {
            mid.push(folder(&format!("b{b}")));
            let files = (0..5u64)
                .map(|c| {
                    let size = (a * 131 + b * 17 + c) * 40_000;
                    file(&format!("f{c}.bin"), size, size.div_ceil(4096) * 4096)
                })
                .collect();
            src = src.dir(&format!(r"C:\Gen\a{a}\b{b}"), files);
        }
        mid.push(file("top.bin", a * MIB, a * MIB));
        src = src.dir(&format!(r"C:\Gen\a{a}"), mid);
    }
    src.dir(r"C:\Gen", top)
}

#[test]
fn one_and_four_threads_give_the_same_totals() {
    let src = generated();
    let one = scan(&src, r"C:\Gen", 1);
    let four = scan(&generated(), r"C:\Gen", 4);
    let (a, b) = (one.summary(), four.summary());
    assert_eq!(
        (
            a.files,
            a.folders,
            a.logical_bytes,
            a.allocated_bytes,
            a.duplicate_candidates
        ),
        (
            b.files,
            b.folders,
            b.logical_bytes,
            b.allocated_bytes,
            b.duplicate_candidates
        )
    );
    assert_eq!(a.files, 20 * 10 * 5 + 20);
    assert_eq!(a.folders, 20 + 200);
    assert_eq!(one.root_row().allocated, four.root_row().allocated);
    let page_one = one.children(0, SortOrder::Logical, 100).unwrap();
    let page_four = four.children(0, SortOrder::Logical, 100).unwrap();
    assert_eq!(names(&page_one), names(&page_four));
}

#[test]
fn the_caps_end_with_flags_not_failures() {
    let src = MemDirs::default().dir(
        r"C:\Caps",
        vec![
            folder("a"),
            folder("b"),
            folder("c"),
            id(file("f1.bin", 2 * MIB, 2 * MIB), 1),
            id(file("f2.bin", 2 * MIB, 2 * MIB), 2),
            id(file("f3.bin", 2 * MIB, 2 * MIB), 2),
        ],
    );
    let limits = Limits {
        max_nodes: 3,
        max_big_files: 1,
        max_file_ids: 1,
    };
    let result = scan_with(&src, r"C:\Caps", 1, limits, &AtomicBool::new(false));
    let s = result.summary();
    assert!(s.node_limit_reached && s.big_file_limit_reached && s.id_limit_reached);
    assert!(s.completed);
    // Past the id cap every file counts, so nothing is dropped as a hard link.
    assert_eq!(s.files, 3);
    assert_eq!(s.hard_links_counted_once, 0);
    let page = result.children(0, SortOrder::Allocated, 10).unwrap();
    assert_eq!(names(&page), ["2 smaller files", "f1.bin", "a", "b"]);
}

#[test]
fn tree_rows_and_the_result_json_match_the_contract() {
    let result = scan(&sample(), r"C:\", 2);
    let json = result.result_json(42);
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "job_id",
            "kind",
            "largest_files",
            "largest_files_by_size",
            "root",
            "summary",
            "warnings"
        ]
    );
    assert_eq!(json["kind"], "scan");
    assert_eq!(json["job_id"], 42);
    let mut row_keys: Vec<&str> = json["root"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    row_keys.sort_unstable();
    assert_eq!(
        row_keys,
        [
            "allocated",
            "count",
            "denied",
            "error",
            "files",
            "folders",
            "has_children",
            "kind",
            "logical",
            "modified",
            "name",
            "node",
            "online_only",
            "path"
        ]
    );
    assert_eq!(json["summary"].as_object().unwrap().len(), 23);
    let progress = serde_json::to_value(ScanProgress {
        files: 1,
        folders: 2,
        logical_bytes: 3,
        allocated_bytes: 4,
        denied_folders: 5,
        current: r"C:\Users".to_string(),
    })
    .unwrap();
    let mut progress_keys: Vec<&str> = progress
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    progress_keys.sort_unstable();
    assert_eq!(
        progress_keys,
        [
            "allocated_bytes",
            "current",
            "denied_folders",
            "files",
            "folders",
            "logical_bytes"
        ]
    );
    let page = serde_json::to_value(result.children(0, SortOrder::Allocated, 3).unwrap()).unwrap();
    assert_eq!(page["order"], "allocated");
    assert_eq!(page["children"][2]["kind"], "more");
    assert_eq!(SortOrder::parse("logical"), Some(SortOrder::Logical));
    assert_eq!(SortOrder::parse("size"), None);
}

// ───────────────────────────── Live folders ─────────────────────────────

fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run mklink");
    assert!(status.success(), "mklink /J failed");
}

fn ntfs(path: &Path) -> bool {
    let letter = &path.to_string_lossy()[..2];
    crate::storage::volumes::file_system(&crate::storage::volumes::root_of(letter))
        .eq_ignore_ascii_case("NTFS")
}

#[test]
fn live_folders_are_listed_with_links_and_hard_links() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = root.join("a");
    std::fs::create_dir_all(a.join("b")).unwrap();
    std::fs::write(a.join("x.bin"), vec![1u8; 2 << 20]).unwrap();
    std::fs::hard_link(a.join("x.bin"), a.join("b").join("x-link.bin")).unwrap();
    junction(&root.join("j"), &a);
    let many = root.join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..5000 {
        std::fs::write(
            many.join(format!("entry-with-a-longish-name-{i:05}.txt")),
            b"x",
        )
        .unwrap();
    }

    let src = LiveDirs { ids: true };
    let mut entries = Vec::new();
    src.list(&a, &mut |e| {
        entries.push(e);
        true
    })
    .unwrap();
    let x = entries.iter().find(|e| e.name == "x.bin").unwrap();
    assert_eq!(x.logical, 2 << 20);
    assert!(x.modified > 0);
    if ntfs(root) {
        assert!(x.file_id.is_some());
    }
    assert!(entries
        .iter()
        .any(|e| e.name == "b" && e.attrs & ATTR_DIRECTORY != 0));
    assert!(!entries.iter().any(|e| e.name == "." || e.name == ".."));
    let mut count = 0;
    src.list(&many, &mut |_| {
        count += 1;
        true
    })
    .unwrap();
    assert_eq!(count, 5000);
    let mut seen = 0;
    src.list(&many, &mut |_| {
        seen += 1;
        seen < 10
    })
    .unwrap();
    assert_eq!(seen, 10);
    assert_eq!(
        src.list(&root.join("j"), &mut |_| true),
        Err(ListError::NotPlain)
    );
    assert_eq!(
        src.list(&root.join("missing"), &mut |_| true),
        Err(ListError::Gone)
    );
    assert_eq!(src.compressed_size(&a.join("x.bin")), Some(2 << 20));
    let plain = LiveDirs { ids: false };
    let mut full = Vec::new();
    plain
        .list(&a, &mut |e| {
            full.push(e);
            true
        })
        .unwrap();
    assert!(full.iter().all(|e| e.file_id.is_none()));
    assert_eq!(full.len(), entries.len());

    let name = root.display().to_string();
    let walked = walk(
        &src,
        &WalkSpec {
            root,
            root_name: &name,
            threads: 4,
            windows_dir: None,
            limits: Limits::default(),
        },
        &AtomicBool::new(false),
        &|_: &ScanProgress| {},
    );
    assert!(walked.completed);
    assert_eq!(walked.links_skipped, 1);
    if ntfs(root) {
        assert_eq!(walked.hard_links_counted_once, 1);
    }
    let result = ScanResult::finish(walked, facts(root, 4));
    assert_eq!(result.summary().folders, 3);
    let expected_files = if ntfs(root) { 5001 } else { 5002 };
    assert_eq!(result.summary().files, expected_files);
    let page = result.children(0, SortOrder::Allocated, 10).unwrap();
    assert_eq!(child(&page, "j").kind, TreeRowKind::Link);
}

#[test]
fn live_folders_named_with_a_trailing_dot_or_space_are_entered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Names a plain path loses to Win32 normalization; they exist only through `\\?\` paths
    // (as WSL and SMB clients create them).
    let space = root.join("space ");
    let trail = root.join("trail.");
    let inner = trail.join("inner");
    for folder in [&space, &trail, &inner] {
        std::fs::create_dir(verbatim(folder)).unwrap();
    }
    std::fs::write(verbatim(&space.join("x.bin")), vec![1u8; 2 << 20]).unwrap();
    std::fs::write(verbatim(&inner.join("y.bin")), vec![2u8; 2 << 20]).unwrap();

    let src = LiveDirs { ids: true };
    let mut listed = Vec::new();
    src.list(&space, &mut |e| {
        listed.push(e.name);
        true
    })
    .unwrap();
    assert_eq!(listed, ["x.bin"]);

    let name = root.display().to_string();
    let walked = walk(
        &src,
        &WalkSpec {
            root,
            root_name: &name,
            threads: 2,
            windows_dir: None,
            limits: Limits::default(),
        },
        &AtomicBool::new(false),
        &|_: &ScanProgress| {},
    );
    assert!(walked.completed);
    let result = ScanResult::finish(walked, facts(root, 2));
    let summary = result.summary();
    assert_eq!(summary.folders, 3);
    assert_eq!(summary.files, 2);
    assert_eq!(summary.logical_bytes, 4 << 20);
    assert_eq!(summary.unreadable_folders, 0);
    let page = result.children(0, SortOrder::Allocated, 10).unwrap();
    for folder in ["space ", "trail."] {
        let row = child(&page, folder);
        assert_eq!(row.kind, TreeRowKind::Folder, "{folder:?}");
        assert_eq!(row.error, None, "{folder:?}");
        assert_eq!(row.files, 1, "{folder:?}");
        assert_eq!(row.logical, 2 << 20, "{folder:?}");
    }
}

// ───────────────────────────── Plan and job ─────────────────────────────

fn plan(host: &JobHost, path: &Path) -> ScanPlan {
    let request = ScanRequest {
        path: path.to_path_buf(),
    };
    let (plan, job) = plan_or_start_scan(host, &request, true).unwrap();
    assert!(job.is_none());
    plan
}

#[test]
fn paths_are_normalized_to_local_drives() {
    assert_eq!(
        local_path(Path::new(r"c:\a\.\b\..\c")),
        Some(PathBuf::from(r"C:\a\c"))
    );
    assert_eq!(
        local_path(Path::new(r"\\?\D:\x")),
        Some(PathBuf::from(r"D:\x"))
    );
    assert_eq!(local_path(Path::new(r"C:\")), Some(PathBuf::from(r"C:\")));
    // A plain path names what Windows opens for it; a `\\?\` path keeps its names.
    assert_eq!(
        local_path(Path::new(r"C:\a.\b. ")),
        Some(PathBuf::from(r"C:\a\b"))
    );
    assert_eq!(
        local_path(Path::new(r"\\?\C:\a.\b. ")),
        Some(PathBuf::from(r"C:\a.\b. "))
    );
    assert_eq!(local_path(Path::new(r"relative\path")), None);
    assert_eq!(local_path(Path::new(r"C:relative")), None);
    assert_eq!(local_path(Path::new(r"\\server\share\x")), None);
    assert_eq!(local_path(Path::new(r"\\?\UNC\server\share")), None);
    assert_eq!(local_path(Path::new(r"\\.\PhysicalDrive0")), None);
    assert_eq!(threads_for(MediaKind::Hdd), 2);
    assert!((2..=8).contains(&threads_for(MediaKind::Ssd)));
}

#[test]
fn plans_say_why_a_scan_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(dir.path());
    assert_eq!(
        plan(&host, Path::new(r"\\server\share"))
            .blocked_reason
            .as_deref(),
        Some(NOT_LOCAL_TEXT)
    );
    let missing = dir.path().join("missing");
    assert_eq!(
        plan(&host, &missing).blocked_reason,
        Some(format!("{} doesn't exist", missing.display()))
    );
    let a_file = dir.path().join("f.txt");
    std::fs::write(&a_file, b"x").unwrap();
    assert_eq!(
        plan(&host, &a_file).blocked_reason,
        Some(format!("{} is a file; choose a folder", a_file.display()))
    );
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = dir.path().join("link");
    junction(&link, &target);
    assert_eq!(
        plan(&host, &link).blocked_reason,
        Some(format!(
            "{} is a link to another folder; scan the folder it points to",
            link.display()
        ))
    );
    let ok = plan(&host, dir.path());
    assert_eq!(ok.blocked_reason, None, "{ok:?}");
    assert!(!ok.whole_volume);
    assert_eq!(
        ok.volume,
        dir.path().to_string_lossy()[..2].to_ascii_uppercase()
    );
    assert!(ok.threads >= 2);
    assert_eq!(ok.elevated, crate::is_elevated());
    assert_eq!(
        ok.notes.contains(&NOT_ELEVATED_NOTE.to_string()),
        !crate::is_elevated()
    );
    assert_eq!(scan_title(&ok), format!("Scan of {}", ok.root));
    let json = serde_json::to_value(&ok).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 9);
    // A refused start writes nothing and starts nothing.
    let refused = plan_or_start_scan(&host, &ScanRequest { path: missing }, false);
    assert!(refused.is_err());
    assert!(host.jobs().is_empty());
}

#[test]
fn a_running_job_blocks_a_scan() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(dir.path());
    let (release, wait) = std::sync::mpsc::channel::<()>();
    host.start(
        JobSpec {
            kind: crate::storage::KIND_SPEED_TEST,
            title: "Speed test of Z:".to_string(),
            command_line: String::new(),
            cancellable: true,
            audit: None,
            needs_journal: false,
            log: false,
        },
        || panic!("no journal"),
        Box::new(move |_| {
            let _ = wait.recv();
            WorkEnd {
                state: JobState::Succeeded,
                summary: String::new(),
                hint: None,
                restart_required: false,
                audit_detail: None,
            }
        }),
    )
    .unwrap();
    assert_eq!(
        plan(&host, dir.path()).blocked_reason.as_deref(),
        Some("Speed test of Z: is running; wait for it to finish or stop it.")
    );
    release.send(()).unwrap();
}

#[test]
fn a_scan_job_publishes_its_tree() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(dir.path());
    let root = dir.path().join("scan");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("big.bin"), vec![3u8; 3 << 20]).unwrap();
    std::fs::write(root.join("small.txt"), b"hello").unwrap();
    let (plan, job) =
        plan_or_start_scan(&host, &ScanRequest { path: root.clone() }, false).unwrap();
    let job = job.unwrap();
    assert_eq!(job.kind, KIND_SCAN);
    assert_eq!(job.title, scan_title(&plan));
    assert!(!job.logged);
    let end = host.wait(job.id, Duration::from_secs(30)).unwrap();
    assert_eq!(end.state, JobState::Succeeded, "{end:?}");
    assert!(end.has_result);
    // The finished job keeps the scan's last progress block; the other kinds' blocks are null.
    let detail = end.detail.unwrap();
    assert_eq!(detail["phase"], "done");
    assert_eq!(detail["scan"]["files"], 2);
    assert_eq!(detail["scan"]["folders"], 1);
    assert_eq!(detail["scan"]["current"], root.display().to_string());
    assert!(detail["speed"].is_null() && detail["duplicates"].is_null());
    let (_, json) = host.result(job.id, 0).unwrap();
    assert_eq!(json["kind"], "scan");
    assert_eq!(json["job_id"], job.id.0);
    assert_eq!(json["summary"]["files"], 2);
    assert_eq!(json["summary"]["folders"], 1);
    assert_eq!(json["summary"]["completed"], true);
    assert_eq!(json["largest_files"][0]["name"], "big.bin");
    let typed = host.typed::<ScanResult>(job.id).unwrap();
    let page = typed.children(0, SortOrder::Allocated, 10).unwrap();
    assert_eq!(names(&page), ["sub", "1 smaller file"]);
    assert_eq!(typed.root_path(), root.as_path());
    assert!(host.running().is_none());
}

#[test]
fn a_typed_folder_is_scanned_as_windows_resolves_it() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(dir.path());
    let videos = dir.path().join("Videos");
    std::fs::create_dir_all(videos.join("sub")).unwrap();
    std::fs::write(videos.join("sub").join("a.bin"), vec![1u8; 2 << 20]).unwrap();
    // A folder whose name ends in a dot, named exactly through its `\\?\` path.
    let trail = dir.path().join("trail.");
    std::fs::create_dir(verbatim(&trail)).unwrap();
    std::fs::write(verbatim(&trail.join("b.bin")), vec![2u8; 2 << 20]).unwrap();
    for (typed, root) in [
        (format!("{}.", videos.display()), &videos),
        (format!("{} ", videos.display()), &videos),
        (verbatim(&trail).display().to_string(), &trail),
    ] {
        let request = ScanRequest {
            path: PathBuf::from(&typed),
        };
        let (plan, job) = plan_or_start_scan(&host, &request, false).unwrap();
        assert_eq!(plan.root, root.display().to_string(), "{typed:?}");
        let end = host.wait(job.unwrap().id, Duration::from_secs(30)).unwrap();
        assert_eq!(end.state, JobState::Succeeded, "{end:?}");
        let result = host.typed::<ScanResult>(end.id).unwrap();
        assert_eq!(result.root_path(), root.as_path(), "{typed:?}");
        assert_eq!(result.root_row().error, None, "{typed:?}");
        assert_eq!(result.summary().files, 1, "{typed:?}");
        assert_eq!(result.summary().unreadable_folders, 0, "{typed:?}");
    }
}

#[test]
fn a_stopped_scan_is_cancelled_with_partial_results() {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(dir.path());
    let flag = Arc::new(AtomicBool::new(false));

    struct Slow(Arc<AtomicBool>);
    impl DirSource for Slow {
        fn list(
            &self,
            _path: &Path,
            visit: &mut dyn FnMut(RawEntry) -> bool,
        ) -> std::result::Result<(), ListError> {
            std::thread::sleep(Duration::from_millis(20));
            self.0.store(true, Ordering::SeqCst);
            for i in 0..3 {
                visit(folder(&format!("d{i}")));
            }
            Ok(())
        }
        fn compressed_size(&self, _path: &Path) -> Option<u64> {
            None
        }
    }
    struct Endless(Arc<AtomicBool>);
    impl SourceFactory for Endless {
        fn source(&self, _ids: bool) -> Box<dyn DirSource> {
            Box::new(Slow(Arc::clone(&self.0)))
        }
    }

    let request = ScanRequest {
        path: dir.path().to_path_buf(),
    };
    let (_, job) =
        plan_or_start_scan_with(&host, &request, false, Arc::new(Endless(Arc::clone(&flag))))
            .unwrap();
    let id = job.unwrap().id;
    while !flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(host.cancel(id).unwrap());
    let end = host.wait(id, Duration::from_secs(30)).unwrap();
    assert_eq!(end.state, JobState::Cancelled);
    let detail = end.detail.unwrap();
    assert_eq!(detail["phase"], "done");
    assert!(detail["scan"]["folders"].is_u64(), "{detail}");
    let (_, json) = host.result(id, 0).unwrap();
    assert_eq!(json["summary"]["completed"], false);
    assert!(host.typed::<ScanResult>(id).is_some());
}
