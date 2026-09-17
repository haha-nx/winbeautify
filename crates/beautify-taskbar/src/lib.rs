//! Taskbar transparency module.
//!
//! # Why a dedicated thread
//!
//! Both `SetWinEventHook` callbacks and the `TaskbarCreated` broadcast require a
//! thread that owns a window and pumps messages. Rather than impose that on the
//! host, the module spawns one thread with a hidden top-level window and a
//! classic `GetMessage` loop, and exposes a lock-free "please re-apply" poke to
//! the outside world.
//!
//! # Why the debounce
//!
//! `EVENT_OBJECT_LOCATIONCHANGE` fires for every window drag, so reacting
//! inline would mean thousands of `EnumWindows` sweeps per second. Instead the
//! hook only sets a dirty flag and arms a short one-shot timer; the expensive
//! recomputation happens once the user stops moving things.

pub mod accent;
pub mod ffi;
pub mod shell;
pub mod winver;

use accent::{AccentApplicator, Backdrop};
use beautify_core::config::{Config, TaskbarMode};
use beautify_core::event::Event;
use beautify_core::geometry::Rect;
use beautify_core::model::{TaskbarState, TaskbarVisualState};
use beautify_core::module::{Module, ModuleContext, ModuleResult};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetTimer, KillTimer, TranslateMessage,
    HMENU, MSG, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_NCCREATE, WM_TIMER,
    WNDCLASSEXW, WS_POPUP, GWLP_USERDATA, SetWindowLongPtrW,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, OBJID_WINDOW, CHILDID_SELF, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Posted to the pump window when the config changed and the backdrop should be
/// re-evaluated right away.
const WM_APP_REAPPLY: u32 = WM_APP + 1;
/// Posted to tear the pump down.
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;
/// One-shot debounce timer.
const TIMER_DEBOUNCE: usize = 1;
/// Safety-net timer that catches shell changes we have no hook for.
const TIMER_SAFETY: usize = 2;
const DEBOUNCE_MS: u32 = 120;
const SAFETY_MS: u32 = 3000;

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.TaskbarHost");

/// Configuration the pump thread reads. Written by [`TaskbarModule::apply`] on
/// the host thread, read on the pump thread.
#[derive(Debug, Clone, PartialEq)]
struct Desired {
    enabled: bool,
    mode: TaskbarMode,
    color: beautify_core::geometry::Color,
    opacity: f32,
    apply_secondary: bool,
    dynamic_mode: bool,
    dynamic_override: TaskbarMode,
    hide_on_fullscreen: bool,
    restore_on_exit: bool,
}

impl Default for Desired {
    fn default() -> Self {
        let d = beautify_core::config::TaskbarConfig::default();
        Self {
            enabled: false,
            mode: d.mode,
            color: d.color,
            opacity: d.opacity,
            apply_secondary: d.apply_to_secondary,
            dynamic_mode: d.dynamic_mode,
            dynamic_override: d.dynamic_mode_override,
            hide_on_fullscreen: d.hide_on_fullscreen,
            restore_on_exit: d.restore_on_exit,
        }
    }
}

impl Desired {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.taskbar.enabled,
            mode: cfg.taskbar.mode,
            color: cfg.taskbar.color,
            opacity: cfg.taskbar.opacity,
            apply_secondary: cfg.taskbar.apply_to_secondary,
            dynamic_mode: cfg.taskbar.dynamic_mode,
            dynamic_override: cfg.taskbar.dynamic_mode_override,
            hide_on_fullscreen: cfg.taskbar.hide_on_fullscreen,
            restore_on_exit: cfg.taskbar.restore_on_exit,
        }
    }
}

/// State shared between the host thread and the pump thread.
struct Shared {
    desired: RwLock<Desired>,
    bus: RwLock<Option<beautify_core::event::EventBus>>,
    pump_hwnd: AtomicIsize,
    pump_thread: AtomicU32,
    dark_theme: AtomicBool,
    /// Set to true once the thread has finished its teardown.
    stopped: AtomicBool,
}

impl Shared {
    fn post(&self, message: u32) {
        let raw = self.pump_hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut core::ffi::c_void);
            unsafe {
                let _ = PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// What the module decided for a given moment, kept so the safety timer can
/// skip work when nothing moved.
#[derive(Debug, Clone, PartialEq)]
struct Applied {
    backdrop: Backdrop,
    state: TaskbarVisualState,
    geometry: Option<Rect>,
    bars: usize,
}

/// Taskbar transparency module.
pub struct TaskbarModule {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for TaskbarModule {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskbarModule {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                desired: RwLock::new(Desired::default()),
                bus: RwLock::new(None),
                pump_hwnd: AtomicIsize::new(0),
                pump_thread: AtomicU32::new(0),
                dark_theme: AtomicBool::new(true),
                stopped: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Current taskbar rectangle, for the widget bar host to position against.
    pub fn taskbar_rect() -> Option<Rect> {
        shell::primary_taskbar().and_then(shell::window_rect)
    }

    /// Set by the UI when Windows switches between light and dark.
    pub fn set_dark_theme(&self, dark: bool) {
        self.shared.dark_theme.store(dark, Ordering::Relaxed);
        self.shared.post(WM_APP_REAPPLY);
    }

    /// True while the pump thread is up and the accent is being managed.
    pub fn is_running(&self) -> bool {
        self.shared.pump_hwnd.load(Ordering::Acquire) != 0
    }
}

impl Module for TaskbarModule {
    fn name(&self) -> &'static str {
        "taskbar"
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.taskbar.enabled
    }

    fn start(&self, ctx: ModuleContext) -> ModuleResult {
        let mut guard = self.thread.lock();
        if guard.is_some() {
            return Ok(());
        }
        *self.shared.bus.write() = Some(ctx.bus.clone());
        *self.shared.desired.write() = Desired::from_config(&ctx.config.get());
        self.shared.stopped.store(false, Ordering::Release);

        let shared = Arc::clone(&self.shared);
        let handle = std::thread::Builder::new()
            .name("wb-taskbar".into())
            .spawn(move || {
                if let Err(e) = run_pump(shared) {
                    tracing::error!("taskbar pump thread exited: {e}");
                }
            })?;
        *guard = Some(handle);
        Ok(())
    }

    fn apply(&self, config: &Config) -> ModuleResult {
        // The registry only calls `apply` while the module is enabled, so the
        // pump is guaranteed to be up here.
        let next = Desired::from_config(config);
        let mut guard = self.shared.desired.write();
        let changed = *guard != next;
        *guard = next;
        drop(guard);
        if changed {
            self.shared.post(WM_APP_REAPPLY);
        }
        Ok(())
    }

    fn stop(&self) -> ModuleResult {
        // Take the handle out before joining: the pump thread never touches
        // `thread`, but holding this lock across a join would block a
        // concurrent start.
        let Some(handle) = self.thread.lock().take() else {
            return Ok(());
        };
        self.shared.post(WM_APP_SHUTDOWN);
        let _ = handle.join();
        Ok(())
    }
}

impl Drop for TaskbarModule {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// Thread-local handle to the shared state, so `wndproc` can reach it without
// the `GWLP_USERDATA` dance. The window lives on this thread for its whole
// life, so there is exactly one value.
thread_local! {
    static PUMP: std::cell::RefCell<Option<Arc<Shared>>> =
        const { std::cell::RefCell::new(None) };
}

fn run_pump(shared: Arc<Shared>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        // The taskbar is a per-monitor-DPI window; without this the rect we read
        // back would be virtualised on mixed-DPI setups.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // WinEvent hooks need an initialized apartment on the pumping thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    // A re-registration after a restart is harmless; ignore the error.
    unsafe { RegisterClassExW(&class) };

    PUMP.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&shared)));

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            w!("WinBeautify"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            Option::<HMENU>::None,
            Some(instance.into()),
            None,
        )
    }?;

    shared
        .pump_hwnd
        .store(hwnd.0 as isize, Ordering::Release);
    shared
        .pump_thread
        .store(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }, Ordering::Release);

    let mut hooks: Vec<HWINEVENTHOOK> = Vec::with_capacity(2);
    // Foreground changes drive the fullscreen and dynamic-mode checks.
    let foreground = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZEEND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if !foreground.is_invalid() {
        hooks.push(foreground);
    }
    // Window moves/resizes drive "did something get maximised?".
    let location = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if !location.is_invalid() {
        hooks.push(location);
    }
    let _ = EVENT_SYSTEM_MINIMIZESTART;

    unsafe {
        SetTimer(Some(hwnd), TIMER_SAFETY, SAFETY_MS, None);
    }

    tracing::debug!("taskbar pump ready (hwnd={:?})", hwnd.0);
    // Evaluate once at startup so the accent is on screen immediately.
    evaluate_and_apply(&shared, hwnd);

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    for hook in hooks {
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    }
    // Hand the taskbar back to Windows unless the user explicitly asked to keep
    // the effect applied after we exit.
    if shared.desired.read().restore_on_exit {
        APPLICATOR.with(|slot| slot.borrow_mut().reset_all());
    }
    unsafe {
        KillTimer(Some(hwnd), TIMER_DEBOUNCE).ok();
        KillTimer(Some(hwnd), TIMER_SAFETY).ok();
        let _ = DestroyWindow(hwnd);
    }
    // Drop the cached state: a restart must re-evaluate from scratch.
    LAST.with(|slot| *slot.borrow_mut() = None);
    shared.pump_hwnd.store(0, Ordering::Release);
    shared.stopped.store(true, Ordering::Release);
    PUMP.with(|slot| *slot.borrow_mut() = None);
    tracing::debug!("taskbar pump stopped");
    Ok(())
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let _ = unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1isize)
            };
            LRESULT(1)
        }
        WM_TIMER => {
            let id = wparam.0;
            let shared = PUMP.with(|s| s.borrow().clone());
            if let Some(shared) = shared {
                match id {
                    TIMER_DEBOUNCE => {
                        unsafe { KillTimer(Some(hwnd), TIMER_DEBOUNCE).ok() };
                        evaluate_and_apply(&shared, hwnd);
                    }
                    TIMER_SAFETY => evaluate_and_apply(&shared, hwnd),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_REAPPLY => {
            // Force a full re-evaluation by dropping the cached state.
            if let Some(shared) = PUMP.with(|s| s.borrow().clone()) {
                LAST.with(|slot| *slot.borrow_mut() = None);
                evaluate_and_apply(&shared, hwnd);
            }
            LRESULT(0)
        }
        WM_APP_SHUTDOWN | WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Window-level events only: this drops the torrent of cursor and child-object
    // notifications that would otherwise wake us on every mouse move.
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    let Some(shared) = PUMP.with(|s| s.borrow().clone()) else {
        return;
    };

    // A foreground change is decisive enough to act on immediately; a mere move
    // only arms the debounce.
    let immediate = event == EVENT_SYSTEM_FOREGROUND
        || event == EVENT_SYSTEM_MINIMIZESTART
        || event == EVENT_SYSTEM_MINIMIZEEND;

    let pump = HWND(shared.pump_hwnd.load(Ordering::Acquire) as *mut core::ffi::c_void);
    if immediate {
        unsafe { KillTimer(Some(pump), TIMER_DEBOUNCE).ok() };
        evaluate_and_apply(&shared, pump);
    } else {
        unsafe { SetTimer(Some(pump), TIMER_DEBOUNCE, DEBOUNCE_MS, None) };
    }
    let _ = hwnd;
}

thread_local! {
    // Last applied state, used to skip redundant DWM round-trips.
    static LAST: std::cell::RefCell<Option<Applied>> = const { std::cell::RefCell::new(None) };
    // Cached applicator so the "touched windows" list survives evaluations.
    static APPLICATOR: std::cell::RefCell<AccentApplicator> =
        std::cell::RefCell::new(AccentApplicator::new());
    // Whether DWM accepts `DWMWA_SYSTEMBACKDROP_TYPE` here.
    static MICA_OK: std::cell::RefCell<Option<bool>> = const { std::cell::RefCell::new(None) };
}

/// Recompute the desired visual state and push it to the taskbars.
fn evaluate_and_apply(shared: &Arc<Shared>, hwnd: HWND) {
    let desired = shared.desired.read().clone();
    let dark = shared.dark_theme.load(Ordering::Relaxed);

    let bars = shell::all_taskbars();
    let bars: Vec<HWND> = if desired.apply_secondary {
        bars
    } else {
        bars.into_iter().take(1).collect()
    };

    let primary = bars.first().copied();
    let geometry = primary.and_then(shell::window_rect);
    let autohide = shell::is_autohide_enabled();
    let monitor = primary.and_then(shell::monitor_rect_of).unwrap_or(Rect::new(0, 0, 0, 0));

    // Resolve the effective mode first: dynamic mode and the fullscreen guard
    // both override whatever the user picked.
    let (effective_mode, state, skip_apply) = if !desired.enabled {
        (TaskbarMode::Normal, TaskbarVisualState::Disabled, false)
    } else if desired.hide_on_fullscreen && !monitor.is_empty() && shell::is_fullscreen_foreground(monitor)
    {
        (TaskbarMode::Normal, TaskbarVisualState::Fullscreen, false)
    } else if autohide && geometry.map(|r| r.top >= monitor.bottom).unwrap_or(false) {
        // Retracted. Reapplying the accent now would be visible as a flash when
        // the taskbar slides back out, so leave the window alone — but still
        // report the geometry, because the widget bar needs to hide itself.
        (desired.mode, TaskbarVisualState::Hidden, true)
    } else if desired.dynamic_mode && !monitor.is_empty() && shell::any_maximized_on(monitor) {
        (desired.dynamic_override, TaskbarVisualState::Dynamic, false)
    } else {
        (desired.mode, TaskbarVisualState::Applied, false)
    };

    let backdrop = Backdrop::new(effective_mode, desired.color, desired.opacity);
    let applied = Applied {
        backdrop,
        state,
        geometry,
        bars: bars.len(),
    };

    let unchanged = LAST.with(|slot| slot.borrow().as_ref() == Some(&applied));
    if unchanged {
        return;
    }

    if !skip_apply {
        let mica_ok = MICA_OK.with(|cell| {
            let mut slot = cell.borrow_mut();
            *slot.get_or_insert_with(|| primary.map(AccentApplicator::probe_mica).unwrap_or(false))
        });

        APPLICATOR.with(|slot| {
            let mut applicator = slot.borrow_mut();
            applicator.prune();
            for bar in &bars {
                if effective_mode == TaskbarMode::Normal {
                    applicator.reset(*bar);
                } else {
                    applicator.apply(*bar, &backdrop, dark, mica_ok);
                }
            }
        });
    }

    LAST.with(|slot| *slot.borrow_mut() = Some(applied.clone()));

    if let Some(bus) = shared.bus.read().clone() {
        bus.publish(&Event::TaskbarChanged(Arc::new(TaskbarState {
            state,
            mode: effective_mode.id().to_string(),
            secondary_bars: bars.len().saturating_sub(1) as u32,
            rect: geometry,
            autohide,
            shell_managed: winver::taskbar_ignores_composition_requests(),
        })));
    }
    warn_if_ignored(effective_mode);
    let _ = hwnd;
    tracing::debug!(
        mode = effective_mode.id(),
        state = ?state,
        bars = bars.len(),
        dpi = primary.map(|h| unsafe { GetDpiForWindow(h) }).unwrap_or(0),
        "taskbar backdrop applied"
    );
}

/// Say once, in the log, that the shell is ignoring the request.
///
/// From Windows 11 22H2 the taskbar is a XAML surface inside `explorer.exe`, so
/// its background is not a window surface and every composition call below
/// succeeds while changing nothing. Without this the log reads as perfectly
/// healthy and the app looks broken rather than limited.
fn warn_if_ignored(mode: TaskbarMode) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    if mode == TaskbarMode::Normal || !winver::taskbar_ignores_composition_requests() {
        return;
    }
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        requested = mode.id(),
        build = winver::build_number().unwrap_or(0),
        "the shell draws the taskbar itself on this build, so the requested appearance is ignored (it needs the taskbar's XAML background overridden from inside explorer.exe)"
    );
}
