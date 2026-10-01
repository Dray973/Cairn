//! Whether this PC has what a tweak's policies are for: Microsoft Office, Microsoft Edge,
//! or a graphics driver with hardware-accelerated GPU scheduling. Read-only.
//!
//! A tweak whose requirement is known to be missing scans as unavailable and is skipped on
//! apply before anything is recorded; a check that fails leaves the tweak available and
//! adds a warning.

use std::cell::OnceCell;
use std::path::Path;

use super::catalog::Requirement;
use crate::win::gpu::{self, AdapterCaps, HWSCH_ENABLED, HWSCH_SUPPORTED};
use crate::win::registry::{read_value, Hive, RegValue};
use crate::{Error, Result};

/// Click-to-Run configuration of Office; `ProductReleaseIds` lists the installed products.
const C2R_CONFIGURATION: &str = r"SOFTWARE\Microsoft\Office\ClickToRun\Configuration";
/// Windows Installer Office 2016 or later, 64-bit and 32-bit registration.
const MSI_INSTALL_ROOT: &str = r"SOFTWARE\Microsoft\Office\16.0\Common\InstallRoot";
const MSI_INSTALL_ROOT_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Office\16.0\Common\InstallRoot";
/// Where the shell finds msedge.exe (default value: the program path).
const EDGE_APP_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe";
/// EdgeUpdate client of stable Edge; `pv` is the installed version.
const EDGE_UPDATE_CLIENT: &str =
    r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{56EB18F8-B008-4CBD-B6D2-8C97FE7E9062}";

/// Hardware-accelerated GPU scheduling as the graphics drivers report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuScheduling {
    /// At least one adapter's driver supports it.
    pub supported: bool,
    /// At least one adapter runs with it now.
    pub enabled: bool,
}

/// The read-only checks behind the requirements. Production uses [`SYSTEM_PROBES`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Probes {
    pub office: fn() -> Result<bool>,
    pub edge: fn() -> Result<bool>,
    pub gpu: fn() -> Result<GpuScheduling>,
}

pub(crate) const SYSTEM_PROBES: Probes = Probes {
    office: office_installed,
    edge: edge_installed,
    gpu: gpu_scheduling,
};

/// Outcome of one requirement check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Availability {
    Met,
    /// Known to be missing; the text is [`Requirement::missing_text`].
    Missing(&'static str),
    /// The check itself failed; the tweak is treated as available and a warning is shown.
    Unknown(String),
}

/// Requirement answers for one scan or apply; each probe runs at most once.
#[derive(Debug)]
pub(crate) struct Requirements {
    probes: Probes,
    office: OnceCell<std::result::Result<bool, String>>,
    edge: OnceCell<std::result::Result<bool, String>>,
    gpu: OnceCell<std::result::Result<GpuScheduling, String>>,
}

impl Requirements {
    pub(crate) fn new(probes: Probes) -> Self {
        Self {
            probes,
            office: OnceCell::new(),
            edge: OnceCell::new(),
            gpu: OnceCell::new(),
        }
    }

    pub(crate) fn check(&self, req: Requirement) -> Availability {
        let present = match req {
            Requirement::Office => self
                .office
                .get_or_init(|| (self.probes.office)().map_err(|e| e.to_string()))
                .clone(),
            Requirement::Edge => self
                .edge
                .get_or_init(|| (self.probes.edge)().map_err(|e| e.to_string()))
                .clone(),
            Requirement::GpuScheduling => self.gpu_result().clone().map(|g| g.supported),
        };
        match present {
            Ok(true) => Availability::Met,
            Ok(false) => Availability::Missing(req.missing_text()),
            Err(e) => Availability::Unknown(e),
        }
    }

    /// The GPU probe's answer, when it succeeded.
    pub(crate) fn gpu(&self) -> Option<GpuScheduling> {
        self.gpu_result().as_ref().ok().copied()
    }

    fn gpu_result(&self) -> &std::result::Result<GpuScheduling, String> {
        self.gpu
            .get_or_init(|| (self.probes.gpu)().map_err(|e| e.to_string()))
    }
}

/// Reads one HKLM value; `Err` only when the key or value exists but cannot be read.
fn read(path: &str, name: &str) -> Result<Option<RegValue>> {
    read_value(Hive::LocalMachine, path, name)
}

/// Combines readings of which any one is enough to say "installed": a positive answer from
/// the readings that succeeded is certain; otherwise a failed read makes the answer unknown.
fn decide(
    readings: Vec<Result<Option<RegValue>>>,
    installed: impl FnOnce(&[Option<RegValue>]) -> bool,
) -> Result<bool> {
    let mut values = Vec::with_capacity(readings.len());
    let mut failure: Option<Error> = None;
    for reading in readings {
        match reading {
            Ok(value) => values.push(value),
            Err(e) => {
                values.push(None);
                failure.get_or_insert(e);
            }
        }
    }
    if installed(&values) {
        return Ok(true);
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(false),
    }
}

/// Microsoft Office 2016 or later (Click-to-Run, or Windows Installer 64-bit or 32-bit).
pub fn office_installed() -> Result<bool> {
    decide(
        vec![
            read(C2R_CONFIGURATION, "ProductReleaseIds"),
            read(MSI_INSTALL_ROOT, "Path"),
            read(MSI_INSTALL_ROOT_32, "Path"),
        ],
        |v| office_from(v[0].clone(), v[1].clone(), v[2].clone()),
    )
}

/// Microsoft Edge (the App Paths registration of an existing msedge.exe, or the EdgeUpdate
/// client of stable Edge).
pub fn edge_installed() -> Result<bool> {
    decide(
        vec![read(EDGE_APP_PATH, ""), read(EDGE_UPDATE_CLIENT, "pv")],
        |v| edge_from(v[0].clone(), v[1].clone(), &|p: &Path| p.is_file()),
    )
}

/// Hardware-accelerated GPU scheduling over every adapter the display kernel lists.
pub fn gpu_scheduling() -> Result<GpuScheduling> {
    Ok(gpu_from(&gpu::wddm_2_7_caps()?))
}

/// Text of a string value, trimmed; `None` for other types and blank strings.
fn text(value: Option<&RegValue>, expand_ok: bool) -> Option<&str> {
    let s = match value? {
        RegValue::Sz(s) => s,
        RegValue::ExpandSz(s) if expand_ok => s,
        _ => return None,
    };
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

fn office_from(
    c2r_release_ids: Option<RegValue>,
    msi64_path: Option<RegValue>,
    msi32_path: Option<RegValue>,
) -> bool {
    text(c2r_release_ids.as_ref(), false).is_some()
        || text(msi64_path.as_ref(), true).is_some()
        || text(msi32_path.as_ref(), true).is_some()
}

fn edge_from(
    app_path: Option<RegValue>,
    update_pv: Option<RegValue>,
    exists: &dyn Fn(&Path) -> bool,
) -> bool {
    let program = text(app_path.as_ref(), true)
        .map(|s| s.trim_matches('"').trim())
        .filter(|s| !s.is_empty());
    if program.is_some_and(|p| exists(Path::new(p))) {
        return true;
    }
    text(update_pv.as_ref(), false).is_some_and(|pv| pv != "0.0.0.0")
}

fn gpu_from(adapters: &[AdapterCaps]) -> GpuScheduling {
    let caps = adapters.iter().filter_map(|a| a.wddm_2_7);
    let mut out = GpuScheduling::default();
    for bits in caps {
        out.supported |= bits & HWSCH_SUPPORTED != 0;
        out.enabled |= bits & HWSCH_ENABLED != 0;
    }
    out
}

/// Note for the GPU scheduling tweak: what the GPU runs with now when that differs from the
/// stored setting. `stored` is HwSchMode (2 on, 1 off), `None` when absent.
pub(crate) fn gpu_note(stored: Option<u32>, gpu: GpuScheduling) -> Option<&'static str> {
    match (stored, gpu.enabled) {
        (Some(2), false) => {
            Some("Turned on in the settings; Windows starts using it after the next restart.")
        }
        (Some(1), true) => {
            Some("Turned off in the settings; the GPU keeps using it until the next restart.")
        }
        (None, true) => Some(
            "The GPU already uses it: Windows turned it on for this GPU without storing the \
             setting. Applying stores the setting so it stays on.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn sz(s: &str) -> Option<RegValue> {
        Some(RegValue::Sz(s.to_string()))
    }

    #[test]
    fn office_from_click_to_run_or_msi() {
        assert!(office_from(sz("O365HomePremRetail"), None, None));
        assert!(office_from(
            sz(" O365HomePremRetail,ProPlus2021Retail "),
            None,
            None
        ));
        assert!(!office_from(sz(""), None, None));
        assert!(!office_from(sz("   "), None, None));
        assert!(!office_from(Some(RegValue::Dword(1)), None, None));
        assert!(office_from(
            None,
            sz(r"C:\Program Files\Microsoft Office\root\Office16\"),
            None
        ));
        assert!(office_from(
            None,
            Some(RegValue::ExpandSz(
                r"%ProgramFiles%\Microsoft Office\".into()
            )),
            None
        ));
        assert!(office_from(
            None,
            None,
            sz(r"C:\Program Files (x86)\Microsoft Office\")
        ));
        assert!(!office_from(None, sz(""), sz(" ")));
        assert!(!office_from(None, Some(RegValue::Dword(0)), None));
        assert!(!office_from(None, None, None));
    }

    #[test]
    fn edge_from_app_path_or_update_client() {
        let edge = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
        let exists = |p: &Path| p == Path::new(edge);
        let missing = |_: &Path| false;
        assert!(edge_from(sz(&format!("\"{edge}\"")), None, &exists));
        assert!(edge_from(sz(&format!("  {edge} ")), None, &exists));
        assert!(!edge_from(sz(&format!("\"{edge}\"")), None, &missing));
        assert!(!edge_from(sz("\"\""), None, &exists));
        assert!(edge_from(None, sz("154.0.1.2"), &missing));
        assert!(edge_from(sz(edge), sz("154.0.1.2"), &missing));
        assert!(!edge_from(None, sz("0.0.0.0"), &missing));
        assert!(!edge_from(None, sz(""), &missing));
        assert!(!edge_from(None, Some(RegValue::Dword(154)), &missing));
        assert!(!edge_from(None, None, &exists));
    }

    #[test]
    fn gpu_from_ors_supported_and_enabled() {
        let caps = |bits: &[Option<u32>]| -> Vec<AdapterCaps> {
            bits.iter()
                .enumerate()
                .map(|(i, &b)| AdapterCaps {
                    luid: i as i64,
                    wddm_2_7: b,
                })
                .collect()
        };
        assert_eq!(
            gpu_from(&caps(&[Some(0xb), Some(0)])),
            GpuScheduling {
                supported: true,
                enabled: true
            }
        );
        assert_eq!(
            gpu_from(&caps(&[Some(0x1)])),
            GpuScheduling {
                supported: true,
                enabled: false
            }
        );
        assert_eq!(gpu_from(&caps(&[None])), GpuScheduling::default());
        assert_eq!(gpu_from(&[]), GpuScheduling::default());
    }

    #[test]
    fn gpu_note_cases() {
        let on = GpuScheduling {
            supported: true,
            enabled: true,
        };
        let off = GpuScheduling {
            supported: true,
            enabled: false,
        };
        assert_eq!(
            gpu_note(Some(2), off),
            Some("Turned on in the settings; Windows starts using it after the next restart.")
        );
        assert_eq!(
            gpu_note(Some(1), on),
            Some("Turned off in the settings; the GPU keeps using it until the next restart.")
        );
        assert_eq!(
            gpu_note(None, on),
            Some(
                "The GPU already uses it: Windows turned it on for this GPU without storing the \
                 setting. Applying stores the setting so it stays on."
            )
        );
        assert_eq!(gpu_note(Some(2), on), None);
        assert_eq!(gpu_note(Some(1), off), None);
        assert_eq!(gpu_note(None, off), None);
        assert_eq!(gpu_note(Some(7), on), None);
        assert_eq!(gpu_note(Some(7), off), None);
    }

    static OFFICE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EDGE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static GPU_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn probes_run_at_most_once() {
        let probes = Probes {
            office: || {
                OFFICE_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            },
            edge: || {
                EDGE_CALLS.fetch_add(1, Ordering::SeqCst);
                Err(Error::Other("probe failed".into()))
            },
            gpu: || {
                GPU_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(GpuScheduling {
                    supported: true,
                    enabled: false,
                })
            },
        };
        let reqs = Requirements::new(probes);
        for _ in 0..2 {
            assert_eq!(
                reqs.check(Requirement::Office),
                Availability::Missing(Requirement::Office.missing_text())
            );
            assert_eq!(
                reqs.check(Requirement::Edge),
                Availability::Unknown("probe failed".into())
            );
            assert_eq!(reqs.check(Requirement::GpuScheduling), Availability::Met);
        }
        assert_eq!(
            reqs.gpu(),
            Some(GpuScheduling {
                supported: true,
                enabled: false
            })
        );
        assert_eq!(OFFICE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(EDGE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(GPU_CALLS.load(Ordering::SeqCst), 1);

        let failing = Requirements::new(Probes {
            office: || Ok(true),
            edge: || Ok(true),
            gpu: || Err(Error::Other("no adapters".into())),
        });
        assert_eq!(
            failing.check(Requirement::GpuScheduling),
            Availability::Unknown("no adapters".into())
        );
        assert_eq!(failing.gpu(), None);
        assert_eq!(failing.check(Requirement::Office), Availability::Met);
    }

    #[test]
    fn a_positive_reading_wins_over_a_failed_one() {
        let failed = || Err(Error::Other("access denied".into()));
        assert!(decide(vec![failed(), Ok(sz("x"))], |v| v[1].is_some()).unwrap());
        let err = decide(vec![failed(), Ok(None)], |v| v.iter().any(Option::is_some))
            .unwrap_err()
            .to_string();
        assert!(err.contains("access denied"), "{err}");
        assert!(!decide(vec![Ok(None), Ok(None)], |v| v.iter().any(Option::is_some)).unwrap());
    }

    #[test]
    fn system_probes_are_read_only() {
        println!("office: {:?}", office_installed());
        println!("edge: {:?}", edge_installed());
        println!("gpu scheduling: {:?}", gpu_scheduling());
        let reqs = Requirements::new(SYSTEM_PROBES);
        for req in [
            Requirement::Office,
            Requirement::Edge,
            Requirement::GpuScheduling,
        ] {
            println!("{req:?}: {:?}", reqs.check(req));
        }
    }
}
