//! Startup tasks of packaged (MSIX / Microsoft Store) apps.
//!
//! A package declares startup tasks as `windows.startupTask` extensions in its manifest.
//! The current user's state of each task is the `State` DWORD under
//! `HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData\<PackageFamilyName>\<TaskId>`,
//! holding a `Windows.ApplicationModel.StartupTaskState` value. Task Manager and
//! Settings > Apps > Startup toggle a task by writing `DisabledByUser` (1) or `Enabled` (2)
//! there; the states set by Group Policy (3 and 4) cannot be changed by the user.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::SHLoadIndirectString;

use super::manifest::{self, Manifest};
pub(crate) use crate::win::package::{installed_packages, Package};
use crate::win::registry::{subkey_names, Hive, Key, RegValue};
use crate::win::wide;
use crate::Result;

/// Parent of one subkey per package family, each holding one subkey per startup task.
pub(super) const TASKS_ROOT: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";
/// DWORD value holding a task's [`TaskState`].
pub(super) const STATE_VALUE: &str = "State";

/// Buffer size, in UTF-16 units, for resolved resource strings.
const RESOURCE_BUFFER: usize = 1_024;

/// `Windows.ApplicationModel.StartupTaskState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskState {
    /// Off; the app may ask the user to turn it on.
    Disabled,
    /// Turned off by the user; only the user can turn it on again.
    DisabledByUser,
    Enabled,
    DisabledByPolicy,
    EnabledByPolicy,
    Unknown(u32),
}

impl TaskState {
    /// Raw value written to turn a task off, as Task Manager does.
    pub const DISABLE_RAW: u32 = 1;
    /// Raw value written to turn a task on.
    pub const ENABLE_RAW: u32 = 2;

    pub fn from_raw(raw: u32) -> TaskState {
        match raw {
            0 => TaskState::Disabled,
            1 => TaskState::DisabledByUser,
            2 => TaskState::Enabled,
            3 => TaskState::DisabledByPolicy,
            4 => TaskState::EnabledByPolicy,
            other => TaskState::Unknown(other),
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, TaskState::Enabled | TaskState::EnabledByPolicy)
    }

    pub fn set_by_policy(self) -> bool {
        matches!(
            self,
            TaskState::DisabledByPolicy | TaskState::EnabledByPolicy
        )
    }
}

/// One startup task key with its raw state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskKey {
    pub family: String,
    pub task_id: String,
    pub state: u32,
}

/// Registry path of the key of `task_id` of `family` below `root`.
pub(super) fn task_key_path(root: &str, family: &str, task_id: &str) -> String {
    format!(r"{root}\{family}\{task_id}")
}

/// Every `<family>\<task>` key below `root` (in HKCU) that has a DWORD `State` value.
/// Family and task keys that cannot be read (for example because of their ACL) are skipped;
/// only a failure to enumerate `root` itself is an error.
pub(super) fn task_keys(root: &str) -> Result<Vec<TaskKey>> {
    let mut out = Vec::new();
    for family in subkey_names(Hive::CurrentUser, root)? {
        let family_path = format!(r"{root}\{family}");
        let Ok(tasks) = subkey_names(Hive::CurrentUser, &family_path) else {
            continue;
        };
        for task_id in tasks {
            let path = format!(r"{family_path}\{task_id}");
            let Ok(Some(key)) = Key::open(Hive::CurrentUser, &path, false) else {
                continue;
            };
            if let Ok(Some(RegValue::Dword(state))) = key.query(STATE_VALUE) {
                out.push(TaskKey {
                    family: family.clone(),
                    task_id,
                    state,
                });
            }
        }
    }
    Ok(out)
}

/// Display fields of one startup task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskInfo {
    pub name: String,
    /// Executable the task starts; empty when unknown.
    pub path: String,
    /// The package's publisher display name; empty when unknown.
    pub publisher: String,
}

/// Describes `task_id` of `family` from the manifests of its installed `packages`.
/// `None` when every installed package's manifest was read and none declares the task
/// (a key left behind by an older version); a fallback name when no manifest is readable.
pub(super) fn describe(family: &str, task_id: &str, packages: &[Package]) -> Option<TaskInfo> {
    let package_name = package_name(family);
    let mut any_manifest = false;
    for package in packages {
        let Some(dir) = &package.install_dir else {
            continue;
        };
        let Some(manifest) = read_manifest(dir) else {
            continue;
        };
        any_manifest = true;
        let Some(task) = manifest.task(task_id) else {
            continue;
        };
        let resolve = |value: &Option<String>| {
            value
                .as_deref()
                .and_then(|v| resolve_resource(v, &package.full_name, package_name))
        };
        let name = resolve(&task.display_name)
            .or_else(|| resolve(&manifest.display_name))
            .unwrap_or_else(|| fallback_name(family, task_id));
        let path = task
            .executable
            .as_deref()
            .map(|exe| dir.join(exe.replace('/', "\\")).display().to_string())
            .unwrap_or_default();
        let publisher = resolve(&manifest.publisher_display_name).unwrap_or_default();
        return Some(TaskInfo {
            name,
            path,
            publisher,
        });
    }
    if any_manifest {
        return None;
    }
    Some(TaskInfo {
        name: fallback_name(family, task_id),
        path: String::new(),
        publisher: String::new(),
    })
}

/// Name part of a package family name (`<Name>_<PublisherId>`).
pub(super) fn package_name(family: &str) -> &str {
    family.rsplit_once('_').map_or(family, |(name, _)| name)
}

pub(super) fn fallback_name(family: &str, task_id: &str) -> String {
    format!("{} ({task_id})", package_name(family))
}

fn read_manifest(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join("AppxManifest.xml")).ok()?;
    let text = if let Some(utf16) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        let utf8 = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        String::from_utf8_lossy(utf8).into_owned()
    };
    Some(manifest::parse(&text))
}

/// A manifest string with `ms-resource:` references resolved through the package's
/// resources. `None` when the value is empty or a reference cannot be resolved.
fn resolve_resource(value: &str, full_name: &str, package_name: &str) -> Option<String> {
    let value = value.trim();
    const SCHEME: &str = "ms-resource:";
    let is_reference = value
        .get(..SCHEME.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(SCHEME));
    if !is_reference {
        return (!value.is_empty()).then(|| value.to_string());
    }
    resource_uris(&value[SCHEME.len()..], package_name)
        .into_iter()
        .find_map(|uri| load_indirect(&format!("@{{{full_name}?{uri}}}")))
}

/// Candidate `ms-resource` URIs for the part of a reference after `ms-resource:`.
fn resource_uris(key: &str, package_name: &str) -> Vec<String> {
    if key.starts_with("//") {
        vec![format!("ms-resource:{key}")]
    } else if key.starts_with('/') {
        vec![
            format!("ms-resource://{key}"),
            format!("ms-resource://{package_name}{key}"),
        ]
    } else {
        vec![
            format!("ms-resource:///resources/{key}"),
            format!("ms-resource:///{key}"),
            format!("ms-resource://{package_name}/resources/{key}"),
            format!("ms-resource://{package_name}/{key}"),
        ]
    }
}

/// Resolves an indirect string (`@{<package full name>?ms-resource://...}`).
fn load_indirect(source: &str) -> Option<String> {
    let source_w = wide(source);
    let mut buffer = vec![0u16; RESOURCE_BUFFER];
    // SAFETY: `source_w` is NUL-terminated and `buffer` is writable for its full length.
    unsafe { SHLoadIndirectString(PCWSTR(source_w.as_ptr()), &mut buffer, None) }.ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..end]).trim().to_string();
    (!text.is_empty() && !text.starts_with('@')).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_map_to_enabled_and_policy() {
        let cases = [
            (0, TaskState::Disabled, false, false),
            (1, TaskState::DisabledByUser, false, false),
            (2, TaskState::Enabled, true, false),
            (3, TaskState::DisabledByPolicy, false, true),
            (4, TaskState::EnabledByPolicy, true, true),
            (9, TaskState::Unknown(9), false, false),
        ];
        for (raw, state, enabled, policy) in cases {
            assert_eq!(TaskState::from_raw(raw), state);
            assert_eq!(state.is_enabled(), enabled, "{state:?}");
            assert_eq!(state.set_by_policy(), policy, "{state:?}");
        }
        assert_eq!(
            TaskState::from_raw(TaskState::ENABLE_RAW),
            TaskState::Enabled
        );
        assert_eq!(
            TaskState::from_raw(TaskState::DISABLE_RAW),
            TaskState::DisabledByUser
        );
    }

    #[test]
    fn family_names_split_at_the_publisher_id() {
        assert_eq!(
            package_name("Microsoft.WindowsTerminal_8wekyb3d8bbwe"),
            "Microsoft.WindowsTerminal"
        );
        assert_eq!(package_name("NoPublisherId"), "NoPublisherId");
        assert_eq!(
            fallback_name("Contoso.App_abc", "Start"),
            "Contoso.App (Start)"
        );
    }

    #[test]
    fn resource_references_expand_to_package_uris() {
        assert_eq!(
            resource_uris("//Pkg/Res/AppName", "Pkg"),
            ["ms-resource://Pkg/Res/AppName"]
        );
        assert_eq!(
            resource_uris("/Resources/AppName", "Pkg"),
            [
                "ms-resource:///Resources/AppName",
                "ms-resource://Pkg/Resources/AppName"
            ]
        );
        assert_eq!(
            resource_uris("AppName", "Pkg"),
            [
                "ms-resource:///resources/AppName",
                "ms-resource:///AppName",
                "ms-resource://Pkg/resources/AppName",
                "ms-resource://Pkg/AppName"
            ]
        );
        assert_eq!(
            resolve_resource("  Plain Name ", "Pkg_1.0.0.0_x64__abc", "Pkg").as_deref(),
            Some("Plain Name")
        );
        assert_eq!(resolve_resource("", "Pkg_1.0.0.0_x64__abc", "Pkg"), None);
        assert_eq!(
            resolve_resource(
                "ms-resource:AppName",
                "PCOptimizer.Missing_1.0.0.0_x64__abc",
                "PCOptimizer.Missing"
            ),
            None
        );
    }

    #[test]
    fn manifest_of_a_package_folder_describes_its_task() {
        let dir = tempfile::tempdir().unwrap();
        let exe_dir = dir.path().join("Bin");
        std::fs::create_dir(&exe_dir).unwrap();
        let mut manifest = vec![0xEF, 0xBB, 0xBF];
        manifest.extend_from_slice(
            br#"<Package><Properties><DisplayName>Contoso</DisplayName>
<PublisherDisplayName>Contoso Ltd</PublisherDisplayName></Properties>
<Applications><Application Id="App" Executable="Bin/App.exe"><Extensions>
<Extension Category="windows.startupTask"><StartupTask TaskId="Named" DisplayName="Contoso Helper"/></Extension>
<Extension Category="windows.startupTask"><StartupTask TaskId="Unnamed"/></Extension>
<Extension Category="windows.startupTask"><StartupTask TaskId="Unresolved" DisplayName="ms-resource:Missing"/></Extension>
</Extensions></Application></Applications></Package>"#,
        );
        std::fs::write(dir.path().join("AppxManifest.xml"), manifest).unwrap();
        let family = "Contoso.App_0000000000000";
        let packages = [
            Package {
                full_name: "Contoso.App_1.0.0.0_neutral_split.scale-100_0000000000000".into(),
                install_dir: None,
            },
            Package {
                full_name: "Contoso.App_1.0.0.0_x64__0000000000000".into(),
                install_dir: Some(dir.path().to_path_buf()),
            },
        ];
        let exe = exe_dir.join("App.exe").display().to_string();

        assert_eq!(
            describe(family, "Named", &packages),
            Some(TaskInfo {
                name: "Contoso Helper".into(),
                path: exe.clone(),
                publisher: "Contoso Ltd".into(),
            })
        );
        assert_eq!(
            describe(family, "Unnamed", &packages).unwrap().name,
            "Contoso"
        );
        assert_eq!(
            describe(family, "Unresolved", &packages).unwrap().name,
            "Contoso"
        );
        // Declared by no readable manifest: a stale key.
        assert_eq!(describe(family, "Removed", &packages), None);
        // No readable manifest at all: listed under a fallback name.
        assert_eq!(
            describe(family, "Removed", &packages[..1]),
            Some(TaskInfo {
                name: "Contoso.App (Removed)".into(),
                path: String::new(),
                publisher: String::new(),
            })
        );
    }
}
