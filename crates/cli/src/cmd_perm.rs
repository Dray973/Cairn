//! `optctl perm`: where Windows Settings manages the camera, microphone and location
//! permissions of apps, and the desktop programs Windows recorded using each device. No
//! permission is changed: `perm set` is refused.

use clap::{Subcommand, ValueEnum};
use optimizer_core::permissions::{self, Capability, CapabilityGuide, PermissionsGuide};
use optimizer_core::APP_NAME;

#[derive(Subcommand, Debug)]
pub(crate) enum PermCmd {
    /// Print the Windows Settings page of each capability and the desktop programs Windows
    /// recorded using the device. Read-only.
    List {
        /// Only this capability.
        #[arg(long, value_enum)]
        capability: Option<CapabilityArg>,
        /// Print the guide as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refused: Windows 11 manages app permissions itself, in Settings, and Cairn changes none
    /// of them. Changes nothing and exits with an error, whatever the arguments.
    Set {
        /// Not used (an entry id, such as `camera:apps`, in earlier versions).
        id: Option<String>,
        /// Not used (allow or deny in earlier versions).
        decision: Option<String>,
        /// Not used.
        #[arg(long)]
        dry_run: bool,
        /// Not used.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub(crate) enum CapabilityArg {
    Camera,
    Microphone,
    Location,
}

impl From<CapabilityArg> for Capability {
    fn from(arg: CapabilityArg) -> Self {
        match arg {
            CapabilityArg::Camera => Capability::Camera,
            CapabilityArg::Microphone => Capability::Microphone,
            CapabilityArg::Location => Capability::Location,
        }
    }
}

pub(crate) fn run(cmd: PermCmd) -> anyhow::Result<()> {
    match cmd {
        PermCmd::List { capability, json } => {
            let mut guide = permissions::list();
            if let Some(only) = capability {
                let only = Capability::from(only);
                guide.capabilities.retain(|c| c.capability == only);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&guide)?);
            } else {
                print_guide(&guide);
            }
            Ok(())
        }
        PermCmd::Set {
            id,
            decision,
            dry_run,
            yes,
        } => {
            // Refused whatever the request names, before anything is read or a journal is
            // opened.
            let _ = (id, decision, dry_run, yes);
            Err(permissions::change_refused().into())
        }
    }
}

fn print_guide(guide: &PermissionsGuide) {
    println!(
        "Windows 11 manages camera, microphone and location permissions itself, in Settings › \
         Privacy & security; on this version of Windows an app like {APP_NAME} cannot change \
         them. Open a page with `start <page>`, such as `start ms-settings:privacy-webcam`."
    );
    for capability in &guide.capabilities {
        print_capability(capability);
    }
    for warning in &guide.warnings {
        println!("warning: {warning}");
    }
}

fn print_capability(capability: &CapabilityGuide) {
    let noun = capability.capability.noun();
    println!();
    println!("{}", capability.label);
    println!(
        "  Settings › Privacy & security › {}  {}",
        capability.label, capability.settings_uri
    );
    println!("  Recently used by (Windows' own record of desktop apps):");
    if capability.recent_desktop_apps.is_empty() {
        println!("    (Windows has no record of a desktop app using the {noun})");
    }
    for recent in &capability.recent_desktop_apps {
        let when = if recent.in_use {
            "in use now".to_string()
        } else {
            recent
                .last_used
                .as_deref()
                .map_or_else(String::new, |t| format!("last used {t}"))
        };
        println!("    {}  {when}", recent.path);
    }
}
