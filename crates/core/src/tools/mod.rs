//! Windows maintenance tools (SFC, DISM, Optimize Drives, Check Disk) run as polled
//! background jobs.
//!
//! - [`mod@catalog`]    the tools, their exact commands and how exit codes are judged
//! - [`mod@runner`]     one job at a time, its output, its audit rows and closing
//! - [`launch`]         starting a tool's process, outside Cairn's job when needed
//! - [`logs`]           the raw output and readable transcript of each run
//! - [`progress`]       percentages in progress output
//! - [`store_health`]   the component store state from the DISM API
//! - [`mod@windows_tools`] opening built-in Windows management tools
//!
//! Tool runs change machine-wide state only, so no per-user check applies. They are
//! recorded in the journal's audit log and never journaled for rollback: repairs cannot be
//! undone. This module also lists the fixed drives the drive tools can run on and creates
//! manual restore points.

pub mod catalog;
pub mod launch;
pub mod logs;
pub mod progress;
pub mod runner;
pub mod store_health;
pub mod windows_tools;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use catalog::{
    catalog, classify, exit_code_hex, Outcome, ToolGroup, ToolId, ToolInfo, ToolRequest,
};
pub use launch::{
    CommandSpec, DetachPolicy, Launched, Launcher, ProgramRoot, RunningProcess, SystemLauncher,
};
pub use runner::{
    runner, Environment, JobId, JobSnapshot, JobState, JobView, ShutdownAction, ShutdownOutcome,
    ToolPlan, ToolRunner, MAX_LINES_PER_VIEW,
};
pub use store_health::StoreHealth;
pub use windows_tools::{
    fix_tools, open_windows_tool, open_windows_tool_with, windows_tool, windows_tools, WindowsTool,
};

use crate::safety::state_log::Journal;
use crate::safety::{RestorePoint, RestorePointPolicy, Safety, SafetyOptions};
use crate::win::storage::MediaKind;
use crate::win::volume::{fixed_volumes, FixedVolume};
use crate::{Error, Result};

/// Description of the restore points created from the Tools tab.
pub const MANUAL_RESTORE_POINT_DESCRIPTION: &str = "Cairn manual checkpoint";

/// Creates a System Restore point now, in a session of its own labelled "restore point".
/// Needs an elevated process; fails when no restore point could be created.
pub fn create_restore_point_now(journal: Arc<Journal>, description: &str) -> Result<RestorePoint> {
    let safety = Safety::begin(
        journal,
        SafetyOptions {
            label: "restore point".to_string(),
            restore_point: RestorePointPolicy::Require,
            restore_description: description.to_string(),
            require_elevation: true,
        },
    )?;
    safety
        .restore_point()
        .cloned()
        .ok_or_else(|| Error::Other("no restore point was created".to_string()))
}

/// A fixed volume and why each drive tool cannot run on it, if it cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVolume {
    #[serde(flatten)]
    pub volume: FixedVolume,
    pub optimize_blocked: Option<String>,
    pub retrim_blocked: Option<String>,
    pub check_blocked: Option<String>,
}

/// The fixed volumes with a drive letter, the Windows volume first, each with the drive
/// tools that cannot run on it. Read-only.
pub fn volumes() -> Result<Vec<ToolVolume>> {
    Ok(fixed_volumes()?.into_iter().map(eligibility).collect())
}

fn fs_name(file_system: &str) -> &str {
    if file_system.is_empty() {
        "this file system"
    } else {
        file_system
    }
}

fn fs_is(file_system: &str, names: &[&str]) -> bool {
    names.iter().any(|n| n.eq_ignore_ascii_case(file_system))
}

/// Which drive tools can run on `volume`.
pub fn eligibility(volume: FixedVolume) -> ToolVolume {
    if let Some(error) = &volume.error {
        let error = Some(error.clone());
        return ToolVolume {
            optimize_blocked: error.clone(),
            retrim_blocked: error.clone(),
            check_blocked: error,
            volume,
        };
    }
    let fs = volume.file_system.as_str();
    let optimize_blocked = (!fs_is(fs, &["NTFS", "ReFS", "FAT32", "exFAT"]))
        .then(|| format!("Optimize Drives doesn't support {}", fs_name(fs)));
    let retrim_blocked = if volume.media == MediaKind::Hdd {
        Some("Retrim is for SSDs; this drive is a hard disk".to_string())
    } else if volume.trim == Some(false) {
        Some("This drive doesn't report TRIM support.".to_string())
    } else {
        None
    };
    let check_blocked = if fs_is(fs, &["ReFS"]) {
        Some("ReFS drives repair themselves and don't use Check Disk".to_string())
    } else if fs_is(fs, &["NTFS", "FAT", "FAT32", "exFAT"]) {
        None
    } else {
        Some(format!("Check Disk doesn't support {}", fs_name(fs)))
    };
    ToolVolume {
        volume,
        optimize_blocked,
        retrim_blocked,
        check_blocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(fs: &str, media: MediaKind, trim: Option<bool>) -> FixedVolume {
        FixedVolume {
            letter: "D:".to_string(),
            label: "Data".to_string(),
            file_system: fs.to_string(),
            size_bytes: 1 << 40,
            free_bytes: 1 << 39,
            media,
            trim,
            system: false,
            error: None,
        }
    }

    #[test]
    fn eligibility_rules() {
        let ssd = eligibility(volume("NTFS", MediaKind::Ssd, Some(true)));
        assert_eq!(
            (ssd.optimize_blocked, ssd.retrim_blocked, ssd.check_blocked),
            (None, None, None)
        );

        let hdd = eligibility(volume("NTFS", MediaKind::Hdd, None));
        assert_eq!(hdd.optimize_blocked, None);
        assert_eq!(
            hdd.retrim_blocked.as_deref(),
            Some("Retrim is for SSDs; this drive is a hard disk")
        );
        assert_eq!(hdd.check_blocked, None);

        let no_trim = eligibility(volume("NTFS", MediaKind::Ssd, Some(false)));
        assert_eq!(
            no_trim.retrim_blocked.as_deref(),
            Some("This drive doesn't report TRIM support.")
        );
        assert_eq!(
            eligibility(volume("NTFS", MediaKind::Unknown, None)).retrim_blocked,
            None,
            "unknown media is allowed; the tool's plan carries a note instead"
        );

        let refs = eligibility(volume("ReFS", MediaKind::Ssd, Some(true)));
        assert_eq!(refs.optimize_blocked, None);
        assert_eq!(
            refs.check_blocked.as_deref(),
            Some("ReFS drives repair themselves and don't use Check Disk")
        );

        for fs in ["FAT32", "exFAT", "ntfs"] {
            let v = eligibility(volume(fs, MediaKind::Ssd, Some(true)));
            assert_eq!((v.optimize_blocked, v.check_blocked), (None, None), "{fs}");
        }
        let fat = eligibility(volume("FAT", MediaKind::Ssd, Some(true)));
        assert_eq!(
            fat.optimize_blocked.as_deref(),
            Some("Optimize Drives doesn't support FAT")
        );
        assert_eq!(fat.check_blocked, None);

        let udf = eligibility(volume("UDF", MediaKind::Unknown, None));
        assert_eq!(
            udf.optimize_blocked.as_deref(),
            Some("Optimize Drives doesn't support UDF")
        );
        assert_eq!(
            udf.check_blocked.as_deref(),
            Some("Check Disk doesn't support UDF")
        );

        let mut locked = volume("", MediaKind::Unknown, None);
        locked.error = Some("cannot read the drive: access denied".to_string());
        let locked = eligibility(locked);
        for blocked in [
            &locked.optimize_blocked,
            &locked.retrim_blocked,
            &locked.check_blocked,
        ] {
            assert_eq!(
                blocked.as_deref(),
                Some("cannot read the drive: access denied")
            );
        }
    }

    #[test]
    fn tool_volumes_serialize_flat() {
        let json = serde_json::to_value(eligibility(volume("NTFS", MediaKind::Hdd, None))).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "check_blocked",
                "error",
                "file_system",
                "free_bytes",
                "label",
                "letter",
                "media",
                "optimize_blocked",
                "retrim_blocked",
                "size_bytes",
                "system",
                "trim"
            ]
        );
        assert_eq!(json["media"], "hdd");
    }
}
