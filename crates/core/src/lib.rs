//! optimizer_core: the engine behind Cairn.
//!
//! - [`safety`]      journal, System Restore checkpoints and rollback
//! - [`win`]         typed wrappers over the registry, Service Control Manager and PowerShell
//! - [`debloat`]     telemetry, Appx and service optimizers built on top of [`safety::Safety`]
//! - [`network`]     adapters, journaled DNS servers, resolver cache, DHCP lease and stack reset
//! - [`sysinfo`]     read-only snapshot of the hardware and Windows configuration
//! - [`tools`]       Windows maintenance tools (SFC, DISM, Optimize Drives, Check Disk) run as
//!   polled background jobs
//! - [`app`]         install location, uninstall steps
//! - [`jobs`]        engine-owned background jobs polled by the UI
//! - [`health`]      read-only security checkup and boot history
//! - [`maintenance`] scheduled maintenance task
//! - [`permissions`] read-only guide to the camera, microphone and location permissions, which
//!   Windows Settings manages
//! - [`profiles`]    settings profiles
//! - [`storage`]     disk speed test, space analyzer, duplicate finder
//! - [`updates`]     winget and Windows Update
//!
//! Every mutation goes through [`safety::Safety`]: the original state is journaled
//! before anything changes, and [`safety::rollback_to_baseline`] undoes it.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod app;
pub mod cleanup;
pub mod debloat;
pub mod error;
pub mod health;
pub mod jobs;
pub mod maintenance;
pub mod network;
pub mod permissions;
pub mod profiles;
pub mod safety;
pub mod startup;
pub mod storage;
pub mod sysinfo;
pub mod tools;
pub mod updates;
pub mod win;

pub use app::APP_NAME;
pub use error::{Error, Result};

/// Semantic version of the core engine (mirrors Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// True when the current process token is an elevated member of Administrators.
pub fn is_elevated() -> bool {
    // SAFETY: IsUserAnAdmin takes no arguments and touches no caller-owned memory.
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}
