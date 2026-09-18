//! The `IVisualTreeServiceCallback2` the XAML diagnostics framework notifies
//! whenever the visual tree mutates. This is where taskbar islands and their
//! background rectangles are discovered.

use crate::com::{self, ComObj, ParentChildRelation, VisualElement, VISUAL_MUTATION_ADD};
use crate::service;
use crate::xaml;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows::core::{IUnknown, Interface};

/// Runtime class names the shell uses for the taskbar islands. Matched
/// verbatim against what the framework reports; `BackgroundFill` and
/// `BackgroundStroke` are the rectangles whose fill the shell repaints.
const XAML_SOURCE_TYPE: &str = "Windows.UI.Xaml.Hosting.DesktopWindowXamlSource";
const TASKBAR_FRAME_TYPE: &str = "Taskbar.TaskbarFrame";
const RECTANGLE_TYPE: &str = "Windows.UI.Xaml.Shapes.Rectangle";

/// Tallest a rectangle may be and still be taken for the shell's hairline, in
/// device-independent pixels. The line is one physical pixel; the slack covers
/// a scaled display, where one pixel is a fraction of a DIP.
const HAIRLINE_MAX_HEIGHT_DIP: f64 = 3.0;
/// How much of the frame's width the hairline must span to be recognised.
/// Anything narrower is one of the island's many other shapes.
const HAIRLINE_MIN_WIDTH_FRACTION: f64 = 0.8;

const SUPPORTED: &[windows::core::GUID] = &[
    com::IID_IVISUAL_TREE_SERVICE_CALLBACK,
    // `IVisualTreeServiceCallback2` is deliberately *not* answered. Answering it
    // switches the framework into its "multiple window support" path, which on
    // this build calls a null function pointer inside Windows.UI.Xaml and the
    // shell dies with FAST_FAIL_GUARD_ICALL_CHECK_FAILURE. The only thing
    // Callback2 adds is `OnElementStateChanged`, which the reference TAP
    // implements as a no-op, so nothing is lost by staying on the v1 callback.
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

#[repr(C)]
pub struct Watcher {
    /// COM object header. `#[repr(C)]` is not optional here: without it Rust
    /// may reorder the fields, and the framework reaches every COM object by
    /// reading the vtable out of the object's first word. With the fields
    /// reordered it read a heap pointer, called what it found there, and the
    /// shell died with `FAST_FAIL_GUARD_ICALL_CHECK_FAILURE` before ever
    /// reaching our code.
    #[allow(dead_code)] // read by the framework through the raw pointer
    vtable: com::VtblPtr,
    xaml_diagnostics: IUnknown,
    ready_event: ReadyEvent,
    /// `DesktopWindowXamlSource` handles seen before their taskbar content
    /// arrives: a new island is created empty and only later gains a
    /// `TaskbarFrame`, so the two are matched up from this set.
    pending_sources: Mutex<HashSet<u64>>,
    /// How often we went looking for taskbars that predate us (see
    /// `claim_taskbars`): the walk is expensive and only the first few tree
    /// events are worth trying it on.
    claim_attempts: AtomicU32,
    ref_count: AtomicU32,
}

impl ComObj for Watcher {
    const SUPPORTED: &'static [windows::core::GUID] = SUPPORTED;
    // Agile. The reference TAP is `non_agile`, but the framework's advise path
    // then has to marshal the callback to the XAML UI thread, and with no
    // registered proxy for `IVisualTreeServiceCallback2` that path leaves XAML
    // calling a null interface pointer — which the shell reports as a
    // control-flow-guard fail-fast inside explorer. Answering `IAgileObject`
    // keeps the callback usable from any apartment, so the framework dispatches
    // to the UI thread and calls us there without a proxy.
    const AGILE_CALLBACK: bool = true;
    fn ref_count(&self) -> &AtomicU32 {
        &self.ref_count
    }
}

/// The callback entry points, as exports.
///
/// explorer's XAML side is control-flow-guard enabled and calls these through
/// the callback's vtable. A target that is neither exported nor listed in this
/// module's guard table fails that check, and the kernel ends the process with
/// `FAST_FAIL_GUARD_ICALL_CHECK_FAILURE` — which is how a non-exported callback
/// shows up: the shell dies the moment the framework first touches us. Export
/// addresses are always valid call targets, so the slots go through these.
#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_query_interface(
    this: *mut core::ffi::c_void,
    iid: *const windows::core::GUID,
    out: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    unsafe { com::com_query_interface::<Watcher>(this, iid, out) }
}

#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_add_ref(this: *mut core::ffi::c_void) -> u32 {
    crate::logging::debug_log_sync("callback: AddRef");
    unsafe { com::com_add_ref::<Watcher>(this) }
}

#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_release(this: *mut core::ffi::c_void) -> u32 {
    crate::logging::debug_log_sync("callback: Release");
    unsafe { com::com_release::<Watcher>(this) }
}

/// Target for the slots beyond the declared interface (see
/// [`com::CallbackVtbl::reserved`]).
#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_reserved(
    _this: *mut core::ffi::c_void,
    _a: usize,
    _b: usize,
    _c: usize,
) -> windows::core::HRESULT {
    crate::logging::debug_log_sync("callback: reserved slot called");
    windows::Win32::Foundation::E_NOTIMPL
}

#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_on_visual_tree_change(
    this: *mut core::ffi::c_void,
    relation: *const ParentChildRelation,
    element: *const VisualElement,
    mutation: i32,
) -> windows::core::HRESULT {
    // Never unwind into the framework: an explorer-side panic would take the
    // whole shell down.
    com::guard(|| {
        crate::logging::debug_log_sync("visual tree callback entered");
        unsafe {
            Watcher::handle_tree_change(&*(this as *const Watcher), &*relation, &*element, mutation)
        };
        windows::core::HRESULT(0)
    })
}

#[no_mangle]
pub unsafe extern "system" fn wbtap_callback_on_element_state_changed(
    _this: *mut core::ffi::c_void,
    _handle: u64,
    _state: i32,
    _context: *const u16,
) -> windows::core::HRESULT {
    windows::core::HRESULT(0)
}

/// The single vtable instance for every `Watcher`.
pub static WATCHER_VTABLE: com::CallbackVtbl = com::CallbackVtbl {
    query_interface: wbtap_callback_query_interface,
    add_ref: wbtap_callback_add_ref,
    release: wbtap_callback_release,
    on_visual_tree_change: wbtap_callback_on_visual_tree_change,
    on_element_state_changed: wbtap_callback_on_element_state_changed,
    reserved: [wbtap_callback_reserved; 7],
};

impl Watcher {
    /// Build the watcher and start advising. The framework calls `SetSite`
    /// with an `IXamlDiagnostics`; from then on our callback receives every
    /// visual tree mutation for the process.
    ///
    /// The advise call happens on the calling thread, which is the XAML UI
    /// thread the framework called `SetSite` on. Advising from a thread of our
    /// own reaches a null function pointer inside XAML's implementation and the
    /// shell dies with a control-flow-guard fail-fast; the reference TAP does
    /// advise from its own thread, but on this build that is what breaks.
    pub fn create(site: IUnknown, ready_event: isize) -> *mut Watcher {
        let watcher = com::new_com_object(Self {
            vtable: com::VtblPtr(&WATCHER_VTABLE as *const com::CallbackVtbl as *const core::ffi::c_void),
            xaml_diagnostics: site,
            ready_event: ReadyEvent(ready_event),
            pending_sources: Mutex::new(HashSet::new()),
            claim_attempts: AtomicU32::new(0),
            ref_count: AtomicU32::new(1),
        });
        // The caller (`TapSite::set_site`) runs inside `com::guard`, so a panic
        // here is caught and logged instead of killing explorer.
        unsafe {
            let diagnostics = &(*watcher).xaml_diagnostics;
            crate::logging::debug_log_sync("advise: entering AdviseVisualTreeChange");
            let advised = advise_visual_tree_change(diagnostics, watcher as *mut _);
            crate::logging::debug_log_sync(if advised {
                "advise: AdviseVisualTreeChange returned true"
            } else {
                "advise: AdviseVisualTreeChange returned false"
            });
            if advised {
                // The command window goes up now rather than when the first
                // taskbar is claimed: the host looks for it right after
                // injecting and treats its absence as a failed injection. This
                // runs on the XAML UI thread, which is the thread the window's
                // messages are then delivered on.
                service::ensure_command_window();
                if ready_event != 0 {
                    let _ = windows::Win32::System::Threading::SetEvent(
                        windows::Win32::Foundation::HANDLE(ready_event as *mut core::ffi::c_void),
                    );
                }
            }
        }
        watcher
    }


    fn handle_tree_change(
        &self,
        relation: &ParentChildRelation,
        element: &VisualElement,
        mutation: i32,
    ) {
        // The framework hands the BSTRs to us; free them once read.
        let type_name = unsafe { com::borrow_bstr(element.type_name) }
            .map(utf16_lossy)
            .unwrap_or_default();
        let name = unsafe { com::borrow_bstr(element.name) }
            .map(utf16_lossy)
            .unwrap_or_default();
        // Runs on the XAML UI thread for every mutation in the process, which is
        // why the log call has to stay allocation-free.
        crate::service::debug_log_fmt(format_args!(
            "tree change: mutation={mutation} type={type_name} name={name}"
        ));
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
        // A taskbar that predates us never announces its frame — it was added
        // before we connected. Any element of its island is a way in, and the
        // taskbar's own elements keep mutating (buttons, clock, hover), so walk
        // up from each one until the frame turns up.
        if !service::has_bars() && mutation == VISUAL_MUTATION_ADD {
            let attempt = self.claim_attempts.fetch_add(1, Ordering::Relaxed);
            self.claim_from(element.handle, attempt < 3);
        }
    }

    /// Walk up from an element looking for the `TaskbarFrame` that contains it,
    /// and claim the taskbar it belongs to. `verbose` reports the ancestry, for
    /// the first few events only.
    fn claim_from(&self, handle: u64, verbose: bool) {
        let mut chain = String::new();
        let Some(mut element) = self.inspectable_at(handle) else {
            return;
        };
        for _ in 0..40 {
            let class = unsafe { xaml::runtime_class_name(element.as_raw()) }.unwrap_or_default();
            if verbose {
                if !chain.is_empty() {
                    chain.push_str(" <- ");
                }
                chain.push_str(if class.is_empty() { "?" } else { &class });
            }
            if class == TASKBAR_FRAME_TYPE {
                if verbose {
                    crate::service::debug_log_fmt(format_args!("claim: ancestry {chain}"));
                }
                let Some(frame_handle) = self.handle_of(&element) else {
                    return;
                };
                match self.taskbar_for_frame(&element) {
                    Some(taskbar) => {
                        service::register_taskbar(frame_handle, taskbar);
                        self.register_frame_children(frame_handle);
                        crate::service::debug_log("claimed an existing taskbar");
                    }
                    None => crate::service::debug_log(
                        "claim: found a frame, but no taskbar window matches its size",
                    ),
                }
                return;
            }
            match unsafe { xaml::parent_of(element.as_raw()) } {
                Some(parent) => element = parent,
                None => break,
            }
        }
        if verbose {
            crate::service::debug_log_fmt(format_args!("claim: no frame above {chain}"));
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
            // the sources we have seen to find this island's window, and take
            // the taskbar it hangs under from that window's parent.
            if let Some(source_hwnd) = self.find_source_for(relation.parent) {
                let taskbar = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetAncestor(
                        windows::Win32::Foundation::HWND(source_hwnd as *mut core::ffi::c_void),
                        windows::Win32::UI::WindowsAndMessaging::GA_PARENT,
                    )
                    .0 as isize
                };
                if taskbar != 0 {
                    service::register_taskbar(handle, taskbar);
                }
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

    fn taskbar_for_frame(&self, frame: &IUnknown) -> Option<isize> {
        let (width_dip, height_dip) = unsafe { xaml::actual_size_of(frame.as_raw()) }?;
        let mut best: Option<(isize, f64)> = None;
        for taskbar in service::taskbar_windows() {
            let dpi = unsafe {
                windows::Win32::UI::HiDpi::GetDpiForWindow(windows::Win32::Foundation::HWND(
                    taskbar as *mut core::ffi::c_void,
                ))
            };
            let dpi = if dpi == 0 { 96 } else { dpi };
            let width_px = width_dip * dpi as f64 / 96.0;
            let height_px = height_dip * dpi as f64 / 96.0;
            for child in service::child_windows(taskbar) {
                let Some(rect) = service::window_rect(child) else {
                    continue;
                };
                let width = (rect.right - rect.left) as f64;
                let height = (rect.bottom - rect.top) as f64;
                let delta = (width - width_px).abs() + (height - height_px).abs();
                if delta <= 8.0 && best.map(|(_, best)| delta < best).unwrap_or(true) {
                    best = Some((taskbar, delta));
                }
            }
        }
        best.map(|(taskbar, _)| taskbar)
    }

    /// Find the `BackgroundFill`/`BackgroundStroke` rectangles of a frame by
    /// walking its children. Names are unique enough inside one taskbar island
    /// — the tray's own background is called `BackgroundBorder`.
    ///
    /// The shell's hairline is the same shape of problem as the background, so
    /// it is found the same way — by geometry rather than by name, because the
    /// name property is not readable from here without faulting inside the
    /// shell's string code. It is the mirror image of the background rectangle:
    /// as wide as the frame and a device pixel or two tall.
    fn register_frame_children(&self, frame_handle: u64) {
        let Some(frame) = self.inspectable_at(frame_handle) else {
            return;
        };
        // Only used as a sanity bound on the hairline candidate; without it no
        // hairline is claimed at all rather than a small rectangle being taken
        // for one and cleared.
        let frame_width = unsafe { xaml::actual_size_of(frame.as_raw()) }
            .map(|(width, _)| width)
            .filter(|width| *width > 0.0);
        let mut queue = vec![frame];
        let mut visited = 0usize;
        // The taskbar paints its background with one big `Rectangle`; the other
        // shapes in the island (running indicators, tray backgrounds) are small.
        // Picking by size rather than by name: the name property is not readable
        // from here without faulting inside the shell's string code.
        let mut best: Option<(f64, u64)> = None;
        let mut hairline: Option<(f64, u64)> = None;
        while let Some(element) = queue.pop() {
            visited += 1;
            if visited > 4000 {
                crate::service::debug_log("stopped walking the frame: tree larger than expected");
                break;
            }
            for child in unsafe { xaml::children_of(element.as_raw(), 64) } {
                let class = unsafe { xaml::runtime_class_name(child.as_raw()) };
                if class.as_deref() == Some(RECTANGLE_TYPE) {
                    // Size decides, and a null fill does not disqualify: the shell
                    // has usually not painted the background yet when we get here
                    // (the reference TAP notes the same), and the rectangle that
                    // covers the whole taskbar is the one to paint.
                    let (width, height) =
                        unsafe { xaml::actual_size_of(child.as_raw()) }.unwrap_or((0.0, 0.0));
                    let area = width * height;
                    if best.map(|(best_area, _)| area > best_area).unwrap_or(true) {
                        if let Some(handle) = self.handle_of(&child) {
                            let fill = unsafe { xaml::fill_of(child.as_raw()) };
                            let color = fill.and_then(|fill| unsafe { xaml::solid_color_of(fill) });
                            if let Some(fill) = fill {
                                unsafe { com::release_raw(fill) };
                            }
                            crate::service::debug_log_fmt(format_args!(
                                "claim: rectangle {width}x{height} fill {color:?} (candidate)"
                            ));
                            best = Some((area, handle));
                        }
                    }
                    // The hairline: full width, a hair tall. Widest wins, and a
                    // frame of unknown size disqualifies every candidate.
                    if height > 0.0
                        && height <= HAIRLINE_MAX_HEIGHT_DIP
                        && frame_width
                            .map(|frame| width >= frame * HAIRLINE_MIN_WIDTH_FRACTION)
                            .unwrap_or(false)
                        && hairline.map(|(widest, _)| width > widest).unwrap_or(true)
                    {
                        if let Some(handle) = self.handle_of(&child) {
                            crate::service::debug_log_fmt(format_args!(
                                "claim: hairline rectangle {width}x{height} dp (candidate)"
                            ));
                            hairline = Some((width, handle));
                        }
                    }
                    continue;
                }
                queue.push(child);
            }
        }
        match best {
            Some((area, handle)) => {
                crate::service::debug_log_fmt(format_args!(
                    "claim: background rectangle is the largest one ({area} dp^2)"
                ));
                self.register_rectangle(frame_handle, handle, true);
            }
            None => crate::service::debug_log("claim: no filled rectangle inside the frame"),
        }
        match hairline {
            Some((width, handle)) => {
                crate::service::debug_log_fmt(format_args!(
                    "claim: hairline rectangle is {width} dp wide"
                ));
                self.register_rectangle(frame_handle, handle, false);
            }
            None => crate::service::debug_log("claim: no hairline rectangle inside the frame"),
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
        // Matched by ABI class name, not by `FrameworkElement.Name`: the name
        // property does not come back as a string this code can read, and asking
        // for it faults inside the shell.s string code.
        let class = unsafe { xaml::runtime_class_name(parent.as_raw()) };
        if class.as_deref() == Some(TASKBAR_FRAME_TYPE) {
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
    // The one thing we cannot verify by reading our own code is the layout of
    // this foreign vtable, and a slot that holds garbage or null only shows up
    // as a control-flow-guard fail-fast inside explorer. Say what the first
    // slots point at, so a wrong layout is a log line instead of a crash. This
    // line is written synchronously: the crash it is meant to explain would
    // otherwise take it down before the logger thread got to it.
    crate::logging::debug_log_sync(&format!(
        "advise: service3 vtable {}",
        com::describe_vtable(service3, 6)
    ));
    let advised = match com::vtbl_of::<com::IVisualTreeServiceVtbl>(service3) {
        Ok(vtbl) => (vtbl.advise_visual_tree_change)(service3, callback).is_ok(),
        Err(_) => false,
    };
    crate::service::debug_log_fmt(format_args!(
        "advise: AdviseVisualTreeChange returned {advised}"
    ));
    com::release_raw(service3);
    advised
}

fn utf16_lossy(chars: &[u16]) -> String {
    String::from_utf16_lossy(chars)
}

// A COM object is found by its vtable, so the vtable pointer has to be the first
// field. Rust reorders fields unless the struct is `#[repr(C)]`, and a framework
// that reads the wrong word calls whatever it finds there — which is how the
// shell died with a control-flow-guard fail-fast the moment it touched this
// object. The assertion is the regression guard for that.
const _: () = assert!(core::mem::offset_of!(Watcher, vtable) == 0);
