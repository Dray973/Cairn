//! optctl: Cairn's command-line tool, a headless front-end to optimizer_core.

mod cmd_health;
mod cmd_maintenance;
mod cmd_net;
mod cmd_perm;
mod cmd_profile;
mod cmd_storage;
mod cmd_sysinfo;
mod cmd_tools;
mod cmd_updates;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use optimizer_core::app;
use optimizer_core::debloat::requirements::Unavailable;
use optimizer_core::debloat::{
    self, catalog, ApplyOptions, ApplyReport, CatalogEntry, Category, DriftKind, DriftReport,
    Engine, ItemOutcome, ItemState, Mismatch, RestartNeed, Risk, ScanReport,
};
use optimizer_core::maintenance;
use optimizer_core::safety::state_log::{self, Journal};
use optimizer_core::safety::{self, rollback, RestorePointPolicy, RollbackReport};
use optimizer_core::win::console_text::oem_code_page;
use optimizer_core::win::registry::{read_value, split_path};
use optimizer_core::win::scm::{Scm, READ_ACCESS};
use optimizer_core::win::task_scheduler::TaskScheduler;

#[derive(Parser, Debug)]
#[command(
    name = "optctl",
    version = optimizer_core::VERSION,
    about = "Cairn command-line tool: scan, apply, revert and inspect the journal"
)]
struct Cli {
    /// Cairn's journal database (default: journal.db in %OPTIMIZER_DATA_DIR%, else in
    /// %LOCALAPPDATA%\PCOptimizer).
    #[arg(long, global = true, env = "OPTIMIZER_JOURNAL")]
    journal: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Environment diagnostics: elevation, System Protection, journal state.
    Doctor,
    /// Create a System Restore point.
    RestorePoint(RestorePointArgs),
    /// Inspect the state journal.
    Journal {
        #[command(subcommand)]
        cmd: JournalCmd,
    },
    /// Revert every journaled change to its recorded baseline.
    Rollback(RollbackArgs),
    /// List every tweak and bloatware package the engine knows, without scanning.
    Catalog(CatalogArgs),
    /// Show the current state of every catalog item. Read-only.
    Scan(ScanArgs),
    /// Apply items by id, or every recommended item of a category.
    Apply(ApplyArgs),
    /// Revert journaled changes by item id, by category or all of them.
    Revert(RevertArgs),
    /// Recorded changes whose targets no longer hold what Cairn set. Read-only. Exit code 0:
    /// none; 3: some; 1: the check failed.
    Drift(DriftArgs),
    /// The same check as `drift`, for the given tweaks only. Read-only. Exit code 0: none;
    /// 3: some; 1: the check failed.
    Verify(VerifyArgs),
    /// Read a registry value, e.g. `reg-get "HKLM\SOFTWARE\Policies\Microsoft\Windows\DataCollection" AllowTelemetry`.
    RegGet {
        key: String,
        #[arg(default_value = "")]
        value: String,
    },
    /// Show a service's configuration and status.
    SvcGet { name: String },
    /// Show a scheduled task's enabled flag and state, e.g. `task-get "\Microsoft\Windows\Autochk\Proxy"`.
    TaskGet { path: String },
    /// Network adapters, DNS servers, the DNS cache, DHCP leases and the network stack.
    Net {
        #[command(subcommand)]
        cmd: cmd_net::NetCmd,
    },
    /// Windows maintenance tools: System File Checker, DISM, Optimize Drives and Check Disk.
    Tools {
        #[command(subcommand)]
        cmd: cmd_tools::ToolsCmd,
    },
    /// Read-only summary of the hardware and Windows configuration.
    Sysinfo(cmd_sysinfo::SysinfoArgs),
    /// Security checkup and boot history. Read-only.
    Health {
        #[command(subcommand)]
        cmd: cmd_health::HealthCmd,
    },
    /// Scheduled maintenance: the weekly task, its runs and their results.
    Maintenance {
        #[command(subcommand)]
        cmd: cmd_maintenance::MaintenanceCmd,
    },
    /// Where Windows Settings manages the camera, microphone and location permissions of apps,
    /// and the desktop apps that used each device. Read-only: no permission is changed.
    Perm {
        #[command(subcommand)]
        cmd: cmd_perm::PermCmd,
    },
    /// Export, preview and apply profiles of Cairn settings.
    Profile {
        #[command(subcommand)]
        cmd: cmd_profile::ProfileCmd,
    },
    /// Disk speed test, space usage and duplicate files.
    Storage {
        #[command(subcommand)]
        cmd: cmd_storage::StorageCmd,
    },
    /// winget app updates and Windows Update settings.
    Updates {
        #[command(subcommand)]
        cmd: cmd_updates::UpdatesCmd,
    },
    /// Removes what Cairn created outside the journal (its scheduled maintenance tasks). Run by
    /// the uninstaller; always exits 0 with --yes.
    #[command(hide = true)]
    UninstallCleanup(UninstallCleanupArgs),
}

#[derive(Args, Debug)]
struct UninstallCleanupArgs {
    /// Required to remove anything; without it the steps are only listed.
    #[arg(long)]
    yes: bool,
}

#[derive(Args, Debug)]
struct RestorePointArgs {
    #[arg(short, long, default_value = safety::restore_point::DEFAULT_DESCRIPTION)]
    description: String,
    /// Respect the 24-hour creation throttle instead of overriding it.
    #[arg(long)]
    no_force: bool,
}

#[derive(Subcommand, Debug)]
enum JournalCmd {
    /// Row counts and last session.
    Summary,
    /// Active (not yet reverted) records.
    List,
    /// Full JSON export.
    Export {
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Whether changes that can be undone are recorded. Exit code 0: none, or no journal
    /// (none is created); 3: some; 1: the journal could not be read. Used by the uninstaller.
    Pending,
}

/// `journal pending`: changes that can be undone are recorded. `drift` and `verify`: some
/// recorded changes no longer hold.
const EXIT_PENDING: i32 = 3;
/// `journal pending`: the journal could not be read. `drift` and `verify`: the check failed.
const EXIT_ERROR: i32 = 1;

#[derive(Args, Debug)]
struct RollbackArgs {
    /// Print the plan without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Required to perform a real rollback.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Args, Debug)]
struct CatalogArgs {
    /// Print the catalog as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ScanArgs {
    /// Print the scan report as JSON.
    #[arg(long)]
    json: bool,
    /// Only show one category (privacy, gaming, performance, interface, security, bloatware).
    #[arg(long, value_parser = parse_category)]
    category: Option<Category>,
}

#[derive(Args, Debug)]
struct DriftArgs {
    /// Print the drift report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct VerifyArgs {
    /// Tweak ids, e.g. `privacy.activity_history`.
    #[arg(value_name = "ID", required = true)]
    ids: Vec<String>,
    /// Print the drift report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("selector").required(true).args(["ids", "category"])))]
struct ApplyArgs {
    /// Item ids, e.g. `privacy.advertising_id` or `appx.Microsoft.BingNews`.
    #[arg(value_name = "ID")]
    ids: Vec<String>,
    /// Apply every recommended item of this category that is not applied yet.
    #[arg(long, value_parser = parse_category)]
    category: Option<Category>,
    /// Print the plan without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// System Restore point before the first change.
    #[arg(long, value_enum, default_value_t = RestorePointArg::Try)]
    restore_point: RestorePointArg,
    /// Required to perform a real apply.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("selector").required(true).args(["ids", "category", "all"])))]
struct RevertArgs {
    /// Item ids, e.g. `privacy.activity_history` or `appx.Microsoft.BingNews`.
    #[arg(value_name = "ID")]
    ids: Vec<String>,
    /// Revert every item of this category, including removed bloatware packages.
    #[arg(long, value_parser = parse_category)]
    category: Option<Category>,
    /// Revert every active journal record.
    #[arg(long)]
    all: bool,
    /// Print the plan without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Required to perform a real revert.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub(crate) enum RestorePointArg {
    /// Do not create a restore point.
    Skip,
    /// Create one; continue with the journal alone if that fails.
    Try,
    /// Create one; abort if that fails.
    Require,
}

impl From<RestorePointArg> for RestorePointPolicy {
    fn from(arg: RestorePointArg) -> Self {
        match arg {
            RestorePointArg::Skip => RestorePointPolicy::Skip,
            RestorePointArg::Try => RestorePointPolicy::Try,
            RestorePointArg::Require => RestorePointPolicy::Require,
        }
    }
}

fn parse_category(value: &str) -> Result<Category, String> {
    Category::parse(value).ok_or_else(|| {
        let valid: Vec<&str> = Category::ALL.iter().map(|c| c.label()).collect();
        format!("expected one of: {}", valid.join(", "))
    })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPTIMIZER_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let open_journal = || -> anyhow::Result<Journal> {
        match &cli.journal {
            Some(p) => Journal::open(p).with_context(|| format!("open journal {}", p.display())),
            None => Journal::open_default().context("open default journal"),
        }
    };
    let open_engine = || -> anyhow::Result<Engine> { Ok(Engine::new(Arc::new(open_journal()?))) };

    match cli.cmd.unwrap_or(Cmd::Doctor) {
        Cmd::Doctor => {
            println!("optimizer_core      {}", optimizer_core::VERSION);
            println!("elevated            {}", optimizer_core::is_elevated());
            match app::installed() {
                Ok(Some(info)) => println!(
                    "installed           {} ({})",
                    info.dir.display(),
                    info.version
                ),
                Ok(None) => println!("installed           not installed"),
                Err(e) => println!("installed           unknown ({e})"),
            }
            println!(
                "code pages          ansi={} oem={}",
                ansi_code_page(),
                oem_code_page()
            );
            match safety::is_system_restore_enabled() {
                Ok(v) => println!(
                    "system protection   {}",
                    if v { "enabled" } else { "disabled" }
                ),
                Err(e) => println!("system protection   unknown ({e})"),
            }
            let journal = open_journal()?;
            let s = journal.summary()?;
            println!("journal             {}", s.path);
            println!("sessions            {}", s.sessions);
            println!(
                "pending records     registry={} services={} tasks={} task_definitions={} dns={} \
                 appx={} power={}",
                s.registry_active,
                s.services_active,
                s.scheduled_tasks_active,
                s.task_definitions_active,
                s.dns_active,
                s.appx_active,
                s.power_active
            );
            if let Some(last) = s.last_session {
                println!(
                    "last session        #{} \"{}\" at {} (restore point: {})",
                    last.id,
                    last.label,
                    last.started_at,
                    last.restore_point_seq
                        .map_or("none".to_string(), |n| n.to_string())
                );
            }
            println!("maintenance         {}", maintenance::doctor_line());
        }

        Cmd::RestorePoint(args) => {
            let rp = safety::create_restore_point(&args.description, !args.no_force)?;
            println!("{}", serde_json::to_string_pretty(&rp)?);
        }

        Cmd::Journal { cmd } => match cmd {
            JournalCmd::Pending => std::process::exit(journal_pending(cli.journal.as_deref())),
            JournalCmd::Summary => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&open_journal()?.summary()?)?
                );
            }
            JournalCmd::List => print_journal_list(&open_journal()?)?,
            JournalCmd::Export { out } => {
                let json = open_journal()?.export_json()?;
                match out {
                    Some(path) => {
                        std::fs::write(&path, json)?;
                        println!("exported to {}", path.display());
                    }
                    None => println!("{json}"),
                }
            }
        },

        Cmd::UninstallCleanup(args) => run_uninstall_cleanup(&args)?,

        Cmd::Rollback(args) => {
            let journal = open_journal()?;
            if !args.dry_run && !args.yes {
                let plan = rollback::rollback_journal(&journal, true)?;
                if plan.actions.is_empty() {
                    println!("journal has no pending changes");
                    return Ok(());
                }
                for a in &plan.actions {
                    println!("  {a}");
                }
                bail!(
                    "{} action(s) planned; re-run with --yes to apply",
                    plan.actions.len()
                );
            }
            let report = rollback::rollback_journal(&journal, args.dry_run)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.total_reverted() > 0 && report.restart != RestartNeed::None {
                eprintln!("{}", restart_text(report.restart));
            }
            if !report.is_clean() {
                bail!("{}", incomplete_revert(&report));
            }
        }

        Cmd::Catalog(args) => run_catalog(&args)?,
        Cmd::Scan(args) => run_scan(&open_engine()?, &args)?,
        Cmd::Apply(args) => run_apply(&open_engine()?, &args)?,
        Cmd::Revert(args) => run_revert(&open_engine()?, &args)?,
        Cmd::Drift(args) => std::process::exit(run_drift(
            || open_engine()?.drift().map_err(Into::into),
            args.json,
        )),
        Cmd::Verify(args) => std::process::exit(run_drift(
            || open_engine()?.verify(&args.ids).map_err(Into::into),
            args.json,
        )),

        Cmd::RegGet { key, value } => {
            let (hive, path) = split_path(&key)?;
            match read_value(hive, &path, &value)? {
                Some(v) => println!("{}", v.display()),
                None => println!("(absent)"),
            }
        }

        Cmd::SvcGet { name } => {
            let scm = Scm::connect()?;
            let svc = scm.open_required(&name, READ_ACCESS)?;
            let cfg = svc.config()?;
            let status = svc.status()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "config": cfg,
                    "status": status,
                }))?
            );
        }

        Cmd::TaskGet { path } => run_task_get(&path)?,
        Cmd::Net { cmd } => cmd_net::run(cmd, &open_journal)?,
        Cmd::Tools { cmd } => cmd_tools::run(cmd, &open_journal)?,
        Cmd::Sysinfo(args) => cmd_sysinfo::run(&args)?,
        Cmd::Health { cmd } => cmd_health::run(cmd, cli.journal.as_deref(), &open_journal)?,
        Cmd::Maintenance { cmd } => {
            cmd_maintenance::run(cmd, cli.journal.as_deref(), &open_journal)?
        }
        Cmd::Perm { cmd } => cmd_perm::run(cmd)?,
        Cmd::Profile { cmd } => cmd_profile::run(cmd, cli.journal.as_deref(), &open_journal)?,
        Cmd::Storage { cmd } => cmd_storage::run(cmd, cli.journal.as_deref(), &open_journal)?,
        Cmd::Updates { cmd } => cmd_updates::run(cmd, cli.journal.as_deref(), &open_journal)?,
    }
    Ok(())
}

// ───────────────────────────── journal and uninstall ─────────────────────────────

/// `journal pending`: prints how many recorded changes can be undone and returns the exit
/// code: 0 when none are (a missing journal counts as none and is not created),
/// [`EXIT_PENDING`] when some are, [`EXIT_ERROR`] when the journal cannot be read, including
/// one written by a newer Cairn.
fn journal_pending(path: Option<&Path>) -> i32 {
    let path = path.map_or_else(state_log::default_path, Path::to_path_buf);
    match std::fs::metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("No journal: nothing to undo.");
            return 0;
        }
        Err(e) => {
            eprintln!("Error: cannot read {}: {e}", path.display());
            return EXIT_ERROR;
        }
        Ok(_) => {}
    }
    match Journal::open(&path).and_then(|journal| journal.summary()) {
        Ok(summary) => {
            let pending = summary.pending_count();
            println!("Revertible changes recorded for this account: {pending}");
            if pending > 0 {
                EXIT_PENDING
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            EXIT_ERROR
        }
    }
}

/// `uninstall-cleanup`: without `--yes` lists the steps and fails; with it runs every step of
/// [`app::uninstall_steps`] and prints each outcome. A failing or panicking step is reported
/// and the others still run, so the uninstaller always continues.
fn run_uninstall_cleanup(args: &UninstallCleanupArgs) -> anyhow::Result<()> {
    let steps = app::uninstall_steps();
    if !args.yes {
        for step in &steps {
            println!("  {}", step.name);
        }
        bail!(
            "{} step(s) planned; re-run with --yes to run them",
            steps.len()
        );
    }
    for step in steps {
        match std::panic::catch_unwind(step.run) {
            Ok(Ok(outcome)) => println!("{}: {outcome}", step.name),
            Ok(Err(e)) => println!("{}: failed: {e}", step.name),
            Err(_) => println!("{}: failed: internal error", step.name),
        }
    }
    Ok(())
}

#[link(name = "kernel32")]
extern "system" {
    fn GetACP() -> u32;
}

/// The ANSI code page of this process (the manifest keeps the system's).
fn ansi_code_page() -> u32 {
    // SAFETY: GetACP takes no arguments and only reads process state.
    unsafe { GetACP() }
}

/// `journal list`: every active record, one line each.
fn print_journal_list(journal: &Journal) -> anyhow::Result<()> {
    let registry = journal.active_registry()?;
    let services = journal.active_services()?;
    let tasks = journal.active_scheduled_tasks()?;
    let appx = journal.active_appx()?;
    let power = journal.active_power()?;
    let dns = journal.active_dns()?;
    let definitions = journal.active_task_definitions()?;
    if registry.is_empty()
        && services.is_empty()
        && tasks.is_empty()
        && appx.is_empty()
        && power.is_empty()
        && dns.is_empty()
        && definitions.is_empty()
    {
        println!("journal has no pending changes");
    }
    for r in registry {
        let original = r
            .original_decoded()
            .map_or("(absent)".to_string(), |v| v.display());
        println!(
            "[reg #{:>4}] {:<70} original: {}",
            r.id,
            r.target(),
            original
        );
    }
    for s in services {
        println!(
            "[svc #{:>4}] {:<30} original: {}{} {}",
            s.id,
            s.name,
            s.start_type.label(),
            if s.delayed_auto_start {
                " (delayed)"
            } else {
                ""
            },
            if s.was_running { "running" } else { "stopped" }
        );
    }
    for t in tasks {
        println!(
            "[task #{:>3}] {:<70} original: {}",
            t.id,
            t.path,
            if t.was_enabled { "enabled" } else { "disabled" }
        );
    }
    for a in appx {
        println!("[appx #{:>3}] {}", a.id, a.package_full_name);
    }
    for p in power {
        println!(
            "[pwr #{:>4}] power scheme {:<57} original: {}",
            p.id, p.target_scheme, p.previous_scheme
        );
    }
    for d in dns {
        println!(
            "[dns  #{:>4}] {}  was {}  ({})",
            d.id,
            d.target(),
            d.previous_text(),
            d.recorded_at
        );
    }
    for t in definitions {
        println!(
            "[tdef #{:>3}] {:<70} created by Cairn ({}{})",
            t.id,
            t.path,
            t.purpose,
            if t.folder_created {
                ", with its folder"
            } else {
                ""
            }
        );
    }
    Ok(())
}

// ───────────────────────────── scheduled tasks ─────────────────────────────

/// Prints a scheduled task's enabled flag and run state as JSON; a missing task prints only
/// its path and `"exists": false`. Read-only.
fn run_task_get(path: &str) -> anyhow::Result<()> {
    let scheduler = TaskScheduler::connect().context("connect to Task Scheduler")?;
    let info = match scheduler.task(path)? {
        Some(task) => serde_json::json!({
            "path": path,
            "exists": true,
            "enabled": task.enabled()?,
            "state": task.state()?,
        }),
        None => serde_json::json!({
            "path": path,
            "exists": false,
        }),
    };
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

// ───────────────────────────── debloat commands ─────────────────────────────

fn run_catalog(args: &CatalogArgs) -> anyhow::Result<()> {
    let entries = debloat::catalog_view();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_catalog(&entries);
    }
    Ok(())
}

fn run_scan(engine: &Engine, args: &ScanArgs) -> anyhow::Result<()> {
    let mut report = engine.scan()?;
    if let Some(category) = args.category {
        report.items.retain(|i| i.category == category);
        report.categories.retain(|c| c.category == category);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_scan(&report);
    }
    Ok(())
}

fn run_apply(engine: &Engine, args: &ApplyArgs) -> anyhow::Result<()> {
    let apply = |dry_run: bool| {
        let opts = ApplyOptions {
            restore_point: args.restore_point.into(),
            dry_run,
        };
        match args.category {
            Some(category) => engine.apply_category(category, &opts),
            None => engine.apply(&args.ids, &opts),
        }
    };

    if !args.dry_run && !args.yes {
        let plan = apply(true)?;
        print_apply(&plan);
        let planned = count_outcome(&plan, ItemOutcome::Planned);
        if planned > 0 {
            bail!("{planned} item(s) planned; re-run with --yes to apply");
        }
        if plan.failed() > 0 {
            bail!("{} item(s) failed", plan.failed());
        }
        return Ok(());
    }

    let report = apply(args.dry_run)?;
    print_apply(&report);
    if report.failed() > 0 {
        bail!("{} item(s) failed", report.failed());
    }
    Ok(())
}

fn run_revert(engine: &Engine, args: &RevertArgs) -> anyhow::Result<()> {
    let revert = |dry_run: bool| {
        if args.all {
            engine.revert_all(dry_run)
        } else if let Some(category) = args.category {
            engine.revert_category(category, dry_run)
        } else {
            engine.revert(&args.ids, dry_run)
        }
    };

    if !args.dry_run && !args.yes {
        let plan = revert(true)?;
        print_rollback(&plan);
        if !plan.actions.is_empty() {
            bail!(
                "{} action(s) planned; re-run with --yes to revert",
                plan.actions.len()
            );
        }
        return Ok(());
    }

    let report = revert(args.dry_run)?;
    print_rollback(&report);
    if !report.is_clean() {
        bail!("{}", incomplete_revert(&report));
    }
    Ok(())
}

/// Why a rollback report is not clean, in one line.
fn incomplete_revert(report: &RollbackReport) -> String {
    format!(
        "revert incomplete: {} failure(s), {} package(s) need a Store reinstall, {} restored \
         value(s) changed again",
        report.failures.len(),
        report.appx_store_required.len(),
        report.not_restored.len()
    )
}

/// `drift` and `verify`: prints the report (as JSON with `json`) and returns the exit code:
/// 0 when every recorded change still holds, [`EXIT_PENDING`] when some no longer hold,
/// [`EXIT_ERROR`] when the check failed.
fn run_drift(check: impl FnOnce() -> anyhow::Result<DriftReport>, json: bool) -> i32 {
    let report = match check() {
        Ok(report) => report,
        Err(e) => {
            eprintln!("Error: {e:#}");
            return EXIT_ERROR;
        }
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("Error: {e}");
                return EXIT_ERROR;
            }
        }
    } else {
        print_drift(&report);
    }
    if report.entries.is_empty() {
        0
    } else {
        EXIT_PENDING
    }
}

// ───────────────────────────── output ─────────────────────────────

fn print_catalog(entries: &[CatalogEntry]) {
    let id_w = column_width("ID", entries.iter().map(|e| e.id.len()));
    println!(
        "{:<id_w$}  {:<11}  {:<6}  {:<7}  TITLE",
        "ID", "CATEGORY", "RISK", "DEFAULT"
    );
    for e in entries {
        println!(
            "{:<id_w$}  {:<11}  {:<6}  {:<7}  {}",
            e.id,
            e.category.label(),
            risk_label(e.risk),
            if e.default_on { "yes" } else { "no" },
            e.title
        );
        for (label, text) in [
            ("windows", &e.windows),
            ("retired", &e.retired),
            ("undo limit", &e.undo_limit),
        ] {
            if let Some(text) = text {
                println!("{:<id_w$}  {label}: {text}", "");
            }
        }
    }
    println!("\n{} item(s)", entries.len());
}

fn print_scan(report: &ScanReport) {
    println!(
        "elevated: {}   battery: {}   scanned in {} ms",
        yes_no(report.elevated),
        yes_no(report.has_battery),
        report.duration_ms
    );
    println!("[x] applied   [~] partial   [ ] not applied   [-] unavailable");

    let id_w = column_width("", report.items.iter().map(|i| i.id.len()));
    let title_w = column_width("", report.items.iter().map(|i| i.title.chars().count()));
    for status in &report.categories {
        println!();
        println!(
            "{}: {}/{} applied, {}/{} recommended applied{}",
            status.category.label(),
            status.applied,
            status.items,
            status.recommended_applied,
            status.recommended,
            if status.active { " (active)" } else { "" }
        );
        for item in report
            .items
            .iter()
            .filter(|i| i.category == status.category)
        {
            let line = format!(
                "  {} {:<id_w$}  {:<title_w$}  {:<6}  {:<11}  {}",
                state_marker(item.state),
                item.id,
                item.title,
                risk_label(item.risk),
                if item.recommended { "recommended" } else { "" },
                if item.revertible { "revertible" } else { "" }
            );
            println!("{}", line.trim_end());
            if let Some(kind) = item.unavailable {
                println!("      unavailable: {}", unavailable_label(kind));
            }
            if let Some(note) = &item.note {
                println!("      note: {note}");
            }
            for m in &item.drift {
                println!(
                    "      changed since applied: {} is {}, not {}",
                    m.label, m.found, m.expected
                );
            }
        }
    }

    if !report.warnings.is_empty() {
        println!("\nwarnings:");
        for w in &report.warnings {
            println!("  {w}");
        }
    }
}

fn print_apply(report: &ApplyReport) {
    if report.dry_run {
        println!("dry run: nothing was changed");
    }
    if let Some(id) = report.session_id {
        match &report.restore_point {
            Some(rp) => println!(
                "session #{id}, restore point #{} \"{}\"",
                rp.sequence, rp.description
            ),
            None => println!("session #{id}, no restore point"),
        }
    }

    for r in &report.results {
        println!("  {:<15}  {}", outcome_label(r.outcome), r.id);
        for detail in &r.details {
            println!("  {:<15}    {detail}", "");
        }
        for m in &r.not_kept {
            println!(
                "  {:<15}    not kept: {}: now {}; Cairn set {}",
                "", m.target, m.found, m.expected
            );
        }
        let undo_limit = catalog::tweak(&r.id).and_then(|t| t.undo_limit);
        if let (ItemOutcome::Applied, Some(limit)) = (r.outcome, undo_limit) {
            println!("  {:<15}    cannot be undone: {limit}", "");
        }
    }
    if report.results.is_empty() {
        println!("nothing to apply");
    }

    if !report.warnings.is_empty() {
        println!("\nwarnings:");
        for w in &report.warnings {
            println!("  {w}");
        }
    }

    let summary: Vec<String> = [
        ItemOutcome::Planned,
        ItemOutcome::Applied,
        ItemOutcome::AlreadyApplied,
        ItemOutcome::Skipped,
        ItemOutcome::Failed,
    ]
    .into_iter()
    .filter_map(|o| match count_outcome(report, o) {
        0 => None,
        n => Some(format!("{n} {}", outcome_label(o))),
    })
    .collect();
    if !summary.is_empty() {
        println!("\n{}", summary.join(", "));
    }

    if report.dry_run {
        let planned = planned_restart(report);
        if planned != RestartNeed::None {
            println!("after applying: {}", restart_text(planned));
        }
    } else {
        println!("{}", restart_text(report.restart));
    }
}

fn print_rollback(report: &RollbackReport) {
    if report.dry_run {
        println!("dry run: nothing was changed");
    }
    if report.actions.is_empty() {
        println!("nothing to revert");
    } else {
        println!("actions:");
        for a in &report.actions {
            println!("  {a}");
        }
    }

    if !report.dry_run {
        println!();
        println!(
            "registry values restored {}, deleted {}",
            report.registry_restored, report.registry_deleted
        );
        println!(
            "services restored {} (started {})",
            report.services_restored, report.services_started
        );
        println!(
            "scheduled tasks restored {}",
            report.scheduled_tasks_restored
        );
        println!(
            "task definitions deleted {}",
            report.task_definitions_deleted
        );
        println!("power plans restored {}", report.power_restored);
        println!("DNS settings restored {}", report.dns_restored);
        println!(
            "packages restored {} (re-registered or already reinstalled)",
            report.appx_restored
        );
    }

    if !report.appx_store_required.is_empty() {
        println!("\nreinstall from the Microsoft Store:");
        let family_w = column_width(
            "",
            report
                .appx_store_required
                .iter()
                .map(|s| s.package_family.len()),
        );
        for s in &report.appx_store_required {
            println!("  {:<family_w$}  {}", s.package_family, s.store_link);
        }
    }

    if !report.failures.is_empty() {
        println!("\nfailures:");
        for f in &report.failures {
            println!("  {}: {}", f.target, f.error);
        }
    }

    if !report.not_restored.is_empty() {
        println!();
        for m in &report.not_restored {
            println!(
                "not restored: {}: now {}; restored {}",
                m.target, m.found, m.expected
            );
        }
    }

    if !report.warnings.is_empty() {
        println!("\nwarnings:");
        for w in &report.warnings {
            println!("  {w}");
        }
    }

    if report.restart != RestartNeed::None {
        if report.dry_run {
            println!("\nafter reverting: {}", restart_text(report.restart));
        } else if report.total_reverted() > 0 {
            println!("\n{}", restart_text(report.restart));
        }
    }
}

fn print_drift(report: &DriftReport) {
    println!(
        "Checked {} recorded changes in {} ms; {} no longer hold.",
        report.checked,
        report.duration_ms,
        report.entries.len()
    );
    for entry in &report.entries {
        let owner = entry
            .item
            .as_deref()
            .unwrap_or_else(|| drift_kind_label(entry.kind));
        println!("  {owner}  {}", drift_line(&entry.mismatch));
    }
    if !report.warnings.is_empty() {
        println!("\nwarnings:");
        for w in &report.warnings {
            println!("  {w}");
        }
    }
}

/// `{target}: now {found}; Cairn set {expected}; original {original}`, with
/// " (back to the original)" when the target holds its value from before Cairn's change.
fn drift_line(m: &Mismatch) -> String {
    format!(
        "{}: now {}; Cairn set {}; original {}{}",
        m.target,
        m.found,
        m.expected,
        m.original.as_deref().unwrap_or("unknown"),
        if m.back_to_original {
            " (back to the original)"
        } else {
            ""
        }
    )
}

fn drift_kind_label(kind: DriftKind) -> &'static str {
    match kind {
        DriftKind::Registry => "registry",
        DriftKind::Service => "service",
        DriftKind::ScheduledTask => "scheduled task",
        DriftKind::Power => "power",
        DriftKind::Dns => "DNS",
    }
}

fn unavailable_label(kind: Unavailable) -> &'static str {
    match kind {
        Unavailable::NotOnThisPc => "Not on this PC",
        Unavailable::WindowsVersion => "Not on this version of Windows",
        Unavailable::Edition => "Not on this edition of Windows",
        Unavailable::Retired => "Retired",
    }
}

fn column_width(header: &str, lens: impl Iterator<Item = usize>) -> usize {
    lens.max().unwrap_or(0).max(header.len())
}

fn count_outcome(report: &ApplyReport, outcome: ItemOutcome) -> usize {
    report
        .results
        .iter()
        .filter(|r| r.outcome == outcome)
        .count()
}

/// Strongest restart requirement among the tweaks a dry run would apply.
fn planned_restart(report: &ApplyReport) -> RestartNeed {
    report
        .results
        .iter()
        .filter(|r| r.outcome == ItemOutcome::Planned)
        .filter_map(|r| catalog::tweak(&r.id))
        .map(|t| t.restart)
        .max()
        .unwrap_or(RestartNeed::None)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn state_marker(state: ItemState) -> &'static str {
    match state {
        ItemState::Applied => "[x]",
        ItemState::Partial => "[~]",
        ItemState::NotApplied => "[ ]",
        ItemState::Unavailable => "[-]",
    }
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

fn outcome_label(outcome: ItemOutcome) -> &'static str {
    match outcome {
        ItemOutcome::Applied => "applied",
        ItemOutcome::AlreadyApplied => "already applied",
        ItemOutcome::Skipped => "skipped",
        ItemOutcome::Failed => "failed",
        ItemOutcome::Planned => "planned",
    }
}

fn restart_text(restart: RestartNeed) -> &'static str {
    match restart {
        RestartNeed::None => "no restart needed",
        RestartNeed::Explorer => "restart File Explorer to finish",
        RestartNeed::SignOut => "sign out and back in to finish",
        RestartNeed::Restart => "restart Windows to finish",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use optimizer_core::debloat::DriftEntry;
    use optimizer_core::safety::rollback::RollbackFilter;

    fn mismatch(original: Option<&str>, back_to_original: bool) -> Mismatch {
        Mismatch {
            target: r"HKLM\SOFTWARE\Policies\Example\Value".into(),
            label: "Value".into(),
            expected: "0".into(),
            found: "1".into(),
            original: original.map(str::to_string),
            back_to_original,
        }
    }

    #[test]
    fn drift_lines_name_the_found_set_and_original_values() {
        assert_eq!(
            drift_line(&mismatch(Some("1"), true)),
            r"HKLM\SOFTWARE\Policies\Example\Value: now 1; Cairn set 0; original 1 (back to the original)"
        );
        assert_eq!(
            drift_line(&mismatch(None, false)),
            r"HKLM\SOFTWARE\Policies\Example\Value: now 1; Cairn set 0; original unknown"
        );
    }

    #[test]
    fn drift_exit_codes_follow_journal_pending() {
        let entry = DriftEntry {
            item: Some("privacy.example".into()),
            kind: DriftKind::Registry,
            mismatch: mismatch(Some("not set"), false),
            filter: RollbackFilter::default(),
            recorded_at: "2026-01-01T00:00:00+00:00".into(),
        };
        let report = |entries: Vec<DriftEntry>| DriftReport {
            checked: entries.len(),
            entries,
            ..Default::default()
        };
        assert_eq!(run_drift(|| Ok(report(Vec::new())), false), 0);
        assert_eq!(
            run_drift(|| Ok(report(vec![entry.clone()])), false),
            EXIT_PENDING
        );
        assert_eq!(run_drift(|| Ok(report(vec![entry])), true), EXIT_PENDING);
        assert_eq!(
            run_drift(|| Err(anyhow::anyhow!("not available")), false),
            EXIT_ERROR
        );
    }

    #[test]
    fn unavailable_labels_are_distinct() {
        let labels = [
            Unavailable::NotOnThisPc,
            Unavailable::WindowsVersion,
            Unavailable::Edition,
            Unavailable::Retired,
        ]
        .map(unavailable_label);
        for (i, label) in labels.iter().enumerate() {
            assert!(!labels[i + 1..].contains(label), "{label}");
        }
        assert_eq!(labels[1], "Not on this version of Windows");
    }

    #[test]
    fn apply_help_names_offered_tweaks() {
        use clap::CommandFactory;
        let cli = Cli::command();
        let apply = cli.find_subcommand("apply").expect("apply subcommand");
        let ids = apply
            .get_arguments()
            .find(|a| a.get_id() == "ids")
            .expect("ids argument");
        let help = ids.get_help().expect("ids help").to_string();
        let tweaks: Vec<&str> = help
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|s| !s.starts_with("appx."))
            .collect();
        assert!(!tweaks.is_empty(), "{help}");
        for id in tweaks {
            let t = catalog::tweak(id).unwrap_or_else(|| panic!("{id} is not a catalog tweak"));
            assert!(
                t.retired.is_none(),
                "the apply help names the retired tweak {id}"
            );
        }
    }
}
