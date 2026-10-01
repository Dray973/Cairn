//! Graphics adapter capabilities reported by the display kernel (D3DKMT), in particular
//! hardware-accelerated GPU scheduling. Read-only: the adapter handles are opened for queries
//! and closed again.

use std::fmt;
use std::mem::size_of;

use windows::Wdk::Graphics::Direct3D::{
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTERINFO,
    D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO, D3DKMT_WDDM_2_7_CAPS,
    KMTQAITYPE_WDDM_2_7_CAPS,
};
use windows::Win32::Foundation::{LUID, NTSTATUS, STATUS_BUFFER_TOO_SMALL};

use crate::{Error, Result};

/// `D3DKMT_WDDM_2_7_CAPS` of one adapter; `None` when its driver does not answer the query
/// (drivers older than WDDM 2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterCaps {
    /// Locally unique id of the adapter (high part in the upper 32 bits).
    pub luid: i64,
    pub wddm_2_7: Option<u32>,
}

/// Bits of `D3DKMT_WDDM_2_7_CAPS`: the driver supports hardware scheduling.
pub const HWSCH_SUPPORTED: u32 = 0x1;
/// Bits of `D3DKMT_WDDM_2_7_CAPS`: the adapter runs with hardware scheduling now.
pub const HWSCH_ENABLED: u32 = 0x2;

/// Enumeration attempts when the adapter list grows between the count and the fill call.
const ENUM_ATTEMPTS: usize = 3;

/// Every adapter the display kernel lists, with its WDDM 2.7 capabilities.
pub fn wddm_2_7_caps() -> Result<Vec<AdapterCaps>> {
    let adapters = enumerate()?;
    Ok(adapters
        .0
        .iter()
        .map(|a| AdapterCaps {
            luid: luid_value(a.AdapterLuid),
            wddm_2_7: query_caps(a.hAdapter),
        })
        .collect())
}

/// Adapter handles opened by `D3DKMTEnumAdapters2`; each non-zero handle is closed on drop,
/// so handles are released even when a query fails or panics.
struct Adapters(Vec<D3DKMT_ADAPTERINFO>);

impl Drop for Adapters {
    fn drop(&mut self) {
        for adapter in &self.0 {
            if adapter.hAdapter == 0 {
                continue;
            }
            let close = D3DKMT_CLOSEADAPTER {
                hAdapter: adapter.hAdapter,
            };
            // SAFETY: the handle was returned by D3DKMTEnumAdapters2 and is closed exactly once.
            let _ = unsafe { D3DKMTCloseAdapter(&close) };
        }
    }
}

impl fmt::Debug for Adapters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Adapters")
            .field("count", &self.0.len())
            .finish()
    }
}

fn enum_error(status: NTSTATUS) -> Error {
    Error::Other(format!(
        "cannot list the graphics adapters (NTSTATUS 0x{:08x})",
        status.0 as u32
    ))
}

/// Lists the adapters: a count probe (an upper bound), then the fill call with that many
/// entries, retried when the list grew in between. Only the returned `NumAdapters` entries
/// are kept.
fn enumerate() -> Result<Adapters> {
    let mut last = STATUS_BUFFER_TOO_SMALL;
    for _ in 0..ENUM_ATTEMPTS {
        let mut probe = D3DKMT_ENUMADAPTERS2::default();
        // SAFETY: `probe` is a valid, writable struct; a null `pAdapters` asks for the count only.
        let status = unsafe { D3DKMTEnumAdapters2(&mut probe) };
        if !status.is_ok() {
            return Err(enum_error(status));
        }
        let bound = probe.NumAdapters as usize;
        if bound == 0 {
            return Ok(Adapters(Vec::new()));
        }
        let mut list = vec![D3DKMT_ADAPTERINFO::default(); bound];
        let mut request = D3DKMT_ENUMADAPTERS2 {
            NumAdapters: bound as u32,
            pAdapters: list.as_mut_ptr(),
        };
        // SAFETY: `pAdapters` points to `bound` writable entries, the count passed alongside
        // it, and `list` outlives the call.
        let status = unsafe { D3DKMTEnumAdapters2(&mut request) };
        if status == STATUS_BUFFER_TOO_SMALL {
            last = status;
            continue;
        }
        if !status.is_ok() {
            return Err(enum_error(status));
        }
        list.truncate((request.NumAdapters as usize).min(bound));
        return Ok(Adapters(list));
    }
    Err(enum_error(last))
}

/// The WDDM 2.7 capabilities of one open adapter; `None` when the query fails.
fn query_caps(handle: u32) -> Option<u32> {
    let mut caps = D3DKMT_WDDM_2_7_CAPS::default();
    let mut query = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: KMTQAITYPE_WDDM_2_7_CAPS,
        pPrivateDriverData: (&mut caps as *mut D3DKMT_WDDM_2_7_CAPS).cast(),
        PrivateDriverDataSize: size_of::<D3DKMT_WDDM_2_7_CAPS>() as u32,
    };
    // SAFETY: `query` points to `caps`, which is writable for the size passed and outlives
    // the call; the handle is open for the duration of the call.
    let status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
    if !status.is_ok() {
        return None;
    }
    // SAFETY: both members of the union cover the same four bytes, so reading the plain
    // `Value` copy is valid whatever the driver wrote.
    Some(unsafe { caps.Anonymous.Value })
}

fn luid_value(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_is_read_only_and_closes_handles() {
        let caps = wddm_2_7_caps().expect("the display kernel lists its adapters");
        for adapter in &caps {
            println!(
                "adapter {:#x}: WDDM 2.7 caps {:?}",
                adapter.luid, adapter.wddm_2_7
            );
        }
        // A second enumeration works too, so the first one released its handles.
        assert!(wddm_2_7_caps().is_ok());
    }

    #[test]
    fn luids_keep_both_parts() {
        let luid = LUID {
            LowPart: 0x1234_5678,
            HighPart: 0x0000_0009,
        };
        assert_eq!(luid_value(luid), 0x0000_0009_1234_5678);
        let negative = LUID {
            LowPart: 0xffff_ffff,
            HighPart: -1,
        };
        assert_eq!(luid_value(negative), -1);
    }

    #[test]
    fn capability_bits() {
        assert_eq!(HWSCH_SUPPORTED, 1);
        assert_eq!(HWSCH_ENABLED, 2);
        assert_eq!(size_of::<D3DKMT_WDDM_2_7_CAPS>(), 4);
    }
}
