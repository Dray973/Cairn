//! Debloat and optimization engine.
//!
//! - [`catalog`]          the fixed set of tweaks and bloatware package definitions
//! - [`registry`]         policy and setting values (the telemetry purger's DWORD writes)
//! - [`services`]         start type and running state of background services
//! - [`scheduled_tasks`]  the enabled flag of Windows scheduled tasks
//! - [`appx`]             inventory, removal and re-registration of Store (UWP) packages
//! - [`power`]            the active power scheme
//! - [`engine`]           scan, apply and revert across all of the above
//! - [`live`]             settings pushed to the running session after their values change
//! - [`requirements`]     whether this PC has what a tweak needs (Office, Edge, GPU scheduling)
//!   and which Windows builds and editions use its settings
//! - `verify`             reading a catalog target back after it was written
//! - [`drift`]            recorded targets that no longer hold what Cairn set, and Keep as is
//!
//! Every mutation goes through [`crate::safety::Safety`], which journals the original
//! state first, so any change made here can be reverted item by item or all at once.

pub mod appx;
pub mod catalog;
pub mod drift;
pub mod engine;
pub mod live;
pub mod power;
pub mod registry;
pub mod requirements;
pub mod scheduled_tasks;
pub mod services;
pub(crate) mod verify;

use serde::{Deserialize, Serialize};

pub use crate::safety::verify::Mismatch;
pub use catalog::{Category, RestartNeed, Risk};
pub use drift::{DriftEntry, DriftKind, DriftReport, ForgetReport};
pub use engine::{
    catalog_view, ApplyOptions, ApplyReport, CatalogEntry, Engine, ItemKind, ItemOutcome,
    ItemResult, ItemState, ScanItem, ScanReport,
};

/// Whether one catalog action is already in its target state on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Applied,
    NotApplied,
    /// The target does not exist here (a service, scheduled task or power scheme missing
    /// from this edition), so the action cannot be applied.
    Unavailable,
}

/// State of one action plus a short human-readable description of what was found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionStatus {
    pub state: ActionState,
    pub detail: String,
    /// Set for a target with an active journal record: whether it still holds what the
    /// action sets (`Some(false)` is drift). `None` without a record.
    #[serde(default)]
    pub held: Option<bool>,
}

impl ActionStatus {
    pub fn new(state: ActionState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            held: None,
        }
    }
}
