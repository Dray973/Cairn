//! COM apartment scope for the calling thread.

use std::marker::PhantomData;

use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED,
};

/// Keeps COM initialized on the calling thread while alive: the single-threaded apartment
/// through [`ComApartment::enter`], the multithreaded apartment through [`enter_mta`].
/// Uninitializes on drop only when `CoInitializeEx` succeeded here (`S_OK` or `S_FALSE`);
/// a thread already in the other apartment keeps using it untouched.
///
/// `CoUninitialize` acts on the thread that calls it, so the guard must be dropped on the
/// thread that entered the apartment: it is neither `Send` nor `Sync`.
#[derive(Debug)]
pub(crate) struct ComApartment {
    initialized: bool,
    _thread: PhantomData<*const ()>,
}

impl ComApartment {
    /// Enters the single-threaded apartment.
    pub(crate) fn enter() -> ComApartment {
        ComApartment::init(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
    }

    fn init(model: COINIT) -> ComApartment {
        // SAFETY: no pointers are passed; a successful call is balanced in Drop.
        let hr = unsafe { CoInitializeEx(None, model) };
        ComApartment {
            initialized: hr.is_ok(),
            _thread: PhantomData,
        }
    }
}

/// Enters the multithreaded apartment on the calling thread, for COM servers that are used
/// from worker threads.
pub(crate) fn enter_mta() -> ComApartment {
    ComApartment::init(COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE)
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx in `init` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apartments_are_entered_on_fresh_threads() {
        let mta = std::thread::spawn(|| enter_mta().initialized)
            .join()
            .unwrap();
        assert!(mta);
        let sta = std::thread::spawn(|| ComApartment::enter().initialized)
            .join()
            .unwrap();
        assert!(sta);
    }

    #[test]
    fn a_thread_in_one_apartment_does_not_join_the_other() {
        let (first, second) = std::thread::spawn(|| {
            let mta = enter_mta();
            let sta = ComApartment::enter();
            (mta.initialized, sta.initialized)
        })
        .join()
        .unwrap();
        assert!(first);
        assert!(!second, "RPC_E_CHANGED_MODE leaves the thread in the MTA");
    }

    /// Compiles only while the guard is neither `Send` nor `Sync`: either would make a second
    /// implementation apply, and the call below would become ambiguous.
    #[test]
    fn the_guard_stays_on_its_thread() {
        trait AmbiguousIfImpl<Marker> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
        struct IfSend;
        struct IfSync;
        impl<T: ?Sized + Send> AmbiguousIfImpl<IfSend> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfImpl<IfSync> for T {}
        <ComApartment as AmbiguousIfImpl<_>>::check();
    }
}
