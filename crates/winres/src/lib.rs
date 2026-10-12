//! Version resources of Cairn's Windows binaries, for their build scripts.
//!
//! Every program and library Cairn ships carries a VERSIONINFO block with the product name
//! `Cairn` and the workspace version: the metadata Windows shows in a file's properties and
//! code signing checks. A build script describes its binary with [`Resource`], and [`compile`]
//! writes the resource script into `OUT_DIR` and compiles it with the Windows SDK's rc.exe
//! (`RC` when set, else the x64 rc.exe of the newest installed Windows 10/11 SDK) into a `.res`
//! file, which the build script hands to the linker.
//!
//! Without rc.exe a binary builds without its resources and a cargo warning says so, unless
//! `CAIRN_REQUIRE_RESOURCES=1` (release builds): then [`unavailable`] fails the build, because
//! a binary without a version resource cannot be signed with its product name and version.

#![warn(missing_debug_implementations)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `ProductName` of every binary.
pub const PRODUCT_NAME: &str = "Cairn";
/// `CompanyName` of every binary.
pub const COMPANY_NAME: &str = "Dray973";
/// `LegalCopyright` of every binary.
pub const COPYRIGHT: &str = "Copyright (c) 2026 Dray973. MIT License.";
/// Set to "1" to fail the build when a binary's resources cannot be compiled.
pub const REQUIRE_VARIABLE: &str = "CAIRN_REQUIRE_RESOURCES";
/// The path of rc.exe, when set.
pub const RC_VARIABLE: &str = "RC";

/// The kind of file a resource describes (VERSIONINFO `FILETYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A program (`VFT_APP`).
    Program,
    /// A dynamic-link library (`VFT_DLL`), such as a Python extension module.
    Library,
}

impl FileKind {
    fn file_type(self) -> &'static str {
        match self {
            FileKind::Program => "0x1L",
            FileKind::Library => "0x2L",
        }
    }
}

/// The resources of one binary.
#[derive(Debug, Clone, Copy)]
pub struct Resource<'a> {
    /// `FileDescription`: the name Task Manager and the file's properties show.
    pub description: &'a str,
    /// `InternalName`.
    pub internal_name: &'a str,
    /// `OriginalFilename`: the file's name as it is shipped.
    pub original_filename: &'a str,
    pub kind: FileKind,
    /// The program's icon (resource id 1); it must exist.
    pub icon: Option<&'a Path>,
}

/// The version a binary's resources carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    /// `FileVersion` and `ProductVersion` as text, such as "0.3.0".
    pub text: String,
}

impl Version {
    /// The version of the package being built (`CARGO_PKG_VERSION`); a part that is not a
    /// 16-bit number counts as 0 in the numeric fields.
    pub fn of_package() -> Version {
        let number = |name: &str| {
            env::var(name)
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(0)
        };
        Version {
            major: number("CARGO_PKG_VERSION_MAJOR"),
            minor: number("CARGO_PKG_VERSION_MINOR"),
            patch: number("CARGO_PKG_VERSION_PATCH"),
            text: env::var("CARGO_PKG_VERSION").unwrap_or_default(),
        }
    }
}

/// `text` as the body of a resource-script string: backslashes doubled and quotes written
/// twice.
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\"\"")
}

/// The resource script of `resource` at `version`: its icon, if any, and its VERSIONINFO
/// block (US English, Unicode).
pub fn script(resource: &Resource<'_>, version: &Version) -> String {
    let icon = match resource.icon {
        Some(path) => format!("1 ICON \"{}\"\n\n", quoted(&path.display().to_string())),
        None => String::new(),
    };
    let Version {
        major,
        minor,
        patch,
        text,
    } = version;
    let strings = [
        ("CompanyName", COMPANY_NAME),
        ("FileDescription", resource.description),
        ("FileVersion", text.as_str()),
        ("InternalName", resource.internal_name),
        ("LegalCopyright", COPYRIGHT),
        ("OriginalFilename", resource.original_filename),
        ("ProductName", PRODUCT_NAME),
        ("ProductVersion", text.as_str()),
    ];
    let values: String = strings
        .iter()
        .map(|(name, value)| format!("            VALUE \"{name}\", \"{}\"\n", quoted(value)))
        .collect();
    let file_type = resource.kind.file_type();
    format!(
        r#"{icon}1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE {file_type}
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
{values}        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

/// Writes `<stem>.rc` into `out_dir` and compiles it into `<stem>.res` there; the path of the
/// `.res` file, or why it could not be made.
pub fn compile(
    resource: &Resource<'_>,
    version: &Version,
    out_dir: &Path,
    stem: &str,
) -> Result<PathBuf, String> {
    if let Some(icon) = resource.icon {
        if !icon.is_file() {
            return Err(format!("{} does not exist", icon.display()));
        }
    }
    let rc = find_rc().ok_or("rc.exe of the Windows SDK was not found (set RC to its path)")?;
    let rc_file = out_dir.join(format!("{stem}.rc"));
    let res_file = out_dir.join(format!("{stem}.res"));
    fs::write(&rc_file, script(resource, version))
        .map_err(|e| format!("writing {}: {e}", rc_file.display()))?;
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/c65001")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status()
        .map_err(|e| format!("starting {}: {e}", rc.display()))?;
    if !status.success() {
        return Err(format!("{} failed with {status}", rc.display()));
    }
    Ok(res_file)
}

/// `RC` when set (None when it names no file), else the x64 rc.exe of the newest Windows 10/11
/// SDK under Program Files (x86).
pub fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os(RC_VARIABLE) {
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
        let Some(version) = name.to_str().and_then(sdk_version) else {
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
fn sdk_version(name: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = name.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() == 4)
}

/// Tells cargo to run the build script again when `RC` or `CAIRN_REQUIRE_RESOURCES` change.
pub fn rerun_if_env_changed() {
    println!("cargo:rerun-if-env-changed={RC_VARIABLE}");
    println!("cargo:rerun-if-env-changed={REQUIRE_VARIABLE}");
}

/// Whether a binary must not be built without its resources (`CAIRN_REQUIRE_RESOURCES=1`).
pub fn required() -> bool {
    env::var(REQUIRE_VARIABLE).is_ok_and(|v| v == "1")
}

/// Reports that `binary` is built without its resources because of `problem`: a cargo
/// warning, or, with `CAIRN_REQUIRE_RESOURCES=1`, a panic that fails the build.
pub fn unavailable(binary: &str, problem: &str) {
    if required() {
        panic!("{binary} must carry its version resource ({REQUIRE_VARIABLE}=1): {problem}");
    }
    println!("cargo:warning={binary} is built without its version resource: {problem}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version() -> Version {
        Version {
            major: 0,
            minor: 3,
            patch: 0,
            text: "0.3.0".into(),
        }
    }

    fn engine() -> Resource<'static> {
        Resource {
            description: "Cairn engine",
            internal_name: "optimizer_engine",
            original_filename: "optimizer_engine.pyd",
            kind: FileKind::Library,
            icon: None,
        }
    }

    #[test]
    fn script_names_the_product_and_its_version() {
        let text = script(&engine(), &version());
        assert!(text.contains("FILEVERSION 0,3,0,0\n"));
        assert!(text.contains("PRODUCTVERSION 0,3,0,0\n"));
        assert!(text.contains("VALUE \"ProductName\", \"Cairn\"\n"));
        assert!(text.contains("VALUE \"ProductVersion\", \"0.3.0\"\n"));
        assert!(text.contains("VALUE \"FileVersion\", \"0.3.0\"\n"));
        assert!(text.contains("VALUE \"CompanyName\", \"Dray973\"\n"));
        assert!(text.contains("VALUE \"FileDescription\", \"Cairn engine\"\n"));
        assert!(text.contains("VALUE \"OriginalFilename\", \"optimizer_engine.pyd\"\n"));
        assert!(text.contains("VALUE \"InternalName\", \"optimizer_engine\"\n"));
        assert!(text.contains("VALUE \"Translation\", 0x409, 1200"));
    }

    #[test]
    fn script_marks_libraries_and_programs() {
        assert!(script(&engine(), &version()).contains("FILETYPE 0x2L\n"));
        let program = Resource {
            kind: FileKind::Program,
            ..engine()
        };
        assert!(script(&program, &version()).contains("FILETYPE 0x1L\n"));
    }

    #[test]
    fn script_has_an_icon_only_when_given_one() {
        assert!(!script(&engine(), &version()).contains("ICON"));
        let icon = Path::new(r"C:\repo\ui\optimizer\assets\cairn.ico");
        let program = Resource {
            icon: Some(icon),
            kind: FileKind::Program,
            ..engine()
        };
        let text = script(&program, &version());
        assert!(
            text.starts_with("1 ICON \"C:\\\\repo\\\\ui\\\\optimizer\\\\assets\\\\cairn.ico\"\n")
        );
    }

    #[test]
    fn script_strings_are_escaped() {
        assert_eq!(quoted(r#"a "b" \c"#), r#"a ""b"" \\c"#);
        let odd = Resource {
            description: "say \"hi\"",
            ..engine()
        };
        assert!(
            script(&odd, &version()).contains("VALUE \"FileDescription\", \"say \"\"hi\"\"\"\n")
        );
    }

    #[test]
    fn sdk_folders_are_ordered_by_version() {
        assert_eq!(sdk_version("10.0.26100.0"), Some(vec![10, 0, 26100, 0]));
        assert_eq!(sdk_version("10.0.22621.0"), Some(vec![10, 0, 22621, 0]));
        assert!(sdk_version("10.0.26100.0") > sdk_version("10.0.22621.0"));
        assert_eq!(sdk_version("arm64"), None);
        assert_eq!(sdk_version("10.0.26100"), None);
    }

    #[test]
    fn a_missing_icon_is_refused_before_rc_runs() {
        let missing = Path::new(r"C:\no\such\folder\cairn.ico");
        let program = Resource {
            icon: Some(missing),
            ..engine()
        };
        let out = env::temp_dir();
        let problem = compile(&program, &version(), &out, "never-written").unwrap_err();
        assert!(problem.contains("does not exist"), "{problem}");
    }
}
