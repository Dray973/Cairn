//! `optctl tools`: Windows maintenance tools (System File Checker, DISM, Optimize Drives,
//! Check Disk).
//!
//! `tools run` starts the tool as a background job and follows its output until it ends.
//! Ctrl+C ends optctl, not the tool: the tool keeps running on its own and its output keeps
//! going to the raw log file, but the audit log gets no final row for the run.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::Subcommand;
use optimizer_core::safety::state_log::Journal;
use optimizer_core::tools::{
    self, exit_code_hex, runner, JobId, JobSnapshot, JobState, ToolId, ToolInfo, ToolPlan,
    ToolRequest, ToolRunner, ToolVolume, MAX_LINES_PER_VIEW,
};

/// How often `tools run` polls the job.
const POLL: Duration = Duration::from_millis(250);
/// Width of the progress line on the terminal.
const PROGRESS_WIDTH: usize = 79;

#[derive(Subcommand, Debug)]
pub(crate) enum ToolsCmd {
    /// List the maintenance tools and the fixed drives the drive tools run on. Read-only.
    List {
        /// Print the tools and drives as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show what a maintenance tool would run and, with --yes, run it and follow its output.
    /// Ctrl+C ends optctl while the tool keeps running.
    Run {
        /// Tool id: sfc_verify, sfc_scan, dism_check, dism_scan, dism_restore,
        /// drive_optimize, drive_retrim or disk_check.
        #[arg(value_parser = parse_tool)]
        tool: ToolId,
        /// Drive the drive tools run on, e.g. C:.
        #[arg(long)]
        volume: Option<String>,
        /// Print the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Start the tool; without it only the plan is printed.
        #[arg(long)]
        yes: bool,
    },
}

fn parse_tool(value: &str) -> Result<ToolId, String> {
    ToolId::parse(value).ok_or_else(|| {
        let valid: Vec<&str> = ToolId::ALL.iter().map(|t| t.as_str()).collect();
        format!("expected one of: {}", valid.join(", "))
    })
}

pub(crate) fn run(
    cmd: ToolsCmd,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    match cmd {
        ToolsCmd::List { json } => list(json),
        ToolsCmd::Run {
            tool,
            volume,
            dry_run,
            yes,
        } => run_tool(tool, volume.as_deref(), dry_run, yes, open_journal),
    }
}

// ───────────────────────────── list ─────────────────────────────

fn list(json: bool) -> anyhow::Result<()> {
    let volumes = tools::volumes().context("read the fixed drives")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tools": tools::catalog(),
                "volumes": volumes,
            }))?
        );
        return Ok(());
    }
    for info in tools::catalog() {
        println!(
            "{:<16}{:<28}{}",
            info.id,
            info.title,
            command_pattern(info)?
        );
        println!("{:<16}{}", "", traits(info));
    }
    println!();
    println!("drives");
    for volume in &volumes {
        print_volume(volume);
    }
    Ok(())
}

/// The tool's command with `<drive>` standing for the drive of the drive tools.
fn command_pattern(info: &ToolInfo) -> anyhow::Result<String> {
    let request = ToolRequest::new(info.id, info.needs_volume.then_some("C:"))?;
    let args = request.args().into_iter().map(|a| {
        if info.needs_volume && a == "C:" {
            "<drive>".to_string()
        } else {
            a
        }
    });
    Ok(std::iter::once(info.program.to_string())
        .chain(args)
        .collect::<Vec<_>>()
        .join(" "))
}

fn traits(info: &ToolInfo) -> String {
    let stop = if info.cancellable {
        "can be stopped"
    } else {
        "runs to completion"
    };
    let effect = if info.requires_detach {
        "repairs can't be undone"
    } else if info.changes_system {
        "nothing to undo"
    } else {
        "read-only"
    };
    format!("{}  ·  {stop}  ·  {effect}", info.duration_hint)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
}

fn print_volume(volume: &ToolVolume) {
    let v = &volume.volume;
    let mut line = format!(
        "  {}  {:<12}{:<7}{:<9}{} free of {}",
        v.letter,
        if v.label.is_empty() { "-" } else { &v.label },
        v.file_system,
        serde_json::to_value(v.media)
            .ok()
            .and_then(|m| m.as_str().map(str::to_string))
            .unwrap_or_default(),
        gib(v.free_bytes),
        gib(v.size_bytes)
    );
    if v.system {
        line.push_str("  (Windows)");
    }
    println!("{line}");
    for (tool, blocked) in [
        ("optimize", &volume.optimize_blocked),
        ("retrim", &volume.retrim_blocked),
        ("check", &volume.check_blocked),
    ] {
        if let Some(reason) = blocked {
            println!("        no {tool}: {reason}");
        }
    }
}

// ───────────────────────────── run ─────────────────────────────

fn run_tool(
    tool: ToolId,
    volume: Option<&str>,
    dry_run: bool,
    yes: bool,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let request = ToolRequest::new(tool, volume)?;
    let runner = runner();
    let plan = runner.plan(&request)?;
    print_plan(&plan);
    if dry_run {
        return Ok(());
    }
    if !yes {
        bail!("the tool was not started; re-run with --yes");
    }
    if let Some(reason) = &plan.blocked_reason {
        bail!("{reason}");
    }
    let journal = Arc::new(open_journal()?);
    let job = runner.start(journal, &request)?;
    println!("log                 {}", job.log_path);
    println!();
    let finished = follow(runner, job.id)?;
    print_result(&finished);
    match finished.state {
        JobState::Succeeded | JobState::Completed => Ok(()),
        state => bail!("{} ended as {}", finished.title, state_label(state)),
    }
}

fn print_plan(plan: &ToolPlan) {
    println!("tool                {} ({})", plan.title, plan.tool);
    println!("command             {}", plan.command_line);
    println!("program             {}", plan.program);
    println!("takes               {}", plan.duration_hint);
    println!(
        "can be stopped      {}",
        if plan.cancellable { "yes" } else { "no" }
    );
    println!(
        "changes the system  {}",
        if plan.changes_system { "yes" } else { "no" }
    );
    for note in &plan.notes {
        println!("note                {note}");
    }
    match &plan.blocked_reason {
        Some(reason) => println!("blocked             {reason}"),
        None => println!("blocked             no"),
    }
}

/// Prints the job's output lines as they arrive, and its progress line on the terminal,
/// until it ends. Returns its final snapshot.
fn follow(runner: &ToolRunner, id: JobId) -> anyhow::Result<JobSnapshot> {
    let terminal = std::io::stderr().is_terminal();
    let mut after = 0;
    let mut shown: Option<String> = None;
    loop {
        let Some(view) = runner.view(id, after, MAX_LINES_PER_VIEW) else {
            bail!("lost track of tool job {id}");
        };
        if !view.lines.is_empty() {
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

fn state_label(state: JobState) -> &'static str {
    match state {
        JobState::Running => "running",
        JobState::Succeeded => "finished",
        JobState::Completed => "finished (read the result in the output)",
        JobState::Attention => "needs attention",
        JobState::Failed => "failed",
        JobState::Cancelled => "stopped",
    }
}

fn print_result(job: &JobSnapshot) {
    println!();
    println!("state               {}", state_label(job.state));
    if let Some(code) = job.exit_code {
        println!("exit code           {code} ({})", exit_code_hex(code));
    }
    if let Some(hint) = &job.hint {
        println!("hint                {hint}");
    }
    if let Some(summary) = &job.summary {
        println!("summary             {summary}");
    }
    if job.restart_required {
        println!("restart             Restart Windows to finish this repair.");
    }
    if !job.logged {
        println!("audit               the final row could not be written to the audit log");
    }
    println!("log                 {}", job.log_path);
}
