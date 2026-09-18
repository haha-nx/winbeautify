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
    GetAncestor, GetClientRect, GetWindowThreadProcessId, RegisterClassExW, CreateWindowExW,
    PostMessageW, GA_PARENT, HMENU, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
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
        if let Some(original) = self.original.take() {
            unsafe { com::release_raw(original.0) };
        }
    }
}

struct Bar {
    source_hwnd: isize,
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

/// Remember a `Taskbar.TaskbarFrame` and the island window hosting it.
pub fn register_taskbar(frame_handle: u64, source_hwnd: isize) {
    ensure_window();
    with_service(|svc| {
        svc.bars.entry(frame_handle).or_insert_with(|| Bar {
            source_hwnd,
            background: ControlInfo::default(),
            border: ControlInfo::default(),
            blur_attached: false,
        });
    });
    debug_log("registered taskbar frame");
}

/// Remember one of the taskbar's background rectangles.
pub fn register_taskbar_background(frame_handle: u64, shape: IUnknown) {
    with_service(|svc| {
        if let Some(bar) = svc.bars.get_mut(&frame_handle) {
            bar.background.shape = Some(own(shape));
        }
    });
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
        if let Some(bar) = svc.bars.remove(&frame_handle) {
            svc.subclassed.remove(&bar.source_hwnd);
        }
    });
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
            .find(|(_, bar)| unsafe {
                GetAncestor(HWND(bar.source_hwnd as *mut _), GA_PARENT).0 as isize == taskbar
            })
            .map(|(handle, _)| *handle)
    })
}

fn set_appearance(cmd: &TapCommand) -> bool {
    let Some(handle) = find_bar(cmd.taskbar as isize) else {
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
            return false;
        }
        if bar.background.original.is_none() {
            let shape_raw = bar.background.shape.as_ref().map(|s| s.0);
            let fill = shape_raw.and_then(|raw| unsafe { xaml::fill_of(raw) });
            let Some(fill) = fill else {
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
    with_service(|svc| {
        for bar in svc.bars.values_mut() {
            restore_bar(bar);
        }
        true
    })
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
        let _ = RegisterClassExW(&class);
        if let Ok(hwnd) = CreateWindowExW(
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
            let raw = hwnd.0 as isize;
            with_service(|svc| svc.window = raw);
        }
    }
    debug_log("tap window ready");
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
            DefSubclassProc(hwnd, msg, wparam, lparam)
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

/// `OutputDebugStringW` — invisible in normal operation, invaluable with a
/// debugger (or DebugView) attached to explorer.
///
/// Additionally, while a `wb_tap_debug.flag` file sits next to the DLL, every
/// line is appended to `wb_tap_debug.log` in the same directory. This is the
/// only way to see what the TAP is doing inside explorer without a debugger;
/// the flag file costs one `exists()` per line and exists only when someone
/// puts it there deliberately.
pub fn debug_log(message: &str) {
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
        let wide: Vec<u16> = format!("wb-tap: {message}\0").encode_utf16().collect();
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }
    if let Some(dir) = module_dir() {
        let flag = dir.join("wb_tap_debug.flag");
        if flag.exists() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("wb_tap_debug.log"))
            {
                use std::io::Write;
                let _ = writeln!(file, "{stamp} {message}");
            }
        }
    }
}

/// Directory of this DLL, resolved from our own code address.
fn module_dir() -> Option<std::path::PathBuf> {
    const FROM_ADDRESS: u32 = windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
        | windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    let mut handle = windows::Win32::Foundation::HMODULE::default();
    unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleExW(
            FROM_ADDRESS,
            windows::core::PCWSTR(module_dir as *const u16),
            &mut handle,
        )
        .ok()?;
        let mut buffer = [0u16; 512];
        let len = windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(handle), &mut buffer);
        if len == 0 || (len as usize) >= buffer.len() {
            return None;
        }
        std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]))
            .parent()
            .map(std::path::Path::to_path_buf)
    }
}

/// Unused today, kept for the secondary-taskbar path: the pid of the process
/// owning a window, for debugging mixed-explorer setups.
pub fn pid_of(window: isize) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(window as *mut _), Some(&mut pid));
    }
    pid
}
