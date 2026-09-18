//! The `IVisualTreeServiceCallback2` the XAML diagnostics framework notifies
//! whenever the visual tree mutates. This is where taskbar islands and their
//! background rectangles are discovered.

use crate::com::{self, ComObj, ParentChildRelation, VisualElement, VISUAL_MUTATION_ADD};
use crate::service;
use crate::xaml;
use std::collections::HashSet;
use std::sync::atomic::AtomicU32;
use std::sync::Mutex;
use windows::core::{IUnknown, Interface};

/// Runtime class names the shell uses for the taskbar islands. Matched
/// verbatim against what the framework reports; `BackgroundFill` and
/// `BackgroundStroke` are the rectangles whose fill the shell repaints.
const XAML_SOURCE_TYPE: &str = "Windows.UI.Xaml.Hosting.DesktopWindowXamlSource";
const TASKBAR_FRAME_TYPE: &str = "Taskbar.TaskbarFrame";
const RECTANGLE_TYPE: &str = "Windows.UI.Xaml.Shapes.Rectangle";
const FRAME_ELEMENT_NAME: &str = "TaskbarFrame";

const SUPPORTED: &[windows::core::GUID] = &[
    com::IID_IVISUAL_TREE_SERVICE_CALLBACK,
    com::IID_IVISUAL_TREE_SERVICE_CALLBACK2,
];

/// Keep the raw Win32 event handle alive without a wrapper type.
struct ReadyEvent(isize);

impl Drop for ReadyEvent {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                    self.0 as *mut core::ffi::c_void,
                ));
            }
        }
    }
}

pub struct Watcher {
    /// COM object header: every hand-rolled object starts with its vtable so
    /// the framework's `QueryInterface`/`Release` land on our shims.
    #[allow(dead_code)] // read by the framework through the raw pointer
    vtable: com::VtblPtr,
    xaml_diagnostics: IUnknown,
    ready_event: ReadyEvent,
    /// `DesktopWindowXamlSource` handles seen before their taskbar content
    /// arrives: a new island is created empty and only later gains a
    /// `TaskbarFrame`, so the two are matched up from this set.
    pending_sources: Mutex<HashSet<u64>>,
    ref_count: AtomicU32,
}

impl ComObj for Watcher {
    const SUPPORTED: &'static [windows::core::GUID] = SUPPORTED;
    const AGILE_CALLBACK: bool = true;
    fn ref_count(&self) -> &AtomicU32 {
        &self.ref_count
    }
}

/// The single vtable instance for every `Watcher`.
pub static WATCHER_VTABLE: com::CallbackVtbl = com::CallbackVtbl {
    query_interface: com::com_query_interface::<Watcher>,
    add_ref: com::com_add_ref::<Watcher>,
    release: com::com_release::<Watcher>,
    on_visual_tree_change: Watcher::on_visual_tree_change,
    on_element_state_changed: Watcher::on_element_state_changed,
};

impl Watcher {
    /// Build the watcher and start advising. The framework calls `SetSite`
    /// with an `IXamlDiagnostics`; from then on our callback receives every
    /// visual tree mutation for the process.
    pub fn create(site: IUnknown, ready_event: isize) -> *mut Watcher {
        let watcher = com::new_com_object(Self {
            vtable: com::VtblPtr(&WATCHER_VTABLE as *const com::CallbackVtbl as *const core::ffi::c_void),
            xaml_diagnostics: site,
            ready_event: ReadyEvent(ready_event),
            pending_sources: Mutex::new(HashSet::new()),
            ref_count: AtomicU32::new(1),
        });
        // Advise from a dedicated thread: the framework moves the callback
        // registration onto the UI thread, and advising synchronously from
        // inside `SetSite` can deadlock during framework init.
        //
        // Only `isize`s cross the thread boundary; the raw `IUnknown`
        // reference is handed over with `std::mem::forget` and re-adopted on
        // the other side, keeping the refcount balanced.
        let this = watcher as isize;
        let diagnostics_raw = unsafe {
            let cloned = (*watcher).xaml_diagnostics.clone();
            let raw = cloned.as_raw();
            std::mem::forget(cloned);
            raw as isize
        };
        let event = unsafe { (*watcher).ready_event.0 };
        let _ = std::thread::Builder::new()
            .name("wb-tap-advise".into())
            .spawn(move || {
                // Never unwind into the framework's thread pool.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    use windows::core::Type;
                    crate::service::debug_log("advise thread started");
                    crate::service::debug_log("advise: adopting diagnostics");
                    let diagnostics: IUnknown = match IUnknown::from_abi(diagnostics_raw as *mut _) {
                        Ok(diagnostics) => diagnostics,
                        Err(_) => {
                            crate::service::debug_log("advise: diagnostics adopt failed");
                            return;
                        }
                    };
                    crate::service::debug_log("advise: diagnostics adopted");
                    let advised = advise_visual_tree_change(&diagnostics, this as *mut _);
                    crate::service::debug_log(&format!("advise_visual_tree_change -> {advised}"));
                    if advised && event != 0 {
                        let _ = windows::Win32::System::Threading::SetEvent(
                            windows::Win32::Foundation::HANDLE(event as *mut core::ffi::c_void),
                        );
                    }
                    // The re-adopted reference is released here; the watcher
                    // still holds its own.
                    drop(diagnostics);
                }));
                if let Err(payload) = result {
                    let text = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "opaque panic".into());
                    crate::service::debug_log(&format!("advise panicked: {text}"));
                }
            });
        watcher
    }

    unsafe extern "system" fn on_visual_tree_change(
        this: *mut core::ffi::c_void,
        relation: *const ParentChildRelation,
        element: *const VisualElement,
        mutation: i32,
    ) -> windows::core::HRESULT {
        // Never unwind into the framework: an explorer-side panic would take
        // the whole shell down.
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::handle_tree_change(&*(this as *const Watcher), &*relation, &*element, mutation)
        }));
        if ran.is_err() {
            service::debug_log("visual tree callback panicked");
        }
        windows::core::HRESULT(0)
    }

    unsafe extern "system" fn on_element_state_changed(
        _this: *mut core::ffi::c_void,
        _handle: u64,
        _state: i32,
        _context: *const u16,
    ) -> windows::core::HRESULT {
        windows::core::HRESULT(0)
    }

    fn handle_tree_change(
        &self,
        relation: &ParentChildRelation,
        element: &VisualElement,
        mutation: i32,
    ) {
        crate::service::debug_log(&format!(
            "tree change: mutation={mutation} handle={:x}",
            element.handle
        ));
        // The framework hands the BSTRs to us; free them once read.
        let type_name = unsafe { com::borrow_bstr(element.type_name) }
            .map(utf16_lossy)
            .unwrap_or_default();
        let name = unsafe { com::borrow_bstr(element.name) }
            .map(utf16_lossy)
            .unwrap_or_default();
        unsafe {
            com::free_bstr(element.type_name);
            com::free_bstr(element.name);
            com::free_bstr(element.src_info.file_name);
            com::free_bstr(element.src_info.hash);
        }

        if mutation == VISUAL_MUTATION_ADD {
            self.on_added(relation, element.handle, &type_name, &name);
        } else {
            // Only the element handle is valid on removal.
            service::unregister_taskbar(element.handle);
            if let Ok(mut set) = self.pending_sources.lock() {
                set.remove(&element.handle);
            }
        }
    }

    fn on_added(&self, relation: &ParentChildRelation, handle: u64, type_name: &str, name: &str) {
        if type_name == XAML_SOURCE_TYPE {
            // The island cannot be checked for taskbar content yet: it starts
            // empty and the TaskbarFrame arrives in a later mutation.
            if let Ok(mut set) = self.pending_sources.lock() {
                set.insert(handle);
            }
        } else if type_name == TASKBAR_FRAME_TYPE {
            // The frame's parent is the island's RootGrid; match it against
            // the sources we have seen to find this island's window.
            if let Some(source_hwnd) = self.find_source_for(relation.parent) {
                service::register_taskbar(handle, source_hwnd);
            }
        } else if type_name == RECTANGLE_TYPE {
            let background = name == "BackgroundFill";
            let border = name == "BackgroundStroke";
            if background || border {
                if let Some(frame) = self.find_frame_ancestor(relation.parent) {
                    self.register_rectangle(frame, handle, background);
                }
            }
        }
    }

    /// Find the pending source whose root content is the element at
    /// `root_grid`, and return its native window handle.
    fn find_source_for(&self, root_grid: u64) -> Option<isize> {
        let pending: Vec<u64> = self
            .pending_sources
            .lock()
            .ok()
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        for source_handle in pending {
            let Some(source_unknown) = self.inspectable_at(source_handle) else {
                continue;
            };
            let matched = match unsafe { xaml::source_content(source_unknown.as_raw()) } {
                Some(content) => {
                    let same = self
                        .inspectable_at(root_grid)
                        .is_some_and(|root| unsafe { com::same_raw_identity(&root, content) });
                    unsafe { com::release_raw(content) };
                    same
                }
                None => false,
            };
            if !matched {
                continue;
            }
            let hwnd = unsafe { xaml::source_window_handle(source_unknown.as_raw()) };
            if let Ok(mut set) = self.pending_sources.lock() {
                set.remove(&source_handle);
            }
            return hwnd;
        }
        None
    }

    /// Walk up from the element at `element_handle` to the `TaskbarFrame`.
    fn find_frame_ancestor(&self, element_handle: u64) -> Option<u64> {
        let element = self.inspectable_at(element_handle)?;
        let frame = self.find_frame(&element)?;
        self.handle_of(&frame)
    }

    fn find_frame(&self, element: &IUnknown) -> Option<IUnknown> {
        let parent = unsafe { xaml::parent_of(element.as_raw()) }?;
        let name = unsafe { xaml::name_of(parent.as_raw()) };
        if name.as_deref() == Some(FRAME_ELEMENT_NAME) {
            Some(parent)
        } else {
            self.find_frame(&parent)
        }
    }

    fn register_rectangle(&self, frame: u64, rectangle_handle: u64, background: bool) {
        let Some(inspectable) = self.inspectable_at(rectangle_handle) else {
            return;
        };
        // Names repeat across unrelated trees; only shapes are interesting.
        if unsafe { !xaml::supports(inspectable.as_raw(), &com::IID_ISHAPE) } {
            return;
        }
        if background {
            service::register_taskbar_background(frame, inspectable);
        } else {
            service::register_taskbar_border(frame, inspectable);
        }
    }

    /// `IXamlDiagnostics.GetIInspectableFromHandle`.
    fn inspectable_at(&self, handle: u64) -> Option<IUnknown> {
        let vtbl: &com::IXamlDiagnosticsVtbl =
            unsafe { com::vtbl_of(self.xaml_diagnostics.as_raw()).ok()? };
        let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
        let ok = unsafe {
            (vtbl.get_iinspectable_from_handle)(self.xaml_diagnostics.as_raw(), handle, &mut out)
        };
        if ok.is_ok() && !out.is_null() {
            unsafe { com::adopt(out).ok() }
        } else {
            None
        }
    }

    /// `IXamlDiagnostics.GetHandleFromIInspectable`.
    fn handle_of(&self, element: &IUnknown) -> Option<u64> {
        let vtbl: &com::IXamlDiagnosticsVtbl =
            unsafe { com::vtbl_of(self.xaml_diagnostics.as_raw()).ok()? };
        let mut out: u64 = 0;
        let ok = unsafe {
            (vtbl.get_handle_from_iinspectable)(self.xaml_diagnostics.as_raw(), element.as_raw(), &mut out)
        };
        ok.is_ok().then_some(out)
    }
}

/// `IVisualTreeService3.AdviseVisualTreeChange(callback)`.
unsafe fn advise_visual_tree_change(
    xaml_diagnostics: &IUnknown,
    callback: *mut core::ffi::c_void,
) -> bool {
    crate::service::debug_log("advise: querying IVisualTreeService3");
    let Some(service3) = com::qi_raw(xaml_diagnostics, &com::IID_IVISUAL_TREE_SERVICE3) else {
        crate::service::debug_log("advise: IVisualTreeService3 qi failed");
        return false;
    };
    crate::service::debug_log("advise: calling AdviseVisualTreeChange");
    let advised = match com::vtbl_of::<com::IVisualTreeServiceVtbl>(service3) {
        Ok(vtbl) => (vtbl.advise_visual_tree_change)(service3, callback).is_ok(),
        Err(_) => false,
    };
    crate::service::debug_log(&format!("advise: AdviseVisualTreeChange returned {advised}"));
    com::release_raw(service3);
    advised
}

fn utf16_lossy(chars: &[u16]) -> String {
    String::from_utf16_lossy(chars)
}
