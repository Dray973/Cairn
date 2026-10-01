//! Embeds Cairn.exe's manifest, icon and version information.
//!
//! The manifest asks for no elevation (asInvoker: the launcher decides and asks through the
//! UAC prompt itself), keeps the system code page (embed-manifest's UTF-8 default would make
//! `GetOEMCP()` return 65001 and garble console tool output the engine decodes) and leaves
//! DPI awareness to CustomTkinter, which sets it at run time. The icon and the version
//! resource are compiled with the Windows SDK's rc.exe (`RC`, else the newest x64 rc.exe of
//! the installed SDKs); without rc.exe the launcher builds without them and a warning says so.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, ExecutionLevel};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RC");
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
        println!("cargo:warning=the repository folder was not found; Cairn.exe has no icon");
        return;
    };
    let icon = repo
        .join("ui")
        .join("optimizer")
        .join("assets")
        .join("cairn.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    if let Err(problem) = compile_resources(&icon, &out_dir) {
        println!(
            "cargo:warning=Cairn.exe is built without its icon and version information: {problem}"
        );
    }
}

/// Writes `cairn.rc`, compiles it to `cairn.res` and links it into the binaries.
fn compile_resources(icon: &Path, out_dir: &Path) -> Result<(), String> {
    if !icon.is_file() {
        return Err(format!("{} does not exist", icon.display()));
    }
    let rc = find_rc().ok_or("rc.exe of the Windows SDK was not found (set RC to its path)")?;
    let script = out_dir.join("cairn.rc");
    let res = out_dir.join("cairn.res");
    fs::write(&script, resource_script(icon)).map_err(|e| format!("writing cairn.rc: {e}"))?;
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/c65001")
        .arg("/fo")
        .arg(&res)
        .arg(&script)
        .status()
        .map_err(|e| format!("starting {}: {e}", rc.display()))?;
    if !status.success() {
        return Err(format!("{} failed with {status}", rc.display()));
    }
    println!("cargo:rustc-link-arg-bins={}", res.display());
    Ok(())
}

/// The resource script: the icon and a VERSIONINFO block built from the package version.
/// It holds no manifest (embed-manifest links that one).
fn resource_script(icon: &Path) -> String {
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let number = |name: &str| {
        env::var(name)
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0)
    };
    let (major, minor, patch) = (
        number("CARGO_PKG_VERSION_MAJOR"),
        number("CARGO_PKG_VERSION_MINOR"),
        number("CARGO_PKG_VERSION_PATCH"),
    );
    let icon = icon.display().to_string().replace('\\', "\\\\");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Dray973"
            VALUE "FileDescription", "Cairn"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "cairn"
            VALUE "LegalCopyright", "Copyright (c) 2026 Dray973. MIT License."
            VALUE "OriginalFilename", "Cairn.exe"
            VALUE "ProductName", "Cairn"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

/// `RC` when set, else the x64 rc.exe of the newest Windows 10/11 SDK under Program Files (x86).
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        let rc = PathBuf::from(rc);
        return rc.is_file().then_some(rc);
    }
    let program_files = env::var_os("ProgramFiles(x86)")?;
    let bin = Path::new(&program_files)
        .join("Windows Kits")
        .join("10")
        .join("bin");
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for entry in fs::read_dir(&bin).ok()?.flatten() {
        let name = entry.file_name();
        let Some(version) = name.to_str().and_then(parse_version) else {
            continue;
        };
        let rc = entry.path().join("x64").join("rc.exe");
        if rc.is_file() && best.as_ref().map_or(true, |(v, _)| version > *v) {
            best = Some((version, rc));
        }
    }
    best.map(|(_, rc)| rc)
}

/// "10.0.26100.0" as numbers; None for other folder names.
fn parse_version(name: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = name.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() == 4)
}
