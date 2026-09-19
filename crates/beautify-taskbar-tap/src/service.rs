//! In-explorer state: the taskbar registry, the message-only command window,
//! and the code that actually repaints the XAML taskbar.
//!
//! Everything except the worker-death watcher runs on the taskbar's XAML UI
//! thread — either an `OnVisualTreeChange` callback or a message on the
//! hidden window. XAML objects are thread-affine, so this is what makes
//! touching them safe.

use crate::com::{self, Color};
use crate::effects::GaussianBlurEffect;
use crate::protocol::{
    BrushKind, CommandKind, TapCommand, COPYDATA_MAGIC, PROTOCOL_VERSION, TAP_WINDOW_CLASS,
};
use crate::xaml;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use windows::core::{IUnknown, Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    FillRect, GetDCEx, GetStockObject, HBRUSH, BLACK_BRUSH, DCX_CACHE, DCX_WINDOW,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    INFINITE,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW,
    GetClientRect, GetWindowThreadProcessId, RegisterClassExW, CreateWindowExW,
    PostMessageW, HMENU, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_COPYDATA, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW,
};
use windows_numerics::Vector2;

/// Posted by the worker-death watcher so the restore runs on the UI thread.
const WM_APP_RESTORE_ALL: u32 = WM_APP + 4;
const SUBCLASS_ID: usize = 0x5F42_5450;
/// One-shot timer id for the delayed re-walk (see [`schedule_rewalk`]).
const TIMER_REWALK: usize = 1;

/// Gaussian radius for the acrylic material. The host does not send a blur
/// amount for acrylic (the mode is defined by its material, not by a knob), so
/// the TAP picks the wider radius its look needs.
const ACRYLIC_BLUR_AMOUNT: f32 = 30.0;

/// One of the two rectangles (`BackgroundFill`/`BackgroundStroke`) tracked per
/// taskbar island. `original` is captured lazily on the first repaint command:
/// right after the tree reports the element the shell may not have assigned
/// its fill yet, and it certainly has by the time the host sends work.
/// A raw COM pointer that may live inside the shared service state (which the
/// static mutex requires to be `Send`). Ownership follows the usual
/// one-reference rule: the wrapper owns exactly one reference, released on
/// drop.
#[derive(Default, Clone, Copy, PartialEq)]
struct SendPtr(*mut core::ffi::c_void);
unsafe impl Send for SendPtr {}

/// Move an owned wrapper into a [`SendPtr`] without touching the refcount.
fn own(unknown: IUnknown) -> SendPtr {
    let raw = unknown.as_raw();
    std::mem::forget(unknown);
    SendPtr(raw)
}

#[derive(Default)]
struct ControlInfo {
    shape: Option<SendPtr>,
    /// The shell.s own brush, captured once. `None` after capture means the
    /// shell had not painted the element yet, and restoring means clearing the
    /// fill again — which is why the capture needs its own flag: a null fill is
    /// a value to restore to, not "not captured yet".
    original: Option<SendPtr>,
    captured: bool,
}

impl Drop for ControlInfo {
    fn drop(&mut self) {
        // Both are owned references: `shape` was adopted when the rectangle was
        // registered, `original` when the shell's fill was read.
        if let Some(shape) = self.shape.take() {
            unsafe { com::release_raw(shape.0) };
        }
        if let Some(original) = self.original.take() {
            unsafe { com::release_raw(original.0) };
        }
    }
}

/// Which property one hairline carrier paints through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarrierKind {
    /// A `Shape`'s `Fill` — the thin `BackgroundStroke` rectangle's fill,
    /// which reads as fully transparent on this build.
    RectangleFill,
    /// A `Shape`'s `Stroke` — the same rectangle's outline. The name says it:
    /// the visible top hairline is this rectangle's stroke brush, which is
    /// why clearing fills and border brushes alone changed nothing.
    RectangleStroke,
    /// A `Border`'s `BorderBrush`.
    BorderBrush,
    /// A `Control`'s `BorderBrush` — the `TaskbarFrame` itself, whose
    /// template paints it along the frame's top edge.
    ControlBorderBrush,
}

/// One element the shell may paint the top hairline with, plus its own brush
/// for putting back. A raw COM pointer that may live inside the shared
/// service state (which the static mutex requires to be `Send`). Ownership
/// follows the usual one-reference rule: adopted at registration, released on
/// drop.
///
/// `captured` means we hold the shell's real brush in `original`. A carrier
/// whose brush has not been read yet is left alone: the shell paints some of
/// these elements seconds after the claim, and clearing before that would
/// either race its paint or, worse, record "no brush" and make *show*
/// unrecoverable.
struct HairlineCarrier {
    kind: CarrierKind,
    target: SendPtr,
    /// The shell's own brush, captured before the first clear.
    original: Option<SendPtr>,
    captured: bool,
}

impl Drop for HairlineCarrier {
    fn drop(&mut self) {
        unsafe { com::release_raw(self.target.0) };
        if let Some(original) = self.original.take() {
            unsafe { com::release_raw(original.0) };
        }
    }
}

impl HairlineCarrier {
    /// Read the shell's current brush (owned reference).
    fn read(&self) -> Option<*mut core::ffi::c_void> {
        Self::read_for(self.kind, self.target)
    }

    fn read_for(kind: CarrierKind, target: SendPtr) -> Option<*mut core::ffi::c_void> {
        unsafe {
            match kind {
                CarrierKind::RectangleFill => xaml::fill_of(target.0),
                CarrierKind::RectangleStroke => xaml::stroke_of(target.0),
                CarrierKind::BorderBrush => xaml::border_brush_of(target.0),
                CarrierKind::ControlBorderBrush => xaml::control_border_brush_of(target.0),
            }
        }
    }


    fn write_for(kind: CarrierKind, target: SendPtr, brush: *mut core::ffi::c_void) -> bool {
        unsafe {
            match kind {
                CarrierKind::RectangleFill => xaml::set_fill(target.0, brush),
                CarrierKind::RectangleStroke => xaml::set_stroke(target.0, brush),
                CarrierKind::BorderBrush => xaml::set_border_brush(target.0, brush),
                CarrierKind::ControlBorderBrush => {
                    xaml::set_control_border_brush(target.0, brush)
                }
            }
        }
    }
}

struct Bar {
    /// The `Shell_TrayWnd`/`Shell_SecondaryTrayWnd` this island belongs to.
    /// Commands address a taskbar by that window, so the mapping is kept ready
    /// rather than derived from the island's host window on every command.
    taskbar: isize,
    background: ControlInfo,
    /// Whether the host wants the top hairline visible, once it has said.
    ///
    /// Remembered rather than only acted on, because the host's command and
    /// the shell's hairline elements are not synchronised: the elements turn
    /// up from visual tree callbacks, which can be after the command that
    /// wanted it hidden. Nothing would ever ask again, so the state waits here
    /// until there is something to apply it to.
    hairline: Option<bool>,
    /// Everything the shell may paint the top hairline with. Several carriers
    /// register per taskbar — the thin rectangle, frame-sized borders, and the
    /// frame's own control brush — and the state applies to all of them.
    hairline_carriers: Vec<HairlineCarrier>,
    blur_attached: bool,
}

#[derive(Default)]
struct Service {
    bars: HashMap<u64, Bar>,
    window: isize,
    subclassed: HashSet<isize>,
    worker_pid: u32,
}

static SERVICE: OnceLock<Mutex<Service>> = OnceLock::new();

/// The command window handle, mirrored out of [`Service`] so the timer can be
/// armed from paths that already hold the lock (a nested `with_service` would
/// deadlock).
static REWALK_WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// How many times the delayed re-walk has armed itself. The shell paints the
/// hairline elements seconds after the claim; a bounded number of retries
/// covers that without an immortal timer.
const REWALK_MAX_ATTEMPTS: u32 = 15;
static REWALK_ATTEMPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The sprite visual each blur graph mounted, keyed by its background
/// rectangle. Detaching a visual whose brush samples the backdrop deadlocks
/// the UI thread against the render thread — the brush is nulled first, which
/// needs the sprite, which is what this table is for.
static BLUR_SPRITES: OnceLock<Mutex<HashMap<usize, SendPtr>>> = OnceLock::new();

fn blur_sprites() -> &'static Mutex<HashMap<usize, SendPtr>> {
    BLUR_SPRITES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn service() -> &'static Mutex<Service> {
    SERVICE.get_or_init(|| Mutex::new(Service::default()))
}

/// Run `body` against the service state; a poisoned lock is recovered because
/// the state is plain data and every entry point here is panic-guarded.
fn with_service<R>(body: impl FnOnce(&mut Service) -> R) -> R {
    let mut guard = service()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    body(&mut guard)
}

// ---------------------------------------------------------------------------
// Registration — called from OnVisualTreeChange on the UI thread.
// ---------------------------------------------------------------------------

/// Remember a `Taskbar.TaskbarFrame` and the taskbar window hosting it.
pub fn register_taskbar(frame_handle: u64, taskbar: isize) {
    ensure_window();
    with_service(|svc| {
        svc.bars.entry(frame_handle).or_insert_with(|| Bar {
            taskbar,
            background: ControlInfo::default(),
            hairline: None,
            hairline_carriers: Vec::new(),
            blur_attached: false,
        });
    });
    debug_log("registered taskbar frame");
}

/// The claimed `TaskbarFrame` handles, for the delayed re-walk.
pub fn frame_handles() -> Vec<u64> {
    with_service(|svc| svc.bars.keys().copied().collect())
}

/// Are any taskbars registered yet? The tree callback retries discovery while
/// this is false, and stops once a frame has been claimed.
pub fn has_bars() -> bool {
    with_service(|svc| !svc.bars.is_empty())
}

/// Has this frame already been claimed?
///
/// The recovery walk runs on tree mutations, and nearly all of them belong to a
/// taskbar that was claimed long ago; this is what keeps those cheap.
pub fn is_registered(frame_handle: u64) -> bool {
    with_service(|svc| svc.bars.contains_key(&frame_handle))
}

/// Is a taskbar of this session still missing from the registry?
///
/// The recovery walk exists for the taskbars that were already there when we
/// were injected: they never announce a frame, so they have to be looked for.
/// It is gated on this rather than on "nothing has been claimed yet", which is
/// what left every monitor but the primary untouched — the first island claimed
/// turned the old gate off, and the rest were never looked for again.
pub fn has_unclaimed_taskbar() -> bool {
    let claimed = claimed_taskbars();
    taskbar_windows()
        .into_iter()
        .any(|window| !claimed.contains(&window))
}

/// The taskbar windows that have an island registered against them.
pub fn claimed_taskbars() -> Vec<isize> {
    with_service(|svc| svc.bars.values().map(|bar| bar.taskbar).collect())
}

/// Remember one of the taskbar's background rectangles.
pub fn register_taskbar_background(frame_handle: u64, shape: IUnknown) {
    with_service(|svc| {
        if let Some(bar) = svc.bars.get_mut(&frame_handle) {
            bar.background.shape = Some(own(shape));
        }
    });
    debug_log("registered background rectangle");
}

/// Register one hairline carrier, or re-apply the pending hairline state to an
/// already-registered one. Returns the plan, if the state is already known —
/// the "command before element" race: the host may have asked for a hairline
/// state while this element was still unknown, and nothing would ever ask
/// again. Only a *first* sighting applies it: this runs inside a visual tree
/// callback, and re-applying on every notification would repaint the element
/// on each of the shell's many tree mutations.
fn register_carrier(
    svc: &mut Service,
    frame_handle: u64,
    kind: CarrierKind,
    target: SendPtr,
) -> Option<HairlinePlan> {
    let bar = svc.bars.get_mut(&frame_handle)?;
    let pending = bar.hairline.is_some();
    let first_sighting = !bar
        .hairline_carriers
        .iter()
        .any(|carrier| carrier.kind == kind && carrier.target == target);
    if first_sighting {
        bar.hairline_carriers.push(HairlineCarrier {
            kind,
            target,
            original: None,
            captured: false,
        });
    }
    if pending && first_sighting {
        let plan = plan_hairline(bar);
        if plan.retry {
            // The element is here but its paint is not; come back on the timer.
            schedule_rewalk();
        }
        Some(plan)
    } else {
        None
    }
}

/// Remember the taskbar's border rectangle (the hairline along its top edge).
///
/// The rectangle is registered through *both* of its paint channels: its fill
/// (transparent on this build) and its stroke.
pub fn register_taskbar_border(frame_handle: u64, shape: IUnknown) {
    let target = own(shape);
    let plan = with_service(|svc| {
        let fill = register_carrier(svc, frame_handle, CarrierKind::RectangleFill, target);
        let stroke = register_carrier(svc, frame_handle, CarrierKind::RectangleStroke, target);
        match (fill, stroke) {
            (Some(mut a), Some(b)) => {
                a.writes.extend(b.writes);
                a.retry |= b.retry;
                Some(a)
            }
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    });
    if let Some(plan) = plan {
        commit_hairline(plan);
    }
}

/// Remember a `Border` element whose `BorderBrush` may paint the hairline.
pub fn register_taskbar_hairline_border(frame_handle: u64, border: IUnknown) {
    let target = own(border);
    let plan =
        with_service(|svc| register_carrier(svc, frame_handle, CarrierKind::BorderBrush, target));
    if let Some(plan) = plan {
        commit_hairline(plan);
    }
}

/// Remember the `TaskbarFrame` itself: a `Control`, whose template paints its
/// `BorderBrush` along the frame's edges — the top one is the hairline.
pub fn register_taskbar_frame_control(frame_handle: u64, frame: IUnknown) {
    let target = own(frame);
    let plan = with_service(|svc| {
        let plan = register_carrier(svc, frame_handle, CarrierKind::ControlBorderBrush, target);
        debug_log("registered frame control brush carrier");
        plan
    });
    if let Some(plan) = plan {
        commit_hairline(plan);
    }
}

/// Remember a frame-wide `Border` discovered by the delayed re-walk, which
/// has no frame handle. The border joins the carriers of every registered bar.
pub fn register_frameless_hairline_border(border: IUnknown) {
    let target = own(border);
    let plan = with_service(|svc| {
        let mut combined: Option<HairlinePlan> = None;
        let bars = svc.bars.keys().copied().collect::<Vec<_>>();
        for frame_handle in bars {
            if let Some(part) = register_carrier(svc, frame_handle, CarrierKind::BorderBrush, target)
            {
                combined
                    .get_or_insert_with(|| HairlinePlan {
                        writes: Vec::new(),
                        retry: false,
                    })
                    .writes
                    .extend(part.writes);
            }
        }
        if combined.is_some() {
            debug_log("rewalk border carrier registered");
        }
        combined
    });
    if let Some(plan) = plan {
        commit_hairline(plan);
    }
}

/// Remember a thin full-width rectangle found by the delayed re-walk — the
/// hairline painter, which had no size and no fill when the claim walk ran.
pub fn register_frameless_hairline_rect(rect: IUnknown) {
    let target = own(rect);
    let plan = with_service(|svc| {
        let mut combined: Option<HairlinePlan> = None;
        let bars = svc.bars.keys().copied().collect::<Vec<_>>();
        for frame_handle in bars {
            for kind in [CarrierKind::RectangleFill, CarrierKind::RectangleStroke] {
                if let Some(part) = register_carrier(svc, frame_handle, kind, target) {
                    combined
                        .get_or_insert_with(|| HairlinePlan {
                            writes: Vec::new(),
                            retry: false,
                        })
                        .writes
                        .extend(part.writes);
                }
            }
        }
        if combined.is_some() {
            debug_log("rewalk rectangle carrier registered");
        }
        combined
    });
    if let Some(plan) = plan {
        commit_hairline(plan);
    }
}

/// Drop a taskbar (its XAML island was torn down).
pub fn unregister_taskbar(frame_handle: u64) {
    with_service(|svc| {
        svc.bars.remove(&frame_handle);
    });
}

/// One level of a window's children. A taskbar's XAML islands are child windows
/// of it, and their sizes say which island is which.
pub fn child_windows(parent: isize) -> Vec<isize> {
    let mut children = Vec::new();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(
            Some(HWND(parent as *mut core::ffi::c_void)),
            Some(collect_child),
            LPARAM(&mut children as *mut Vec<isize> as isize),
        );
    }
    children
}

unsafe extern "system" fn collect_child(window: HWND, param: LPARAM) -> windows::core::BOOL {
    let children = &mut *(param.0 as *mut Vec<isize>);
    children.push(window.0 as isize);
    true.into()
}

/// Screen rectangle of a window, in physical pixels.
pub fn window_rect(window: isize) -> Option<windows::Win32::Foundation::RECT> {
    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
            HWND(window as *mut core::ffi::c_void),
            &mut rect,
        )
        .ok()?;
    }
    Some(rect)
}

/// The taskbar windows of this session: the primary `Shell_TrayWnd` first, then
/// one `Shell_SecondaryTrayWnd` per extra monitor.
pub fn taskbar_windows() -> Vec<isize> {
    let mut bars = Vec::new();
    unsafe {
        if let Ok(primary) = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
            windows::core::w!("Shell_TrayWnd"),
            PCWSTR::null(),
        ) {
            if !primary.is_invalid() {
                bars.push(primary.0 as isize);
            }
        }
        let mut previous = HWND::default();
        loop {
            let Ok(next) = windows::Win32::UI::WindowsAndMessaging::FindWindowExW(
                None,
                Some(previous),
                windows::core::w!("Shell_SecondaryTrayWnd"),
                PCWSTR::null(),
            ) else {
                break;
            };
            if next.is_invalid() {
                break;
            }
            bars.push(next.0 as isize);
            previous = next;
        }
    }
    bars
}

// ---------------------------------------------------------------------------
// Command handling — WM_COPYDATA on the UI thread.
// ---------------------------------------------------------------------------

/// Execute one host command. `false` means "not applied"; the host treats it
/// as the channel not being ready yet.
fn handle_command(cmd: &TapCommand) -> bool {
    if cmd.version != PROTOCOL_VERSION {
        return false;
    }
    match cmd.command {
        c if c == CommandKind::Set as u32 => {
            watch_worker(cmd.worker_pid);
            set_appearance(cmd)
        }
        c if c == CommandKind::Restore as u32 => {
            watch_worker(cmd.worker_pid);
            restore_taskbar(cmd.taskbar as isize)
        }
        c if c == CommandKind::RestoreAll as u32 => restore_all(),
        c if c == CommandKind::SetHairline as u32 => {
            watch_worker(cmd.worker_pid);
            set_hairline(cmd.taskbar as isize, cmd.argb != 0)
        }
        _ => false,
    }
}

/// Resolve the host's top-level taskbar window to a registered island.
fn find_bar(taskbar: isize) -> Option<u64> {
    with_service(|svc| {
        svc.bars
            .iter()
            .find(|(_, bar)| bar.taskbar == taskbar)
            .map(|(handle, _)| *handle)
    })
}

/// Diagnostic: what the registry holds, for a command that did not match.
fn describe_bars() -> String {
    with_service(|svc| {
        svc.bars
            .iter()
            .map(|(frame, bar)| format!("frame {frame:x} -> taskbar {:#x}", bar.taskbar))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn set_appearance(cmd: &TapCommand) -> bool {
    let Some(handle) = find_bar(cmd.taskbar as isize) else {
        debug_log_fmt(format_args!(
            "appearance: no taskbar registered for {:#x}; have [{}]",
            cmd.taskbar,
            describe_bars()
        ));
        return false;
    };
    let brush = match cmd.brush {
        b if b == BrushKind::Solid as u32 => BrushKind::Solid,
        b if b == BrushKind::Acrylic as u32 => BrushKind::Acrylic,
        _ => BrushKind::Blur,
    };
    let color = Color::from_argb(cmd.argb);
    let blur_amount = if cmd.blur_amount > 0.0 { cmd.blur_amount } else { 10.0 };

    // Lazy capture: wait for the shell to have initialised the fill, then
    // snapshot it as the restore target. The host retries on its own cycle,
    // so not being ready yet is not an error.
    let ready = with_service(|svc| {
        let Some(bar) = svc.bars.get_mut(&handle) else {
            return false;
        };
        if bar.background.shape.is_none() {
            debug_log("appearance: the background rectangle is not registered yet");
            return false;
        }
        if !bar.background.captured {
            let shape_raw = bar.background.shape.as_ref().map(|s| s.0);
            bar.background.original =
                shape_raw.and_then(|raw| unsafe { xaml::fill_of(raw) }).map(SendPtr);
            bar.background.captured = true;
            debug_log("appearance: captured the shell's own fill");
        }
        // The same lazy capture for every hairline carrier: by the time the
        // host paints, the shell has initialised its brushes. Only a *real*
        // brush is captured — a brushless read means "not painted yet" and is
        // left for the re-walk timer, or "show" would restore a null and the
        // line could never come back.
        for carrier in bar.hairline_carriers.iter_mut() {
            if !carrier.captured {
                if let Some(brush) = carrier.read() {
                    carrier.original = Some(SendPtr(brush));
                    carrier.captured = true;
                }
            }
        }
        true
    });
    if !ready {
        return false;
    }

    ensure_subclass(cmd.taskbar as isize);

    let shape = with_service(|svc| svc.bars.get(&handle).and_then(|bar| bar.background.shape));
    let Some(shape) = shape else {
        return false;
    };

    let ok = unsafe {
        match brush {
            BrushKind::Solid => paint_fill(shape, || xaml::create_solid_brush(color)),
            BrushKind::Acrylic => {
                // Acrylic = the blur visual (behind) + a translucent tint (in
                // front). The child visual a blur graph mounts renders *below*
                // the element's own `Fill` — the standard XAML acrylic
                // pattern — so the tint is a plain translucent solid brush
                // layered over the blur.
                //
                // The shell's own `AcrylicBrush` was tried here first: created
                // through `XamlReader` (default activation answers
                // E_NOTIMPL), it renders — and then crashes the taskbar about
                // twenty seconds later, once or a hundred times applied. The
                // blur-graph material stays.
                if !attach_blur(shape, blur_amount.max(ACRYLIC_BLUR_AMOUNT)) {
                    debug_log("acrylic: the composition graph failed, using a solid tint");
                    paint_fill(shape, || xaml::create_solid_brush(color))
                } else {
                    // Half-strength tint: strong enough to read as a colour,
                    // weak enough to leave the blur visible.
                    let mut tint = color;
                    tint.a = 128;
                    paint_fill_keep_visual(shape, || xaml::create_solid_brush(tint))
                }
            }
            BrushKind::Blur => {
                let mut painted = attach_blur(shape, blur_amount);
                if !painted {
                    debug_log("blur: the composition graph failed, using a solid tint");
                    painted = paint_fill(shape, || xaml::create_solid_brush(color));
                }
                painted
            }
        }
    };
    debug_log(if ok { "appearance applied" } else { "appearance failed" });
    ok
}

/// Replace the background fill without touching the mounted blur visual:
/// the acrylic tint sits on top of the blur, and `paint_fill` would strip it.
unsafe fn paint_fill_keep_visual(
    shape: SendPtr,
    create: impl FnOnce() -> Option<*mut core::ffi::c_void>,
) -> bool {
    let Some(brush) = create() else {
        return false;
    };
    let ok = xaml::set_fill(shape.0, brush);
    com::release_raw(brush);
    ok
}

/// Replace the background fill with a freshly created XAML brush.
/// Detach the blur child visual of `shape`, if one is mounted. Nulling the
/// visual's brush *before* the detach is the whole point: a visual whose brush
/// samples the backdrop deadlocks the UI thread against the render thread when
/// it is torn down while still sampling.
unsafe fn detach_blur_visual(shape: SendPtr) {
    let sprite = blur_sprites().lock().unwrap_or_else(|p| p.into_inner()).remove(&(shape.0 as usize));
    if let Some(sprite) = sprite {
        if let Ok(unknown) = com::adopt(sprite.0) {
            if let Ok(visual) = unknown.cast::<windows::UI::Composition::SpriteVisual>() {
                let _ = visual.SetBrush(None::<&windows::UI::Composition::CompositionBrush>);
            }
        }
    }
    xaml::set_element_child_visual(shape.0, core::ptr::null_mut());
}

unsafe fn paint_fill(
    shape: SendPtr,
    create: impl FnOnce() -> Option<*mut core::ffi::c_void>,
) -> bool {
    // Any previous blur child visual must go: it would keep painting on top
    // of the new fill.
    detach_blur_visual(shape);
    with_service(|svc| {
        for bar in svc.bars.values_mut() {
            if bar.background.shape == Some(shape) {
                bar.blur_attached = false;
            }
        }
    });
    let Some(brush) = create() else {
        return false;
    };
    let ok = xaml::set_fill(shape.0, brush);
    com::release_raw(brush);
    ok
}

/// Build the backdrop-blur effect graph — a single Gaussian blur over the
/// backdrop — and mount it as a child visual of the background rectangle. The
/// rectangle's own fill is cleared: the child visual paints the blur instead,
/// and stacking it on the shell's opaque fill would hide it entirely. A tint
/// can be layered on top with the acrylic material; plain blur stays untinted.
///
/// Every step logs its failure: the graph involves six foreign COM calls whose
/// failure modes are invisible from outside explorer, and "the composition
/// graph failed" without a step made the first debugging session guesswork.
unsafe fn attach_blur(shape: SendPtr, blur_amount: f32) -> bool {
    use windows::UI::Composition::CompositionEffectSourceParameter;

    macro_rules! fail {
        ($step:expr, $result:expr) => {{
            match $result {
                Ok(value) => value,
                Err(error) => {
                    debug_log_fmt(format_args!("blur: {} failed {error:?}", $step));
                    return false;
                }
            }
        }};
    }

    let compositor = fail!("get element compositor", xaml::element_compositor(shape.0));
    let parameter = fail!(
        "create source parameter",
        CompositionEffectSourceParameter::Create(&windows::core::HSTRING::from("backdrop"))
    );
    let source = fail!(
        "cast parameter to IGraphicsEffectSource",
        parameter.cast::<windows::Graphics::Effects::IGraphicsEffectSource>()
    );
    let blur = GaussianBlurEffect::new(blur_amount, source);
    let graph: windows::Graphics::Effects::IGraphicsEffect = blur.into();
    let factory = fail!("CreateEffectFactory", compositor.CreateEffectFactory(&graph));
    let effect_brush = fail!("CreateBrush", factory.CreateBrush());
    let backdrop_brush = fail!("CreateBackdropBrush", compositor.CreateBackdropBrush());
    if let Err(error) =
        effect_brush.SetSourceParameter(&windows::core::HSTRING::from("backdrop"), &backdrop_brush)
    {
        debug_log_fmt(format_args!("blur: SetSourceParameter failed {error:?}"));
        return false;
    }
    let sprite = fail!("CreateSpriteVisual", compositor.CreateSpriteVisual());
    if let Err(error) = sprite.SetBrush(&effect_brush) {
        debug_log_fmt(format_args!("blur: SetBrush failed {error:?}"));
        return false;
    }
    // A child visual does not inherit the element's size; size it to the
    // rectangle now and again on the next apply — the host re-sends on every
    // geometry change, e.g. DPI switches. A visual of zero size renders
    // nothing, which would look exactly like the effect having failed, so an
    // unreadable size is a loud log line.
    let size = xaml::actual_size_of(shape.0);
    if let Some((width, height)) = size {
        let _ = sprite.SetSize(Vector2 {
            X: width as f32,
            Y: height as f32,
        });
    } else {
        debug_log("blur: the rectangle has no readable size; sprite stays unsized");
    }
    if !xaml::set_element_child_visual(shape.0, sprite.as_raw()) {
        debug_log("blur: SetElementChildVisual failed");
        return false;
    }
    debug_log_fmt(format_args!(
        "blur: effect graph mounted (size {size:?})"
    ));
    // Remember the sprite so a later detach can null its brush first (see
    // `detach_blur_visual`).
    if let Ok(unknown) = sprite.cast::<windows::core::IUnknown>() {
        blur_sprites()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(shape.0 as usize, own(unknown));
    }
    // The graph paints the background now. Clearing the shell's own fill makes
    // the blur visible regardless of which of the element's contents render on
    // top of the child visual, and `restore_bar` puts the captured fill back.
    xaml::set_fill(shape.0, core::ptr::null_mut());
    with_service(|svc| {
        for bar in svc.bars.values_mut() {
            if bar.background.shape == Some(shape) {
                bar.blur_attached = true;
            }
        }
    });
    true
}

/// Show or hide the hairline along the taskbar's top edge.
///
/// The line reaches the screen through several carriers — the thin
/// `BackgroundStroke` rectangle, frame-sized `Border`s, and the frame's own
/// control brush — so hiding clears all registered carriers and showing puts
/// their captured brushes back. `argb` carries the flag rather than a colour —
/// see [`CommandKind::SetHairline`].
fn set_hairline(taskbar: isize, visible: bool) -> bool {
    let Some(handle) = find_bar(taskbar) else {
        debug_log_fmt(format_args!(
            "hairline: no taskbar registered for {:#x}",
            taskbar
        ));
        return false;
    };
    let (applied, retry) = with_service(|svc| {
        let Some(bar) = svc.bars.get_mut(&handle) else {
            return (false, false);
        };
        bar.hairline = Some(visible);
        let plan = plan_hairline(bar);
        let retry = plan.retry;
        let applied = commit_hairline(plan);
        (applied, retry)
    });
    if retry {
        // Some carriers have not been painted by the shell yet; the timer
        // re-runs the apply until they show up or the retries run out.
        schedule_rewalk();
    }
    // "Nothing to write" is not a failure here: the wanted state stays in the
    // registry and the carriers apply it the moment they are registered.
    debug_log(if applied {
        "hairline applied"
    } else if retry {
        "hairline: waiting for the shell to paint its carriers"
    } else {
        "hairline: nothing to apply yet"
    });
    applied
}

/// What [`plan_hairline`] decided. `retry` is set when the wanted state is
/// "hide" but some carrier's brush has not appeared yet — the shell paints
/// them seconds after the claim — and the apply should be re-run on a timer.
struct HairlinePlan {
    writes: Vec<(CarrierKind, SendPtr, SendPtr)>,
    retry: bool,
}

/// Work out the writes that carry out the remembered hairline state.
///
/// Separated from performing them because the decision needs the registry lock
/// and the writes are XAML calls: holding the lock across one would deadlock if
/// the shell re-entered our tree callback.
///
/// Hiding captures each carrier's real brush the first time it can be read and
/// clears it; showing puts a captured brush back (a carrier that was never
/// cleared is already showing the shell's own state and is skipped). A carrier
/// that reads as brushless while hiding leaves `retry` set — its paint has not
/// happened yet, and capturing now would lose the brush forever.
fn plan_hairline(bar: &mut Bar) -> HairlinePlan {
    let mut plan = HairlinePlan {
        writes: Vec::new(),
        retry: false,
    };
    let Some(wanted) = bar.hairline else {
        return plan;
    };

    for index in 0..bar.hairline_carriers.len() {
        let carrier = &mut bar.hairline_carriers[index];
        if wanted {
            if !carrier.captured {
                // Never cleared, so the shell's own state is showing.
                continue;
            }
            let original = carrier
                .original
                .map(|original| original.0)
                .unwrap_or(core::ptr::null_mut());
            plan.writes.push((carrier.kind, carrier.target, SendPtr(original)));
        } else if carrier.captured {
            plan.writes.push((carrier.kind, carrier.target, SendPtr(core::ptr::null_mut())));
        } else if let Some(brush) = carrier.read() {
            carrier.original = Some(SendPtr(brush));
            carrier.captured = true;
            plan.writes.push((carrier.kind, carrier.target, SendPtr(core::ptr::null_mut())));
        } else {
            plan.retry = true;
        }
    }

    plan
}

/// Perform writes from [`plan_hairline`], outside the registry lock.
fn commit_hairline(plan: HairlinePlan) -> bool {
    let mut any = false;
    for (kind, target, brush) in plan.writes {
        any |= HairlineCarrier::write_for(kind, target, brush.0);
    }
    any
}

/// The writes that hand one taskbar back to the shell, collected under the
/// registry lock and performed outside it — a XAML write fires a synchronous
/// tree callback, and performing one *under* the lock re-enters
/// [`with_service`] on the same thread, which deadlocks a non-reentrant mutex.
struct RestorePlan {
    /// Background rectangles whose blur visual must be detached.
    detach: Vec<SendPtr>,
    /// Hairline carriers: (kind, target, the shell's brush to put back).
    carriers: Vec<(CarrierKind, SendPtr, SendPtr)>,
    /// Background fills: (shape, the shell's fill to put back).
    fills: Vec<(SendPtr, SendPtr)>,
}

/// Collect [`RestorePlan`] for one bar, resetting the bar's live state.
fn plan_restore(bar: &mut Bar, plan: &mut RestorePlan) {
    if bar.blur_attached {
        if let Some(shape) = bar.background.shape {
            plan.detach.push(shape);
        }
        bar.blur_attached = false;
    }
    for carrier in bar.hairline_carriers.iter() {
        // Only a brush this process actually read gets written back. Restoring
        // a carrier that was never captured would clear a brush the shell had
        // painted, which for the hairline means erasing it rather than leaving
        // it alone — the exact opposite of "hand the taskbar back".
        if !carrier.captured {
            continue;
        }
        // A missing original is a value: the shell had no brush to begin with.
        let original = carrier
            .original
            .map(|original| original.0)
            .unwrap_or(core::ptr::null_mut());
        plan.carriers.push((carrier.kind, carrier.target, SendPtr(original)));
    }
    // Only a fill this process actually read gets written back, for the same
    // reason as the carriers above.
    if bar.background.captured {
        if let Some(shape) = bar.background.shape {
            let original = bar
                .background
                .original
                .map(|original| original.0)
                .unwrap_or(core::ptr::null_mut());
            plan.fills.push((shape, SendPtr(original)));
        }
    }
}

/// Perform a [`RestorePlan`] outside the registry lock.
fn perform_restore(plan: RestorePlan) {
    for shape in &plan.detach {
        unsafe { detach_blur_visual(*shape) };
    }
    for (kind, target, brush) in &plan.carriers {
        HairlineCarrier::write_for(*kind, *target, brush.0);
    }
    for (shape, brush) in &plan.fills {
        unsafe { xaml::set_fill(shape.0, brush.0) };
    }
}

/// Hand a taskbar back to the shell's own brushes.
fn restore_taskbar(taskbar: isize) -> bool {
    let Some(handle) = find_bar(taskbar) else {
        return false;
    };
    let plan = with_service(|svc| {
        let mut plan = RestorePlan {
            detach: Vec::new(),
            carriers: Vec::new(),
            fills: Vec::new(),
        };
        if let Some(bar) = svc.bars.get_mut(&handle) {
            plan_restore(bar, &mut plan);
        }
        plan
    });
    perform_restore(plan);
    true
}

/// Restore every registered island.
fn restore_all() -> bool {
    debug_log("restore_all: starting");
    let (restored, plan) = with_service(|svc| {
        let mut plan = RestorePlan {
            detach: Vec::new(),
            carriers: Vec::new(),
            fills: Vec::new(),
        };
        for bar in svc.bars.values_mut() {
            plan_restore(bar, &mut plan);
        }
        (svc.bars.len(), plan)
    });
    perform_restore(plan);
    debug_log_fmt(format_args!("restore_all: {restored} taskbars"));
    true
}

/// Re-run the pending hairline state for every bar. `true` when some carrier
/// is still waiting for the shell to paint it. The plans are collected under
/// the lock and performed outside it — see [`RestorePlan`].
pub fn retry_pending_hairline() -> bool {
    let plans = with_service(|svc| {
        let mut plans = Vec::new();
        for bar in svc.bars.values_mut() {
            if bar.hairline.is_none() {
                continue;
            }
            plans.push(plan_hairline(bar));
        }
        plans
    });
    let mut pending = false;
    for plan in plans {
        pending |= plan.retry;
        commit_hairline(plan);
    }
    pending
}

/// Schedule the delayed re-walk: a one-shot timer on the command window, which
/// lives on the XAML UI thread, so `watcher::rewalk` runs where XAML is safe
/// to touch. Fired a few seconds after the claim, when the island's layout has
/// settled, and re-armed while hairline carriers are still waiting for their
/// paint.
pub fn schedule_rewalk() {
    let window = REWALK_WINDOW.load(Ordering::Acquire);
    if window == 0 {
        return;
    }
    let attempts = REWALK_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    if attempts >= REWALK_MAX_ATTEMPTS {
        return;
    }
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(HWND(window as *mut core::ffi::c_void)),
            TIMER_REWALK,
            4000,
            None,
        );
    }
}

// ---------------------------------------------------------------------------
// Worker death watch — the taskbar must self-restore if the host dies.
// ---------------------------------------------------------------------------

/// Watch the host process; when it dies, ask the UI thread to restore every
/// taskbar. Replaces a previous watch instead of stacking threads.
fn watch_worker(pid: u32) {
    if pid == 0 {
        return;
    }
    let (already, window) = with_service(|svc| {
        if svc.worker_pid == pid {
            (true, svc.window)
        } else {
            svc.worker_pid = pid;
            (false, svc.window)
        }
    });
    if already || window == 0 {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("wb-tap-watch".into())
        .spawn(move || unsafe {
            const SYNCHRONIZE: u32 = 0x0010_0000;
            let process = OpenProcess(
                PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0 | SYNCHRONIZE),
                false,
                pid,
            );
            let Ok(process) = process else {
                return;
            };
            if process.is_invalid() {
                return;
            }
            WaitForSingleObject(HANDLE(process.0), INFINITE);
            debug_log("worker process died, asking for a restore");
            let _ = PostMessageW(
                Some(HWND(window as *mut _)),
                WM_APP_RESTORE_ALL,
                WPARAM(0),
                LPARAM(0),
            );
        });
}

// ---------------------------------------------------------------------------
// Hidden message-only window — lives on the XAML UI thread.
// ---------------------------------------------------------------------------

/// Create the command window if it does not exist yet, on the calling thread.
///
/// Called as soon as the framework is connected, before any taskbar has been
/// claimed: the host looks for this window right after injecting, and gives up
/// on the injection if it is not there.
pub fn ensure_command_window() {
    ensure_window();
}

/// Create the message-only command window on the calling thread. The XAML UI
/// thread pumps Win32 messages, so no dedicated loop is needed.
fn ensure_window() {
    let exists = with_service(|svc| svc.window != 0);
    if exists {
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        // `w!` needs a literal; the class name comes from the shared protocol.
        let class_name = windows::core::HSTRING::from(TAP_WINDOW_CLASS);
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tap_wndproc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hInstance: instance.into(),
            ..Default::default()
        };
        // Re-registration after an explorer restart is harmless.
        let class_result = RegisterClassExW(&class);
        match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            Option::<HMENU>::None,
            Some(instance.into()),
            None,
        ) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                let raw = hwnd.0 as isize;
                with_service(|svc| svc.window = raw);
                REWALK_WINDOW.store(raw, Ordering::Release);
                debug_log_fmt(format_args!("tap window ready: {raw:#x}"));
            }
            other => {
                let error = windows::Win32::Foundation::GetLastError();
                debug_log_fmt(format_args!(
                    "tap window creation failed: {other:?} (class register: {class_result:?}, last error {error:?})"
                ));
            }
        }
    }
}

unsafe extern "system" fn tap_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Never unwind into the shell's message loop.
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_COPYDATA => handle_copydata(wparam, lparam).map(|applied| LRESULT(applied as isize)),
        WM_APP_RESTORE_ALL => Some(LRESULT(restore_all() as isize)),
        WM_TIMER if wparam.0 == TIMER_REWALK => {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(
                    Some(hwnd),
                    TIMER_REWALK,
                );
            }
            let rewrote = crate::watcher::rewalk();
            let pending = retry_pending_hairline();
            if pending {
                // Carriers still waiting for their paint; try again.
                schedule_rewalk();
            }
            Some(LRESULT((rewrote || pending) as isize))
        }
        _ => None,
    }));
    match handled {
        Ok(Some(result)) => result,
        Ok(None) => {
            if msg == WM_NCDESTROY {
                with_service(|svc| svc.window = 0);
            }
            // `DefWindowProcW`, not `DefSubclassProc`: this is the window class's
            // own procedure. `DefSubclassProc` looks up a subclass chain that does
            // not exist here and returns without handling the creation messages,
            // which makes `CreateWindowExW` fail — the command window then never
            // exists, and the host gives up on the TAP.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        Err(_) => {
            debug_log("window proc panicked");
            LRESULT(0)
        }
    }
}

/// `Some(1)` when the command was applied, `Some(0)` when not.
fn handle_copydata(_wparam: WPARAM, lparam: LPARAM) -> Option<u32> {
    unsafe {
        let data = &*(lparam.0 as *const COPYDATASTRUCT);
        if data.dwData != COPYDATA_MAGIC as usize
            || data.cbData as usize != std::mem::size_of::<TapCommand>()
            || data.lpData.is_null()
        {
            return Some(0);
        }
        let cmd = &*(data.lpData as *const TapCommand);
        Some(u32::from(handle_command(cmd)))
    }
}

// ---------------------------------------------------------------------------
// Taskbar window subclass — zeroes the legacy GDI surface so the XAML island
// sits on a transparent window (Clear mode needs this to show through).
// ---------------------------------------------------------------------------

/// `WM_PAINT` on the taskbar's Win32 window would repaint the opaque legacy
/// surface over our work. Fill it with the stock black brush instead —
/// `BLACK_BRUSH` is colour `0x00000000`, which in premultiplied alpha is
/// *transparent* black — then let the XAML content paint on top.
unsafe extern "system" fn taskbar_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_PAINT {
        let mut client = Default::default();
        if GetClientRect(hwnd, &mut client).is_ok() {
            let dc = GetDCEx(Some(hwnd), None, DCX_WINDOW | DCX_CACHE);
            FillRect(dc, &client, HBRUSH(GetStockObject(BLACK_BRUSH).0));
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn ensure_subclass(taskbar: isize) {
    let already = with_service(|svc| !svc.subclassed.insert(taskbar));
    if !already {
        let ok = unsafe {
            SetWindowSubclass(HWND(taskbar as *mut _), Some(taskbar_subclass), SUBCLASS_ID, 0)
        };
        if !ok.as_bool() {
            with_service(|svc| {
                svc.subclassed.remove(&taskbar);
            });
            return;
        }
    }
    // The window's own surface is painted by the shell and stays opaque until
    // something asks it to paint; a translucent XAML brush on top of an opaque
    // surface still looks opaque. Force one paint so the subclass below can
    // zero the surface out.
    unsafe {
        let hwnd = HWND(taskbar as *mut core::ffi::c_void);
        debug_log("appearance: forcing a taskbar repaint");
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, true);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// The TAP's logging lives in [`crate::logging`], which is written to be safe
/// to call from the COM entry points this module runs under. Re-exported so the
/// call sites read as plain `debug_log(..)`.
pub use crate::logging::{debug_log, debug_log_fmt};

/// Unused today, kept for the secondary-taskbar path: the pid of the process
/// owning a window, for debugging mixed-explorer setups.
pub fn pid_of(window: isize) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(window as *mut _), Some(&mut pid));
    }
    pid
}
