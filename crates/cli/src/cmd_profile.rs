//! `optctl profile`: starter profiles, checking, planning, applying and exporting profiles of
//! Cairn settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Args, Subcommand};
use optimizer_core::debloat::RestartNeed;
use optimizer_core::profiles::step::{StepOutcome, StepStatus};
use optimizer_core::profiles::{
    self, Profile, ProfileApplyReport, ProfilePlan, ProfileSummary, SectionCounts,
};
use optimizer_core::safety::state_log::Journal;

#[derive(Subcommand, Debug)]
pub(crate) enum ProfileCmd {
    /// List the built-in starter profiles.
    Starters {
        /// Print the starters as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check a profile (a file or starter:<id>) and show what it contains. Reads nothing else.
    Show {
        /// A profile file (.json) or starter:gaming, starter:privacy, starter:clean.
        source: String,
        /// Print the profile summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What applying a profile would change on this PC. Read-only.
    Plan {
        /// A profile file (.json) or starter:<id>.
        source: String,
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Apply a profile; every change is journaled (undo with `optctl revert` or `optctl rollback`).
    Apply(ProfileApplyArgs),
    /// Write this PC's Cairn settings to a profile file.
    Export(ProfileExportArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ProfileApplyArgs {
    /// A profile file (.json) or starter:<id>.
    source: String,
    /// Apply only these row keys (comma-separated), e.g. tweak:gaming.game_mode,startup:user_run:Discord
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Also apply rows that start unselected (High risk, battery power plan, cautions).
    #[arg(long, conflicts_with = "only")]
    all: bool,
    /// System Restore point before the changes.
    #[arg(long, value_enum, default_value_t = crate::RestorePointArg::Try)]
    restore_point: crate::RestorePointArg,
    /// Required to apply; without it the plan is printed.
    #[arg(short = 'y', long)]
    yes: bool,
    /// Print the plan or the report as JSON (the report includes the undo filter).
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub(crate) struct ProfileExportArgs {
    /// Output file (.json). Without it, the candidate rows are printed and nothing is written.
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// Name of the profile.
    #[arg(long)]
    name: String,
    /// Optional description.
    #[arg(long, default_value = "")]
    description: String,
    /// Export only these row keys (comma-separated); by default every row that starts selected.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
}

pub(crate) fn run(
    cmd: ProfileCmd,
    _journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let journal = || -> anyhow::Result<Arc<Journal>> { Ok(Arc::new(open_journal()?)) };
    match cmd {
        ProfileCmd::Starters { json } => {
            let starters = profiles::starters();
            if json {
                println!("{}", serde_json::to_string_pretty(&starters)?);
            } else {
                for s in &starters {
                    println!(
                        "starter:{:<8}  {:<8}  {}",
                        s.id,
                        s.name,
                        counts_text(&s.counts)
                    );
                    println!("  {}", s.description);
                }
            }
        }
        ProfileCmd::Show { source, json } => {
            let profile = load(&source)?;
            let summary = profiles::summary(&profile);
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_profile(&profile, &summary);
            }
        }
        ProfileCmd::Plan { source, json } => {
            let profile = load(&source)?;
            let plan = profiles::plan(journal()?, &profile)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print_plan(&plan);
            }
        }
        ProfileCmd::Apply(args) => apply(args, &journal)?,
        ProfileCmd::Export(args) => export(args, &journal)?,
    }
    Ok(())
}

/// The profile `source` names: `starter:<id>` or a file.
fn load(source: &str) -> anyhow::Result<Profile> {
    if let Some(id) = source.strip_prefix("starter:") {
        return profiles::starter(id).with_context(|| {
            let ids: Vec<&str> = profiles::starters().iter().map(|s| s.id).collect();
            format!(
                "unknown starter profile {id:?}; starters: {}",
                ids.join(", ")
            )
        });
    }
    let bytes = profiles::read_file(Path::new(source))?;
    match profiles::parse_bytes(&bytes) {
        Ok(profile) => Ok(profile),
        Err(invalid) => bail!("{source}: {invalid}"),
    }
}

fn apply(
    args: ProfileApplyArgs,
    journal: &dyn Fn() -> anyhow::Result<Arc<Journal>>,
) -> anyhow::Result<()> {
    let profile = load(&args.source)?;
    let journal = journal()?;
    let plan = profiles::plan(journal.clone(), &profile)?;
    let keys: Option<Vec<String>> = if !args.only.is_empty() {
        Some(args.only.clone())
    } else if args.all {
        Some(
            plan.rows
                .iter()
                .filter(|r| r.status == StepStatus::Change)
                .map(|r| r.key.clone())
                .collect(),
        )
    } else {
        None
    };
    if !args.yes {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan);
        }
        let planned = match &keys {
            None => plan.rows.iter().filter(|r| r.selected).count(),
            Some(keys) => plan
                .rows
                .iter()
                .filter(|r| r.status == StepStatus::Change && keys.contains(&r.key))
                .count(),
        };
        if planned > 0 {
            bail!("{planned} change(s) planned; re-run with --yes to apply");
        }
        println!("nothing to apply");
        return Ok(());
    }
    let report = profiles::apply(
        journal,
        &profile,
        keys.as_deref(),
        args.restore_point.into(),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    if report.failed > 0 {
        bail!("{} change(s) failed", report.failed);
    }
    Ok(())
}

fn export(
    args: ProfileExportArgs,
    journal: &dyn Fn() -> anyhow::Result<Arc<Journal>>,
) -> anyhow::Result<()> {
    let journal = journal()?;
    let keys = (!args.only.is_empty()).then_some(args.only.as_slice());
    let Some(out) = args.out else {
        let candidates = profiles::candidates(journal)?;
        if let Some(other) = &candidates.other_account {
            println!("{other}");
        }
        let key_w = candidates
            .rows
            .iter()
            .map(|r| r.key.len())
            .max()
            .unwrap_or(0);
        for r in &candidates.rows {
            let chosen = match keys {
                Some(keys) => keys.contains(&r.key),
                None => r.selected,
            };
            println!(
                "  {} {:<key_w$}  {}   ({})",
                if chosen { "[x]" } else { "[ ]" },
                r.key,
                r.title,
                r.detail
            );
            if let Some(caution) = &r.caution {
                println!("      ⚠ {caution}");
            }
        }
        if candidates.rows.is_empty() {
            println!("no Cairn settings to export");
        }
        print_warnings(&candidates.warnings);
        println!("\nnothing was written; pass --out <file.json> to save the profile");
        return Ok(());
    };
    let out = std::path::absolute(&out).with_context(|| format!("resolve {}", out.display()))?;
    let report = profiles::export(journal, &out, &args.name, &args.description, keys)?;
    println!(
        "wrote {} ({}): {}",
        report.path,
        report.name,
        counts_text(&report.counts)
    );
    if !report.missing.is_empty() {
        println!(
            "left out, no longer settings of this PC: {}",
            report.missing.join(", ")
        );
    }
    Ok(())
}

// ───────────────────────────── output ─────────────────────────────

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// "10 settings · 21 apps · 2 startup apps · DNS · Windows Update · maintenance".
fn counts_text(counts: &SectionCounts) -> String {
    let mut parts = Vec::new();
    if counts.tweaks > 0 {
        parts.push(plural(counts.tweaks, "setting"));
    }
    if counts.apps > 0 {
        parts.push(plural(counts.apps, "app"));
    }
    if counts.startup > 0 {
        parts.push(plural(counts.startup, "startup app"));
    }
    if counts.dns > 0 {
        parts.push("DNS".into());
    }
    if counts.windows_update > 0 {
        parts.push("Windows Update".into());
    }
    if counts.maintenance > 0 {
        parts.push("maintenance".into());
    }
    parts.join(" · ")
}

fn restart_text(restart: RestartNeed) -> Option<&'static str> {
    match restart {
        RestartNeed::None => None,
        RestartNeed::Explorer => Some("File Explorer has to restart for some of these changes."),
        RestartNeed::SignOut => Some("Sign out and back in to finish applying these changes."),
        RestartNeed::Restart => Some("Restart Windows to finish applying these changes."),
    }
}

fn print_warnings(warnings: &[String]) {
    if !warnings.is_empty() {
        println!("\nwarnings:");
        for w in warnings {
            println!("  {w}");
        }
    }
}

fn print_profile(profile: &Profile, summary: &ProfileSummary) {
    println!("{}", summary.name);
    if !summary.description.is_empty() {
        println!("  {}", summary.description);
    }
    if let Some(created) = &summary.created {
        println!("  created {created}");
    }
    if let Some(with) = &summary.created_with {
        println!("  written by {with}");
    }
    println!("  {}", counts_text(&summary.counts));
    for id in &profile.tweaks {
        println!("  tweak      {id}");
    }
    for app in &profile.apps {
        println!("  app        {app}");
    }
    for entry in &profile.startup {
        if entry.name.is_empty() {
            println!("  startup    {}", entry.id);
        } else {
            println!("  startup    {} ({})", entry.id, entry.name);
        }
    }
    for (kind, families) in [
        ("ethernet", &profile.dns.ethernet),
        ("wifi", &profile.dns.wifi),
    ] {
        if let Some(f) = families {
            println!(
                "  dns        {kind}: IPv4 {}, IPv6 {}",
                f.ipv4.as_deref().unwrap_or("unchanged"),
                f.ipv6.as_deref().unwrap_or("unchanged")
            );
        }
    }
    if let Some(wu) = &profile.windows_update {
        println!(
            "  windows    {}",
            serde_json::to_string(wu).unwrap_or_default()
        );
    }
    if let Some(m) = &profile.maintenance {
        println!(
            "  schedule   {}",
            serde_json::to_string(m).unwrap_or_default()
        );
    }
}

fn status_word(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Change => "change",
        StepStatus::Already => "already",
        StepStatus::Skipped => "skipped",
    }
}

fn print_plan(plan: &ProfilePlan) {
    println!(
        "{}: {} to change, {} already set, {} skipped (planned in {} ms; nothing was changed)",
        plan.name, plan.changes, plan.already, plan.skipped, plan.duration_ms
    );
    println!("[x] applied by default   [ ] change that starts unselected   [-] no change");
    if let Some(other) = &plan.other_account {
        println!("{other}");
    }
    let key_w = plan.rows.iter().map(|r| r.key.len()).max().unwrap_or(0);
    let title_w = plan
        .rows
        .iter()
        .map(|r| r.title.chars().count())
        .max()
        .unwrap_or(0);
    for r in &plan.rows {
        let marker = match (r.status, r.selected) {
            (StepStatus::Change, true) => "[x]",
            (StepStatus::Change, false) => "[ ]",
            _ => "[-]",
        };
        let detail = match r.reason {
            Some(reason) => format!("{}: {}", profiles::reason_text(reason), r.detail),
            None => r.detail.clone(),
        };
        println!(
            "  {marker} {:<7}  {:<key_w$}   {:<title_w$}   ({detail})",
            status_word(r.status),
            r.key,
            r.title
        );
        if let Some(caution) = &r.caution {
            println!("      ⚠ {caution}");
        }
    }
    print_warnings(&plan.warnings);
    if let Some(text) = restart_text(plan.restart) {
        println!("\nafter applying: {text}");
    }
    if !plan.elevated && plan.changes > 0 {
        println!("\napplying needs an elevated (Administrator) process");
    }
}

fn outcome_word(outcome: StepOutcome) -> &'static str {
    match outcome {
        StepOutcome::Applied => "applied",
        StepOutcome::AlreadySet => "already set",
        StepOutcome::Skipped => "skipped",
        StepOutcome::Failed => "failed",
    }
}

fn print_report(report: &ProfileApplyReport) {
    match (report.session_id, &report.restore_point) {
        (Some(id), Some(rp)) => println!(
            "session #{id}, restore point #{} \"{}\"",
            rp.sequence, rp.description
        ),
        (Some(id), None) => println!("session #{id}, no restore point"),
        (None, _) => println!("nothing was applied; no session was opened"),
    }
    for r in &report.results {
        println!("  {:<11}  {}  {}", outcome_word(r.outcome), r.key, r.title);
        for detail in &r.details {
            println!("  {:<11}    {detail}", "");
        }
    }
    print_warnings(&report.warnings);
    println!(
        "\n{} applied, {} already set, {} skipped, {} failed",
        report.applied, report.already, report.skipped, report.failed
    );
    if report.applied > 0 {
        if let Some(text) = restart_text(report.restart) {
            println!("{text}");
        }
    }
    if !report.undo.is_empty() {
        println!(
            "every change is journaled: undo them from History in Cairn (--json prints the undo \
             filter), or revert everything with `optctl rollback`"
        );
    }
}
