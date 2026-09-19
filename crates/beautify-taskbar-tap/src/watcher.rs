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
const BORDER_TYPE: &str = "Windows.UI.Xaml.Controls.Border";
/// The frame-sized `Border` of the taskbar template. Its `BorderBrush` paints
/// the hairline along the taskbar's top edge: pixel probes showed the thin
/// rectangle's fill is fully transparent while the line stays visible, so the
/// line must come from this element's border.
const HAIRLINE_BORDER_NAME: &str = "BackgroundElement";

/// Tallest a rectangle may be and still be taken for the shell's hairline, in
/// device-independent pixels. The line is one physical pixel; the slack covers
/// a scaled display, where one pixel is a fraction of a DIP.
const HAIRLINE_MAX_HEIGHT_DIP: f64 = 3.0;
/// Tallest a *border* may be and still count as a hairline strip. Borders that
/// are frame-sized are also accepted (their top edge is the line), but a strip
/// beats a full-size one when both appear.
const HAIRLINE_BORDER_STRIP_HEIGHT_DIP: f64 = 6.0;
/// How much of the frame's width the hairline must span to be recognised.
/// Anything narrower is one of the island's many other shapes.
const HAIRLINE_MIN_WIDTH_FRACTION: f64 = 0.8;
/// Upper bound on the diagnostic border log, so a pathological tree cannot
/// flood it.
const BORDER_LOG_LIMIT: usize = 40;

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
    /// `claim_is_due`): the walk is expensive, so it runs at most once per
    /// interval and only while a taskbar of this session is unclaimed.
    claim_attempts: AtomicU32,
    /// When that walk last ran, for the interval above.
    last_claim: Mutex<Option<std::time::Instant>>,
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

/// The diagnostics interface, for walks scheduled after the claim (the claim
/// itself can only use what the tree callback hands over). A leaked clone of
/// the site pointer: the connection lives as long as explorer, and so does
/// this reference.
static DIAGNOSTICS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Elements at least this wide (in DIPs) count as frame-wide in the delayed
/// walk: every real taskbar is wider than this, and no other island element
/// is.
const FRAME_WIDE_MIN_DIP: f64 = 1000.0;

/// Re-walk the claimed taskbar frames once the layout has settled.
///
/// Called on a timer a few seconds after the claim. The claim-time walk runs
/// while the shell is still building the island, where frame-wide elements
/// still read as 0x0 and are invisible to any geometry filter; by now every
/// element has its real size, so this walk logs everything that spans the
/// taskbar — the hairline painter hides among them — and registers frame-wide
/// `Border`s the first pass could not see.
pub fn rewalk() -> bool {
    let Some(&raw) = DIAGNOSTICS.get() else {
        return false;
    };
    let Ok(vtbl) = (unsafe { com::vtbl_of::<com::IXamlDiagnosticsVtbl>(raw as *mut core::ffi::c_void) })
    else {
        return false;
    };
    let inspectable_at = |handle: u64| -> Option<IUnknown> {
        let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
        if unsafe { (vtbl.get_iinspectable_from_handle)(raw as *mut core::ffi::c_void, handle, &mut out) }
            .is_ok()
            && !out.is_null()
        {
            unsafe { com::adopt(out).ok() }
        } else {
            None
        }
    };

    let mut visited = 0usize;
    let mut seen = 0usize;
    for frame_handle in service::frame_handles() {
        let Some(frame) = inspectable_at(frame_handle) else {
            continue;
        };
        let mut queue = vec![frame];
        while let Some(element) = queue.pop() {
            visited += 1;
            if visited > 6000 {
                crate::service::debug_log("rewalk: tree larger than expected");
                break;
            }
            let class =
                unsafe { xaml::runtime_class_name(element.as_raw()) }.unwrap_or_default();
            let (width, height) =
                unsafe { xaml::actual_size_of(element.as_raw()) }.unwrap_or((0.0, 0.0));
            if width >= FRAME_WIDE_MIN_DIP {
                seen += 1;
                let mut detail = String::new();
                if class == BORDER_TYPE {
                    let thickness = unsafe { xaml::border_thickness_of(element.as_raw()) };
                    let brush = unsafe { xaml::border_brush_of(element.as_raw()) };
                    let brush_color =
                        brush.and_then(|brush| unsafe { xaml::solid_color_of(brush) });
                    if let Some(brush) = brush {
                        unsafe { com::release_raw(brush) };
                    }
                    detail = format!(" top {:?} brush {brush_color:?}", thickness);
                } else if class == RECTANGLE_TYPE {
                    let stroke = unsafe { xaml::stroke_of(element.as_raw()) };
                    let stroke_color =
                        stroke.and_then(|stroke| unsafe { xaml::solid_color_of(stroke) });
                    if let Some(stroke) = stroke {
                        unsafe { com::release_raw(stroke) };
                    }
                    let fill = unsafe { xaml::fill_of(element.as_raw()) };
                    let fill_color = fill.and_then(|fill| unsafe { xaml::solid_color_of(fill) });
                    if let Some(fill) = fill {
                        unsafe { com::release_raw(fill) };
                    }
                    detail = format!(" fill {fill_color:?} stroke {stroke_color:?}");
                } else if unsafe { xaml::supports(element.as_raw(), &com::IID_ICONTROL) } {
                    let thickness =
                        unsafe { xaml::control_border_thickness_of(element.as_raw()) };
                    let brush = unsafe { xaml::control_border_brush_of(element.as_raw()) };
                    let brush_color =
                        brush.and_then(|brush| unsafe { xaml::solid_color_of(brush) });
                    if let Some(brush) = brush {
                        unsafe { com::release_raw(brush) };
                    }
                    detail = format!(" border-top {:?} brush {brush_color:?}", thickness);
                }
                crate::service::debug_log_fmt(format_args!(
                    "rewalk: {class} {width}x{height}{detail}"
                ));
                if class == BORDER_TYPE {
                    // The queue owns `element`; the carrier registration gets
                    // its own reference.
                    self_register_border(element.clone());
                }
                // A thin full-width rectangle is a hairline carrier the claim
                // walk could not see — the real painter here reads 0x0 with no
                // fill until the shell lays it out and paints it.
                if class == RECTANGLE_TYPE && height > 0.0 && height <= HAIRLINE_MAX_HEIGHT_DIP {
                    let stroke = unsafe { xaml::stroke_of(element.as_raw()) };
                    let stroke_color =
                        stroke.and_then(|stroke| unsafe { xaml::solid_color_of(stroke) });
                    if let Some(stroke) = stroke {
                        unsafe { com::release_raw(stroke) };
                    }
                    let fill = unsafe { xaml::fill_of(element.as_raw()) };
                    let fill_color = fill.and_then(|fill| unsafe { xaml::solid_color_of(fill) });
                    if let Some(fill) = fill {
                        unsafe { com::release_raw(fill) };
                    }
                    crate::service::debug_log_fmt(format_args!(
                        "rewalk: hairline rectangle {width}x{height} fill {fill_color:?} stroke {stroke_color:?} (carrier)"
                    ));
                    service::register_frameless_hairline_rect(element.clone());
                }
            }
            for child in unsafe { xaml::children_of(element.as_raw(), 64) } {
                queue.push(child);
            }
        }
    }
    crate::service::debug_log_fmt(format_args!(
        "rewalk: visited {visited}, {seen} frame-wide elements"
    ));
    visited > 0
}

/// Hand a frame-wide `Border` found by the delayed walk to the service. The
/// walk has no frame handle — the UI layer root is not one of the claimed
/// frames — so the border joins the hairline carriers of every registered
/// bar: writes to a foreign island's element are harmless, and the primary
/// island is the one this layer belongs to.
fn self_register_border(border: IUnknown) {
    if unsafe { !xaml::supports(border.as_raw(), &com::IID_IBORDER) } {
        return;
    }
    service::register_frameless_hairline_border(border);
}

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
        // Keep the diagnostics connection reachable for the delayed re-walk:
        // a leaked reference the rewalk reads from a timer on the UI thread.
        let _ = DIAGNOSTICS.set({
            let raw = site.as_raw();
            unsafe { com::add_ref_raw(raw) };
            raw as usize
        });
        let watcher = com::new_com_object(Self {
            vtable: com::VtblPtr(&WATCHER_VTABLE as *const com::CallbackVtbl as *const core::ffi::c_void),
            xaml_diagnostics: site,
            ready_event: ReadyEvent(ready_event),
            pending_sources: Mutex::new(HashSet::new()),
            claim_attempts: AtomicU32::new(0),
            last_claim: Mutex::new(None),
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
        //
        // The gate is "a taskbar of this session is still unclaimed", not "none
        // has been claimed": with more than one monitor, the first island
        // claimed used to turn the search off, and the other monitors were
        // never beautified. Rate-limited, because the walk is far too expensive
        // to run on every mutation.
        if mutation == VISUAL_MUTATION_ADD && self.claim_is_due() {
            let attempt = self.claim_attempts.fetch_add(1, Ordering::Relaxed);
            self.claim_from(element.handle, attempt < 3);
        }
    }

    /// Is it worth walking the tree looking for an unclaimed taskbar?
    ///
    /// The framework replays a handful of tree events when we connect, and those
    /// are the best chance of finding a taskbar that predates us — so the first
    /// few are all tried. After that the events are rare and mostly belong to a
    /// taskbar that was claimed long ago, so the walk is rate-limited, and it
    /// stops altogether once every taskbar of the session is registered.
    fn claim_is_due(&self) -> bool {
        /// Events at the start that are all worth a walk.
        const BURST: u32 = 30;
        /// How often the walk may run once the burst is over.
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        /// A taskbar that cannot be claimed must not keep the walk alive for
        /// ever; the islands that need this path exist at connect time, so if
        /// they have not turned up by now they are not going to.
        const GIVE_UP_AFTER: u32 = 120;

        let attempts = self.claim_attempts.load(Ordering::Relaxed);
        if attempts >= GIVE_UP_AFTER {
            return false;
        }
        if attempts >= BURST {
            let Ok(mut last) = self.last_claim.lock() else {
                return false;
            };
            let now = std::time::Instant::now();
            if last.is_some_and(|last| now.duration_since(last) < INTERVAL) {
                return false;
            }
            *last = Some(now);
        }
        // Nothing claimed yet counts too: that is the start-up case, where the
        // window enumeration would say the same thing but has not been asked.
        service::has_unclaimed_taskbar() || !service::has_bars()
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
                // Already claimed: this element belongs to a taskbar we know, so
                // there is nothing to do — and re-registering would re-run the
                // frame walk and arm the re-walk timer on every attempt.
                if service::is_registered(frame_handle) {
                    return;
                }
                match self.taskbar_for_frame(&element) {
                    Some(taskbar) => {
                        service::register_taskbar(frame_handle, taskbar);
                        self.register_frame_control(frame_handle);
                        self.register_frame_children(frame_handle);
                        service::schedule_rewalk();
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
                    self.register_frame_control(handle);
                    service::schedule_rewalk();
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
        } else if type_name == BORDER_TYPE {
            // The frame's background border: the element whose brush paints
            // the top hairline on the builds where the frame's own control
            // brush does not. The name recurs in taskbar-button templates, so
            // only a frame-sized one is accepted; the rest are logged and
            // ignored.
            if name == HAIRLINE_BORDER_NAME {
                if let Some(frame) = self.find_frame_ancestor(relation.parent) {
                    self.register_background_border(frame, handle);
                }
            }
        }
    }

    /// Which taskbar window a claimed `TaskbarFrame` belongs to.
    ///
    /// Taken from the island's own window wherever possible: walk up past the
    /// frame to the `DesktopWindowXamlSource` hosting it, ask that for its
    /// window, and take that window's parent. It is the same route the
    /// tree-change path uses when a new island appears, and the only one that
    /// can tell two identical monitors apart.
    fn taskbar_for_frame(&self, frame: &IUnknown) -> Option<isize> {
        if let Some(taskbar) = self.taskbar_of_island(frame) {
            return Some(taskbar);
        }
        // Fallback for a tree that does not expose the source above the frame:
        // match the frame's size against the taskbars — but only when a single
        // taskbar fits. Two monitors of the same size are indistinguishable
        // that way, and guessing mapped one monitor's island onto the other's
        // taskbar, which is what left the second monitor unpainted.
        self.taskbar_by_size(frame)
    }

    /// The taskbar window hosting this frame's island, via the island's window.
    fn taskbar_of_island(&self, frame: &IUnknown) -> Option<isize> {
        let mut element = unsafe { xaml::parent_of(frame.as_raw()) }?;
        for _ in 0..16 {
            let class = unsafe { xaml::runtime_class_name(element.as_raw()) }.unwrap_or_default();
            if class == XAML_SOURCE_TYPE {
                let window = unsafe { xaml::source_window_handle(element.as_raw()) }?;
                let taskbar = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetAncestor(
                        windows::Win32::Foundation::HWND(window as *mut core::ffi::c_void),
                        windows::Win32::UI::WindowsAndMessaging::GA_PARENT,
                    )
                    .0 as isize
                };
                return (taskbar != 0).then_some(taskbar);
            }
            match unsafe { xaml::parent_of(element.as_raw()) } {
                Some(parent) => element = parent,
                None => break,
            }
        }
        None
    }

    /// The taskbar whose island is the size of this frame, when that is a single
    /// candidate.
    ///
    /// Two monitors of the same size are indistinguishable by size alone, and
    /// the first match used to win — which mapped the second monitor's island
    /// onto the first monitor's taskbar, so one island was never resolved and
    /// only one taskbar was ever painted. When more than one taskbar matches,
    /// the frame is attributed by elimination: if exactly one of them still has
    /// no island, the frame is that one's. That is the case this recovery path
    /// exists for — the second monitor's island arriving after the first was
    /// claimed. Nothing is claimed when even that cannot tell them apart.
    fn taskbar_by_size(&self, frame: &IUnknown) -> Option<isize> {
        let (width_dip, height_dip) = unsafe { xaml::actual_size_of(frame.as_raw()) }?;
        let mut matched: Vec<isize> = Vec::new();
        for taskbar in service::taskbar_windows() {
            let dpi = unsafe {
                windows::Win32::UI::HiDpi::GetDpiForWindow(windows::Win32::Foundation::HWND(
                    taskbar as *mut core::ffi::c_void,
                ))
            };
            let dpi = if dpi == 0 { 96 } else { dpi };
            let width_px = width_dip * dpi as f64 / 96.0;
            let height_px = height_dip * dpi as f64 / 96.0;
            let fits = service::child_windows(taskbar).into_iter().any(|child| {
                let Some(rect) = service::window_rect(child) else {
                    return false;
                };
                let width = (rect.right - rect.left) as f64;
                let height = (rect.bottom - rect.top) as f64;
                (width - width_px).abs() + (height - height_px).abs() <= 8.0
            });
            if fits && !matched.contains(&taskbar) {
                matched.push(taskbar);
            }
        }

        if matched.len() == 1 {
            return matched.first().copied();
        }
        if matched.len() > 1 {
            let claimed = service::claimed_taskbars();
            let free: Vec<isize> = matched
                .iter()
                .copied()
                .filter(|taskbar| !claimed.contains(taskbar))
                .collect();
            if let [only] = free.as_slice() {
                return Some(*only);
            }
            crate::service::debug_log(
                "claim: two taskbars are the same size; leaving the frame unclaimed",
            );
        }
        None
    }

    /// Find the `BackgroundFill`/`BackgroundStroke` rectangles of a frame by
    /// walking its children. Names are unique enough inside one taskbar island
    /// — the tray's own background is called `BackgroundBorder`.
    ///
    /// The shell's hairline is the same shape of problem as the background, so
    /// it is found the same way — by geometry rather than by name, because the
    /// name property is not readable from here without faulting inside the
    /// shell's string code. It is the mirror image of the background rectangle:
    /// as wide as the frame and a device pixel or two tall. The walk also looks
    /// for the `Border` whose brush paints that line — the rectangle turns out
    /// to be transparent on this build — and registers the best candidate it
    /// saw, logging every bordered element it passed so a wrong guess is a log
    /// line rather than a mystery.
    fn register_frame_children(&self, frame_handle: u64) {
        let Some(frame) = self.inspectable_at(frame_handle) else {
            return;
        };
        // Only used as a sanity bound on the hairline candidates; without it no
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
        // Ranked hairline-border candidate: strips before frame-sized, lower
        // height first within each class.
        let mut hairline_border: Option<((u8, f64), u64)> = None;
        let mut border_logs = 0usize;
        while let Some(element) = queue.pop() {
            visited += 1;
            if visited > 4000 {
                crate::service::debug_log("stopped walking the frame: tree larger than expected");
                break;
            }
            for child in unsafe { xaml::children_of(element.as_raw(), 64) } {
                let class = unsafe { xaml::runtime_class_name(child.as_raw()) };
                match class.as_deref() {
                    Some(RECTANGLE_TYPE) => {
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
                        // Hairline carriers: EVERY full-width, hair-tall
                        // rectangle, not just the widest one. The visible line
                        // on this build is a 2560x1.33 dp rectangle filled
                        // with 40% grey that only gains its fill (and size)
                        // after the claim-time walk has run; a frame of
                        // unknown size disqualifies every candidate.
                        if height > 0.0
                            && height <= HAIRLINE_MAX_HEIGHT_DIP
                            && frame_width
                                .map(|frame| width >= frame * HAIRLINE_MIN_WIDTH_FRACTION)
                                .unwrap_or(false)
                        {
                            if let Some(handle) = self.handle_of(&child) {
                                let stroke = unsafe { xaml::stroke_of(child.as_raw()) };
                                let stroke_color =
                                    stroke.and_then(|stroke| unsafe { xaml::solid_color_of(stroke) });
                                if let Some(stroke) = stroke {
                                    unsafe { com::release_raw(stroke) };
                                }
                                let fill = unsafe { xaml::fill_of(child.as_raw()) };
                                let fill_color = fill.and_then(|fill| unsafe { xaml::solid_color_of(fill) });
                                if let Some(fill) = fill {
                                    unsafe { com::release_raw(fill) };
                                }
                                crate::service::debug_log_fmt(format_args!(
                                    "claim: hairline rectangle {width}x{height} dp fill {fill_color:?} stroke {stroke_color:?} (carrier)"
                                ));
                                self.register_rectangle(frame_handle, handle, false);
                            }
                        }
                    }
                    Some(BORDER_TYPE) => {
                        let (width, height) =
                            unsafe { xaml::actual_size_of(child.as_raw()) }.unwrap_or((0.0, 0.0));
                        let thickness = unsafe { xaml::border_thickness_of(child.as_raw()) };
                        let top = thickness.unwrap_or_default().top;
                        if top > 0.0
                            && frame_width
                                .map(|frame| width >= frame * HAIRLINE_MIN_WIDTH_FRACTION)
                                .unwrap_or(false)
                        {
                            if let Some(handle) = self.handle_of(&child) {
                                let strip = height <= HAIRLINE_BORDER_STRIP_HEIGHT_DIP;
                                let rank = (u8::from(!strip), height);
                                if hairline_border
                                    .map(|(best_rank, _)| rank < best_rank)
                                    .unwrap_or(true)
                                {
                                    crate::service::debug_log_fmt(format_args!(
                                        "claim: hairline border {width}x{height} dp top {top:#?} (candidate)"
                                    ));
                                    hairline_border = Some((rank, handle));
                                }
                            }
                        }
                        // Diagnostic: what borders the island actually carries.
                        // The interesting ones have a top edge, and their brush
                        // is what the hairline switch will have to clear.
                        if top > 0.0 && border_logs < BORDER_LOG_LIMIT {
                            border_logs += 1;
                            let brush = unsafe { xaml::border_brush_of(child.as_raw()) };
                            let brush_color =
                                brush.and_then(|brush| unsafe { xaml::solid_color_of(brush) });
                            if let Some(brush) = brush {
                                unsafe { com::release_raw(brush) };
                            }
                            let background = unsafe { xaml::border_background_of(child.as_raw()) };
                            let background_color = background
                                .and_then(|background| unsafe { xaml::solid_color_of(background) });
                            if let Some(background) = background {
                                unsafe { com::release_raw(background) };
                            }
                            crate::service::debug_log_fmt(format_args!(
                                "claim: border {width}x{height} top {top:#?} brush {brush_color:?} background {background_color:?}"
                            ));
                        }
                    }
                    _ => {}
                }
                if class.as_deref() != Some(RECTANGLE_TYPE) {
                    queue.push(child);
                }
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
        match hairline_border {
            Some(((rank, height), handle)) => {
                crate::service::debug_log_fmt(format_args!(
                    "claim: hairline border {height} dp tall (class {})",
                    if rank == 0 { "strip" } else { "frame-sized" }
                ));
                self.register_border(frame_handle, handle);
            }
            None => crate::service::debug_log("claim: no hairline border inside the frame"),
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

    /// Register a `Border` element as the hairline carrier for `frame`.
    fn register_border(&self, frame: u64, border_handle: u64) {
        let Some(inspectable) = self.inspectable_at(border_handle) else {
            return;
        };
        if unsafe { !xaml::supports(inspectable.as_raw(), &com::IID_IBORDER) } {
            return;
        }
        service::register_taskbar_hairline_border(frame, inspectable);
    }

    /// Register the `TaskbarFrame` itself as a Control-level hairline carrier:
    /// its template paints its `BorderBrush` along the frame's top edge, which
    /// is where the visible hairline lives on this build.
    fn register_frame_control(&self, frame_handle: u64) {
        let Some(inspectable) = self.inspectable_at(frame_handle) else {
            return;
        };
        if unsafe { !xaml::supports(inspectable.as_raw(), &com::IID_ICONTROL) } {
            crate::service::debug_log("frame does not implement IControl");
            return;
        }
        service::register_taskbar_frame_control(frame_handle, inspectable);
    }

    /// A `Border` named `BackgroundElement` was added. The name also appears
    /// in taskbar-button templates, so only a frame-sized one is taken for the
    /// frame's background border; everything else is logged and ignored.
    fn register_background_border(&self, frame_handle: u64, border_handle: u64) {
        let Some(inspectable) = self.inspectable_at(border_handle) else {
            return;
        };
        let (width, height) =
            unsafe { xaml::actual_size_of(inspectable.as_raw()) }.unwrap_or((0.0, 0.0));
        let frame_width = self
            .inspectable_at(frame_handle)
            .and_then(|frame| unsafe { xaml::actual_size_of(frame.as_raw()) })
            .map(|(width, _)| width);
        let thickness = unsafe { xaml::border_thickness_of(inspectable.as_raw()) };
        let brush = unsafe { xaml::border_brush_of(inspectable.as_raw()) };
        let brush_color = brush.and_then(|brush| unsafe { xaml::solid_color_of(brush) });
        if let Some(brush) = brush {
            unsafe { com::release_raw(brush) };
        }
        let frame_sized = frame_width
            .map(|frame| width >= frame * HAIRLINE_MIN_WIDTH_FRACTION)
            .unwrap_or(false);
        crate::service::debug_log_fmt(format_args!(
            "on_added: BackgroundElement border {width}x{height} top {:?} brush {brush_color:?} frame-sized: {frame_sized}"
        , thickness.unwrap_or_default()));
        if frame_sized {
            self.register_border(frame_handle, border_handle);
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
