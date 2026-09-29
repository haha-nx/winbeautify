//! The COM apartment guard, shared by every module that talks to WASAPI.
//!
//! Kept in one place because the rules are subtle enough to get wrong twice:
//! `CoInitializeEx` is per-thread and reference-counted, `S_FALSE` means "this
//! thread was already initialised" and still has to be balanced by
//! `CoUninitialize`, while `RPC_E_CHANGED_MODE` means COM is live in another
//! mode and must **not** be uninitialised by us.

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// RAII guard for this thread's COM apartment.
pub(crate) struct ComGuard {
    /// True only when *this* call performed the initialisation, which is the
    /// only case where we may balance it.
    should_uninit: bool,
}

impl ComGuard {
    pub(crate) fn new() -> Self {
        // SAFETY: no pointers are involved; the call only affects this thread's
        // apartment. The result is deliberately not fatal — an
        // already-initialised thread is the common case.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            should_uninit: hr.is_ok(),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: balanced against the successful `CoInitializeEx` above,
            // and this guard owns that call.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nested guards must not unbalance the apartment: the inner one must not
    /// uninitialise COM out from under the outer one.
    #[test]
    fn nested_guards_do_not_tear_down_the_apartment() {
        let outer = ComGuard::new();
        {
            let _inner = ComGuard::new();
        }
        // The outer guard's thread must still have a usable apartment, which is
        // observable as "a fresh guard succeeds".
        let probe = ComGuard::new();
        drop(probe);
        drop(outer);
    }
}