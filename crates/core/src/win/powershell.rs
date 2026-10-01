//! Headless PowerShell invocation for operations Windows exposes only through cmdlets
//! (Appx packaging, System Protection configuration, the Delivery Optimization cache).
//!
//! Windows PowerShell 5.1 is started by its absolute System32 path through
//! [`hardened_command`], so it gets the computed child environment with `PSModulePath` set to
//! the System32 modules folder: modules in the user's Documents folder are never searched.
//! Scripts import every non-core module they use with [`import_system_module`] and call its
//! cmdlets module-qualified (`Appx\Get-AppxPackage`), so a same-named function or alias
//! defined anywhere else is never picked up.

use std::os::windows::process::CommandExt;

use serde::de::DeserializeOwned;

use crate::win::paths::system_dir;
use crate::win::process::{hardened_command, CREATE_NO_WINDOW};
use crate::{Error, Result};

/// Windows PowerShell 5.1, relative to System32.
const POWERSHELL: &str = r"WindowsPowerShell\v1.0\powershell.exe";
/// Modules shipped with Windows, relative to System32.
const SYSTEM_MODULES: &str = r"WindowsPowerShell\v1.0\Modules";

/// Runs a script in Windows PowerShell 5.1 with no profile, no window and no prompts.
/// Returns stdout (UTF-8). A non-zero exit code becomes [`Error::PowerShell`].
pub fn run(script: &str) -> Result<String> {
    let wrapped = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $ErrorActionPreference = 'Stop'; {script}"
    );
    let output = hardened_command(&system_dir()?.join(POWERSHELL))?
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &wrapped,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::PowerShell {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Runs a script whose final expression is piped through `ConvertTo-Json` and
/// deserializes the result. An empty pipeline yields `T::default()` semantics via
/// `Option`: callers expecting arrays should request `Vec<T>` and handle `null`.
pub fn run_json<T: DeserializeOwned>(script: &str) -> Result<T> {
    let text = run(&json_script(script))?;
    let text = text.trim();
    let text = if text.is_empty() { "[]" } else { text };
    Ok(serde_json::from_str(text)?)
}

/// The script [`run_json`] runs for `script`.
fn json_script(script: &str) -> String {
    format!("@({script}) | Microsoft.PowerShell.Utility\\ConvertTo-Json -Depth 6 -Compress")
}

/// `Import-Module -Name '<System32>\WindowsPowerShell\v1.0\Modules\<name>\<name>.psd1'
/// -ErrorAction Stop; `, the statement a script starts with before it calls `name`'s cmdlets
/// module-qualified. Fails when `name` is not a plain module name (letters and dots) or the
/// module's manifest does not exist in the System32 modules folder.
pub fn import_system_module(name: &str) -> Result<String> {
    if !is_module_name(name) {
        return Err(Error::Other(format!("not a Windows module name: {name:?}")));
    }
    let manifest = system_dir()?
        .join(SYSTEM_MODULES)
        .join(name)
        .join(format!("{name}.psd1"));
    if !manifest.is_file() {
        return Err(Error::Other(format!(
            "the Windows PowerShell module {name} is not installed ({} is missing)",
            manifest.display()
        )));
    }
    Ok(import_statement(&manifest.to_string_lossy()))
}

/// Letters and dots only, starting and ending with a letter.
fn is_module_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'.')
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
}

/// The import statement for a module manifest path, quoted as a PowerShell literal string.
fn import_statement(manifest: &str) -> String {
    format!(
        "Import-Module -Name '{}' -ErrorAction Stop; ",
        manifest.replace('\'', "''")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_is_converted_module_qualified() {
        assert_eq!(
            json_script("Get-Thing"),
            r"@(Get-Thing) | Microsoft.PowerShell.Utility\ConvertTo-Json -Depth 6 -Compress"
        );
    }

    #[test]
    fn system_modules_are_imported_by_absolute_path() {
        let line = import_system_module("Appx").unwrap();
        let expected = system_dir()
            .unwrap()
            .join(r"WindowsPowerShell\v1.0\Modules\Appx\Appx.psd1");
        assert_eq!(
            line,
            format!(
                "Import-Module -Name '{}' -ErrorAction Stop; ",
                expected.display()
            )
        );
        assert!(import_system_module("DeliveryOptimization").is_ok());
        assert!(import_system_module("Microsoft.PowerShell.Management").is_ok());
    }

    #[test]
    fn module_names_are_validated() {
        for bad in [
            "",
            "..",
            ".Appx",
            "Appx.",
            "Ap..px",
            r"..\Appx",
            "Appx;Remove-Item",
            "Appx'",
            "Appx Module",
            "Appx1",
            "C:Appx",
        ] {
            assert!(import_system_module(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(import_system_module("PCOptimizerNoSuchModule").is_err());
    }

    #[test]
    fn quotes_in_the_path_are_doubled() {
        assert_eq!(
            import_statement(r"C:\it's\m.psd1"),
            r"Import-Module -Name 'C:\it''s\m.psd1' -ErrorAction Stop; "
        );
    }
}
