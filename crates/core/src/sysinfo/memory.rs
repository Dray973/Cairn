//! Memory totals from `GlobalMemoryStatusEx` and `GetPhysicallyInstalledSystemMemory`, and
//! the slots and modules from the SMBIOS table (`GetSystemFirmwareTable('RSMB')`).

use std::mem::size_of;

use windows::Win32::System::SystemInformation::{
    GetPhysicallyInstalledSystemMemory, GetSystemFirmwareTable, GlobalMemoryStatusEx,
    MEMORYSTATUSEX, RSMB,
};

use super::smbios::{memory_layout, parse};
use super::MemoryInfo;
use crate::Result;

pub(super) fn read() -> Result<MemoryInfo> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX whose `dwLength` is set, as required.
    unsafe { GlobalMemoryStatusEx(&mut status) }?;

    let mut installed_kib = 0u64;
    // SAFETY: `installed_kib` is a valid out pointer for the call.
    let installed_bytes = unsafe { GetPhysicallyInstalledSystemMemory(&mut installed_kib) }
        .ok()
        .filter(|_| installed_kib > 0)
        .map(|_| installed_kib.saturating_mul(1024));

    // SAFETY: each call passes either no buffer or a writable slice whose length the
    // wrapper passes along with it.
    let raw = read_table(|buf| unsafe { GetSystemFirmwareTable(RSMB, 0, buf) });
    let layout = raw
        .map(|raw| memory_layout(&parse(&raw)))
        .unwrap_or_default();

    Ok(MemoryInfo {
        installed_bytes,
        usable_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys.min(status.ullTotalPhys),
        load_percent: status.dwMemoryLoad.min(100),
        slots: layout.slots,
        max_capacity_bytes: layout.max_capacity_bytes,
        modules: layout.modules,
    })
}

/// Runs a `GetSystemFirmwareTable`-style call: without a buffer it returns the size
/// needed; with one it returns the bytes written, or the size needed when the buffer is
/// too small (the table can change between the calls, so the fill call is retried once).
/// A size of 0 means the table is not available: no module data, not an error.
fn read_table(mut call: impl FnMut(Option<&mut [u8]>) -> u32) -> Option<Vec<u8>> {
    let mut size = call(None) as usize;
    for _ in 0..2 {
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let written = call(Some(&mut buf)) as usize;
        if written == 0 {
            return None;
        }
        if written <= buf.len() {
            buf.truncate(written);
            return Some(buf);
        }
        size = written;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake firmware table call serving `sizes` answers to the size call in turn.
    fn table_call(data: Vec<u8>, mut sizes: Vec<usize>) -> impl FnMut(Option<&mut [u8]>) -> u32 {
        move |buf| match buf {
            None => sizes.remove(0) as u32,
            Some(buf) => {
                let needed = if sizes.is_empty() {
                    data.len()
                } else {
                    sizes.remove(0)
                };
                if buf.len() < needed {
                    return needed as u32;
                }
                buf[..data.len()].copy_from_slice(&data);
                data.len() as u32
            }
        }
    }

    #[test]
    fn firmware_table_is_read_with_a_size_call_then_a_fill_call() {
        let data = vec![1u8, 2, 3, 4, 5];
        assert_eq!(
            read_table(table_call(data.clone(), vec![5])),
            Some(data.clone())
        );
    }

    #[test]
    fn a_table_that_grows_between_the_calls_is_read_again_once() {
        let data = vec![7u8; 12];
        assert_eq!(
            read_table(table_call(data.clone(), vec![8, 12])),
            Some(data)
        );
        // Growing twice gives up.
        assert_eq!(read_table(table_call(vec![7u8; 20], vec![8, 12, 20])), None);
    }

    #[test]
    fn a_missing_table_means_no_module_data() {
        assert_eq!(read_table(|_| 0), None);
        assert_eq!(read_table(table_call(Vec::new(), vec![0])), None);
    }

    #[test]
    fn memory_totals_are_read() {
        let info = read().unwrap();
        assert!(info.usable_bytes > 0);
        assert!(info.available_bytes <= info.usable_bytes);
        assert!(info.load_percent <= 100);
        if let Some(installed) = info.installed_bytes {
            assert!(installed >= info.usable_bytes / 2, "{installed}");
        }
    }
}
