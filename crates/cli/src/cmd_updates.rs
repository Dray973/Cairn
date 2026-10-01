//! `optctl updates`: winget app updates and installs, the install list and Windows Update
//! settings.
//!
//! `updates list`, `upgrade` and `install` run winget as background jobs of the engine and
//! follow them until they end. Ctrl+C ends optctl and with it the batch thread: the current
//! app's winget keeps running on its own, the remaining apps are not started, and no final
//! or left_running row is written for the current app. Windows Update settings are
//! journaled registry values; `wu-undo` restores them.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use optimizer_core::jobs::{HostJobId, HostJobSnapshot, JobState};
use optimizer_core::safety::state_log::Journal;
use optimizer_core::safety::{
    rollback_filtered, RestorePointPolicy, RollbackFilter, Safety, SafetyOptions,
};
use optimizer_core::tools::MAX_LINES_PER_VIEW;
use optimizer_core::updates::apps::{self, AppCategory, AppList};
use optimizer_core::updates::winget::{
    self, lane, Availability, BatchResult, ItemState, ScanResult, UpdateItem, UpdatesKind,
    UpdatesPlan, UpdatesRequest, UpgradeRow, WingetStatus,
};
use optimizer_core::updates::wu::{self, ServiceState, WuChange, WuLayout, WuSettingId, WuState};
use optimizer_core::Error;

/// How often a followed job is polled.
const POLL: Duration = Duration::from_millis(250);
/// Width of the progress line on the terminal.
const PROGRESS_WIDTH: usize = 79;

#[derive(Subcommand, Debug)]
pub(crate) enum UpdatesCmd {
    /// winget's status for this account and a Windows Update summary. Runs only
    /// `winget --version`.
    Status {
        /// Print the status as JSON (starts no process).
        #[arg(long)]
        json: bool,
    },
    /// Check for app updates with winget and list them. Read-only.
    List {
        /// Print the check's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check for app updates, then update the chosen apps one after another. App updates
    /// can't be undone; each one is recorded in the audit log.
    Upgrade {
        /// Package ids from the check.
        ids: Vec<String>,
        /// Every app the check offers, except apps winget updates only by name and
        /// Microsoft Store apps.
        #[arg(long)]
        all: bool,
        /// With --all, also Microsoft Store apps.
        #[arg(long)]
        include_store: bool,
        /// Print the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Start the updates; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
    /// Install apps with winget one after another; apps already installed are skipped.
    /// Installs can't be undone by Cairn; each one is recorded in the audit log.
    Install {
        /// winget package ids.
        ids: Vec<String>,
        /// Every app of the install list (`optctl updates apps`).
        #[arg(long)]
        from_list: bool,
        /// Print the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Start the installs; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
    /// The install list the Install apps view offers. Read-only.
    Apps {
        /// Print the list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The Windows Update settings. Read-only.
    Wu {
        /// Print the settings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change a Windows Update setting, journaled so it can be undone: pause 14|off,
    /// active-hours 8-17|auto, exclude-drivers on|off, defer-feature 90|off,
    /// restart-notify on|off.
    ///
    /// While updates are paused, pause N extends the pause: its end moves N days later, to at
    /// most 35 days after the pause began.
    WuSet {
        /// pause, active-hours, exclude-drivers, defer-feature or restart-notify.
        setting: String,
        /// The new value, such as 14, 8-17, auto, on or off.
        value: String,
        /// Print what would be written and stop.
        #[arg(long)]
        dry_run: bool,
        /// Make the change; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
    /// Put a Windows Update setting back the way it was before Cairn first changed it.
    WuUndo {
        /// pause, active-hours, exclude-drivers, defer-feature or restart-notify.
        setting: String,
        /// Print what would be restored and stop.
        #[arg(long)]
        dry_run: bool,
        /// Restore the values; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
}

pub(crate) fn run(
    cmd: UpdatesCmd,
    _journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    match cmd {
        UpdatesCmd::Status { json } => status(json, open_journal),
        UpdatesCmd::List { json } => list(json),
        UpdatesCmd::Upgrade {
            ids,
            all,
            include_store,
            dry_run,
            yes,
        } => upgrade(&ids, all, include_store, dry_run, yes, open_journal),
        UpdatesCmd::Install {
            ids,
            from_list,
            dry_run,
            yes,
        } => install(&ids, from_list, dry_run, yes, open_journal),
        UpdatesCmd::Apps { json } => print_apps(&apps::app_list(), json),
        UpdatesCmd::Wu { json } => {
            let journal = open_journal().ok();
            let state = wu::wu_state(journal.as_ref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                print_wu_state(&state);
            }
            Ok(())
        }
        UpdatesCmd::WuSet {
            setting,
            value,
            dry_run,
            yes,
        } => wu_set(&setting, &value, dry_run, yes, open_journal),
        UpdatesCmd::WuUndo {
            setting,
            dry_run,
            yes,
        } => wu_undo(&setting, dry_run, yes, open_journal),
    }
}

/// The journal opener a job that writes no rows is given; never called.
fn no_journal() -> optimizer_core::Result<Arc<Journal>> {
    Err(Error::Other("this job opens no journal".into()))
}

// ───────────────────────────── status ─────────────────────────────

fn availability_text(availability: Availability) -> &'static str {
    match availability {
        Availability::Ready => "ready",
        Availability::Missing => "not set up for this account",
        Availability::OtherUser => "turned off: Cairn runs as another account",
        Availability::UserUnknown => "turned off: the signed-in account is unknown",
    }
}

fn print_winget_status(status: &WingetStatus) {
    println!(
        "winget              {}",
        availability_text(status.availability)
    );
    if let Some(message) = &status.message {
        println!("                    {message}");
    }
    if let Some(location) = &status.location {
        println!(
            "App Installer       {} at {}",
            location.package_version,
            location
                .path
                .parent()
                .map_or_else(|| location.path.display(), |dir| dir.display())
        );
    }
    println!("needs               winget {} or newer", status.min_version);
}

fn status(json: bool, open_journal: &dyn Fn() -> anyhow::Result<Journal>) -> anyhow::Result<()> {
    let status = winget::winget_status();
    let journal = open_journal().ok();
    let state = wu::wu_state(journal.as_ref());
    if json {
        let windows_update = match &state {
            Ok(state) => serde_json::to_value(state)?,
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "winget": status,
                "windows_update": windows_update,
            }))?
        );
        return Ok(());
    }
    print_winget_status(&status);
    if status.availability == Availability::Ready {
        match winget::probe_version() {
            Ok(Some(version)) => println!("version             {version}"),
            Ok(None) => println!("version             winget is not installed"),
            Err(e) => println!("version             unknown: {e}"),
        }
    }
    println!();
    match &state {
        Ok(state) => {
            println!("{}", edition_line(state));
            println!("update service      {}", service_text(state.service));
            if let Some(pause) = state.settings.iter().find(|s| s.id == WuSettingId::Pause) {
                println!("Windows Update      {}", wu::value_text(&pause.value));
            }
            if state.restart_pending {
                println!("restart             Windows is waiting for a restart to finish installing updates.");
            }
            for note in &state.managed {
                println!("note                {note}");
            }
        }
        Err(e) => println!("Windows Update      cannot read: {e}"),
    }
    Ok(())
}

// ───────────────────────────── jobs ─────────────────────────────

/// Follows a job until it ends, printing its output lines when `echo` is set and its
/// progress line on the terminal. Returns its final snapshot.
fn follow(id: HostJobId, echo: bool) -> anyhow::Result<HostJobSnapshot> {
    let terminal = std::io::stderr().is_terminal();
    let mut after = 0;
    let mut shown: Option<String> = None;
    loop {
        let Some(view) = lane().view(id, after, MAX_LINES_PER_VIEW) else {
            bail!("lost track of winget job {id}");
        };
        if echo && !view.lines.is_empty() {
            if shown.take().is_some() {
                clear_progress();
            }
            let mut out = std::io::stdout().lock();
            for line in &view.lines {
                writeln!(out, "{line}")?;
            }
            out.flush()?;
        }
        after = view.next;
        if view.job.state.is_finished() && !view.more {
            if shown.is_some() {
                clear_progress();
            }
            return Ok(view.job);
        }
        if terminal && view.job.progress_line != shown {
            if let Some(text) = &view.job.progress_line {
                let fitted: String = text.chars().take(PROGRESS_WIDTH).collect();
                eprint!("\r{fitted:<width$}", width = PROGRESS_WIDTH);
                let _ = std::io::stderr().flush();
            }
            shown = view.job.progress_line.clone();
        }
        if !view.more {
            thread::sleep(POLL);
        }
    }
}

fn clear_progress() {
    eprint!("\r{:width$}\r", "", width = PROGRESS_WIDTH);
    let _ = std::io::stderr().flush();
}

/// The job's published result as JSON.
fn published(id: HostJobId) -> anyhow::Result<serde_json::Value> {
    let (_, value) = lane()
        .result(id, 0)
        .ok_or_else(|| anyhow!("winget job {id} published no result"))?;
    Ok((*value).clone())
}

/// Runs a check for app updates and returns its published result.
fn check() -> anyhow::Result<ScanResult> {
    let outcome = winget::plan_or_start(&UpdatesRequest::scan(), false, no_journal)?;
    let job = outcome
        .job
        .ok_or_else(|| anyhow!("the check for app updates did not start"))?;
    let finished = follow(job.id, false)?;
    if finished.state == JobState::Cancelled {
        bail!("the check for app updates was stopped");
    }
    serde_json::from_value::<ScanResult>(published(job.id)?).context("read the check's result")
}

// ───────────────────────────── list ─────────────────────────────

fn column_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values
        .map(|v| v.chars().count())
        .max()
        .unwrap_or(0)
        .max(header.len())
}

fn print_upgrades(rows: &[UpgradeRow]) {
    let name_w = column_width("Name", rows.iter().map(|r| r.name.as_str()));
    let id_w = column_width("Id", rows.iter().map(|r| r.id.as_str()));
    let installed_w = column_width("Installed", rows.iter().map(|r| r.installed.as_str()));
    let available_w = column_width("Available", rows.iter().map(|r| r.available.as_str()));
    println!(
        "{:<name_w$}  {:<id_w$}  {:<installed_w$}  {:<available_w$}  Source",
        "Name", "Id", "Installed", "Available"
    );
    for row in rows {
        println!(
            "{:<name_w$}  {:<id_w$}  {:<installed_w$}  {:<available_w$}  {}",
            row.name, row.id, row.installed, row.available, row.source
        );
        if let Some(note) = &row.note {
            println!("{:<name_w$}  {note}", "");
        }
    }
}

fn list(json: bool) -> anyhow::Result<()> {
    let result = check()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if let Some(error) = &result.error {
        bail!("{}", error.message);
    }
    if !result.upgrades.is_empty() {
        print_upgrades(&result.upgrades);
        println!();
    }
    let version = result
        .winget_version
        .as_deref()
        .map(|v| format!("  ·  winget {v}"))
        .unwrap_or_default();
    match result.upgrades.len() {
        0 => println!("All apps are up to date{version}"),
        1 => println!("1 update available{version}"),
        n => println!("{n} updates available{version}"),
    }
    for warning in &result.warnings {
        println!("warning: {warning}");
    }
    if result.unparsed_rows > 0 {
        println!(
            "warning: {} line(s) of winget's list could not be read; those apps are not shown.",
            result.unparsed_rows
        );
    }
    if !result.inventory_complete {
        println!(
            "warning: Installed apps could not be read, so some apps may be missing from the list."
        );
    }
    Ok(())
}

// ───────────────────────────── batches ─────────────────────────────

fn print_plan(plan: &UpdatesPlan) {
    println!("plan                {}", plan.title);
    if let Some(program) = &plan.program {
        println!("program             {program}");
    }
    for line in &plan.command_lines {
        println!("command             {line}");
    }
    if plan.irreversible {
        println!("undo                none: app updates and installs can't be undone by Cairn");
    }
    for note in &plan.notes {
        println!("note                {note}");
    }
    match &plan.blocked_reason {
        Some(reason) => println!("blocked             {reason}"),
        None => println!("blocked             no"),
    }
}

fn item_state_text(kind: UpdatesKind, state: ItemState) -> &'static str {
    match state {
        ItemState::Queued => "waiting",
        ItemState::Running => "running",
        ItemState::Succeeded if kind == UpdatesKind::Install => "installed",
        ItemState::Succeeded => "updated",
        ItemState::AlreadyCurrent => "already up to date",
        ItemState::AlreadyInstalled => "already installed",
        ItemState::RestartRequired => "restart Windows to finish",
        ItemState::Failed => "failed",
        ItemState::TimedOut => "still running after 60 min",
        ItemState::LeftRunning => "left running",
        ItemState::NotStarted => "not started",
        ItemState::Skipped => "skipped",
    }
}

fn print_batch(batch: &BatchResult) {
    println!();
    for item in &batch.items {
        let mut line = format!(
            "{:<26}{} ({})",
            item_state_text(batch.kind, item.state),
            item.name,
            item.id
        );
        if let (Some(from), Some(to)) = (&item.from, &item.to) {
            line.push_str(&format!("  {from} → {to}"));
        }
        println!("{line}");
        if let Some(message) = &item.message {
            let code = item
                .exit_code_hex
                .as_deref()
                .map(|hex| format!(" (exit {hex})"))
                .unwrap_or_default();
            println!("{:<26}{message}{code}", "");
        }
    }
    if batch.restart_required {
        println!();
        println!("Restart Windows to finish.");
    }
}

/// Prints the plan of `request` and, with `yes`, starts it and follows it to the end.
fn run_batch(
    request: &UpdatesRequest,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let plan = winget::plan_or_start(request, true, no_journal)?.plan;
    print_plan(&plan);
    if dry_run {
        return Ok(());
    }
    if !yes {
        let n = request.items().len();
        bail!(
            "{n} app{} planned; nothing was started. Re-run with --yes",
            if n == 1 { "" } else { "s" }
        );
    }
    if let Some(reason) = &plan.blocked_reason {
        bail!("{reason}");
    }
    let open = || {
        open_journal()
            .map(Arc::new)
            .map_err(|e| Error::Other(format!("{e:#}")))
    };
    let job = winget::plan_or_start(request, false, open)?
        .job
        .ok_or_else(|| anyhow!("the job did not start"))?;
    if let Some(log) = &job.log_path {
        println!("log                 {log}");
    }
    println!();
    let finished = follow(job.id, true)?;
    let batch = serde_json::from_value::<BatchResult>(published(job.id)?)
        .context("read the batch's result")?;
    print_batch(&batch);
    if let Some(summary) = &finished.summary {
        println!();
        println!("{summary}");
    }
    match finished.state {
        JobState::Succeeded | JobState::Completed => Ok(()),
        _ => bail!("{} did not finish cleanly", finished.title),
    }
}

fn update_item(row: &UpgradeRow) -> UpdateItem {
    UpdateItem {
        id: row.id.clone(),
        source: row.source.clone(),
        name: row.name.clone(),
        from: Some(row.installed.clone()),
        to: Some(row.available.clone()),
    }
}

/// The apps `upgrade` updates: the named ids, which must be in the check, and with `all`
/// every row Update all takes.
fn chosen_upgrades(
    rows: &[UpgradeRow],
    ids: &[String],
    all: bool,
    include_store: bool,
) -> anyhow::Result<Vec<UpdateItem>> {
    let mut items: Vec<UpdateItem> = Vec::new();
    if all {
        items.extend(
            rows.iter()
                .filter(|r| winget::left_out_of_update_all(r, include_store).is_none())
                .map(update_item),
        );
    }
    for id in ids {
        let row = rows
            .iter()
            .find(|r| r.id.eq_ignore_ascii_case(id.trim()))
            .ok_or_else(|| anyhow!("no update available for {id}"))?;
        if !row.selectable {
            bail!(
                "{} can't be updated by Cairn: {}",
                row.id,
                row.note.as_deref().unwrap_or("winget can't target it")
            );
        }
        if !items.iter().any(|i| i.id.eq_ignore_ascii_case(&row.id)) {
            items.push(update_item(row));
        }
    }
    Ok(items)
}

fn upgrade(
    ids: &[String],
    all: bool,
    include_store: bool,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    if include_store && !all {
        bail!("--include-store works with --all");
    }
    if ids.is_empty() && !all {
        bail!("name the apps to update, or use --all");
    }
    let result = check()?;
    if let Some(error) = &result.error {
        bail!("{}", error.message);
    }
    let items = chosen_upgrades(&result.upgrades, ids, all, include_store)?;
    if items.is_empty() {
        // Named ids always give apps or an error, so only --all can take none.
        for line in winget::update_all_takes_none(&result.upgrades, include_store) {
            println!("{line}");
        }
        return Ok(());
    }
    let request = UpdatesRequest::new(UpdatesKind::Upgrade, items)?;
    run_batch(&request, dry_run, yes, open_journal)
}

fn install(
    ids: &[String],
    from_list: bool,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let mut items: Vec<UpdateItem> = ids
        .iter()
        .map(|id| UpdateItem {
            id: id.trim().to_string(),
            source: "winget".into(),
            name: String::new(),
            from: None,
            to: None,
        })
        .collect();
    if from_list {
        let list = apps::app_list();
        for warning in &list.warnings {
            println!("warning: {warning}");
        }
        items.extend(list.apps.into_iter().map(|app| UpdateItem {
            id: app.id,
            source: app.source,
            name: app.name,
            from: None,
            to: None,
        }));
    }
    if items.is_empty() {
        bail!("name the apps to install, or use --from-list");
    }
    let request = UpdatesRequest::new(UpdatesKind::Install, items)?;
    run_batch(&request, dry_run, yes, open_journal)
}

// ───────────────────────────── install list ─────────────────────────────

fn print_apps(list: &AppList, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    for category in AppCategory::ALL {
        let apps: Vec<_> = list
            .apps
            .iter()
            .filter(|a| a.category == category)
            .collect();
        if apps.is_empty() {
            continue;
        }
        println!("{}", category.title());
        let name_w = column_width("", apps.iter().map(|a| a.name.as_str()));
        for app in apps {
            println!("  {:<name_w$}  {}", app.name, app.id);
        }
    }
    println!();
    let origin = if list.custom {
        "your list"
    } else {
        "the built-in list"
    };
    println!("{} apps from {origin}  ·  {}", list.apps.len(), list.path);
    for warning in &list.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

// ───────────────────────────── Windows Update ─────────────────────────────

fn edition_line(state: &WuState) -> String {
    let edition = &state.edition;
    let mut line = edition.name.clone();
    if let Some(version) = &edition.version {
        line.push(' ');
        line.push_str(version);
    }
    if !edition.build.is_empty() {
        line.push_str(&format!("  ·  build {}", edition.build));
    }
    line
}

fn service_text(service: ServiceState) -> &'static str {
    match service {
        ServiceState::Automatic => "starts automatically",
        ServiceState::Manual => "starts when needed",
        ServiceState::Disabled => "disabled: Windows doesn't check for updates",
        ServiceState::Missing => "missing",
        ServiceState::Unknown => "unknown",
    }
}

fn print_wu_state(state: &WuState) {
    println!("{}", edition_line(state));
    println!("update service      {}", service_text(state.service));
    if state.restart_pending {
        println!(
            "restart             Windows is waiting for a restart to finish installing updates."
        );
    }
    for note in &state.managed {
        println!("note                {note}");
    }
    println!();
    for setting in &state.settings {
        let mut line = format!("{:<32}{}", setting.title, wu::value_text(&setting.value));
        if setting.by_cairn && setting.differs {
            line.push_str("  ·  set by Cairn (wu-undo restores it)");
        }
        println!("{line}");
        if let Some(reason) = &setting.unavailable_reason {
            println!("{:<32}unavailable: {reason}", "");
        }
        if let Some(caveat) = &setting.caveat {
            println!("{:<32}{caveat}", "");
        }
    }
    for warning in &state.warnings {
        println!("warning: {warning}");
    }
}

/// Prints the dry run with `--dry-run` or without `--yes`; with `--yes` alone it makes the
/// change at once and prints only what was written.
fn wu_set(
    setting: &str,
    value: &str,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let change = WuChange::parse_cli(setting, value)?;
    let begin = || {
        let journal = open_journal().map_err(|e| Error::Other(format!("{e:#}")))?;
        Safety::begin(
            Arc::new(journal),
            SafetyOptions {
                label: change.label(),
                restore_point: RestorePointPolicy::Skip,
                require_elevation: true,
                ..Default::default()
            },
        )
    };
    for line in wu::cli_set_lines(&change, dry_run, yes, begin)? {
        println!("{line}");
    }
    if !dry_run && !yes {
        bail!("nothing was changed; re-run with --yes");
    }
    Ok(())
}

fn wu_undo(
    setting: &str,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let id = WuSettingId::parse(setting).ok_or_else(|| {
        anyhow!(
            "unknown Windows Update setting {setting:?}; expected pause, active-hours, \
             exclude-drivers, defer-feature or restart-notify"
        )
    })?;
    let filter = RollbackFilter {
        registry: WuLayout::system().targets(id),
        ..Default::default()
    };
    let journal = open_journal()?;
    let preview = rollback_filtered(&journal, &filter, true)?;
    if dry_run || !yes {
        crate::print_rollback(&preview);
        if !dry_run {
            bail!("nothing was reverted; re-run with --yes");
        }
        return Ok(());
    }
    let report = rollback_filtered(&journal, &filter, false)?;
    crate::print_rollback(&report);
    if !report.is_clean() {
        bail!("some values could not be restored");
    }
    Ok(())
}
