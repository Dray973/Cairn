//! Embeds the version information of the engine module, which ships as optimizer_engine.pyd.
//!
//! The resource is compiled by `cairn_winres` with the Windows SDK's rc.exe and linked into the
//! cdylib only; without rc.exe the module builds without it and a warning says so, unless
//! `CAIRN_REQUIRE_RESOURCES=1` makes that an error.

use std::env;
use std::path::PathBuf;

use cairn_winres::{FileKind, Resource, Version};

const MODULE: Resource<'static> = Resource {
    description: "Cairn engine",
    internal_name: "optimizer_engine",
    original_filename: "optimizer_engine.pyd",
    kind: FileKind::Library,
    icon: None,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    cairn_winres::rerun_if_env_changed();
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    match cairn_winres::compile(
        &MODULE,
        &Version::of_package(),
        &out_dir,
        "optimizer_engine",
    ) {
        Ok(res) => println!("cargo:rustc-link-arg-cdylib={}", res.display()),
        Err(problem) => cairn_winres::unavailable(MODULE.original_filename, &problem),
    }
}
