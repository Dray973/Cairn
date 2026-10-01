use embed_manifest::manifest::{ActiveCodePage, ExecutionLevel};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // activeCodePage System keeps GetOEMCP() at the console code page the servicing tools
        // write in; embed-manifest's UTF-8 default would make it return 65001.
        let manifest = new_manifest("PCOptimizer.optctl")
            .active_code_page(ActiveCodePage::System)
            .requested_execution_level(ExecutionLevel::RequireAdministrator)
            .ui_access(false);
        embed_manifest(manifest).expect("failed to embed Win32 manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
