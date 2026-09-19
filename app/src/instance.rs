//! One copy of WinBeautify per session.
//!
//! The app is a daemon: it installs a tray icon, hooks the clipboard, drives
//! the taskbar's composition and keeps a widget bar floating over it. Every one
//! of those is process-global, so a second copy does not run alongside the first
//! — it fights it. Two tray icons, two copies of every hook, two bars drawing
//! over each other, and "退出" only stops one of them.
//!
//! So a launch takes a named mutex before anything else. If the name is already
//! held, the copy that is running is asked to show itself and this process exits
//! without touching a single shared resource.
//!
//! # Why the objects are named the way they are
//!
//! `Local\` — the per-session namespace — rather than `Global\`: WinBeautify is
//! installed per user, and a second user logged in at the same time is entitled
//! to their own copy. Two copies inside one session are what is wrong.
//!
//! # Why an event as well as a mutex
//!
//! The mutex says "someone is already running"; it cannot say anything to them.
//! A second launch is almost always a person trying to get at the settings
//! window, so the running copy waits on a named event and opens it. The event is
//! auto-reset, so each signal wakes exactly one wait.

use std::sync::OnceLock;
use std::time::Duration;

use beautify_core::APP_NAME;
use tauri::AppHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    INFINITE,
};

/// What the first copy holds for the life of the process.
///
/// The handles are kept as plain integers because `HANDLE` is neither `Send`
/// nor `Sync`, and this lives in a `static`. Nothing here dereferences them —
/// they are only ever passed back to the kernel — which is the same reason the
/// pin registry stores its window handles this way.
struct Instance {
    /// Held, never waited on: the kernel releases it when this process exits,
    /// which is exactly the lifetime the guard needs.
    _mutex: isize,
    /// Signalled by later launches. `None` when the event could not be created,
    /// in which case the guard still works but a second launch is silent.
    event: Option<isize>,
}

/// A raw handle as the integer the static keeps.
fn raw(handle: HANDLE) -> isize {
    handle.0 as isize
}

/// The integer back as a handle.
fn handle(raw: isize) -> HANDLE {
    HANDLE(raw as *mut core::ffi::c_void)
}

static INSTANCE: OnceLock<Instance> = OnceLock::new();

/// Is this the only copy? `false` when another one holds the mutex.
///
/// Must be called before anything else in `main`: the whole point is that a
/// second copy never gets as far as starting a module.
pub fn claim() -> bool {
    let (mutex_name, event_name) = names();
    match take(&mutex_name, &event_name) {
        Some(instance) => {
            let _ = INSTANCE.set(instance);
            true
        }
        None => false,
    }
}

/// Take the mutex and create the event a later launch signals.
///
/// `None` when another copy already holds the name — the whole answer, in one
/// place, so it can be exercised under names of its own.
fn take(mutex_name: &[u16], event_name: &[u16]) -> Option<Instance> {
    let mutex = match unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())) } {
        Ok(mutex) => mutex,
        Err(e) => {
            // A guard that cannot be created must not stop the app from
            // starting; it only means two copies would be allowed.
            tracing::warn!("could not take the single-instance mutex: {e}");
            return Some(Instance {
                _mutex: 0,
                event: None,
            });
        }
    };

    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // The running copy owns the mutex and the event. This process only came
        // to hand over the message, so it lets go of the handle it was given.
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return None;
    }

    let event = unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) }.ok();
    if event.is_none() {
        tracing::warn!(
            "could not create the single-instance event; a second launch will be silent"
        );
    }
    Some(Instance {
        _mutex: raw(mutex),
        event: event.map(raw),
    })
}

/// Ask the copy that is already running to come forward, then let the caller
/// exit. Only meaningful after [`claim`] has said the mutex was taken.
pub fn activate_running() {
    let (_, event_name) = names();
    // The running copy creates the event immediately after the mutex, so a
    // launch that arrives in that gap can find it missing. Waiting briefly is
    // cheaper than not being noticed at all.
    for _ in 0..40 {
        let opened = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr())) };
        if let Ok(event) = opened {
            unsafe {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    tracing::warn!("another WinBeautify is running but did not answer; leaving it alone");
}

/// Show the settings window whenever another launch asks this copy to.
///
/// Called once the app handle exists; the thread lives as long as the process.
pub fn watch(app: &AppHandle) {
    let Some(event) = INSTANCE.get().and_then(|instance| instance.event) else {
        return;
    };
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("wb-single-instance".into())
        .spawn(move || loop {
            // Auto-reset: one wake per signal, and the wait is re-armed each
            // time round rather than being re-created.
            if unsafe { WaitForSingleObject(handle(event), INFINITE) } != WAIT_OBJECT_0 {
                break;
            }
            tracing::info!("another launch asked for the settings window");
            crate::settings::open(&app);
        });
    if let Err(e) = spawned {
        tracing::warn!("could not watch for other launches: {e}");
    }
}

/// The mutex and event names, in UTF-16 for the Win32 calls.
fn names() -> (Vec<u16>, Vec<u16>) {
    let wide = |suffix: &str| {
        format!("Local\\{APP_NAME}.{suffix}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    (wide("SingleInstance"), wide("Activate"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both names have to be in the session namespace and carry the app name,
    /// or two different users — or two different programs — could collide.
    #[test]
    fn the_names_are_namespaced_and_wide() {
        let (mutex, event) = names();
        assert_eq!(mutex.last(), Some(&0), "a Win32 string is NUL terminated");
        let text = String::from_utf16(&mutex[..mutex.len() - 1]).unwrap();
        assert!(text.starts_with("Local\\"), "got {text}");
        assert!(text.contains(APP_NAME), "got {text}");
        assert_ne!(mutex, event, "the two objects must not share a name");
    }

    /// The event has to be auto-reset, so a signal wakes one waiter and is not
    /// left standing for the next one to find without a launch behind it.
    ///
    /// A name of its own rather than the real one: kernel objects are shared by
    /// name, and the test below signals that one — two tests on the same event
    /// would see each other's wakes.
    #[test]
    fn the_event_is_created_auto_reset() {
        let event_name: Vec<u16> = "Local\\WinBeautify.TestAutoReset\0"
            .encode_utf16()
            .collect();
        let event = unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) };
        let event = event.expect("the event could not be created");
        // Already signalled: the next wait returns at once, and the one after it
        // has to wait for a fresh signal.
        unsafe {
            let _ = SetEvent(event);
        }
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        unsafe {
            let _ = CloseHandle(event);
        }
    }

    /// The whole guard rests on `CreateMutexW` reporting an already-taken name
    /// through `GetLastError` *after succeeding*, rather than by failing: the
    /// second caller gets a handle to the same mutex. A wrapper that cleared the
    /// last error on success would quietly turn the guard into a no-op, so this
    /// is asserted rather than assumed — along with the first copy keeping the
    /// event, which is what a later launch signals.
    #[test]
    fn the_second_taker_of_a_name_is_the_one_that_loses() {
        let mutex_name: Vec<u16> = "Local\\WinBeautify.TestClaimMutex\0".encode_utf16().collect();
        let event_name: Vec<u16> = "Local\\WinBeautify.TestClaimEvent\0".encode_utf16().collect();

        let first = take(&mutex_name, &event_name).expect("the first copy should have won");
        assert!(first.event.is_some(), "the first copy owns the event");
        assert!(
            take(&mutex_name, &event_name).is_none(),
            "the second copy must lose"
        );
    }

    /// A second launch reaches the first through the event, and that is the only
    /// thing it does to it — the whole hand-over.
    #[test]
    fn a_second_launch_wakes_the_copy_that_is_running() {
        let (_, event_name) = names();
        // The running copy owns the event.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) }
            .expect("the event could not be created");

        // The launch arriving second can only reach it by name, which is what
        // `activate_running` does before giving up.
        let second = std::thread::spawn(activate_running);
        let woken = unsafe { WaitForSingleObject(event, 2_000) };
        second.join().expect("the second launch panicked");

        assert_eq!(woken, WAIT_OBJECT_0, "the running copy was never woken");
        unsafe {
            let _ = CloseHandle(event);
        }
    }
}
