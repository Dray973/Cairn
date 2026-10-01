//! Camera, microphone and location permissions, as a guide.
//!
//! Windows 11 manages these permissions itself, in Settings › Privacy & security. On Windows 11
//! 25H2 Settings applies a change through an internal path: the capability consent store in the
//! registry and the location sensor's override are only a copy of what Settings shows, and
//! writing them changes nothing Windows enforces (with the location switch for the whole PC or
//! the desktop-apps switch written as Deny, location stays on for apps and in Settings); the
//! camera and microphone switches for the whole PC are not kept there at all. So no permission
//! is changed here: [`list`] names each capability's Settings page and the desktop programs
//! Windows recorded using the device, and [`change_refused`] is the answer to every request to
//! allow, deny or plan a change, given before anything is read or a journal is opened.
//!
//! Journal records of permission changes made by earlier builds (consent-store values and the
//! location sensor's override) are ordinary registry records: History and Revert All restore
//! them with the other registry records, and no code here is involved.

mod store;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use self::store::{Layout, SYSTEM};
use crate::win::registry::{subkey_names, Key};
use crate::{Error, APP_NAME};

/// Desktop programs listed per capability, newest first.
const RECENT_LIMIT: usize = 50;

/// A device whose use Windows lets the user allow or deny per app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Camera,
    Microphone,
    Location,
}

impl Capability {
    pub const ALL: [Capability; 3] = [
        Capability::Camera,
        Capability::Microphone,
        Capability::Location,
    ];

    /// Serialized name: "camera", "microphone" or "location".
    pub fn key(self) -> &'static str {
        match self {
            Capability::Camera => "camera",
            Capability::Microphone => "microphone",
            Capability::Location => "location",
        }
    }

    /// Subkey of the consent store: "webcam", "microphone" or "location".
    pub fn store_key(self) -> &'static str {
        match self {
            Capability::Camera => "webcam",
            Capability::Microphone => "microphone",
            Capability::Location => "location",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::Camera => "Camera",
            Capability::Microphone => "Microphone",
            Capability::Location => "Location",
        }
    }

    /// The device in running text: "camera", "microphone" or "location".
    pub fn noun(self) -> &'static str {
        self.key()
    }

    /// The Windows Settings page that shows and changes the capability's permissions:
    /// Settings › Privacy & security › Camera, Microphone or Location.
    pub fn settings_uri(self) -> &'static str {
        match self {
            Capability::Camera => "ms-settings:privacy-webcam",
            Capability::Microphone => "ms-settings:privacy-microphone",
            Capability::Location => "ms-settings:privacy-location",
        }
    }
}

/// A desktop program Windows recorded using a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentUse {
    pub path: String,
    /// RFC 3339 UTC time of the newer of the start and stop stamps; None when neither is set.
    pub last_used: Option<String>,
    /// Started and not stopped.
    pub in_use: bool,
}

/// One capability of the guide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityGuide {
    pub capability: Capability,
    pub label: String,
    /// [`Capability::settings_uri`].
    pub settings_uri: String,
    /// Desktop programs with a usage record (the consent store's `NonPackaged` subkeys, which
    /// Windows writes), newest first, at most 50.
    pub recent_desktop_apps: Vec<RecentUse>,
}

/// The guide: per capability its Settings page and recent desktop use. It holds no
/// permission state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsGuide {
    /// Camera, Microphone, Location.
    pub capabilities: Vec<CapabilityGuide>,
    /// Usage records that could not be read.
    pub warnings: Vec<String>,
}

/// Every capability's Settings page and the desktop programs Windows recorded using it.
/// Reads Windows' usage records only and opens no journal.
pub fn list() -> PermissionsGuide {
    SYSTEM.guide()
}

/// The error every request to allow or deny a permission, or to plan that, gets, whatever
/// entry it names: no permission is changed here (see the module documentation). Nothing is
/// read, written or recorded, so callers return it before they open a journal or start a
/// session.
pub fn change_refused() -> Error {
    Error::Other(format!(
        "{APP_NAME} does not change app permissions: Windows 11 manages camera, microphone and \
         location permissions itself, in Settings › Privacy & security, and on this version of \
         Windows an app like {APP_NAME} cannot change them. Nothing was changed."
    ))
}

impl Layout<'_> {
    fn guide(&self) -> PermissionsGuide {
        let mut warnings = Vec::new();
        let capabilities = Capability::ALL
            .into_iter()
            .map(|cap| CapabilityGuide {
                capability: cap,
                label: cap.label().to_string(),
                settings_uri: cap.settings_uri().to_string(),
                recent_desktop_apps: self.recent_desktop_apps(cap, &mut warnings),
            })
            .collect();
        PermissionsGuide {
            capabilities,
            warnings,
        }
    }

    /// Desktop programs with a usage record under `NonPackaged`, newest first, at most 50.
    /// A program without a start or stop stamp is left out. Their paths are never logged.
    fn recent_desktop_apps(&self, cap: Capability, warnings: &mut Vec<String>) -> Vec<RecentUse> {
        let (hive, key) = self.desktop_apps(cap);
        let children = match subkey_names(hive, &key) {
            Ok(children) => children,
            Err(e) => {
                warnings.push(format!(
                    "{}: cannot list the desktop apps that used the {}: {e}",
                    cap.label(),
                    cap.noun()
                ));
                return Vec::new();
            }
        };
        let mut used = Vec::new();
        let mut unreadable = 0usize;
        for child in children {
            let times = Key::open(hive, &format!(r"{key}\{child}"), false).and_then(|k| match k {
                Some(k) => store::usage_times(&k),
                None => Ok((0, 0)),
            });
            match times {
                Ok((start, stop)) if start != 0 || stop != 0 => {
                    let (last_used, in_use) = store::usage(start, stop);
                    used.push((
                        start.max(stop),
                        RecentUse {
                            path: child.replace('#', "\\"),
                            last_used,
                            in_use,
                        },
                    ));
                }
                Ok(_) => {}
                Err(_) => unreadable += 1,
            }
        }
        if unreadable > 0 {
            warnings.push(format!(
                "{}: the usage of {unreadable} desktop app(s) could not be read",
                cap.label()
            ));
        }
        used.sort_by_key(|(newest, _)| std::cmp::Reverse(*newest));
        used.truncate(RECENT_LIMIT);
        used.into_iter().map(|(_, recent)| recent).collect()
    }
}
