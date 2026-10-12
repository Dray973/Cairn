//! Embeds Cairn.exe's manifest, icon and version information.
//!
//! The manifest asks for no elevation (asInvoker: the launcher decides and asks through the
//! UAC prompt itself), keeps the system code page (embed-manifest's UTF-8 default would make
//! `GetOEMCP()` return 65001 and garble console tool output the engine decodes) and leaves
//! DPI awareness to CustomTkinter, which sets it at run time. The icon and the version
//! resource are compiled by `cairn_winres` with the Windows SDK's rc.exe; without rc.exe the
//! launcher builds without them and a warning says so, unless `CAIRN_REQUIRE_RESOURCES=1`
//! makes that an error.

use std::env;
use std::path::{Path, PathBuf};

use cairn_winres::{FileKind, Resource, Version};
use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, ExecutionLevel};
use embed_manifest::{embed_manifest, new_manifest};

const PROGRAM: &str = "Cairn.exe";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    cairn_winres::rerun_if_env_changed();
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let manifest = new_manifest("Cairn.Launcher")
        .active_code_page(ActiveCodePage::System)
        .dpi_awareness(DpiAwareness::UnawareByDefault)
        .requested_execution_level(ExecutionLevel::AsInvoker)
        .ui_access(false);
    embed_manifest(manifest).expect("failed to embed the Win32 manifest");

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let Some(repo) = manifest_dir.parent().and_then(Path::parent) else {
        cairn_winres::unavailable(PROGRAM, "the repository folder was not found");
        return;
    };
    let icon = repo
        .join("ui")
        .join("optimizer")
        .join("assets")
        .join("cairn.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let resource = Resource {
        description: "Cairn",
        internal_name: "cairn",
        original_filename: PROGRAM,
        kind: FileKind::Program,
        icon: Some(&icon),
    };
    match cairn_winres::compile(&resource, &Version::of_package(), &out_dir, "cairn") {
        Ok(res) => println!("cargo:rustc-link-arg-bins={}", res.display()),
        Err(problem) => cairn_winres::unavailable(PROGRAM, &problem),
    }
}
