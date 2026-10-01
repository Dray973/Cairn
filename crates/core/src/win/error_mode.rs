//! Thread error mode that keeps Windows from showing critical-error dialogs.

use windows::Win32::System::Diagnostics::Debug::{
    SetThreadErrorMode, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, THREAD_ERROR_MODE,
};

/// Suppresses critical-error dialogs ("insert a disk into drive E:") on the calling thread;
/// the previous mode is restored on drop.
#[derive(Debug)]
pub(crate) struct ErrorModeGuard {
    previous: THREAD_ERROR_MODE,
}

impl ErrorModeGuard {
    /// `None` when the thread's error mode cannot be changed.
    pub(crate) fn new() -> Option<ErrorModeGuard> {
        let mut previous = THREAD_ERROR_MODE(0);
        // SAFETY: `previous` is a valid out pointer; the mode only affects this thread.
        unsafe {
            SetThreadErrorMode(
                SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX,
                Some(&mut previous),
            )
        }
        .ok()?;
        Some(ErrorModeGuard { previous })
    }
}

impl Drop for ErrorModeGuard {
    fn drop(&mut self) {
        // SAFETY: restores the mode this guard replaced on the same thread.
        unsafe {
            let _ = SetThreadErrorMode(self.previous, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Diagnostics::Debug::GetThreadErrorMode;

    #[test]
    fn the_guard_sets_and_restores_the_thread_mode() {
        std::thread::spawn(|| {
            // SAFETY: reads the calling thread's error mode.
            let before = unsafe { GetThreadErrorMode() };
            {
                let _guard = ErrorModeGuard::new().expect("error mode");
                // SAFETY: as above.
                let during = unsafe { GetThreadErrorMode() };
                let wanted = SEM_FAILCRITICALERRORS.0 | SEM_NOOPENFILEERRORBOX.0;
                assert_eq!(during & wanted, wanted);
            }
            // SAFETY: as above.
            assert_eq!(unsafe { GetThreadErrorMode() }, before);
        })
        .join()
        .unwrap();
    }
}
