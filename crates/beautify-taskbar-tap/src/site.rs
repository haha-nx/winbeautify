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
        if !outer.is_null() {
            // No aggregation support; the framework does not aggregate TAPs.
            return windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
        }
        crate::service::debug_log("class factory create_instance");
        // The framework probes with `CreateInstance(IID_IMarshal)` and falls
        // back to the real interface; every call must be able to hand out a
        // fresh object. Single-connection semantics live in
        // `TapSite::set_site`. Agility is answered by the standard marshaler
        // wrapper (see `marshal.rs`).
        let tap = com::new_com_object(TapSite {
            vtable: com::VtblPtr(&SITE_VTABLE as *const com::SiteVtbl as *const core::ffi::c_void),
            site: AtomicIsize::new(0),
            ref_count: AtomicU32::new(1),
        });
        let ok = com::com_query_interface::<TapSite>(tap as *mut core::ffi::c_void, iid, out);
        let iid_text = (!iid.is_null()).then(|| unsafe { *iid }).map(|g| format!("{g:?}"));
        crate::service::debug_log(&format!(
            "create_instance: qi ok {}, iid {}",
            ok.is_ok(),
            iid_text.unwrap_or_default()
        ));
        if ok.is_err() {
            com::release_raw(tap as *mut core::ffi::c_void);
        }
        ok
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
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            let unknown = match com::adopt(site) {
                Ok(unknown) => unknown,
                Err(_err) => {
                    crate::service::debug_log("set_site: site adopt failed");
                    return windows::core::HRESULT(1);
                }
            };
            let speaks_xaml_diagnostics =
                com::qi_raw(&unknown, &com::IID_IXAML_DIAGNOSTICS)
                    .map(|raw| com::release_raw(raw))
                    .is_some();
            crate::service::debug_log(&format!(
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
            crate::service::debug_log(&format!("set_site accepted, ready event handle {event}"));
            Watcher::create(unknown, event);
            crate::service::debug_log("site set, watcher advising");
            windows::core::HRESULT(0)
        }));
        ran.unwrap_or(windows::core::HRESULT(1))
    }

    unsafe extern "system" fn get_site(
        this: *mut core::ffi::c_void,
        iid: *const windows::core::GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT {
        let tap = &*(this as *const TapSite);
        let raw = tap.site.load(Ordering::Acquire);
        if raw == 0 || iid.is_null() || out.is_null() {
            return windows::Win32::Foundation::E_FAIL;
        }
        // QI the stored site directly for the requested interface.
        let vtbl = *(raw as *mut *mut windows::core::IUnknown_Vtbl);
        ((*vtbl).QueryInterface)(raw as *mut core::ffi::c_void, iid as *mut _, out)
    }
}

/// Create (or open) the named manual-reset event the host waits on. The
/// handle ownership transfers to the watcher.
fn create_ready_event() -> isize {
    unsafe {
        let name: Vec<u16> = crate::protocol::READY_EVENT_NAME
            .encode_utf16()
            .chain([0])
            .collect();
        let event = windows::Win32::System::Threading::CreateEventW(
            None,
            true, // manual reset
            false,
            windows::core::PCWSTR(name.as_ptr()),
        );
        match event {
            Ok(handle) if !handle.is_invalid() => handle.0 as isize,
            _ => 0,
        }
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
    let ok = std::panic::catch_unwind(|| install_inner(module)).unwrap_or_else(|_| {
        crate::service::debug_log("install panicked");
        false
    });
    if !ok {
        // Signal the host so it does not wait out its whole timeout.
        signal_ready();
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
        if hr.is_ok() {
            crate::service::debug_log("XAML diagnostics connected");
            return true;
        }
        crate::service::debug_log(&format!("connection attempt {attempt} failed: {hr:?}"));
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
    unsafe {
        let name: Vec<u16> = crate::protocol::READY_EVENT_NAME
            .encode_utf16()
            .chain([0])
            .collect();
        let event = windows::Win32::System::Threading::CreateEventW(
            None,
            true,
            false,
            windows::core::PCWSTR(name.as_ptr()),
        );
        if let Ok(handle) = event {
            if !handle.is_invalid() {
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
}

/// Are we loaded into explorer.exe? The hook vehicle can drag the DLL into
/// other processes on the same desktop; only explorer hosts the taskbar.
fn is_explorer() -> bool {
    unsafe {
        let mut buffer = [0u16; 260];
        let len = windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut buffer);
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
    }
}

/// `DllGetClassObject` — how the XAML diagnostics framework reaches the class
/// factory.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const windows::core::GUID,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    let iid_text = (!iid.is_null())
        .then(|| unsafe { *iid })
        .map(|g| format!("{g:?}"));
    crate::service::debug_log(&format!(
        "DllGetClassObject clsid match: {}, iid: {}",
        !clsid.is_null() && *clsid == TAP_CLSID,
        iid_text.unwrap_or_default()
    ));
    if clsid.is_null() || *clsid != TAP_CLSID {
        return windows::Win32::Foundation::CLASS_E_CLASSNOTAVAILABLE;
    }
    com::com_query_interface::<ClassFactory>(
        factory() as *const ClassFactory as *mut ClassFactory as *mut core::ffi::c_void,
        iid,
        out,
    )
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
