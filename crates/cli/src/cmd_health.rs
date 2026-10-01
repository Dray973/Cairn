//! `optctl health`: the security checkup and boot history. Both only read; the journal is
//! never opened.

use std::io::Write as _;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use clap::Subcommand;
use optimizer_core::health::{self, text, ScanState, MAX_BOOT_LIMIT};
use optimizer_core::safety::state_log::Journal;

#[derive(Subcommand, Debug)]
pub(crate) enum HealthCmd {
    /// Security checkup: antivirus, firewall, updates, encryption, accounts. Read-only.
    Security {
        /// Print the checkup as JSON.
        #[arg(long)]
        json: bool,
        /// Search Windows Update online first; can take several minutes.
        #[arg(long)]
        online: bool,
    },
    /// Start and shutdown durations and what slowed them. Read-only.
    Boots {
        /// Starts to read, newest first (1 to 500).
        #[arg(long, default_value_t = 60)]
        limit: usize,
        /// Print the history as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(
    cmd: HealthCmd,
    journal: Option<&Path>,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    // Nothing here is journaled.
    let _ = (journal, open_journal);
    match cmd {
        HealthCmd::Security { json, online } => security(json, online),
        HealthCmd::Boots { limit, json } => boots(limit, json),
    }
}

/// "12 s", "1 min 20 s".
fn elapsed_text(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {} s", secs / 60, secs % 60)
    }
}

/// Runs an online Windows Update search and waits for it, showing its progress on stderr.
fn online_scan() -> anyhow::Result<()> {
    health::start_update_scan(true).context("could not start the Windows Update check")?;
    let mut stderr = std::io::stderr();
    loop {
        let view = health::update_scan();
        if view.state != ScanState::Running {
            let _ = writeln!(stderr);
            match view.state {
                ScanState::Done => {}
                ScanState::Cancelled => eprintln!("The Windows Update check was stopped."),
                _ => eprintln!(
                    "Windows Update check failed: {}",
                    view.error.as_deref().unwrap_or("unknown error")
                ),
            }
            return Ok(());
        }
        let _ = write!(
            stderr,
            "\rChecking Windows Update online… {}   ",
            elapsed_text(view.elapsed_ms)
        );
        let _ = stderr.flush();
        thread::sleep(Duration::from_millis(500));
    }
}

fn security(json: bool, online: bool) -> anyhow::Result<()> {
    if online {
        online_scan()?;
    }
    let checkup = health::security_checkup();
    if json {
        println!("{}", serde_json::to_string_pretty(&checkup)?);
    } else {
        print!("{}", text::checkup_text_local(&checkup));
    }
    Ok(())
}

fn boots(limit: usize, json: bool) -> anyhow::Result<()> {
    if !(1..=MAX_BOOT_LIMIT).contains(&limit) {
        anyhow::bail!("--limit must be between 1 and {MAX_BOOT_LIMIT}");
    }
    let history = health::boot_history(limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&history)?);
    } else {
        print!("{}", text::boot_text_local(&history));
    }
    Ok(())
}
