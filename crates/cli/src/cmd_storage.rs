//! `optctl storage`: disk speed test, space usage and duplicate files.
//!
//! `storage speed` writes a test file to the drive, measures it and deletes it; it needs an
//! elevated process and is recorded in the audit log. Ctrl+C ends optctl: Windows deletes the
//! test file (it is opened delete-on-close), the empty test folder is removed the next time a
//! test runs or with `storage leftovers --remove`, and the audit log gets no final row for
//! the run. `storage scan` and `storage duplicates` only read.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::Subcommand;
use optimizer_core::jobs::{HostJobId, HostJobSnapshot, JobState};
use optimizer_core::safety::state_log::Journal;
use optimizer_core::storage::scan::ChildrenPage;
use optimizer_core::storage::speed::{Direction, SpeedTestId, SpeedTestPlan, MIB};
use optimizer_core::storage::{
    self, plan_or_start_duplicates, plan_or_start_scan, plan_or_start_speed_test,
    remove_leftover_opening, speed_history, DuplicatesRequest, DuplicatesResult, ScanRequest,
    ScanResult, SortOrder, SpeedEnv, SpeedTestRequest, SpeedTestResult, StorageVolume, TreeRow,
    TreeRowKind,
};
use serde_json::{json, Value};

/// How often a running job is polled.
const POLL: Duration = Duration::from_millis(250);
/// Width of the progress line on the terminal.
const PROGRESS_WIDTH: usize = 79;

#[derive(Subcommand, Debug)]
pub(crate) enum StorageCmd {
    /// List the fixed drives, what they are and test folders a speed test left on them.
    /// Read-only.
    Volumes {
        /// Print the drives as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the plan of a disk speed test and, with --yes, run it and print the results.
    /// Writes up to size × (1 + 4 × runs) to the drive; needs an elevated process. Ctrl+C
    /// ends optctl and Windows deletes the test file.
    Speed {
        /// Drive to test, e.g. C:.
        volume: String,
        /// Size of the test file: a number with K, M or G (1024-based), e.g. 64M or 1G.
        #[arg(long, default_value = "1G", value_parser = parse_size)]
        size: u64,
        /// Runs of each measurement; the best run counts (1 to 9).
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=9))]
        runs: u32,
        /// Print the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Run the test; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
        /// Print the plan and the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Earlier speed-test results, newest first. Read-only.
    History {
        /// Print the results as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List test folders a speed test left at the drives' roots and, with --remove --yes,
    /// remove them (needs an elevated process).
    Leftovers {
        /// Remove the listed folders that are not in use.
        #[arg(long)]
        remove: bool,
        /// Confirm the removal.
        #[arg(long)]
        yes: bool,
    },
    /// Show what takes up space in a folder or on a drive. Read-only.
    Scan {
        /// Folder or drive root to scan, e.g. C:\ or C:\Users\Test.
        path: PathBuf,
        /// Folder levels to list below the scanned folder.
        #[arg(long, default_value_t = 1)]
        depth: u32,
        /// Entries per folder and largest files to list.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Print the summary, folders and largest files as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Scan a folder, then find files with identical content in it. Read-only.
    Duplicates {
        /// Folder or drive root to search.
        path: PathBuf,
        /// Smallest file compared (at least 1M).
        #[arg(long, default_value = "1M", value_parser = parse_size)]
        min_size: u64,
        /// Groups to list.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// A size such as "64M", "1G", "512K" or a plain number of bytes (1024-based units).
fn parse_size(value: &str) -> Result<u64, String> {
    let text = value.trim();
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, ""),
    };
    let number: u64 = digits
        .parse()
        .map_err(|_| format!("expected a number with K, M or G, got {value:?}"))?;
    let shift = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        _ => {
            return Err(format!(
                "expected K, M or G after the number, got {value:?}"
            ))
        }
    };
    number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("{value:?} is too large"))
}

pub(crate) fn run(
    cmd: StorageCmd,
    journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    match cmd {
        StorageCmd::Volumes { json } => volumes(json),
        StorageCmd::Speed {
            volume,
            size,
            runs,
            dry_run,
            yes,
            json,
        } => speed(
            &SpeedArgs {
                volume,
                size,
                runs,
                dry_run,
                yes,
                json,
            },
            journal,
            open_journal,
        ),
        StorageCmd::History { json } => history(json, journal),
        StorageCmd::Leftovers { remove, yes } => leftovers(remove, yes, open_journal),
        StorageCmd::Scan {
            path,
            depth,
            top,
            json,
        } => scan(&path, depth, top, json),
        StorageCmd::Duplicates {
            path,
            min_size,
            top,
            json,
        } => duplicates(&path, min_size, top, json),
    }
}

// ───────────────────────────── text ─────────────────────────────

/// "512 B", "8 KB", "64 MB", "9.4 GB" (1024-based, as Cairn shows sizes).
fn size_text(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if size < 1024.0 || unit == "GB" {
            if unit == "B" || unit == "KB" {
                return format!("{size:.0} {unit}");
            }
            let text = format!("{size:.1}");
            let text = text.strip_suffix(".0").unwrap_or(&text);
            return format!("{text} {unit}");
        }
        size /= 1024.0;
    }
    format!("{bytes} B")
}

/// `n` with thousands separators.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", grouped(n), if n == 1 { one } else { many })
}

fn snake(value: serde_json::Result<Value>) -> String {
    value
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn duration_text(ms: u64) -> String {
    let secs = ms / 1000;
    match (secs / 60, secs % 60) {
        (0, 0) => format!("{ms} ms"),
        (0, s) => format!("{s} s"),
        (m, s) => format!("{m} min {s} s"),
    }
}

fn state_label(state: JobState) -> &'static str {
    match state {
        JobState::Running => "running",
        JobState::Succeeded | JobState::Completed => "finished",
        JobState::Attention => "needs attention",
        JobState::Failed => "failed",
        JobState::Cancelled => "stopped",
    }
}

// ───────────────────────────── following a job ─────────────────────────────

/// Polls the job, showing its progress line on the terminal, until it ends.
fn follow(id: HostJobId) -> anyhow::Result<HostJobSnapshot> {
    let terminal = std::io::stderr().is_terminal();
    let mut shown: Option<String> = None;
    loop {
        let Some(job) = storage::lane().snapshot(id) else {
            bail!("lost track of storage job {id}");
        };
        if job.state.is_finished() {
            if shown.is_some() {
                clear_progress();
            }
            return Ok(job);
        }
        if terminal && job.progress_line != shown {
            if let Some(text) = &job.progress_line {
                let fitted: String = text.chars().take(PROGRESS_WIDTH).collect();
                eprint!("\r{fitted:<width$}", width = PROGRESS_WIDTH);
                let _ = std::io::stderr().flush();
            }
            shown = job.progress_line.clone();
        }
        thread::sleep(POLL);
    }
}

fn clear_progress() {
    eprint!("\r{:width$}\r", "", width = PROGRESS_WIDTH);
    let _ = std::io::stderr().flush();
}

fn published(id: HostJobId) -> anyhow::Result<Value> {
    storage::lane()
        .result(id, 0)
        .map(|(_, value)| (*value).clone())
        .context("the job published no result")
}

fn finish(job: &HostJobSnapshot) -> anyhow::Result<()> {
    match job.state {
        JobState::Succeeded | JobState::Completed => Ok(()),
        state => bail!(
            "{} ended as {}{}",
            job.title,
            state_label(state),
            job.summary
                .as_deref()
                .map(|s| format!(": {s}"))
                .unwrap_or_default()
        ),
    }
}

// ───────────────────────────── volumes ─────────────────────────────

fn volumes(json: bool) -> anyhow::Result<()> {
    let list = storage::volumes().context("read the fixed drives")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    for volume in &list {
        print_volume(volume);
    }
    Ok(())
}

fn print_volume(v: &StorageVolume) {
    let mut line = format!(
        "{}  {:<12}{:<7}{:<9}",
        v.letter,
        if v.label.is_empty() { "-" } else { &v.label },
        v.file_system,
        snake(serde_json::to_value(v.media)),
    );
    if let (Some(free), Some(size)) = (v.free_bytes, v.size_bytes) {
        line.push_str(&format!("{} free of {}", size_text(free), size_text(size)));
    }
    if v.system {
        line.push_str("  (Windows)");
    }
    println!("{line}");
    let disk: Vec<&str> = [v.model.as_deref(), v.bus.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if !disk.is_empty() {
        println!("    disk            {}", disk.join("  ·  "));
    }
    if let Some(reason) = &v.speed_test_blocked {
        println!("    no speed test   {reason}");
    }
    if let Some(reason) = &v.scan_blocked {
        println!("    no scan         {reason}");
    }
    for leftover in &v.leftovers {
        println!(
            "    leftover        {} ({}){}",
            leftover.path,
            leftover
                .bytes
                .map_or_else(|| "size unknown".to_string(), size_text),
            if leftover.in_use { "  in use" } else { "" }
        );
    }
}

// ───────────────────────────── speed ─────────────────────────────

/// `<journal folder>\storage` for a journal given with --journal, else the storage data
/// folder.
fn history_dir(journal: Option<&Path>) -> PathBuf {
    match journal.and_then(Path::parent) {
        Some(folder) => folder.join("storage"),
        None => storage::data_dir(),
    }
}

/// What `storage speed` was asked for.
#[derive(Debug)]
struct SpeedArgs {
    volume: String,
    size: u64,
    runs: u32,
    dry_run: bool,
    yes: bool,
    json: bool,
}

fn speed(
    args: &SpeedArgs,
    journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let SpeedArgs {
        ref volume,
        size,
        runs,
        dry_run,
        yes,
        json,
    } = *args;
    let request = SpeedTestRequest::new(volume, size, runs)?.with_history_dir(history_dir(journal));
    let lane = storage::lane();
    let (plan, _) = plan_or_start_speed_test(lane, &SpeedEnv::SYSTEM, &request, true, || {
        Err(optimizer_core::Error::Other(
            "a plan opens no journal".to_string(),
        ))
    })?;
    if json && (dry_run || !yes) {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else if !json {
        print_speed_plan(&plan);
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        bail!("the test was not started; re-run with --yes");
    }
    let open = || {
        open_journal()
            .map(Arc::new)
            .map_err(|e| optimizer_core::Error::Other(format!("{e:#}")))
    };
    let (_, job) = plan_or_start_speed_test(lane, &SpeedEnv::SYSTEM, &request, false, open)?;
    let job = job.context("the test did not start")?;
    let finished = follow(job.id)?;
    let result = published(finished.id).ok();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({ "plan": plan, "job": finished, "result": result })
            )?
        );
    } else {
        match result
            .clone()
            .and_then(|r| serde_json::from_value::<SpeedTestResult>(r).ok())
        {
            Some(result) => print_speed_result(&result),
            None => println!(),
        }
        if let Some(summary) = &finished.summary {
            println!("summary             {summary}");
        }
        if !finished.logged {
            println!("audit               the audit log could not be written for this test");
        }
    }
    finish(&finished)
}

fn print_speed_plan(plan: &SpeedTestPlan) {
    println!("drive               {}", plan.volume);
    println!(
        "test file           {}, {}",
        size_text(plan.size_bytes),
        plural(u64::from(plan.runs), "run", "runs")
    );
    println!("writes up to        {}", size_text(plan.max_write_bytes));
    println!(
        "takes about         {}",
        duration_text(plan.estimated_seconds * 1000)
    );
    println!("folder              {}", plan.folder_pattern);
    for leftover in &plan.leftovers {
        println!("leftover            {}", leftover.path);
    }
    for note in &plan.notes {
        println!("note                {note}");
    }
    match &plan.blocked_reason {
        Some(reason) => println!("blocked             {reason}"),
        None => println!("blocked             no"),
    }
}

fn print_speed_result(result: &SpeedTestResult) {
    println!();
    println!(
        "{:<14}{:>12}{:>10}{:>11}  {:>12}{:>10}{:>11}",
        "MB/s", "read", "IOPS", "latency", "write", "IOPS", "latency"
    );
    for id in SpeedTestId::ALL {
        let cell = |direction: Direction| {
            if result.skipped.contains(&id) {
                return ("skipped".to_string(), String::new(), String::new());
            }
            match result
                .measurements
                .iter()
                .find(|m| m.test == id && m.direction == direction)
            {
                Some(m) => (
                    format!("{:.1}", m.mb_s),
                    format!("{:.0}", m.iops),
                    if m.latency_us >= 1000.0 {
                        format!("{:.1} ms", m.latency_us / 1000.0)
                    } else {
                        format!("{:.0} µs", m.latency_us)
                    },
                ),
                None => ("–".to_string(), String::new(), String::new()),
            }
        };
        let (read, read_iops, read_latency) = cell(Direction::Read);
        let (write, write_iops, write_latency) = cell(Direction::Write);
        println!(
            "{:<14}{read:>12}{read_iops:>10}{read_latency:>11}  {write:>12}{write_iops:>10}{write_latency:>11}",
            id.label()
        );
    }
    println!();
    println!(
        "wrote               {} in {}",
        size_text(result.bytes_written),
        duration_text(result.elapsed_ms)
    );
    for note in &result.notes {
        println!("note                {note}");
    }
}

// ───────────────────────────── history ─────────────────────────────

fn history(json: bool, journal: Option<&Path>) -> anyhow::Result<()> {
    let results = speed_history(&history_dir(journal)).context("read the earlier results")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }
    if results.is_empty() {
        println!("No earlier results.");
        return Ok(());
    }
    for result in &results {
        let best = |id: SpeedTestId, direction: Direction| {
            result
                .measurements
                .iter()
                .find(|m| m.test == id && m.direction == direction)
                .map_or_else(|| "–".to_string(), |m| format!("{:.1}", m.mb_s))
        };
        println!(
            "{}  {}  {} × {}{}",
            result.started_at,
            result.volume,
            size_text(result.size_bytes),
            result.runs,
            result
                .end_word()
                .map(|word| format!("  {word}"))
                .unwrap_or_default()
        );
        println!(
            "    SEQ {} / {}  ·  RND4K Q1 {} / {} MB/s",
            best(SpeedTestId::Seq1mQ8t1, Direction::Read),
            best(SpeedTestId::Seq1mQ8t1, Direction::Write),
            best(SpeedTestId::Rnd4kQ1t1, Direction::Read),
            best(SpeedTestId::Rnd4kQ1t1, Direction::Write)
        );
        if let Some(error) = &result.error {
            println!("    error: {error}");
        }
    }
    Ok(())
}

// ───────────────────────────── leftovers ─────────────────────────────

fn leftovers(
    remove: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let list = storage::volumes().context("read the fixed drives")?;
    let found: Vec<_> = list.iter().flat_map(|v| v.leftovers.iter()).collect();
    if found.is_empty() {
        println!("No test folders were left by a speed test.");
        return Ok(());
    }
    for leftover in &found {
        println!(
            "{}  ({}){}",
            leftover.path,
            leftover
                .bytes
                .map_or_else(|| "size unknown".to_string(), size_text),
            if leftover.in_use {
                "  in use by a running speed test"
            } else {
                ""
            }
        );
    }
    if !remove {
        return Ok(());
    }
    if !yes {
        bail!("nothing was removed; re-run with --yes");
    }
    // The journal is opened only for a removal that is allowed, so a refusal creates nothing.
    let open = || open_journal().map_err(|e| optimizer_core::Error::Other(format!("{e:#}")));
    let mut failed = 0;
    for leftover in found.iter().filter(|l| !l.in_use) {
        let done = remove_leftover_opening(&SpeedEnv::SYSTEM, Path::new(&leftover.path), open)?;
        println!(
            "{}  {}",
            if done.removed { "removed" } else { "kept   " },
            done.detail
        );
        if !done.removed {
            failed += 1;
        }
    }
    if failed > 0 {
        bail!(
            "{} could not be removed",
            plural(failed, "folder", "folders")
        );
    }
    Ok(())
}

// ───────────────────────────── scan ─────────────────────────────

/// Scans `path` on the storage lane and returns the finished job and its tree.
fn run_scan(path: &Path) -> anyhow::Result<(HostJobSnapshot, Arc<ScanResult>)> {
    let request = ScanRequest {
        path: path.to_path_buf(),
    };
    let (plan, job) = plan_or_start_scan(storage::lane(), &request, false)?;
    for note in &plan.notes {
        eprintln!("note: {note}");
    }
    let job = job.context("the scan did not start")?;
    let finished = follow(job.id)?;
    if finished.state == JobState::Failed {
        finish(&finished)?;
    }
    let tree = storage::lane()
        .typed::<ScanResult>(finished.id)
        .context("the scan published no result")?;
    Ok((finished, tree))
}

fn scan(path: &Path, depth: u32, top: usize, json: bool) -> anyhow::Result<()> {
    let (job, tree) = run_scan(path)?;
    let top = top.clamp(1, storage::scan::CHILDREN_LIMIT_MAX);
    if json {
        let largest: Vec<TreeRow> = tree
            .largest_files(SortOrder::Allocated)
            .into_iter()
            .take(top)
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "summary": tree.summary(),
                "warnings": tree.warnings(),
                "folders": folders_json(&tree, 0, depth, top),
                "largest_files": largest,
            }))?
        );
        return finish(&job);
    }
    print_scan_summary(&tree);
    if depth > 0 {
        println!();
        println!("{:>10}  {:>10}  name", "on disk", "size");
        print_folder(&tree, 0, depth, top, 0);
    }
    let largest = tree.largest_files(SortOrder::Allocated);
    if !largest.is_empty() {
        println!();
        println!("largest files");
        for row in largest.iter().take(top) {
            println!(
                "{:>10}  {:>10}  {}",
                size_text(row.allocated),
                size_text(row.logical),
                row.path.as_deref().unwrap_or(&row.name)
            );
        }
    }
    finish(&job)
}

fn print_scan_summary(tree: &ScanResult) {
    let s = tree.summary();
    println!("scanned             {}", s.root);
    println!(
        "on disk             {} in {} and {}",
        size_text(s.allocated_bytes),
        plural(s.files, "file", "files"),
        plural(s.folders, "folder", "folders")
    );
    println!("size                {}", size_text(s.logical_bytes));
    println!("took                {}", duration_text(s.elapsed_ms));
    if let Some(bytes) = s.not_reached_bytes.filter(|b| *b > 0) {
        println!("not reached         {}", size_text(bytes));
    }
    if s.online_only_bytes > 0 {
        println!("online-only         {}", size_text(s.online_only_bytes));
    }
    if s.denied_folders + s.unreadable_folders > 0 {
        println!(
            "unreadable          {}",
            plural(s.denied_folders + s.unreadable_folders, "folder", "folders")
        );
    }
    if s.hard_links_counted_once > 0 {
        println!(
            "hard links          {} counted once",
            grouped(s.hard_links_counted_once)
        );
    }
    if s.links_skipped > 0 {
        println!(
            "links               {} not followed",
            grouped(s.links_skipped)
        );
    }
    if !s.completed {
        println!("stopped             the numbers cover only what was scanned");
    }
    for warning in tree.warnings() {
        println!("note                {warning}");
    }
}

fn row_name(row: &TreeRow) -> String {
    match row.kind {
        TreeRowKind::Link => format!("{}  (link, not followed)", row.name),
        _ if row.denied => format!("{}  (can't read)", row.name),
        _ => row.name.clone(),
    }
}

fn print_folder(tree: &ScanResult, node: u32, depth: u32, top: usize, level: usize) {
    let Some(page) = tree.children(node, SortOrder::Allocated, top) else {
        return;
    };
    for row in &page.children {
        println!(
            "{:>10}  {:>10}  {}{}",
            size_text(row.allocated),
            size_text(row.logical),
            "  ".repeat(level),
            row_name(row)
        );
        if let (Some(child), true) = (row.node, depth > 1 && row.kind == TreeRowKind::Folder) {
            print_folder(tree, child, depth - 1, top, level + 1);
        }
    }
}

fn folders_json(tree: &ScanResult, node: u32, depth: u32, top: usize) -> Value {
    if depth == 0 {
        return Value::Array(Vec::new());
    }
    let Some(ChildrenPage { children, .. }) = tree.children(node, SortOrder::Allocated, top) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        children
            .iter()
            .map(|row| {
                let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
                if let (Some(child), Value::Object(map)) = (row.node, &mut value) {
                    if row.kind == TreeRowKind::Folder && depth > 1 {
                        map.insert(
                            "children".to_string(),
                            folders_json(tree, child, depth - 1, top),
                        );
                    }
                }
                value
            })
            .collect(),
    )
}

// ───────────────────────────── duplicates ─────────────────────────────

fn duplicates(path: &Path, min_size: u64, top: usize, json: bool) -> anyhow::Result<()> {
    if min_size < MIB {
        bail!("--min-size must be at least 1M");
    }
    let (scan_job, _) = run_scan(path)?;
    let request = DuplicatesRequest::new(scan_job.id, min_size)?;
    let (plan, job) = plan_or_start_duplicates(storage::lane(), &request, false)?;
    for note in &plan.notes {
        eprintln!("note: {note}");
    }
    let job = job.context("the duplicate search did not start")?;
    let finished = follow(job.id)?;
    let value = published(finished.id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return finish(&finished);
    }
    let result: DuplicatesResult =
        serde_json::from_value(value).context("read the duplicate search's result")?;
    print_duplicates(&result, top);
    finish(&finished)
}

fn print_duplicates(result: &DuplicatesResult, top: usize) {
    if result.group_count == 0 {
        println!(
            "No duplicate files of {} or more.",
            size_text(result.min_size)
        );
    } else {
        println!(
            "{} of identical files  ·  {} would be freed by keeping one copy of each",
            plural(result.group_count, "group", "groups"),
            size_text(result.wasted_bytes)
        );
    }
    let unreadable = result.skipped_in_use + result.skipped_unreadable;
    if unreadable > 0 {
        println!(
            "{} couldn't be read (in use or access denied)",
            plural(unreadable, "file", "files")
        );
    }
    if result.skipped_changed > 0 {
        println!(
            "{} changed since the scan",
            plural(result.skipped_changed, "file", "files")
        );
    }
    if !result.completed {
        println!("Stopped: only the files compared so far are listed.");
    }
    for group in result.groups.iter().take(top) {
        println!();
        println!(
            "{}  ·  {} each  ·  {} extra",
            plural(group.count, "copy", "copies"),
            size_text(group.size),
            size_text(group.wasted)
        );
        for file in &group.files {
            println!("    {}", file.path);
        }
        if group.more_files > 0 {
            println!("    … and {} more", grouped(group.more_files));
        }
    }
}
