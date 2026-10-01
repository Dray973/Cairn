//! Profiles: a versioned JSON file listing Cairn settings (catalog tweak ids, Store apps to
//! remove, startup entries to turn off, a DNS preset per adapter kind, Windows Update and
//! scheduled-maintenance choices), exported from this PC or built in, and applied through the
//! existing journaled mutators in one session labelled `profile: <name>`.
//!
//! A profile never names a registry path, service, task, file, command, URL or server address:
//! its strings are only compared with the catalog, the DNS presets and what is listed on this
//! PC, and every write target comes from those. Unknown but well-formed ids are skipped rows.
//! Previewing, reading and exporting change nothing and open no session; applying needs an
//! elevated process, records every baseline before writing and logs `apply_profile` rows
//! (started before the first change, one final row).
//!
//! - `format`    the file: grammar, limits, reading and atomic writing
//! - `plan`      plan, apply and export against the `ProfileSystem` seam
//! - `starters`  the built-in Gaming, Privacy and Clean profiles
//! - `system`    the seam over the engine, startup, network, Windows Update and maintenance
//! - [`step`]    rows that Windows Update and scheduled maintenance plan and apply

pub mod step;

mod format;
mod plan;
mod starters;
mod system;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

pub use format::{
    check_export_fields, is_profile_path, parse, parse_bytes, read_file, to_text, write_file,
    DnsChoices, DnsFamilies, Invalid, Profile, ProfileSummary, SectionCounts, StartupChoice,
    FORMAT, MAX_FILE_BYTES, PROFILE_PATH_TEXT, SCHEMA,
};
pub use plan::{
    reason_text, ExportCandidates, ExportReport, ExportRow, PlanOrApply, PlanRow,
    ProfileApplyReport, ProfilePlan, RowResult, Section,
};
pub use starters::{starter, starters, StarterSummary};

use crate::safety::state_log::Journal;
use crate::safety::{RestorePointPolicy, Safety, SafetyOptions};
use crate::Result;
use system::LiveProfiles;

/// Audit-log operation of a profile apply.
pub const OP_APPLY_PROFILE: &str = "apply_profile";
/// Most row keys `apply` and `export` accept.
pub const MAX_KEYS: usize = 2000;
/// Longest row key accepted.
pub const MAX_KEY_CHARS: usize = 300;

/// Counts and canonical text of a validated profile.
pub fn summary(profile: &Profile) -> ProfileSummary {
    ProfileSummary {
        name: profile.name.clone(),
        description: profile.description.clone(),
        created: profile.created.clone(),
        created_with: profile.created_with.clone(),
        counts: SectionCounts::of(profile),
        text: to_text(profile),
    }
}

/// Reads and validates a profile file. Outer Err: the file could not be read; inner Err: it
/// is not a valid profile.
pub fn load(path: &Path) -> Result<std::result::Result<ProfileSummary, Invalid>> {
    let bytes = read_file(path)?;
    Ok(parse_bytes(&bytes).map(|p| summary(&p)))
}

/// What applying `profile` would change on this PC. Read-only: opens no session, creates no
/// restore point, writes no ops row, needs no elevation.
pub fn plan(journal: Arc<Journal>, profile: &Profile) -> Result<ProfilePlan> {
    let profile = format::validate(profile)?;
    plan::plan_with(&LiveProfiles::new(journal), &profile)
}

/// Applies the rows `keys` selects (None: every change row whose `selected` is true) in one
/// session labelled `profile: <name>`. Plans again first, so rows that no longer need a
/// change are reported, not applied.
pub fn apply(
    journal: Arc<Journal>,
    profile: &Profile,
    keys: Option<&[String]>,
    restore_point: RestorePointPolicy,
) -> Result<ProfileApplyReport> {
    let begin = begin_session(journal.clone(), profile, restore_point);
    plan::apply_with(&LiveProfiles::new(journal), begin, profile, keys)
}

/// [`plan`] when `dry_run` is set (no session, no restore point, `keys` and `restore_point`
/// unused), else [`apply`].
pub fn plan_or_apply(
    journal: Arc<Journal>,
    profile: &Profile,
    keys: Option<&[String]>,
    restore_point: RestorePointPolicy,
    dry_run: bool,
) -> Result<PlanOrApply> {
    let begin = begin_session(journal.clone(), profile, restore_point);
    plan::plan_or_apply_with(&LiveProfiles::new(journal), dry_run, begin, profile, keys)
}

/// Opens the elevated session a profile applies in; called only once a change is chosen.
fn begin_session(
    journal: Arc<Journal>,
    profile: &Profile,
    restore_point: RestorePointPolicy,
) -> impl FnOnce() -> Result<Safety> {
    let label = plan::session_label(&profile.name);
    move || {
        Safety::begin(
            journal,
            SafetyOptions {
                label,
                restore_point,
                require_elevation: true,
                ..Default::default()
            },
        )
    }
}

/// This PC's exportable settings. Read-only.
pub fn candidates(journal: Arc<Journal>) -> Result<ExportCandidates> {
    plan::candidates_with(&LiveProfiles::new(journal))
}

/// The profile made of the candidate rows `keys` selects (None: rows whose `selected` is
/// true) and the keys that are no longer candidates.
pub fn build(
    journal: Arc<Journal>,
    name: &str,
    description: &str,
    keys: Option<&[String]>,
) -> Result<(Profile, Vec<String>)> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    plan::build_with(&LiveProfiles::new(journal), name, description, keys, &today)
}

/// [`build`], then [`write_file`]. Writes only the chosen file.
pub fn export(
    journal: Arc<Journal>,
    path: &Path,
    name: &str,
    description: &str,
    keys: Option<&[String]>,
) -> Result<ExportReport> {
    let (profile, missing) = build(journal, name, description, keys)?;
    write_file(path, &to_text(&profile))?;
    Ok(ExportReport {
        path: path.display().to_string(),
        name: profile.name.clone(),
        counts: SectionCounts::of(&profile),
        missing,
    })
}
