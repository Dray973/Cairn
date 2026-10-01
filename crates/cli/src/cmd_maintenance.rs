//! `optctl maintenance`: scheduled maintenance.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Args, Subcommand};
use optimizer_core::maintenance::{
    self, MaintenanceReport, MaintenanceRun, MaintenanceStatus, RunOrigin, RunPlan, RunRequest,
    ScheduleConfig, ScheduleDay, SchedulePlan, ScheduleResult, ScheduleTime, StepOutcome,
};
use optimizer_core::safety::state_log::{self, Journal};
use optimizer_core::safety::{rollback_filtered, RollbackFilter};

/// Exit code of `status --running` while a run is in progress.
const EXIT_RUN_IN_PROGRESS: i32 = 4;

#[derive(Subcommand, Debug)]
pub(crate) enum MaintenanceCmd {
    /// Show the schedule, the task and the latest runs. Read-only.
    Status {
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
        /// Print nothing; exit 4 while a maintenance run is in progress, 0 otherwise, 1 on
        /// error.
        #[arg(long, conflicts_with = "json")]
        running: bool,
    },
    /// Turn scheduled maintenance on, or change it. Starts from the registered schedule
    /// (else the defaults) and changes only the given options.
    Enable(EnableArgs),
    /// Turn scheduled maintenance off (undoes its journal record, which deletes the task).
    Disable {
        /// Show what would be removed; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Required to remove the task.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Remove a maintenance task the journal has no record of (irreversible).
        #[arg(long)]
        unrecorded: bool,
    },
    /// Start the scheduled task now; it runs in the background as scheduled.
    RunNow {
        /// Required to start it.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Run maintenance now in this process. Cleaned files are deleted permanently.
    Run(RunArgs),
    /// List the latest maintenance runs.
    Runs {
        /// Print the runs as JSON.
        #[arg(long)]
        json: bool,
        /// How many runs to list.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

#[derive(Args, Debug)]
pub(crate) struct EnableArgs {
    /// Day of the week (monday … sunday, or mon … sun).
    #[arg(long)]
    day: Option<String>,
    /// Time of day, HH:MM on the 24-hour clock.
    #[arg(long)]
    time: Option<String>,
    /// Cleanup locations to clean (comma-separated ids), e.g. user_temp,windows_temp.
    #[arg(long, value_delimiter = ',', conflicts_with = "no_cleanup")]
    targets: Option<Vec<String>>,
    /// Clean nothing; run only the checks.
    #[arg(long)]
    no_cleanup: bool,
    /// Skip the system file check (sfc /verifyonly).
    #[arg(long)]
    no_sfc: bool,
    /// Skip the component store check (DISM CheckHealth).
    #[arg(long)]
    no_dism: bool,
    /// Show the plan; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Required to register the task.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Args, Debug)]
pub(crate) struct RunArgs {
    /// Cleanup locations to clean (comma-separated ids).
    #[arg(long, value_delimiter = ',')]
    targets: Vec<String>,
    /// Check system files (sfc /verifyonly).
    #[arg(long)]
    sfc: bool,
    /// Check the component store (DISM CheckHealth).
    #[arg(long)]
    dism: bool,
    /// Show what the run would do; run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Required to run.
    #[arg(short = 'y', long)]
    yes: bool,
}

pub(crate) fn run(
    cmd: MaintenanceCmd,
    journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    match cmd {
        MaintenanceCmd::Status {
            json: _,
            running: true,
        } => running(journal),
        MaintenanceCmd::Status {
            json,
            running: false,
        } => status(json, open_journal),
        MaintenanceCmd::Enable(args) => enable(args, open_journal),
        MaintenanceCmd::Disable {
            dry_run,
            yes,
            unrecorded,
        } => disable(dry_run, yes, unrecorded, open_journal),
        MaintenanceCmd::RunNow { yes } => run_now(yes, open_journal),
        MaintenanceCmd::Run(args) => run_here(args, open_journal),
        MaintenanceCmd::Runs { json, limit } => runs(json, limit, open_journal),
    }
}

// ───────────────────────────── status ─────────────────────────────

/// `status --running`: exits 4 while a run is in progress; returns (exit 0) otherwise. A
/// missing journal is not created.
fn running(journal: Option<&Path>) -> anyhow::Result<()> {
    let path = journal
        .map(Path::to_path_buf)
        .unwrap_or_else(state_log::default_path);
    if maintenance::run_in_progress(&path)
        .with_context(|| format!("check the maintenance runs of {}", path.display()))?
    {
        std::process::exit(EXIT_RUN_IN_PROGRESS);
    }
    Ok(())
}

fn describe_config(config: &ScheduleConfig) -> String {
    let checks = match (config.system_file_check, config.component_store_check) {
        (true, true) => "system files and the component store",
        (true, false) => "system files",
        (false, true) => "the component store",
        (false, false) => "nothing",
    };
    let cleans = if config.targets.is_empty() {
        "nothing".to_string()
    } else {
        config.targets.join(", ")
    };
    format!(
        "every {} at {}; cleans {cleans}; checks {checks}",
        config.day.label(),
        config.time
    )
}

fn status(json: bool, open_journal: &dyn Fn() -> anyhow::Result<Journal>) -> anyhow::Result<()> {
    let journal = open_journal()?;
    let status = maintenance::status_in(&journal)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    print_status(&status);
    Ok(())
}

fn print_status(status: &MaintenanceStatus) {
    let schedule = match (&status.task, status.recorded) {
        (None, _) => "off".to_string(),
        (Some(_), false) => "not recorded by Cairn (remove it with `disable --unrecorded`)".into(),
        (Some(task), true) => {
            let when = task
                .config
                .as_ref()
                .map_or_else(|| "changed outside Cairn".into(), describe_config);
            if task.enabled {
                format!("on · {when}")
            } else {
                format!("on · {when} · disabled in Task Scheduler")
            }
        }
    };
    println!("schedule            {schedule}");
    if let Some(path) = &status.task_path {
        println!("task                {path}");
    }
    if let Some(task) = &status.task {
        if let Some(next) = &task.next_run_time {
            println!("next run            {next}");
        }
        if let Some(last) = &task.last_run_time {
            println!("last run            {last}");
        }
        if let Some(text) = &task.last_result_text {
            println!("last result         {text}");
        }
        for drift in &task.drift {
            println!("changed             {drift}");
        }
    }
    match (&status.program.path, &status.program.reason) {
        (Some(path), None) => println!("program             {path}"),
        (_, Some(reason)) => println!("program             not usable: {reason}"),
        (None, None) => println!("program             unknown"),
    }
    if let Some(reason) = &status.blocked_reason {
        println!("can't turn on       {reason}");
    }
    if status.running {
        println!("running             a run is in progress");
    }
    for warning in &status.warnings {
        println!("warning             {warning}");
    }
    if !status.runs.is_empty() {
        println!();
        print_runs(&status.runs);
    }
}

// ───────────────────────────── enable / disable ─────────────────────────────

fn edited_config(args: &EnableArgs, base: ScheduleConfig) -> anyhow::Result<ScheduleConfig> {
    let mut config = base;
    if let Some(day) = &args.day {
        config.day = ScheduleDay::parse(day)
            .with_context(|| format!("not a day of the week: {day:?}; expected monday … sunday"))?;
    }
    if let Some(time) = &args.time {
        config.time = ScheduleTime::parse(time)?;
    }
    if args.no_cleanup {
        config.targets.clear();
    } else if let Some(targets) = &args.targets {
        config.targets = targets.clone();
    }
    if args.no_sfc {
        config.system_file_check = false;
    }
    if args.no_dism {
        config.component_store_check = false;
    }
    Ok(config.validated()?)
}

fn print_plan(plan: &SchedulePlan) {
    let verb = if plan.unchanged {
        "unchanged"
    } else if plan.creates {
        "create"
    } else {
        "update"
    };
    println!("{verb} {}", plan.task_path);
    println!("  schedule   {}", describe_config(&plan.config));
    println!("  next run   {}", plan.next_run);
    println!("  runs as    {} with administrator rights", plan.account);
    println!("  program    {}", plan.program);
    println!("  arguments  {}", plan.arguments);
    if !plan.config.targets.is_empty() {
        println!("  each run permanently deletes the files in the selected locations");
    }
    for note in &plan.notes {
        println!("  note       {note}");
    }
    if let Some(reason) = &plan.blocked_reason {
        println!("  blocked    {reason}");
    }
}

fn enable(
    args: EnableArgs,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let journal = Arc::new(open_journal()?);
    let status = maintenance::status_in(&journal)?;
    let base = status
        .task
        .as_ref()
        .filter(|_| status.recorded)
        .and_then(|t| t.config.clone())
        .unwrap_or_else(ScheduleConfig::default_config);
    let config = edited_config(&args, base)?;
    let ScheduleResult::Plan(plan) =
        maintenance::set_schedule(Arc::clone(&journal), &config, true)?
    else {
        bail!("the plan could not be read");
    };
    print_plan(&plan);
    if args.dry_run {
        return Ok(());
    }
    if let Some(reason) = &plan.blocked_reason {
        bail!("{reason}");
    }
    if plan.unchanged {
        println!("The schedule is already set this way.");
        return Ok(());
    }
    if !args.yes {
        bail!("nothing changed; re-run with --yes");
    }
    match maintenance::set_schedule(journal, &config, false)? {
        ScheduleResult::Report(report) => {
            println!(
                "{} {} (next run {})",
                serde_json::to_value(report.outcome)?
                    .as_str()
                    .unwrap_or("done"),
                report.task_path,
                report.next_run.as_deref().unwrap_or("unknown")
            );
            Ok(())
        }
        ScheduleResult::Plan(_) => bail!("the schedule was not saved"),
    }
}

fn disable(
    dry_run: bool,
    yes: bool,
    unrecorded: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let journal = open_journal()?;
    if unrecorded {
        let plan = maintenance::remove_unrecorded(&journal, true)?;
        println!("remove {} (irreversible)", plan.task_path);
        if dry_run {
            return Ok(());
        }
        if !yes {
            bail!("nothing changed; re-run with --yes");
        }
        let done = maintenance::remove_unrecorded(&journal, false)?;
        println!(
            "{} {}",
            if done.removed == Some(true) {
                "removed"
            } else {
                "already gone:"
            },
            done.task_path
        );
        return Ok(());
    }
    let status = maintenance::status_in(&journal)?;
    let Some(path) = status.task_path.filter(|_| status.recorded) else {
        println!("Scheduled maintenance is off; Cairn has no record of a task to remove.");
        return Ok(());
    };
    let filter = RollbackFilter {
        task_definitions: vec![path],
        ..RollbackFilter::default()
    };
    let plan = rollback_filtered(&journal, &filter, true)?;
    for action in &plan.actions {
        println!("  {action}");
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        bail!("nothing changed; re-run with --yes");
    }
    let report = rollback_filtered(&journal, &filter, false)?;
    for failure in &report.failures {
        println!("failed: {}: {}", failure.target, failure.error);
    }
    if !report.failures.is_empty() {
        bail!("the maintenance task could not be removed");
    }
    println!("Scheduled maintenance is off; its task was removed from Task Scheduler.");
    Ok(())
}

fn run_now(yes: bool, open_journal: &dyn Fn() -> anyhow::Result<Journal>) -> anyhow::Result<()> {
    let journal = open_journal()?;
    if !yes {
        println!(
            "Maintenance starts now in the background and permanently deletes the files in the \
             selected locations; it keeps running when optctl exits."
        );
        bail!("nothing started; re-run with --yes");
    }
    let result = maintenance::run_now(&journal)?;
    println!(
        "Started {}; follow it with `optctl maintenance status`.",
        result.task_path
    );
    Ok(())
}

// ───────────────────────────── run ─────────────────────────────

fn print_run_plan(plan: &RunPlan) {
    for step in &plan.steps {
        println!("  {:<28}{}", step.title, step.detail);
    }
    for note in &plan.notes {
        println!("  note: {note}");
    }
    if let Some(reason) = &plan.blocked_reason {
        println!("  blocked: {reason}");
    }
}

fn outcome_text(outcome: StepOutcome) -> &'static str {
    match outcome {
        StepOutcome::Ok => "ok",
        StepOutcome::Attention => "needs attention",
        StepOutcome::Failed => "failed",
        StepOutcome::Skipped => "skipped",
        StepOutcome::NotRun => "not run",
        StepOutcome::LeftRunning => "left running",
        StepOutcome::Unknown => "see the log",
    }
}

/// The result of a run whose transcript was already echoed line by line.
fn print_report(report: &MaintenanceReport) {
    println!();
    if let Some(cleanup) = &report.cleanup {
        println!(
            "clean up                    {}",
            outcome_text(cleanup.outcome)
        );
    }
    for check in &report.checks {
        println!(
            "{:<28}{}: {}",
            check.title,
            outcome_text(check.outcome),
            check.text
        );
    }
    for item in &report.attention {
        println!("attention: {item}");
    }
    if let Some(log) = &report.log_path {
        println!("log: {log}");
    }
    println!(
        "{}: {}",
        serde_json::to_value(report.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        report.headline
    );
}

fn run_here(
    args: RunArgs,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    if args.targets.is_empty() && !args.sfc && !args.dism {
        bail!("name at least one of --targets, --sfc or --dism");
    }
    let request = RunRequest {
        targets: args.targets,
        system_file_check: args.sfc,
        component_store_check: args.dism,
        origin: RunOrigin::Cli,
    };
    let plan = maintenance::plan_run(&request)?;
    print_run_plan(&plan);
    if args.dry_run {
        return Ok(());
    }
    if !args.yes {
        bail!("nothing ran; re-run with --yes (cleaned files are deleted permanently)");
    }
    let journal = Arc::new(open_journal()?);
    println!();
    let report = maintenance::run_echoing(journal, &request, &|line| println!("{line}"))?;
    print_report(&report);
    std::process::exit(report.state.exit_code());
}

// ───────────────────────────── runs ─────────────────────────────

fn print_runs(runs: &[MaintenanceRun]) {
    for run in runs {
        let state = if run.stale {
            "interrupted".to_string()
        } else {
            serde_json::to_value(run.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        let headline = run
            .report
            .as_ref()
            .map(|r| r.headline.as_str())
            .or_else(|| run.progress.as_ref().map(|p| p.title.as_str()))
            .unwrap_or("");
        println!(
            "#{:<5} {:<22} {:<12} {headline}",
            run.id, run.started_at, state
        );
    }
}

fn runs(
    json: bool,
    limit: usize,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let journal = open_journal()?;
    let runs = maintenance::runs(&journal, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
    } else if runs.is_empty() {
        println!("Maintenance hasn't run yet.");
    } else {
        print_runs(&runs);
    }
    Ok(())
}
