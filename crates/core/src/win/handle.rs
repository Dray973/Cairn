//! An owned kernel handle, closed on drop.

use std::fmt;

use windows::Win32::Foundation::{CloseHandle, HANDLE};

/// Owns a kernel object handle and closes it when dropped. Null and
/// `INVALID_HANDLE_VALUE` are held without being closed.
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// Takes ownership of `h`; nothing else may close it.
    pub fn new(h: HANDLE) -> OwnedHandle {
        OwnedHandle(h)
    }

    /// The handle, still owned by `self`.
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl fmt::Debug for OwnedHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OwnedHandle").field(&self.0 .0).finish()
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is owned by this value and closed exactly once, here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: a kernel handle is a process-wide value that any thread may use or close; the
// wrapper holds no thread-affine state.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; `raw` only copies the value, and closing requires ownership.
unsafe impl Sync for OwnedHandle {}
