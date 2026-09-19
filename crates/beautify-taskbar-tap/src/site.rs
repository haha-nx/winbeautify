//! The TAP entry point: the class factory the XAML diagnostics framework
//! instantiates, the `IObjectWithSite` it drives, the thread that connects us
//! to the framework, and the exported hook proc the host uses as a loader
//! vehicle.

use crate::com::{self, ComObj};
use crate::watcher::{Watcher, WATCHER_VTABLE};
use windows::core::{IUnknown, Interface};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

/// CLSID of this TAP, passed to `InitializeXamlDiagnosticsEx` and answered by
/// `DllGetClassObject`. Private to WinBeautify.
pub const TAP_CLSID: windows::core::GUID =
    windows::core::GUID::from_u128(0x7b1f3a2e_5c4d_4e8f_9a2b_1d0c3e5f7a9b);

const SITE_SUPPORTED: &[windows::core::GUID] = &[com::IID_IOBJECT_WITH_SITE];

/// Set once the install thread has been spawned in this process. The hook
/// proc fires for every sent message while installed, so this also keeps the
/// boot single-shot.
static INSTALL_STARTED: AtomicBool = AtomicBool::new(false);

/// Only one live connection per process: the first  with a real
/// site wins, later ones are refused.
static SITE_TAKEN: AtomicBool = AtomicBool::new(false);

#[repr(C)]
pub struct TapSite {
    /// COM object header: see the comment on `Watcher`.
    #[allow(dead_code)] // read by the framework through the raw pointer
    vtable: com::VtblPtr,
    /// The `IXamlDiagnostics` site, as an owned `IUnknown` reference.
    site: AtomicIsize,
    ref_count: AtomicU32,
}

impl ComObj for TapSite {
    const SUPPORTED: &'static [windows::core::GUID] = SITE_SUPPORTED;
    fn ref_count(&self) -> &AtomicU32 {
        &self.ref_count
    }
}

impl Drop for TapSite {
    fn drop(&mut self) {
        let raw = self.site.swap(0, Ordering::Acquire);
        unsafe { com::release_raw(raw as *mut core::ffi::c_void) };
    }
}

pub static SITE_VTABLE: com::SiteVtbl = com::SiteVtbl {
    query_interface: com::com_query_interface::<TapSite>,
    add_ref: com::com_add_ref::<TapSite>,
    release: com::com_release::<TapSite>,
    set_site: TapSite::set_site,
    get_site: TapSite::get_site,
};

pub static FACTORY_VTABLE: com::FactoryVtbl = com::FactoryVtbl {
    query_interface: com::com_query_interface::<ClassFactory>,
    add_ref: com::com_add_ref::<ClassFactory>,
    release: com::com_release::<ClassFactory>,
    create_instance: ClassFactory::create_instance,
    lock_server: ClassFactory::lock_server,
};

#[repr(C)]
pub struct ClassFactory {
    /// COM object header: see the comment on `Watcher`.
    #[allow(dead_code)] // read by the framework through the raw pointer
    vtable: com::VtblPtr,
    ref_count: AtomicU32,
}

impl ComObj for ClassFactory {
    const SUPPORTED: &'static [windows::core::GUID] = &[com::IID_ICLASS_FACTORY];
    fn ref_count(&self) -> &AtomicU32 {
        &self.ref_count
    }
}

/// The process-lifetime factory instance. Its initial reference is never
/// released, so the generic `Release` never frees it.
static FACTORY: std::sync::OnceLock<ClassFactory> = std::sync::OnceLock::new();

fn factory() -> &'static ClassFactory {
    FACTORY.get_or_init(|| ClassFactory {
        vtable: com::VtblPtr(&FACTORY_VTABLE as *const com::FactoryVtbl as *const core::ffi::c_void),
        ref_count: AtomicU32::new(1),
    })
}

impl ClassFactory {
    unsafe extern "system" fn create_instance(
        _this: *mut core::ffi::c_void,
        outer: *mut core::ffi::c_void,
        iid: *const windows::core::GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT {
        com::guard(|| {
            if !outer.is_null() {
                // No aggregation support; the framework does not aggregate TAPs.
                return windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
            }
            crate::service::debug_log("class factory create_instance");
            // The framework probes with `CreateInstance(IID_IMarshal)` and falls
            // back to the real interface; every call must be able to hand out a
            // fresh object. Single-connection semantics live in
            // `TapSite::set_site`, and agility is answered per object through
            // `ComObj::AGILE_CALLBACK`.
            let tap = com::new_com_object(TapSite {
                vtable: com::VtblPtr(&SITE_VTABLE as *const com::SiteVtbl as *const core::ffi::c_void),
                site: AtomicIsize::new(0),
                ref_count: AtomicU32::new(1),
            });
            let ok = com::com_query_interface::<TapSite>(tap as *mut core::ffi::c_void, iid, out);
            crate::service::debug_log_fmt(format_args!("create_instance: qi ok {}", ok.is_ok()));
            if !iid.is_null() {
                // No `format!` here: this runs on a thread the framework owns, and
                // a heap allocation on that thread is what wedged explorer before.
                let iid = unsafe { *iid };
                crate::service::debug_log_fmt(format_args!("create_instance: iid {iid:?}"));
            }
            if ok.is_err() {
                com::release_raw(tap as *mut core::ffi::c_void);
            }
            ok
        })
    }

    unsafe extern "system" fn lock_server(
        _this: *mut core::ffi::c_void,
        _lock: i32,
    ) -> windows::core::HRESULT {
        windows::core::HRESULT(0)
    }
}

impl TapSite {
    unsafe extern "system" fn set_site(
        this: *mut core::ffi::c_void,
        site: *mut core::ffi::c_void,
    ) -> windows::core::HRESULT {
        com::guard(|| {
            let tap = &*(this as *const TapSite);
            crate::service::debug_log("set_site entered");
            if site.is_null() {
                return windows::core::HRESULT(0);
            }
            if SITE_TAKEN.swap(true, Ordering::SeqCst) {
                crate::service::debug_log("second connection refused");
                return windows::Win32::Foundation::E_FAIL;
            }
            // The site must speak `IXamlDiagnostics` — that is the object the
            // watcher walks the tree with. Rejecting anything else mirrors
            // the reference TAP.
            //
            // `SetSite`'s parameter is borrowed, and `adopt` takes ownership of
            // the reference it is handed, so the watcher's reference has to be
            // taken here; without this the watcher would release a reference the
            // framework still owns.
            com::add_ref_raw(site);
            let unknown = match com::adopt(site) {
                Ok(unknown) => unknown,
                Err(_err) => {
                    com::release_raw(site);
                    crate::service::debug_log("set_site: site adopt failed");
                    return windows::core::HRESULT(1);
                }
            };
            let speaks_xaml_diagnostics =
                com::qi_raw(&unknown, &com::IID_IXAML_DIAGNOSTICS)
                    .map(|raw| com::release_raw(raw))
                    .is_some();
            crate::service::debug_log_fmt(format_args!(
                "set_site: site speaks IXamlDiagnostics: {speaks_xaml_diagnostics}"
            ));
            if !speaks_xaml_diagnostics {
                return windows::core::HRESULT(0);
            }

            // Keep an owned reference for `get_site`.
            let mut stored: *mut core::ffi::c_void = core::ptr::null_mut();
            let vtbl = unknown.vtable();
            if (vtbl.QueryInterface)(
                unknown.as_raw(),
                &<IUnknown as Interface>::IID,
                &mut stored,
            )
            .is_ok()
            {
                tap.site.store(stored as isize, Ordering::Release);
            }

            let event = create_ready_event();
            crate::service::debug_log_fmt(format_args!(
                "set_site accepted, ready event handle {event}"
            ));
            // Advise on this thread: it is the XAML UI thread, and the
            // framework's `AdviseVisualTreeChange` needs the XAML context the
            // UI thread has. Called from a thread of our own it reaches a null
            // function pointer inside XAML and the shell dies with a
            // control-flow-guard fail-fast. (The reference TAP advises from its
            // own thread, but its DLL is built by MSVC with different defaults,
            // and on this build that is what breaks.)
            Watcher::create(unknown, event);
            crate::service::debug_log("site set, watcher advising");
            windows::core::HRESULT(0)
        })
    }

    unsafe extern "system" fn get_site(
        this: *mut core::ffi::c_void,
        iid: *const windows::core::GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT {
        com::guard(|| {
            let tap = &*(this as *const TapSite);
            let raw = tap.site.load(Ordering::Acquire);
            if raw == 0 || iid.is_null() || out.is_null() {
                return windows::Win32::Foundation::E_FAIL;
            }
            // QI the stored site directly for the requested interface.
            let vtbl = *(raw as *mut *mut windows::core::IUnknown_Vtbl);
            ((*vtbl).QueryInterface)(raw as *mut core::ffi::c_void, iid as *mut _, out)
        })
    }
}

/// Create (or open) the named manual-reset event the host waits on. The handle
/// ownership transfers to the watcher.
///
/// Allocation-free: `SetSite` calls this, and that runs on a thread the
/// framework owns.
fn create_ready_event() -> isize {
    match open_ready_event() {
        Ok(handle) => handle.0 as isize,
        Err(_) => 0,
    }
}

/// Open the host's ready event, creating it if the host has not yet.
fn open_ready_event() -> windows::core::Result<windows::Win32::Foundation::HANDLE> {
    let mut name = [0u16; 64];
    let mut len = 0;
    for unit in crate::protocol::READY_EVENT_NAME.encode_utf16() {
        if len + 1 >= name.len() {
            break;
        }
        name[len] = unit;
        len += 1;
    }
    name[len] = 0;
    unsafe {
        windows::Win32::System::Threading::CreateEventW(
            None,
            true, // manual reset
            false,
            windows::core::PCWSTR(name.as_ptr()),
        )
    }
}

// ---------------------------------------------------------------------------
// Install thread — runs inside explorer after the hook forced the DLL load.
// ---------------------------------------------------------------------------

/// Install thread body: connect to the XAML diagnostics framework.
///
/// XAML Diagnostics can only be initialized once per thread, so each attempt
/// runs on a fresh thread, and the connection name is rotated in case another
/// tool is already attached. On success the framework pins this module in the
/// process and drives it through `DllGetClassObject`/`SetSite`.
fn install_thread(module: isize) {
    // Before anything else: this thread is ours, so it is the one place where
    // resolving the DLL directory (which takes the loader lock) and starting the
    // logger is safe. Every later entry point is reached from threads the
    // framework owns, where that work can deadlock explorer.
    crate::logging::start();
    pin_module();
    let ok = std::panic::catch_unwind(|| install_inner(module)).unwrap_or_else(|_| {
        crate::service::debug_log("install panicked");
        false
    });
    if !ok {
        // Signal the host so it does not wait out its whole timeout.
        signal_ready();
        // Let a later injection try again. The framework's diagnostics endpoints
        // do not exist yet while Explorer is still starting: an injection that
        // lands in the first second after the taskbar appears fails every one of
        // its 60 attempts with `ERROR_NOT_FOUND` (measured), and the host retries
        // every 30 s — but the boot latch turned that into a permanent failure
        // for the whole Explorer session, so the taskbar stayed unbeautified
        // until the shell was restarted a second time.
        INSTALL_STARTED.store(false, Ordering::SeqCst);
    }
}

/// Take a reference on this module that is never released.
///
/// Windows hands this DLL to explorer for the hook, and when the connection
/// does not complete either the framework or the hook going away calls
/// `FreeLibrary` on it — while the install and logger threads are still running
/// inside it. Windows reports the result as
/// `beautify_taskbar_tap.dll_unloaded`: an access violation, or a control-flow
/// guard fail-fast, in code that is no longer mapped. This is the same
/// situation TranslucentTB ends up in (its DLL is pinned by the framework once
/// it connects); pinning here covers the case where it never connects. The
/// reference is deliberately leaked: it lasts until explorer exits, which is
/// exactly as long as the TAP can be needed, and a TAP cannot be unloaded
/// safely anyway.
fn pin_module() {
    let mut handle = windows::Win32::Foundation::HMODULE::default();
    // No `UNCHANGED_REFCOUNT` here: adding the reference is the whole point.
    unsafe {
        let _ = windows::Win32::System::LibraryLoader::GetModuleHandleExW(
            windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            windows::core::PCWSTR(pin_module as *const u16),
            &mut handle,
        );
    }
}

fn install_inner(module: isize) -> bool {
    // Module path of *this* DLL, for the framework to load us by.
    let Some(path) = module_path(module) else {
        crate::service::debug_log("cannot resolve own module path");
        return false;
    };
    crate::service::debug_log("installing TAP");

    let Ok(entry) = crate::xaml::get_xaml_diagnostics_entry() else {
        crate::service::debug_log("InitializeXamlDiagnosticsEx not found");
        return false;
    };

    let pid = std::process::id();
    let path = std::sync::Arc::new(path.encode_utf16().chain([0]).collect::<Vec<u16>>());
    let mut attempt: u32 = 1;
    loop {
        // The XAML diagnostics framework registers its endpoints under this
        // exact name family inside the target process; an arbitrary name
        // fails with ERROR_NOT_FOUND. The suffix rotates when another tool
        // (or another TranslucentTB) already holds connection N.
        let connection = format!("VisualDiagConnection{attempt}");
        let connection: Vec<u16> = connection.encode_utf16().chain([0]).collect();
        let path = std::sync::Arc::clone(&path);
        crate::service::debug_log_fmt(format_args!(
            "attempt {attempt}: calling InitializeXamlDiagnosticsEx"
        ));
        let spawned = std::thread::Builder::new()
            .name("wb-tap-ixde".into())
            .spawn(move || unsafe {
                (entry)(
                    windows::core::PCWSTR(connection.as_ptr()),
                    pid,
                    windows::core::PCWSTR::null(),
                    windows::core::PCWSTR(path.as_ptr()),
                    TAP_CLSID,
                    windows::core::PCWSTR::null(),
                )
            });
        let hr = match spawned {
            Ok(handle) => handle.join().unwrap_or(windows::core::HRESULT(1)),
            Err(_) => windows::core::HRESULT(1),
        };
        crate::service::debug_log_fmt(format_args!("attempt {attempt}: ixde returned {hr:?}"));
        if hr.is_ok() {
            crate::service::debug_log("XAML diagnostics connected");
            return true;
        }
        crate::service::debug_log_fmt(format_args!(
            "connection attempt {attempt} failed: {hr:?}"
        ));
        attempt += 1;
        if attempt > 60 {
            // 60 × 500 ms, matching the reference implementation.
            crate::service::debug_log("giving up on XAML diagnostics connection");
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn module_path(module: isize) -> Option<String> {
    unsafe {
        let mut buffer = [0u16; 512];
        let len = windows::Win32::System::LibraryLoader::GetModuleFileNameW(
            Some(windows::Win32::Foundation::HMODULE(module as *mut core::ffi::c_void)),
            &mut buffer,
        );
        (len > 0 && (len as usize) < buffer.len())
            .then(|| String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

/// Signal the host: connection established, or definitely failed.
fn signal_ready() {
    if let Ok(handle) = open_ready_event() {
        if !handle.is_invalid() {
            unsafe {
                let _ = windows::Win32::System::Threading::SetEvent(handle);
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Exported entry points
// ---------------------------------------------------------------------------

/// The hook proc the host points `SetWindowsHookEx` at.
///
/// Its real job is to live inside this DLL so Windows maps the module into
/// the hooked thread's process; the first call also boots the install thread.
/// Hooks fire in the host process too, hence the explorer check.
#[no_mangle]
pub unsafe extern "system" fn tap_hook_proc(
    n_code: i32,
    w_param: windows::Win32::Foundation::WPARAM,
    l_param: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // Windows calls this from a win32k callback: it must not unwind, and it
    // must not swallow the hook result either.
    com::guard_value(windows::Win32::Foundation::LRESULT(0), || {
        if n_code >= 0 && !INSTALL_STARTED.swap(true, Ordering::SeqCst) && is_explorer() {
            let mut module = windows::Win32::Foundation::HMODULE::default();
            let ok = windows::Win32::System::LibraryLoader::GetModuleHandleExW(
                windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                windows::core::PCWSTR(tap_hook_proc as *const u16),
                &mut module,
            );
            if ok.is_ok() {
                let module_raw = module.0 as isize;
                let spawned = std::thread::Builder::new()
                    .name("wb-tap-install".into())
                    .spawn(move || install_thread(module_raw));
                if spawned.is_err() {
                    INSTALL_STARTED.store(false, Ordering::SeqCst);
                }
            } else {
                INSTALL_STARTED.store(false, Ordering::SeqCst);
            }
        }
        windows::Win32::UI::WindowsAndMessaging::CallNextHookEx(None, n_code, w_param, l_param)
    })
}

/// Are we loaded into explorer.exe? The hook vehicle can drag the DLL into
/// other processes on the same desktop; only explorer hosts the taskbar.
///
/// Allocates nothing: this runs in the hook proc, which the shell calls from a
/// win32k callback.
fn is_explorer() -> bool {
    const NAME: &[u8] = b"explorer.exe";
    let mut buffer = [0u16; 260];
    let len = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut buffer) as usize
    };
    if len < NAME.len() {
        return false;
    }
    let start = len - NAME.len();
    // The name has to be the whole file name, so a separator must precede it:
    // `my-explorer.exe` is a different program.
    if start > 0 {
        let before = buffer[start - 1];
        if before != b'\\' as u16 && before != b'/' as u16 {
            return false;
        }
    }
    buffer[start..len]
        .iter()
        .zip(NAME)
        .all(|(unit, expected)| *unit < 0x80 && (*unit as u8).to_ascii_lowercase() == *expected)
}

/// `DllGetClassObject` — how the XAML diagnostics framework reaches the class
/// factory.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const windows::core::GUID,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    com::guard(|| {
        crate::service::debug_log_fmt(format_args!(
            "DllGetClassObject: clsid matches {}",
            !clsid.is_null() && *clsid == TAP_CLSID
        ));
        if !iid.is_null() {
            // Still no `format!`: the framework calls this while it holds the loader
            // lock, and taking the heap from here is what wedged explorer.
            let iid = unsafe { *iid };
            crate::service::debug_log_fmt(format_args!("DllGetClassObject: iid {iid:?}"));
        }
        if clsid.is_null() || *clsid != TAP_CLSID {
            return windows::Win32::Foundation::CLASS_E_CLASSNOTAVAILABLE;
        }
        com::com_query_interface::<ClassFactory>(
            factory() as *const ClassFactory as *mut ClassFactory as *mut core::ffi::c_void,
            iid,
            out,
        )
    })
}

/// `DllCanUnloadNow` — the framework pins us while the connection is live;
/// `S_FALSE` keeps teardown trivial.
#[no_mangle]
pub unsafe extern "system" fn DllCanUnloadNow() -> windows::core::HRESULT {
    windows::Win32::Foundation::S_FALSE
}

// The vtables are referenced here so a stray rename fails at compile time.
const _: () = {
    let _ = &WATCHER_VTABLE as *const com::CallbackVtbl;
};

// See the note in `watcher.rs`: the vtable pointer has to be the object's first
// field, and only `#[repr(C)]` guarantees that.
const _: () = assert!(core::mem::offset_of!(TapSite, vtable) == 0);
const _: () = assert!(core::mem::offset_of!(ClassFactory, vtable) == 0);
