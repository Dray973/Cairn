//! Embeds the manifest and the version information of optctl.exe and cairn-maintenance.exe.
//!
//! The manifest of both programs asks for administrator rights when they start
//! (requireAdministrator) and keeps the system code page: activeCodePage System keeps
//! `GetOEMCP()` at the console code page the servicing tools write in, where embed-manifest's
//! UTF-8 default would make it return 65001. Each program gets its own version resource,
//! compiled by `cairn_winres`; without rc.exe they build without one and a warning says so,
//! unless `CAIRN_REQUIRE_RESOURCES=1` makes that an error.

use std::env;
use std::path::PathBuf;

use cairn_winres::{FileKind, Resource, Version};
use embed_manifest::manifest::{ActiveCodePage, ExecutionLevel};
use embed_manifest::{embed_manifest, new_manifest};

/// Each program of this package: its cargo target name and its resources.
const PROGRAMS: [(&str, Resource<'static>); 2] = [
    (
        "optctl",
        Resource {
            description: "Cairn command line",
            internal_name: "optctl",
            original_filename: "optctl.exe",
            kind: FileKind::Program,
            icon: None,
        },
    ),
    (
        "cairn-maintenance",
        Resource {
            description: "Cairn scheduled maintenance",
            internal_name: "cairn-maintenance",
            original_filename: "cairn-maintenance.exe",
            kind: FileKind::Program,
            icon: None,
        },
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    cairn_winres::rerun_if_env_changed();
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let manifest = new_manifest("PCOptimizer.optctl")
        .active_code_page(ActiveCodePage::System)
        .requested_execution_level(ExecutionLevel::RequireAdministrator)
        .ui_access(false);
    embed_manifest(manifest).expect("failed to embed Win32 manifest");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let version = Version::of_package();
    for (target, resource) in &PROGRAMS {
        match cairn_winres::compile(resource, &version, &out_dir, target) {
            Ok(res) => println!("cargo:rustc-link-arg-bin={target}={}", res.display()),
            Err(problem) => cairn_winres::unavailable(resource.original_filename, &problem),
        }
    }
}
