//! The installed apps as `winget export --include-versions` writes them: JSON in winget's
//! packages schema 2.0, the same in every display language.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// One installed package winget can manage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledPackage {
    pub id: String,
    pub version: Option<String>,
    /// Name of the winget source the package comes from, such as "winget" or "msstore".
    pub source: String,
    pub scope: Option<String>,
}

/// The installed packages and the names of the sources they come from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub packages: Vec<InstalledPackage>,
    pub sources: Vec<String>,
}

impl Inventory {
    /// The package with id `id`, ignoring ASCII case.
    pub fn find(&self, id: &str) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| p.id.eq_ignore_ascii_case(id))
    }

    /// Every package id, in the export's order.
    pub fn ids(&self) -> Vec<String> {
        self.packages.iter().map(|p| p.id.clone()).collect()
    }
}

/// Largest export file read.
pub const MAX_EXPORT_BYTES: u64 = 16 * 1024 * 1024;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Deserialize)]
struct ExportFile {
    #[serde(rename = "Sources", default)]
    sources: Vec<ExportSource>,
}

#[derive(Deserialize)]
struct ExportSource {
    #[serde(rename = "SourceDetails", default)]
    details: SourceDetails,
    #[serde(rename = "Packages", default)]
    packages: Vec<ExportPackage>,
}

#[derive(Deserialize, Default)]
struct SourceDetails {
    #[serde(rename = "Name", default)]
    name: String,
}

#[derive(Deserialize)]
struct ExportPackage {
    #[serde(rename = "PackageIdentifier")]
    id: String,
    #[serde(rename = "Version", default)]
    version: Option<String>,
    #[serde(rename = "Scope", default)]
    scope: Option<String>,
}

/// Reads an export file. Unknown fields are ignored; a UTF-8 byte order mark is skipped.
/// Fails for invalid JSON and for more than [`MAX_EXPORT_BYTES`].
pub fn parse_export(bytes: &[u8]) -> Result<Inventory> {
    if bytes.len() as u64 > MAX_EXPORT_BYTES {
        return Err(Error::Other(format!(
            "the installed apps list is larger than {MAX_EXPORT_BYTES} bytes"
        )));
    }
    let bytes = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let file: ExportFile = serde_json::from_slice(bytes)?;
    let mut inventory = Inventory::default();
    for source in file.sources {
        let name = source.details.name.trim().to_string();
        if !name.is_empty() && !inventory.sources.contains(&name) {
            inventory.sources.push(name.clone());
        }
        for package in source.packages {
            let id = package.id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            inventory.packages.push(InstalledPackage {
                id,
                version: package.version.filter(|v| !v.trim().is_empty()),
                source: name.clone(),
                scope: package.scope.filter(|s| !s.trim().is_empty()),
            });
        }
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "$schema": "https://aka.ms/winget-packages.schema.2.0.json",
      "CreationDate": "2026-09-25T10:00:00.000-00:00",
      "Sources": [
        {
          "Packages": [
            { "PackageIdentifier": "Contoso.Editor", "Version": "1.2.0" },
            { "PackageIdentifier": "Fabrikam.Player", "Version": "2.0.0", "Scope": "user" },
            { "PackageIdentifier": "Northwind.Tools" }
          ],
          "SourceDetails": {
            "Argument": "https://cdn.winget.microsoft.com/cache",
            "Identifier": "Microsoft.Winget.Source_8wekyb3d8bbwe",
            "Name": "winget",
            "Type": "Microsoft.PreIndexed.Package"
          }
        },
        {
          "Packages": [ { "PackageIdentifier": "9NTAILSPIN0001", "Version": "1.0.0.0" } ],
          "SourceDetails": { "Name": "msstore", "Type": "Microsoft.Rest", "Extra": 1 }
        }
      ],
      "WinGetVersion": "1.29.380"
    }"#;

    #[test]
    fn a_sample_with_two_sources_and_a_bom() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(SAMPLE.as_bytes());
        let inventory = parse_export(&bytes).unwrap();
        assert_eq!(inventory.sources, ["winget", "msstore"]);
        assert_eq!(
            inventory.ids(),
            [
                "Contoso.Editor",
                "Fabrikam.Player",
                "Northwind.Tools",
                "9NTAILSPIN0001"
            ]
        );
        let player = inventory.find("fabrikam.PLAYER").unwrap();
        assert_eq!(player.version.as_deref(), Some("2.0.0"));
        assert_eq!(player.scope.as_deref(), Some("user"));
        assert_eq!(player.source, "winget");
        let tools = inventory.find("Northwind.Tools").unwrap();
        assert_eq!(tools.version, None);
        assert_eq!(tools.scope, None);
        assert_eq!(inventory.find("9ntailspin0001").unwrap().source, "msstore");
        assert!(inventory.find("Missing.App").is_none());
    }

    #[test]
    fn empty_packages_and_no_sources() {
        let inventory =
            parse_export(br#"{"Sources":[{"Packages":[],"SourceDetails":{"Name":"winget"}}]}"#)
                .unwrap();
        assert!(inventory.packages.is_empty());
        assert_eq!(inventory.sources, ["winget"]);
        assert_eq!(parse_export(b"{}").unwrap(), Inventory::default());
    }

    #[test]
    fn invalid_or_oversized_input_is_an_error() {
        assert!(parse_export(b"not json").is_err());
        assert!(parse_export(br#"{"Sources":[{"Packages":[{"Version":"1"}]}]}"#).is_err());
        let huge = vec![b' '; (MAX_EXPORT_BYTES + 1) as usize];
        let err = parse_export(&huge).unwrap_err();
        assert!(err.to_string().contains("larger than"), "{err}");
    }
}
