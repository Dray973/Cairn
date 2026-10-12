//! Every catalog tweak has a verification record, `docs/verification/<id>.md`, unless its id
//! is in [`PENDING`]; `docs/verification/README.md` describes the format and the rules checked
//! here. Records are parsed line by line. The counts the repository's README gives (tweaks,
//! checkup checks, apps of the install list) are checked against the code as well. Read-only:
//! the tests read the records, the catalog, `README.md` and `scripts/verification/targets.json`,
//! and write only fixture records into temporary folders.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use optimizer_core::debloat::catalog::{
    Action, Build, Category, Editions, RegistryAction, Requirement, RestartNeed, Risk, Topic,
    Tweak, WindowsRange, ANY_WINDOWS, TWEAKS,
};
use optimizer_core::debloat::requirements::servicing_family;
use optimizer_core::health::CheckId;
use optimizer_core::updates::apps::DEFAULT_APPS;
use serde_json::{json, Value};

/// Tweaks whose record is not written yet. The list only shrinks, and it is empty in a release
/// build: with `CAIRN_RELEASE=1` the records test fails while it lists any id.
const PENDING: &[&str] = &[];

/// Tweaks that no PC available for checks can run as offered: the Copilot policy is for
/// Windows 11 23H2 and earlier, Recall needs a Copilot+ PC, and the Office policies need
/// Microsoft 365 Apps for business or enterprise (consumer Microsoft 365 ignores them). Their
/// records show the scan's "not offered" state on Windows 11 24H2 or later and repeat what
/// Microsoft documents; they are never recommended.
const NO_MACHINE_IN_RANGE: &[&str] = &[
    "privacy.copilot",
    "privacy.recall",
    "privacy.office_telemetry",
    "privacy.office_connected_experiences",
    "privacy.office_cloud_content",
];

/// Tweaks whose state every PC available for checks already had, so applying them wrote
/// nothing and neither the apply nor its undo could be exercised. Their records hold a
/// `documented` check of what that state showed, and they are never recommended; the list
/// shrinks when a PC in the other state becomes available.
const ALREADY_IN_STATE: &[&str] = &["gaming.sticky_keys"];

/// Tweaks whose check found an Undo that was not exact because of a fault fixed since: their
/// records keep that check (`Undo exact` = `no`) until a check of the fixed build shows an exact
/// undo, which moves them to [`UNDO_CHECKED_AGAIN`] (or off both lists when that check was also
/// kept after a restart). The list only shrinks, and it is empty in a release build: with
/// `CAIRN_RELEASE=1` the records test fails while it lists any id.
const RECHECK: &[&str] = &[];

/// Tweaks whose check kept them after the restart but found an Undo that was not exact because
/// of a fault fixed since, and whose later check of the fixed build, made without a restart,
/// undid them exactly. Their records need both: a `works` check kept after the restart (its Undo
/// may be inexact) and a `works` check no older than it with an exact Undo.
const UNDO_CHECKED_AGAIN: &[&str] = &[
    "privacy.diagtrack_service",
    "performance.sysmain",
    "performance.link_tracking",
];

const RESULTS: &[&str] = &[
    "works",
    "partial",
    "no-effect",
    "broken",
    "documented",
    "not-offered",
];
const EVIDENCE: &[&str] = &["functional", "ui-state", "none"];
const YES_NO: &[&str] = &["yes", "no", "n/a"];
const EDITIONS: &[&str] = &["Home", "Pro", "Enterprise", "Education"];
const CHECKS_HEADER: [&str; 9] = [
    "Date",
    "Version",
    "Edition",
    "Build",
    "Result",
    "Evidence",
    "Kept after restart",
    "Undo exact",
    "Notes",
];
/// The first build of Windows 11 24H2's servicing family.
const FAMILY_24H2: u32 = 26100;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn records_dir() -> PathBuf {
    repo_root().join("docs").join("verification")
}

fn release_build() -> bool {
    std::env::var("CAIRN_RELEASE").is_ok_and(|v| v == "1")
}

/// What one run of the records check compares against.
struct Expected<'a> {
    tweaks: Vec<&'a Tweak>,
    pending: &'a [&'a str],
    no_machine: &'a [&'a str],
    in_state: &'a [&'a str],
    recheck: &'a [&'a str],
    undo_again: &'a [&'a str],
    release: bool,
}

impl Expected<'static> {
    fn catalog(release: bool) -> Self {
        Expected {
            tweaks: TWEAKS.iter().collect(),
            pending: PENDING,
            no_machine: NO_MACHINE_IN_RANGE,
            in_state: ALREADY_IN_STATE,
            recheck: RECHECK,
            undo_again: UNDO_CHECKED_AGAIN,
            release,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Check {
    line: usize,
    date: String,
    version: String,
    edition: String,
    build: String,
    result: String,
    evidence: String,
    kept: String,
    undo: String,
}

#[derive(Debug, Default)]
struct Record {
    heading: Option<String>,
    sources: Vec<String>,
    checks: Vec<Check>,
}

/// The cells of a Markdown table row, trimmed; `None` for a line that is not a row.
fn cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let inner = line.strip_prefix('|')?.strip_suffix('|')?;
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

fn is_separator(row: &[String]) -> bool {
    row.iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'))
}

/// Parses a record; problems are reported with `name` and the line number.
fn parse(name: &str, text: &str, problems: &mut Vec<String>) -> Record {
    let mut record = Record::default();
    let mut in_checks = false;
    let mut header_seen = false;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if record.heading.is_none() {
            let heading = line.strip_prefix("# ").map_or("", str::trim);
            if heading.is_empty() {
                problems.push(format!("{name}:{number}: the first line is not '# <id>'"));
            }
            record.heading = Some(heading.to_string());
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            in_checks = heading.trim() == "Checks";
            header_seen = false;
            continue;
        }
        let Some(row) = cells(line) else {
            continue;
        };
        if is_separator(&row) {
            continue;
        }
        if !in_checks {
            if row.len() != 2 {
                problems.push(format!(
                    "{name}:{number}: a field row has {} cells, not 2",
                    row.len()
                ));
                continue;
            }
            if row[0] == "Source" {
                record.sources.push(row[1].clone());
            }
            continue;
        }
        if !header_seen {
            header_seen = true;
            if row != CHECKS_HEADER {
                problems.push(format!(
                    "{name}:{number}: the checks header is not '| {} |'",
                    CHECKS_HEADER.join(" | ")
                ));
            }
            continue;
        }
        if row.len() != CHECKS_HEADER.len() {
            problems.push(format!(
                "{name}:{number}: a check has {} cells, not {}",
                row.len(),
                CHECKS_HEADER.len()
            ));
            continue;
        }
        let check = Check {
            line: number,
            date: row[0].clone(),
            version: row[1].clone(),
            edition: row[2].clone(),
            build: row[3].clone(),
            result: row[4].clone(),
            evidence: row[5].clone(),
            kept: row[6].clone(),
            undo: row[7].clone(),
        };
        for problem in check_values(&check) {
            problems.push(format!("{name}:{number}: {problem}"));
        }
        record.checks.push(check);
    }
    record
}

fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// `CurrentBuild.UBR`: five digits, a dot and the revision.
fn parse_build(text: &str) -> Option<Build> {
    let (number, revision) = text.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if number.len() != 5 || !digits(number) || !digits(revision) {
        return None;
    }
    Some(Build::new(number.parse().ok()?, revision.parse().ok()?))
}

fn check_values(check: &Check) -> Vec<String> {
    let mut problems = Vec::new();
    if !is_date(&check.date) {
        problems.push(format!("date '{}' is not YYYY-MM-DD", check.date));
    }
    if check.version.is_empty() {
        problems.push("the version is empty".to_string());
    }
    if !EDITIONS.contains(&check.edition.as_str()) {
        problems.push(format!(
            "edition '{}' is not one of {EDITIONS:?}",
            check.edition
        ));
    }
    if parse_build(&check.build).is_none() {
        problems.push(format!("build '{}' is not like 26300.9550", check.build));
    }
    if !RESULTS.contains(&check.result.as_str()) {
        problems.push(format!(
            "result '{}' is not one of {RESULTS:?}",
            check.result
        ));
    }
    if !EVIDENCE.contains(&check.evidence.as_str()) {
        problems.push(format!(
            "evidence '{}' is not one of {EVIDENCE:?}",
            check.evidence
        ));
    }
    for (column, value) in [
        ("kept after restart", &check.kept),
        ("undo exact", &check.undo),
    ] {
        if !YES_NO.contains(&value.as_str()) {
            problems.push(format!("{column} '{value}' is not one of {YES_NO:?}"));
        }
    }
    problems
}

/// Whether a PC on `build` runs a Windows version `range` is meant for: a floor applies to
/// its own servicing family and needs both the build number and the revision; a family newer
/// than every floor is in the range, an older one is not.
fn in_range(range: &WindowsRange, build: Build) -> bool {
    let family = servicing_family(build.number);
    if range.through.is_some_and(|last| family > last) {
        return false;
    }
    if range.from.is_empty() {
        return true;
    }
    match range
        .from
        .iter()
        .find(|floor| servicing_family(floor.number) == family)
    {
        Some(floor) => build.number >= floor.number && build.revision >= floor.revision,
        None => range
            .from
            .iter()
            .all(|floor| servicing_family(floor.number) < family),
    }
}

fn edition_offered(editions: Editions, edition: &str) -> bool {
    match editions {
        Editions::All => true,
        Editions::ProOrHigher => matches!(edition, "Pro" | "Enterprise" | "Education"),
        Editions::EnterpriseOrEducation => matches!(edition, "Enterprise" | "Education"),
    }
}

/// The rules one record must meet to support its tweak.
fn check_record(t: &Tweak, record: &Record, expected: &Expected<'_>) -> Vec<String> {
    let mut problems = Vec::new();
    let id = t.id;
    if record.heading.as_deref() != Some(id) {
        problems.push(format!("{id}: the heading is not '# {id}'"));
    }
    if !record.sources.iter().any(|s| s.starts_with("https://")) {
        problems.push(format!("{id}: no Source starts with https://"));
    }
    let checks = &record.checks;
    let has = |result: &str| checks.iter().any(|c| c.result == result);
    if t.retired.is_some() {
        if !has("no-effect") && !has("not-offered") && !has("broken") {
            problems.push(format!(
                "{id}: a retired tweak needs a no-effect, not-offered or broken check"
            ));
        }
        return problems;
    }
    if t.default_on {
        for c in checks.iter().filter(|c| c.result == "works") {
            if !matches!(c.evidence.as_str(), "functional" | "ui-state") {
                problems.push(format!(
                    "{id}: line {}: a recommended tweak's works check needs functional or \
                     ui-state evidence",
                    c.line
                ));
            }
        }
    }
    if expected.no_machine.contains(&id) {
        let not_offered = checks.iter().any(|c| {
            c.result == "not-offered"
                && parse_build(&c.build).is_some_and(|b| servicing_family(b.number) >= FAMILY_24H2)
        });
        if !not_offered {
            problems.push(format!(
                "{id}: needs a not-offered check on build {FAMILY_24H2} or later"
            ));
        }
        if !has("documented") {
            problems.push(format!("{id}: needs a documented check"));
        }
        if t.default_on {
            problems.push(format!(
                "{id}: no available PC runs it, so it must not be recommended"
            ));
        }
        return problems;
    }
    // A check made on a build and edition the tweak is offered on.
    let offered_here = |c: &Check| {
        parse_build(&c.build).is_some_and(|b| in_range(&t.windows, b))
            && edition_offered(t.windows.editions, &c.edition)
    };
    if expected.in_state.contains(&id) {
        if !checks
            .iter()
            .any(|c| c.result == "documented" && offered_here(c))
        {
            problems.push(format!(
                "{id}: every PC available was already in its state, so it needs a documented \
                 check on a build and edition it is offered on"
            ));
        }
        if t.default_on {
            problems.push(format!(
                "{id}: no available PC could apply or undo it, so it must not be recommended"
            ));
        }
        return problems;
    }
    let exact = |c: &Check| c.kept == "yes" && c.undo == "yes" && offered_here(c);
    let recheck = expected.recheck.contains(&id);
    let undo_again = expected.undo_again.contains(&id);
    // A works check kept after the restart, on a build and edition the tweak is offered on;
    // its Undo must have been exact, unless the tweak waits for a check of a fixed undo or had
    // its undo checked again.
    let works = |c: &Check| {
        c.result == "works"
            && c.kept == "yes"
            && offered_here(c)
            && (c.undo == "yes" || recheck || undo_again)
    };
    // A works check whose Undo was exact, made with or without a restart.
    let undone_exactly = |c: &Check| c.result == "works" && c.undo == "yes" && offered_here(c);
    let documented = |c: &Check| c.result == "documented" && exact(c);
    if !checks.iter().any(|c| works(c) || documented(c)) {
        problems.push(format!(
            "{id}: needs a works check (or a documented check of values no available PC could \
             show the effect of), kept after restart and undone exactly, on a build and edition \
             it is offered on"
        ));
    } else if t.default_on && !checks.iter().any(works) {
        problems.push(format!(
            "{id}: a recommended tweak needs a works check; a documented check shows no observed \
             effect"
        ));
    }
    if recheck && checks.iter().any(undone_exactly) {
        problems.push(format!(
            "{id}: has a works check undone exactly, so it leaves RECHECK"
        ));
    }
    if undo_again {
        // The undo was checked again on the fixed build: an exact undo no older than the
        // check that was kept after the restart.
        let kept_since = checks
            .iter()
            .filter(|c| works(c))
            .map(|c| c.date.as_str())
            .min();
        let undone_again =
            |c: &Check| undone_exactly(c) && kept_since.is_some_and(|date| c.date.as_str() >= date);
        if !checks.iter().any(undone_again) {
            problems.push(format!(
                "{id}: its undo was checked again, so it needs a works check with an exact undo, \
                 no older than its works check kept after restart"
            ));
        }
        if checks.iter().any(|c| c.result == "works" && exact(c)) {
            problems.push(format!(
                "{id}: has a works check kept after restart and undone exactly, so it leaves \
                 UNDO_CHECKED_AGAIN"
            ));
        }
    }
    problems
}

/// Every problem of the records in `dir` against `expected`; empty when they all hold.
fn check_folder(dir: &Path, expected: &Expected<'_>) -> Vec<String> {
    let mut problems = Vec::new();
    let known: HashSet<&str> = expected.tweaks.iter().map(|t| t.id).collect();
    let mut records = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        problems.push(format!("cannot list {}: {e}", dir.display()));
                        continue;
                    }
                };
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    problems.push(format!("{name}: only record files belong here"));
                    continue;
                }
                if name == "README.md" {
                    continue;
                }
                match name.strip_suffix(".md") {
                    Some(id) if known.contains(id) => records.push((id.to_string(), path)),
                    Some(id) => problems.push(format!("{name}: '{id}' is not a catalog tweak")),
                    None => problems.push(format!(
                        "{name}: only Markdown records belong here (no screenshots)"
                    )),
                }
            }
        }
        Err(e) => problems.push(format!("cannot list {}: {e}", dir.display())),
    }
    let recorded: HashSet<&str> = records.iter().map(|(id, _)| id.as_str()).collect();
    let mut pending_seen = HashSet::new();
    for id in expected.pending {
        if !pending_seen.insert(*id) {
            problems.push(format!("PENDING lists {id} twice"));
        }
        if !known.contains(id) {
            problems.push(format!("PENDING lists {id}, which is not a catalog tweak"));
        }
        if recorded.contains(id) {
            problems.push(format!("{id} has a record, so it leaves PENDING"));
        }
    }
    for (list, ids) in [
        ("NO_MACHINE_IN_RANGE", expected.no_machine),
        ("ALREADY_IN_STATE", expected.in_state),
        ("RECHECK", expected.recheck),
        ("UNDO_CHECKED_AGAIN", expected.undo_again),
    ] {
        for id in ids {
            if !known.contains(id) {
                problems.push(format!("{list} lists {id}, which is not a catalog tweak"));
            }
        }
    }
    for id in expected.undo_again {
        if expected.recheck.contains(id) {
            problems.push(format!(
                "{id} is listed in RECHECK and UNDO_CHECKED_AGAIN; a tweak waits for its new check \
                 or has had it"
            ));
        }
    }
    for t in &expected.tweaks {
        match records.iter().find(|(id, _)| id == t.id) {
            Some((_, path)) => match fs::read_to_string(path) {
                Ok(text) => {
                    let name = format!("{}.md", t.id);
                    if text.contains(r"\Users\") {
                        problems.push(format!("{name}: holds a path with a user name"));
                    }
                    let record = parse(&name, &text, &mut problems);
                    problems.extend(check_record(t, &record, expected));
                }
                Err(e) => problems.push(format!("cannot read {}: {e}", path.display())),
            },
            None if expected.pending.contains(&t.id) => {}
            None => problems.push(format!("{} has no record and is not in PENDING", t.id)),
        }
    }
    if expected.release && !expected.pending.is_empty() {
        problems.push(format!(
            "CAIRN_RELEASE=1, but PENDING still lists {} tweaks without a record: {}",
            expected.pending.len(),
            expected.pending.join(", ")
        ));
    }
    if expected.release && !expected.recheck.is_empty() {
        problems.push(format!(
            "CAIRN_RELEASE=1, but RECHECK still lists {} tweaks whose undo needs a new check: {}",
            expected.recheck.len(),
            expected.recheck.join(", ")
        ));
    }
    problems
}

#[test]
fn every_tweak_has_a_verification_record() {
    let problems = check_folder(&records_dir(), &Expected::catalog(release_build()));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_records_folder_documents_its_format() {
    let readme = fs::read_to_string(records_dir().join("README.md")).expect("records README");
    for phrase in [
        "| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |",
        "`works`",
        "`not-offered`",
        "`functional`",
        "`ui-state`",
        "PENDING",
        "NO_MACHINE_IN_RANGE",
        "ALREADY_IN_STATE",
        "RECHECK",
        "UNDO_CHECKED_AGAIN",
        "CAIRN_RELEASE=1",
    ] {
        assert!(readme.contains(phrase), "README lacks {phrase:?}");
    }
    for result in RESULTS {
        assert!(readme.contains(&format!("| `{result}` |")), "{result}");
    }
}

/// The README's meaning of `documented` covers each reason a tweak of `NO_MACHINE_IN_RANGE`
/// cannot run on the PCs available.
#[test]
fn the_documented_result_covers_every_reason_no_pc_can_run_a_tweak() {
    let readme = fs::read_to_string(records_dir().join("README.md")).expect("records README");
    let meaning = readme
        .lines()
        .find_map(|line| line.strip_prefix("| `documented` |"))
        .expect("the README defines `documented`");
    for id in NO_MACHINE_IN_RANGE {
        let t = TWEAKS.iter().find(|t| t.id == *id).expect("catalog tweak");
        let missing = if t.windows.through.is_some_and(|last| last < FAMILY_24H2) {
            "Windows version"
        } else if matches!(t.requires, Some(Requirement::RecallComponent)) {
            "hardware"
        } else if matches!(t.requires, Some(Requirement::OfficeBusiness)) {
            "Office edition"
        } else {
            panic!(
                "{id}: neither its Windows range nor its requirement rules out the PCs available"
            );
        };
        assert!(
            meaning.contains(missing),
            "{id} has a documented check because of a missing {missing}, but the README says \
             `documented` means:{meaning}"
        );
    }
}

#[test]
fn pending_lists_every_tweak_without_a_record_in_catalog_order() {
    let ids: Vec<&str> = TWEAKS.iter().map(|t| t.id).collect();
    let pending_in_catalog: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| PENDING.contains(id))
        .collect();
    assert_eq!(
        pending_in_catalog, PENDING,
        "PENDING follows the catalog order"
    );
    for id in NO_MACHINE_IN_RANGE {
        let t = TWEAKS.iter().find(|t| t.id == *id).expect("catalog tweak");
        assert!(!t.default_on, "{id}");
        assert!(t.retired.is_none(), "{id}");
    }
    for id in ALREADY_IN_STATE {
        let t = TWEAKS.iter().find(|t| t.id == *id).expect("catalog tweak");
        assert!(t.retired.is_none(), "{id}");
        assert!(!t.default_on, "{id}");
        assert!(
            !NO_MACHINE_IN_RANGE.contains(id),
            "{id}: one list per tweak"
        );
    }
    for id in RECHECK.iter().chain(UNDO_CHECKED_AGAIN) {
        let t = TWEAKS.iter().find(|t| t.id == *id).expect("catalog tweak");
        assert!(t.retired.is_none(), "{id}");
        assert!(
            !NO_MACHINE_IN_RANGE.contains(id) && !ALREADY_IN_STATE.contains(id),
            "{id}: one list per tweak"
        );
        assert!(
            !(RECHECK.contains(id) && UNDO_CHECKED_AGAIN.contains(id)),
            "{id}: one list per tweak"
        );
    }
}

// ───────────────────────────── fixtures ─────────────────────────────

static OFFERED: Tweak = Tweak {
    id: "privacy.fixture_offered",
    category: Category::Privacy,
    topic: Topic::Telemetry,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: true,
    restart: RestartNeed::None,
    requires: None,
    windows: WindowsRange {
        from: &[Build::new(26100, 3915)],
        through: None,
        editions: Editions::All,
    },
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static PRO_ONLY: Tweak = Tweak {
    id: "privacy.fixture_pro",
    category: Category::Privacy,
    topic: Topic::Telemetry,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: false,
    restart: RestartNeed::None,
    requires: None,
    windows: WindowsRange {
        from: &[],
        through: None,
        editions: Editions::ProOrHigher,
    },
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static RETIRED: Tweak = Tweak {
    id: "privacy.fixture_retired",
    category: Category::Privacy,
    topic: Topic::Telemetry,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: false,
    restart: RestartNeed::None,
    requires: None,
    windows: ANY_WINDOWS,
    retired: Some("Cairn found no effect of this fixture."),
    undo_limit: None,
    actions: &[],
    former: &[],
};

static OLD_WINDOWS: Tweak = Tweak {
    id: "privacy.fixture_old_windows",
    category: Category::Privacy,
    topic: Topic::Ai,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: false,
    restart: RestartNeed::None,
    requires: None,
    windows: WindowsRange {
        from: &[Build::new(22621, 2361)],
        through: Some(22621),
        editions: Editions::All,
    },
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static IN_STATE: Tweak = Tweak {
    id: "gaming.fixture_in_state",
    category: Category::Gaming,
    topic: Topic::Input,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: false,
    restart: RestartNeed::None,
    requires: None,
    windows: WindowsRange {
        from: &[Build::new(26100, 0)],
        through: None,
        editions: Editions::All,
    },
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static OPTIONAL: Tweak = Tweak {
    id: "privacy.fixture_optional",
    category: Category::Privacy,
    topic: Topic::Telemetry,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: false,
    restart: RestartNeed::None,
    requires: None,
    windows: ANY_WINDOWS,
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static RECHECKED: Tweak = Tweak {
    id: "performance.fixture_recheck",
    category: Category::Performance,
    topic: Topic::Services,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: true,
    restart: RestartNeed::None,
    requires: None,
    windows: ANY_WINDOWS,
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

static UNDO_AGAIN: Tweak = Tweak {
    id: "performance.fixture_undo_again",
    category: Category::Performance,
    topic: Topic::Services,
    title: "Fixture",
    description: "A tweak for the record checks.",
    risk: Risk::Low,
    default_on: true,
    restart: RestartNeed::None,
    requires: None,
    windows: ANY_WINDOWS,
    retired: None,
    undo_limit: None,
    actions: &[],
    former: &[],
};

const HEADER: &str = "| Date | Version | Edition | Build | Result | Evidence | Kept after restart \
                      | Undo exact | Notes |\n|---|---|---|---|---|---|---|---|---|\n";

fn record(id: &str, source: &str, checks: &[&str]) -> String {
    let mut text = format!(
        "# {id}\n\n| Field | Value |\n|---|---|\n| Source | {source} |\n| Mechanism | policy |\n\n\
         ## Checks\n\n{HEADER}"
    );
    for check in checks {
        text.push_str(check);
        text.push('\n');
    }
    text
}

const WORKS: &str =
    "| 2026-10-08 | 26H2 | Home | 26300.9550 | works | functional | yes | yes | ok |";
/// A check of values set, kept and undone exactly whose effect the PC could not show.
const DOCUMENTED: &str =
    "| 2026-10-08 | 26H2 | Home | 26300.9550 | documented | none | yes | yes | no control |";
/// A check of a PC that was already in the tweak's state, so nothing was applied or undone.
const STATE_ONLY: &str =
    "| 2026-10-07 | 26H2 | Home | 26300.9457 | documented | none | n/a | n/a | already so |";
/// A check whose Undo left a difference behind, through a fault fixed since.
const UNDO_NOT_EXACT: &str =
    "| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | no | extra value |";
/// A later check of the fixed build, without a restart, whose Undo was exact.
const UNDONE_AGAIN: &str =
    "| 2026-10-11 | 26H2 | Home | 26300.9457 | works | functional | n/a | yes | fixed undo |";
const SOURCE: &str =
    "https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-search";

/// The `WORKS` check with one part replaced.
fn works_with(from: &str, to: &str) -> String {
    WORKS.replace(from, to)
}

fn fixture_folder(files: &[(String, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(dir.path().join("README.md"), "# Verification records\n").unwrap();
    for (name, text) in files {
        fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

fn fixture_tweaks() -> Vec<&'static Tweak> {
    vec![
        &OFFERED,
        &PRO_ONLY,
        &RETIRED,
        &OLD_WINDOWS,
        &IN_STATE,
        &OPTIONAL,
        &RECHECKED,
        &UNDO_AGAIN,
    ]
}

fn fixture_expected(release: bool) -> Expected<'static> {
    Expected {
        tweaks: fixture_tweaks(),
        pending: &[],
        no_machine: &["privacy.fixture_old_windows"],
        in_state: &["gaming.fixture_in_state"],
        recheck: &["performance.fixture_recheck"],
        undo_again: &["performance.fixture_undo_again"],
        release,
    }
}

/// Records that meet every rule for the eight fixture tweaks.
fn good_records() -> Vec<(String, String)> {
    let file = |id: &str, checks: &[&str]| (format!("{id}.md"), record(id, SOURCE, checks));
    vec![
        file("privacy.fixture_offered", &[WORKS]),
        file(
            "privacy.fixture_pro",
            &["| 2026-10-09 | 26H2 | Pro | 26300.9550 | works | ui-state | yes | yes | |"],
        ),
        file(
            "privacy.fixture_retired",
            &["| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | none | n/a | yes | |"],
        ),
        file(
            "privacy.fixture_old_windows",
            &[
                "| 2026-10-04 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | |",
                "| 2026-10-04 | 23H2 | Home | 22631.2361 | documented | none | n/a | n/a | |",
            ],
        ),
        file("gaming.fixture_in_state", &[STATE_ONLY]),
        file("privacy.fixture_optional", &[DOCUMENTED]),
        file("performance.fixture_recheck", &[UNDO_NOT_EXACT]),
        file(
            "performance.fixture_undo_again",
            &[UNDO_NOT_EXACT, UNDONE_AGAIN],
        ),
    ]
}

#[test]
fn records_that_meet_every_rule_pass() {
    let dir = fixture_folder(&good_records());
    let problems = check_folder(dir.path(), &fixture_expected(false));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn release_mode_fails_while_recheck_is_not_empty() {
    let dir = fixture_folder(&good_records());
    let problems = check_folder(dir.path(), &fixture_expected(true));
    assert_eq!(
        problems,
        [
            "CAIRN_RELEASE=1, but RECHECK still lists 1 tweaks whose undo needs a new check: \
             performance.fixture_recheck"
        ]
    );
}

#[test]
fn release_mode_fails_while_pending_is_not_empty() {
    // The catalog with no record at all: fine while every id is pending, not in a release.
    let dir = fixture_folder(&[]);
    let all: Vec<&str> = TWEAKS.iter().map(|t| t.id).collect();
    let expected = |release| Expected {
        tweaks: TWEAKS.iter().collect(),
        pending: &all,
        no_machine: NO_MACHINE_IN_RANGE,
        in_state: ALREADY_IN_STATE,
        recheck: &[],
        undo_again: &[],
        release,
    };
    assert_eq!(
        check_folder(dir.path(), &expected(false)),
        Vec::<String>::new()
    );
    let problems = check_folder(dir.path(), &expected(true));
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].starts_with("CAIRN_RELEASE=1, but PENDING still lists"));
    assert!(problems[0].contains("privacy.diagnostic_data, privacy.diagtrack_service"));
    assert!(problems[0].contains(&TWEAKS.len().to_string()));

    // The same holds for the real PENDING list.
    let real = check_folder(dir.path(), &Expected::catalog(true));
    assert!(real.iter().any(|p| p.starts_with("CAIRN_RELEASE=1")) || PENDING.is_empty());
}

/// Problems after replacing one fixture record with `text`.
fn problems_with(name: &str, text: String) -> Vec<String> {
    let mut files = good_records();
    files.retain(|(n, _)| n != name);
    files.push((name.to_string(), text));
    let dir = fixture_folder(&files);
    check_folder(dir.path(), &fixture_expected(false))
}

fn assert_one_problem(problems: &[String], expected: &str) {
    assert!(
        problems.iter().any(|p| p.contains(expected)),
        "no problem contains {expected:?}: {problems:#?}"
    );
}

#[test]
fn records_that_break_a_rule_fail() {
    let offered = "privacy.fixture_offered.md";
    let id = "privacy.fixture_offered";
    let with = |from: &str, to: &str| record(id, SOURCE, &[works_with(from, to).as_str()]);
    let cases: Vec<(String, &str)> = vec![
        (
            record("privacy.other", SOURCE, &[WORKS]),
            "the heading is not",
        ),
        (
            record(id, "learn.microsoft.com/windows", &[WORKS]),
            "no Source starts with https://",
        ),
        (with("2026-10-08", "8.10.2026"), "is not YYYY-MM-DD"),
        (with("26300.9550", "26300"), "is not like 26300.9550"),
        (with("| works |", "| fine |"), "result 'fine'"),
        (with("functional", "seen"), "evidence 'seen'"),
        (
            with("| yes | yes |", "| maybe | yes |"),
            "kept after restart 'maybe'",
        ),
        (with("Home", "Server"), "edition 'Server'"),
        // Below the 26100.3915 floor of its family.
        (with("26300.9550", "26100.3000"), "needs a works check"),
        // An older family than the floor.
        (with("26300.9550", "22631.4890"), "needs a works check"),
        (with("| yes | yes |", "| no | yes |"), "needs a works check"),
        (with("| yes | yes |", "| yes | no |"), "needs a works check"),
        (
            with("functional", "none"),
            "needs functional or ui-state evidence",
        ),
        (with("| works |", "| partial |"), "needs a works check"),
        (with(" | ok |", " |"), "a check has 8 cells"),
        (
            record(id, SOURCE, &[WORKS]).replace("Kept after restart", "Kept"),
            "the checks header is not",
        ),
        // A documented check stands in for a works check only with both columns yes.
        (
            record(
                id,
                SOURCE,
                &[&DOCUMENTED.replace("| yes | yes |", "| no | yes |")],
            ),
            "needs a works check",
        ),
        (
            record(
                id,
                SOURCE,
                &[&DOCUMENTED.replace("| yes | yes |", "| yes | n/a |")],
            ),
            "needs a works check",
        ),
        (record(id, SOURCE, &[STATE_ONLY]), "needs a works check"),
    ];
    for (text, expected) in cases {
        assert_one_problem(&problems_with(offered, text), expected);
    }

    // Values set, kept and undone exactly whose effect no available PC could show support a
    // tweak that is not recommended; a recommended one needs an observed effect.
    assert_eq!(
        problems_with(
            "privacy.fixture_optional.md",
            record("privacy.fixture_optional", SOURCE, &[DOCUMENTED])
        ),
        Vec::<String>::new()
    );
    assert_one_problem(
        &problems_with(offered, record(id, SOURCE, &[DOCUMENTED])),
        "a recommended tweak needs a works check",
    );
    assert!(
        problems_with(offered, record(id, SOURCE, &[DOCUMENTED, WORKS])).is_empty(),
        "a works check beside it is enough"
    );

    // A tweak waiting for a check of its fixed undo: the inexact check supports it until a
    // check undone exactly exists, which takes it off RECHECK.
    let recheck = "performance.fixture_recheck";
    let recheck_file = "performance.fixture_recheck.md";
    assert_one_problem(
        &problems_with(
            recheck_file,
            record(recheck, SOURCE, &[UNDO_NOT_EXACT, WORKS]),
        ),
        "has a works check undone exactly, so it leaves RECHECK",
    );
    assert_one_problem(
        &problems_with(
            recheck_file,
            record(
                recheck,
                SOURCE,
                &[&UNDO_NOT_EXACT.replace("| yes | no |", "| no | no |")],
            ),
        ),
        "needs a works check",
    );
    assert_one_problem(
        &problems_with(recheck_file, record(recheck, SOURCE, &[DOCUMENTED])),
        "a recommended tweak needs a works check",
    );
    // A check of the fixed build that undid it exactly, even without a restart, ends the wait.
    assert_one_problem(
        &problems_with(
            recheck_file,
            record(recheck, SOURCE, &[UNDO_NOT_EXACT, UNDONE_AGAIN]),
        ),
        "has a works check undone exactly, so it leaves RECHECK",
    );
    // Outside RECHECK an inexact undo never supports a tweak.
    assert_one_problem(
        &problems_with(offered, record(id, SOURCE, &[UNDO_NOT_EXACT])),
        "needs a works check",
    );

    // A tweak whose undo was checked again needs the check kept after the restart and an exact
    // undo no older than it; one check showing both takes it off the list.
    let again = "performance.fixture_undo_again";
    let again_file = "performance.fixture_undo_again.md";
    let needs_again = "its undo was checked again, so it needs a works check with an exact undo";
    assert_one_problem(
        &problems_with(again_file, record(again, SOURCE, &[UNDO_NOT_EXACT])),
        needs_again,
    );
    assert_one_problem(
        &problems_with(
            again_file,
            record(
                again,
                SOURCE,
                &[
                    &UNDONE_AGAIN.replace("2026-10-11", "2026-10-07"),
                    UNDO_NOT_EXACT,
                ],
            ),
        ),
        needs_again,
    );
    assert_one_problem(
        &problems_with(again_file, record(again, SOURCE, &[UNDONE_AGAIN])),
        "needs a works check",
    );
    assert_one_problem(
        &problems_with(
            again_file,
            record(
                again,
                SOURCE,
                &[
                    &UNDONE_AGAIN.replace("| works |", "| documented |"),
                    UNDO_NOT_EXACT,
                ],
            ),
        ),
        needs_again,
    );
    assert_one_problem(
        &problems_with(again_file, record(again, SOURCE, &[UNDO_NOT_EXACT, WORKS])),
        "so it leaves UNDO_CHECKED_AGAIN",
    );
    let dir = fixture_folder(&good_records());
    let both = check_folder(
        dir.path(),
        &Expected {
            recheck: &["performance.fixture_recheck", again],
            ..fixture_expected(false)
        },
    );
    assert_one_problem(
        &both,
        "performance.fixture_undo_again is listed in RECHECK and UNDO_CHECKED_AGAIN",
    );

    // A tweak no available PC could apply or undo is never recommended.
    let dir = fixture_folder(&{
        let mut files = good_records();
        files.retain(|(n, _)| n != offered);
        files.push((offered.to_string(), record(id, SOURCE, &[STATE_ONLY])));
        files
    });
    let in_state_recommended = check_folder(
        dir.path(),
        &Expected {
            in_state: &["gaming.fixture_in_state", "privacy.fixture_offered"],
            ..fixture_expected(false)
        },
    );
    assert_one_problem(
        &in_state_recommended,
        "privacy.fixture_offered: no available PC could apply or undo it, so it must not be \
         recommended",
    );

    // Offered on Pro and higher only: a Home check does not count.
    assert_one_problem(
        &problems_with(
            "privacy.fixture_pro.md",
            record(
                "privacy.fixture_pro",
                SOURCE,
                &["| 2026-10-09 | 26H2 | Home | 26300.9550 | works | ui-state | yes | yes | |"],
            ),
        ),
        "needs a works check",
    );
    assert_one_problem(
        &problems_with(
            "privacy.fixture_retired.md",
            record("privacy.fixture_retired", SOURCE, &[WORKS]),
        ),
        "a retired tweak needs a no-effect, not-offered or broken check",
    );
    // A change Windows no longer lets programs make retires a tweak too.
    assert_eq!(
        problems_with(
            "privacy.fixture_retired.md",
            record(
                "privacy.fixture_retired",
                SOURCE,
                &["| 2026-10-07 | 26H2 | Home | 26300.9457 | broken | none | n/a | no | |"],
            ),
        ),
        Vec::<String>::new()
    );
    // A PC already in the tweak's state: only a documented check of that state counts.
    assert_one_problem(
        &problems_with(
            "gaming.fixture_in_state.md",
            record("gaming.fixture_in_state", SOURCE, &[WORKS]),
        ),
        "already in its state, so it needs a documented check",
    );
    // An older family than the fixture's floor.
    assert_one_problem(
        &problems_with(
            "gaming.fixture_in_state.md",
            record(
                "gaming.fixture_in_state",
                SOURCE,
                &[&STATE_ONLY.replace("26300.9457", "22631.4890")],
            ),
        ),
        "already in its state, so it needs a documented check",
    );
    assert_one_problem(
        &problems_with(
            "privacy.fixture_old_windows.md",
            record(
                "privacy.fixture_old_windows",
                SOURCE,
                &["| 2026-10-04 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | |"],
            ),
        ),
        "needs a documented check",
    );
    assert_one_problem(
        &problems_with(
            "privacy.fixture_old_windows.md",
            record(
                "privacy.fixture_old_windows",
                SOURCE,
                &[
                    "| 2026-10-04 | 23H2 | Home | 22631.2361 | not-offered | none | n/a | n/a | |",
                    "| 2026-10-04 | 23H2 | Home | 22631.2361 | documented | none | n/a | n/a | |",
                ],
            ),
        ),
        "needs a not-offered check on build 26100 or later",
    );
}

#[test]
fn the_folder_holds_only_records_of_catalog_tweaks() {
    let mut files = good_records();
    files.push((
        "privacy.unknown.md".to_string(),
        record("privacy.unknown", SOURCE, &[WORKS]),
    ));
    files.push(("screen.png".to_string(), String::new()));
    let dir = fixture_folder(&files);
    let problems = check_folder(dir.path(), &fixture_expected(false));
    assert_one_problem(&problems, "'privacy.unknown' is not a catalog tweak");
    assert_one_problem(&problems, "screen.png: only Markdown records belong here");

    // A missing record, a pending id with a record, an unknown pending id.
    let dir = fixture_folder(&good_records()[1..]);
    let problems = check_folder(
        dir.path(),
        &Expected {
            tweaks: fixture_tweaks(),
            pending: &["privacy.fixture_pro", "privacy.nothing"],
            no_machine: &["privacy.fixture_old_windows"],
            in_state: &["gaming.fixture_in_state", "gaming.nothing"],
            recheck: &["performance.fixture_recheck", "performance.nothing"],
            undo_again: &["performance.fixture_undo_again", "performance.none"],
            release: false,
        },
    );
    assert_one_problem(
        &problems,
        "privacy.fixture_offered has no record and is not in PENDING",
    );
    assert_one_problem(
        &problems,
        "privacy.fixture_pro has a record, so it leaves PENDING",
    );
    assert_one_problem(&problems, "PENDING lists privacy.nothing");
    assert_one_problem(&problems, "ALREADY_IN_STATE lists gaming.nothing");
    assert_one_problem(&problems, "RECHECK lists performance.nothing");
    assert_one_problem(&problems, "UNDO_CHECKED_AGAIN lists performance.none");

    // Records hold no personal paths.
    let personal = record(
        "privacy.fixture_offered",
        SOURCE,
        &[works_with("| ok |", r"| C:\Users\Test\Documents\file.txt |").as_str()],
    );
    assert_one_problem(
        &problems_with("privacy.fixture_offered.md", personal),
        "holds a path with a user name",
    );
}

#[test]
fn builds_are_in_range_by_servicing_family() {
    let range = |from: &'static [Build], through: Option<u32>| WindowsRange {
        from,
        through,
        editions: Editions::All,
    };
    let build = |text: &str| parse_build(text).unwrap();
    const RECALL: &[Build] = &[Build::new(26100, 3915)];
    const PAINT: &[Build] = &[Build::new(22621, 4870), Build::new(26100, 3360)];
    const ENABLEMENT: &[Build] = &[Build::new(26200, 0)];
    let cases = [
        (range(RECALL, None), "26300.9457", true),
        (range(RECALL, None), "26100.3000", false),
        (range(RECALL, None), "22631.4890", false),
        (range(RECALL, None), "28000.1", true),
        (range(PAINT, None), "22631.4900", true),
        (range(PAINT, None), "22631.4317", false),
        (range(PAINT, None), "26200.3000", false),
        (range(PAINT, None), "26200.5000", true),
        (range(PAINT, None), "22000.2000", false),
        (range(PAINT, None), "28000.1", true),
        (range(&[], Some(22621)), "22631.1", true),
        (range(&[], Some(22621)), "26100.1", false),
        (range(&[], Some(22621)), "28000.1", false),
        (range(ENABLEMENT, None), "26100.9550", false),
        (range(ENABLEMENT, None), "26300.9457", true),
        (ANY_WINDOWS, "22000.1", true),
    ];
    for (range, text, expected) in cases {
        assert_eq!(in_range(&range, build(text)), expected, "{range:?} {text}");
    }
    assert!(parse_build("2630.1").is_none());
    assert!(parse_build("26300.").is_none());
    assert!(parse_build("26300.12a").is_none());
    assert!(edition_offered(Editions::ProOrHigher, "Education"));
    assert!(!edition_offered(Editions::ProOrHigher, "Home"));
    assert!(!edition_offered(Editions::EnterpriseOrEducation, "Pro"));
}

// ───────────────────────────── session targets ─────────────────────────────

/// The registry values, their keys, the services and the scheduled tasks of the catalog,
/// including the values earlier versions set (`former`), in catalog order without repeats:
/// the shape `scripts/live_state_snapshot.ps1` reads from `scripts/verification/targets.json`.
fn expected_targets() -> Value {
    fn add_registry(r: &RegistryAction, values: &mut Vec<Value>, keys: &mut Vec<Value>) {
        let value = json!({ "hive": r.hive.short(), "path": r.path, "name": r.name });
        if !values.contains(&value) {
            values.push(value);
        }
        let key = json!({ "hive": r.hive.short(), "path": r.path });
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    let mut values: Vec<Value> = Vec::new();
    let mut keys: Vec<Value> = Vec::new();
    let mut services: Vec<Value> = Vec::new();
    let mut tasks: Vec<Value> = Vec::new();
    for t in TWEAKS {
        for a in t.actions {
            match a {
                Action::Registry(r) => add_registry(r, &mut values, &mut keys),
                Action::Service(s) => {
                    let name = json!(s.name);
                    if !services.contains(&name) {
                        services.push(name);
                    }
                }
                Action::ScheduledTask(s) => {
                    let path = json!(s.path);
                    if !tasks.contains(&path) {
                        tasks.push(path);
                    }
                }
                Action::Power(_) => {}
            }
        }
        for f in t.former {
            add_registry(&f.action, &mut values, &mut keys);
        }
    }
    json!({
        "registry_values": values,
        "registry_keys": keys,
        "services": services,
        "tasks": tasks,
    })
}

#[test]
fn verification_targets_file_matches_the_catalog() {
    let path = repo_root()
        .join("scripts")
        .join("verification")
        .join("targets.json");
    let expected = expected_targets();
    let pretty = serde_json::to_string_pretty(&expected).unwrap();
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nIt must hold:\n{pretty}",
            path.display()
        )
    });
    let found: Value = serde_json::from_str(&text).expect("targets.json is JSON");
    assert!(
        found == expected,
        "scripts/verification/targets.json no longer matches the catalog; it must hold:\n{pretty}"
    );
    let count = |list: &str| expected[list].as_array().map_or(0, Vec::len);
    let registry_targets: usize = TWEAKS
        .iter()
        .map(|t| {
            t.actions
                .iter()
                .filter(|a| matches!(a, Action::Registry(_)))
                .count()
                + t.former.len()
        })
        .sum();
    assert_eq!(
        count("registry_values"),
        registry_targets,
        "one target, one tweak"
    );
    assert_eq!(count("tasks"), 12);
    assert!(count("services") >= 1 && count("registry_keys") >= 1);
}

// ───────────────────────────── README counts ─────────────────────────────

/// Small numbers as the README writes them.
const NUMBER_WORDS: [&str; 13] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve",
];

fn number_word(n: usize) -> &'static str {
    NUMBER_WORDS
        .get(n)
        .copied()
        .unwrap_or_else(|| panic!("the README spells numbers up to twelve, not {n}"))
}

/// The README's numbers are the code's: the tweaks Optimize offers (a retired tweak is listed
/// only while a change of it can still be undone) and their groups, the checks of the security
/// checkup and their groups, and the apps and categories of the default install list.
#[test]
fn readme_counts_match_the_code() {
    let readme = fs::read_to_string(repo_root().join("README.md")).expect("README.md");
    let offered: Vec<&Tweak> = TWEAKS.iter().filter(|t| t.retired.is_none()).collect();
    let tweak_groups: HashSet<Category> = offered.iter().map(|t| t.category).collect();
    let check_groups: HashSet<_> = CheckId::ALL.iter().map(|c| c.group()).collect();
    let app_categories: HashSet<_> = DEFAULT_APPS.iter().map(|(_, _, c)| *c).collect();
    let expected = [
        format!(
            "**Optimize**: {} tweaks in {} groups",
            offered.len(),
            number_word(tweak_groups.len())
        ),
        format!(
            "a read-only checkup of {} checks in {} groups",
            CheckId::ALL.len(),
            number_word(check_groups.len())
        ),
        format!(
            "a list of {} apps in {} categories",
            DEFAULT_APPS.len(),
            number_word(app_categories.len())
        ),
    ];
    let missing: Vec<&String> = expected.iter().filter(|e| !readme.contains(*e)).collect();
    assert!(missing.is_empty(), "README.md must say {missing:?}");
}
