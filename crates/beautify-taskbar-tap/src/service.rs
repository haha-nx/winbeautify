//! In-explorer state: the taskbar registry, the message-only command window,
//! and the code that actually repaints the XAML taskbar.
//!
//! Everything except the worker-death watcher runs on the taskbar's XAML UI
//! thread — either an `OnVisualTreeChange` callback or a message on the
//! hidden window. XAML objects are thread-affine, so this is what makes
//! touching them safe.

use crate::com::{self, Color};
use crate::effects::{CompositeEffect, FloodEffect, GaussianBlurEffect};
use crate::protocol::{
    BrushKind, CommandKind, TapCommand, COPYDATA_MAGIC, PROTOCOL_VERSION, TAP_WINDOW_CLASS,
};
use crate::xaml;

use std::collections::{HashMap, HashSet};
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
    WM_COPYDATA, WM_NCDESTROY, WM_PAINT, WNDCLASSEXW,
};
use windows_numerics::Vector2;

/// Posted by the worker-death watcher so the restore runs on the UI thread.
const WM_APP_RESTORE_ALL: u32 = WM_APP + 4;
const SUBCLASS_ID: usize = 0x5F42_5450;

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
    original: Option<SendPtr>,
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

struct Bar {
    /// The `Shell_TrayWnd`/`Shell_SecondaryTrayWnd` this island belongs to.
    /// Commands address a taskbar by that window, so the mapping is kept ready
    /// rather than derived from the island's host window on every command.
    taskbar: isize,
    background: ControlInfo,
    border: ControlInfo,
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
            border: ControlInfo::default(),
            blur_attached: false,
        });
    });
    debug_log("registered taskbar frame");
}

/// Are any taskbars registered yet? The tree callback retries discovery while
/// this is false, and stops once a frame has been claimed.
pub fn has_bars() -> bool {
    with_service(|svc| !svc.bars.is_empty())
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

/// Remember the taskbar's border rectangle (kept for restore only).
pub fn register_taskbar_border(frame_handle: u64, shape: IUnknown) {
    with_service(|svc| {
        if let Some(bar) = svc.bars.get_mut(&frame_handle) {
            bar.border.shape = Some(own(shape));
        }
    });
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
        if bar.background.original.is_none() {
            let shape_raw = bar.background.shape.as_ref().map(|s| s.0);
            let fill = shape_raw.and_then(|raw| unsafe { xaml::fill_of(raw) });
            let Some(fill) = fill else {
                debug_log("appearance: the shell.s fill is still null");
                return false;
            };
            bar.background.original = Some(SendPtr(fill));
        }
        if bar.border.shape.is_some() && bar.border.original.is_none() {
            let border_raw = bar.border.shape.as_ref().map(|s| s.0);
            bar.border.original = border_raw
                .and_then(|raw| unsafe { xaml::fill_of(raw) })
                .map(SendPtr);
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
            BrushKind::Acrylic => paint_fill(shape, || xaml::create_acrylic_brush(color)),
            BrushKind::Blur => attach_blur(shape, color, blur_amount),
        }
    };
    debug_log(if ok { "appearance applied" } else { "appearance failed" });
    ok
}

/// Replace the background fill with a freshly created XAML brush.
unsafe fn paint_fill(
    shape: SendPtr,
    create: impl FnOnce() -> Option<*mut core::ffi::c_void>,
) -> bool {
    // Any previous blur child visual must go: it would keep painting on top
    // of the new fill.
    xaml::set_element_child_visual(shape.0, core::ptr::null_mut());
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

/// Build the backdrop-blur effect graph and mount it as a child visual of the
/// background rectangle; the rectangle's own fill is cleared because the
/// visual paints the blur + tint instead.
unsafe fn attach_blur(shape: SendPtr, color: Color, blur_amount: f32) -> bool {
    use windows::UI::Composition::CompositionEffectSourceParameter;

    let Ok(compositor) = xaml::element_compositor(shape.0) else {
        return false;
    };
    let Ok(parameter) =
        CompositionEffectSourceParameter::Create(&windows::core::HSTRING::from("wb-backdrop"))
    else {
        return false;
    };
    let Ok(source) = parameter.cast::<windows::Graphics::Effects::IGraphicsEffectSource>() else {
        return false;
    };
    let blur = GaussianBlurEffect::new(blur_amount, source);
    let flood = FloodEffect::new([
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        color.a as f32 / 255.0,
    ]);
    let graph: windows::Graphics::Effects::IGraphicsEffect =
        CompositeEffect::new(vec![blur.into(), flood.into()]).into();
    let Ok(factory) = compositor.CreateEffectFactory(&graph) else {
        return false;
    };
    let Ok(effect_brush) = factory.CreateBrush() else {
        return false;
    };
    let Ok(backdrop_brush) = compositor.CreateBackdropBrush() else {
        return false;
    };
    if effect_brush
        .SetSourceParameter(&windows::core::HSTRING::from("wb-backdrop"), &backdrop_brush)
        .is_err()
    {
        return false;
    }
    let Ok(sprite) = compositor.CreateSpriteVisual() else {
        return false;
    };
    if sprite.SetBrush(&effect_brush).is_err() {
        return false;
    }
    // A child visual does not inherit the element's size; size it to the
    // rectangle now and again on the next apply — the host re-sends on every
    // geometry change, e.g. DPI switches.
    if let Some((width, height)) = xaml::actual_size_of(shape.0) {
        let _ = sprite.SetSize(Vector2 {
            X: width as f32,
            Y: height as f32,
        });
    }
    if !xaml::set_element_child_visual(shape.0, sprite.as_raw()) {
        return false;
    }
    with_service(|svc| {
        for bar in svc.bars.values_mut() {
            if bar.background.shape == Some(shape) {
                bar.blur_attached = true;
            }
        }
    });
    true
}

/// Hand a taskbar back to the shell's own brushes.
fn restore_taskbar(taskbar: isize) -> bool {
    let Some(handle) = find_bar(taskbar) else {
        return false;
    };
    with_service(|svc| {
        match svc.bars.get_mut(&handle) {
            Some(bar) => {
                restore_bar(bar);
                true
            }
            None => false,
        }
    })
}

/// Restore every registered island.
fn restore_all() -> bool {
    let restored = with_service(|svc| {
        for bar in svc.bars.values_mut() {
            restore_bar(bar);
        }
        svc.bars.len()
    });
    debug_log_fmt(format_args!("restore_all: {restored} taskbars"));
    true
}

fn restore_bar(bar: &mut Bar) {
    if bar.blur_attached {
        if let Some(shape) = bar.background.shape.as_ref() {
            unsafe { xaml::set_element_child_visual(shape.0, core::ptr::null_mut()) };
        }
        bar.blur_attached = false;
    }
    for control in [&mut bar.background, &mut bar.border] {
        if let (Some(shape), Some(original)) = (&control.shape, control.original) {
            unsafe { xaml::set_fill(shape.0, original.0) };
        }
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
    if already {
        return;
    }
    let ok =
        unsafe { SetWindowSubclass(HWND(taskbar as *mut _), Some(taskbar_subclass), SUBCLASS_ID, 0) };
    if !ok.as_bool() {
        with_service(|svc| {
            svc.subclassed.remove(&taskbar);
        });
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
