//! The toast's window and message pump.
//!
//! The shape is the widget bar's: a dedicated thread owns one window, the
//! window's state lives in a thread local that the window procedure can reach
//! without the `GWLP_USERDATA` dance, and the host talks to it by posting
//! messages to a handle published through an atomic. Nothing here blocks a
//! caller, and nothing here can panic across the public API.
//!
//! # Why a popup rather than a notification
//!
//! The shell's own toast API would put this in the Action Center and would
//! require a shortcut with an AppUserModelID; what is wanted is a two-second
//! confirmation for an action the user just took. `WS_EX_NOACTIVATE` and
//! `WS_EX_TOOLWINDOW` keep it out of the foreground and out of Alt-Tab,
//! `WS_EX_TRANSPARENT` makes it click-through so it can never eat a click aimed
//! at whatever is underneath it, and `WS_EX_LAYERED` is what gives it per-pixel
//! alpha.
//!
//! # Why `WS_EX_TOPMOST` is used here but was avoided on the widget bar
//!
//! The bar lives on the taskbar and is structurally kept above its owner; what
//! it must *not* do is sit in front of a full-screen overlay the user put up
//! deliberately. A toast is the opposite case: it reports something that just
//! happened, and a confirmation the user never sees because a browser window
//! came up over it is a confirmation that did not happen. Topmost *without*
//! `WS_EX_NOACTIVATE` would steal the foreground — the two together are what
//! make it visible without being intrusive.
//!
//! # Why the window outlives every toast
//!
//! The window and its pump are created on the first `show` and then kept,
//! hidden. A toast is shown in response to a hotkey, and spawning a thread,
//! registering a window class and creating a window inside that response is
//! latency a keyboard shortcut gets judged on. Only the frame buffers — the
//! size of the panel, which follows the text — are released when a toast ends.
//!
//! # The fade, and why it is not `SetLayeredWindowAttributes`
//!
//! The obvious fade is `SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA)`
//! on a timer. It does not work here, and it fails *silently*: a layered window
//! is in one of two modes, and `UpdateLayeredWindow` — which is how this window
//! gets its per-pixel alpha, and how the widget bar does too — puts it in the
//! mode where `SetLayeredWindowAttributes` returns `FALSE` and changes nothing.
//! A toast built on that would simply never fade, look correct in a code
//! review, and be impossible to explain from the outside.
//!
//! So the fade scales the frame's own premultiplied pixels and presents it
//! again. Multiplying all four channels by the same factor is exactly what
//! fading a premultiplied image means — scaling only the alpha is what produces
//! the classic "bright halo around a vanishing window" artifact — and the step
//! that takes the alpha to zero hides the window rather than presenting one
//! last frame nothing would see.
//!
//! # Auto-hide
//!
//! A `SetTimer` on the toast's own window, because there is already a message
//! pump and a timer costs nothing but a `WM_TIMER`. Every route out of the
//! visible state funnels into [`Pump::hide_now`], which kills both timers and
//! hides the window, so "cannot be left stuck visible" is a property of the
//! control flow rather than of the timing.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetTimer, ShowWindow, TranslateMessage,
    MA_NOACTIVATE, MSG, SW_HIDE, SW_SHOWNA, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_MOUSEACTIVATE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use beautify_core::geometry::Rect;

use crate::layout::{self, Metrics, Panel};
use crate::paint::Painter;
use crate::surface::LayeredSurface;

/// Posted by [`crate::Toast::show`], with the panel's two lines boxed behind it.
pub const WM_APP_SHOW: u32 = WM_APP + 81;
/// Posted by [`crate::Toast::hide`].
pub const WM_APP_HIDE: u32 = WM_APP + 82;

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.Toast");

/// Title of the window. Never shown — it is a `WS_POPUP` tool window — but a
/// window with no name is a nuisance in a debugger and in Spy++.
const WINDOW_TITLE: PCWSTR = w!("WinBeautify 提示");

/// How long a toast stays up, before its fade begins.
pub const VISIBLE_MS: u32 = 2500;

/// The hide timer and the fade timer. Two ids because they run at different
/// rates, and because the fade is restarted on its own when a toast replaces
/// one that is already fading.
const TIMER_HIDE: usize = 1;
const TIMER_FADE: usize = 2;

/// Fade tick and step, in alpha levels.
///
/// Fade tick and step, in alpha levels.
///
/// 17 divides 255 exactly, so fifteen ticks bring the alpha to zero and the
/// window is hidden on that fifteenth tick: the ramp costs fourteen faded
/// presents and no wasted final frame that nothing would see. At 16 ms a tick
/// the fade is 240 ms — short enough not to feel like the toast is being taken
/// away slowly, long enough that it does not blink out.
const FADE_STEP_MS: u32 = 16;
const FADE_ALPHA_STEP: u8 = 17;

/// The alpha a shown toast sits at.
const OPAQUE: u8 = 255;

/// The two strings of a `WM_APP_SHOW`, boxed together.
///
/// `PostMessageW` has room for two machine words and a toast needs two strings,
/// so the pair travels as one `Box`. Exactly one of [`take_lines`] — in the
/// window procedure, or in [`post`] when the message could not be queued at all
/// — reclaims it, which is what keeps a toast from leaking two `String`s per
/// switch.
fn boxed_lines(title: &str, detail: &str) -> isize {
    Box::into_raw(Box::new((title.to_string(), detail.to_string()))) as isize
}

/// Reclaim the boxed pair from a `WM_APP_SHOW`.
///
/// A zero word — a `show` that was posted with nothing to say, or a message
/// from something else entirely — is `None` rather than a null dereference.
fn take_lines(raw: isize) -> Option<(String, String)> {
    if raw == 0 {
        return None;
    }
    // SAFETY: the only producer of a non-zero `WM_APP_SHOW` payload is
    // `boxed_lines`, which hands over ownership of a live `Box<(String,
    // String)>`; this is the only consumer, and it consumes it once.
    Some(*unsafe { Box::from_raw(raw as *mut (String, String)) })
}

/// Queue a toast on the pump thread's message loop.
fn post(hwnd: HWND, title: &str, detail: &str) {
    let payload = boxed_lines(title, detail);
    let posted =
        unsafe { PostMessageW(Some(hwnd), WM_APP_SHOW, WPARAM(payload as usize), LPARAM(0)) };
    if posted.is_err() {
        // The message was never queued, so nothing will ever free the box.
        // Reclaiming it here is the difference between a no-op and a leak that
        // grows with every switch.
        drop(take_lines(payload));
        tracing::warn!("could not post the toast to its window");
    }
}

/// A toast asked for before the window existed, waiting to be delivered.
///
/// The pump thread is started on the first `show`, and starting a thread does
/// not wait for it: the caller that woke it up has no window to post to yet.
/// Without somewhere to leave the lines, *the first toast of every run would be
/// silently dropped* — and it would be the very first one a user ever sees,
/// which is the worst possible one to lose.
///
/// So the first caller leaves its lines here and the pump takes them as soon as
/// it has a handle. Both sides use [`take_pending`], which removes rather than
/// reads, so if the caller comes back to find the window has appeared in the
/// meantime exactly one of the two delivers the toast.
type Pending = Mutex<Option<(String, String)>>;

/// Leave `title`/`detail` for the pump to deliver.
fn put_pending(pending: &Pending, title: &str, detail: &str) {
    let lines = Some((title.to_string(), detail.to_string()));
    // A poisoned lock cannot happen — nothing panics while holding this — but
    // recovering rather than unwrapping keeps that from being load-bearing: a
    // stranded toast is a worse outcome than a mutex whose guard was dropped.
    match pending.lock() {
        Ok(mut slot) => *slot = lines,
        Err(poisoned) => *poisoned.into_inner() = lines,
    }
}

/// Claim whatever is waiting, if anything.
fn take_pending(pending: &Pending) -> Option<(String, String)> {
    match pending.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

/// Handle to the toast's window, shared with the pump thread.
///
/// Not `Clone`: the app keeps one of these in shared state and calls it from
/// whatever thread the action landed on, so making it cloneable would invite a
/// second handle whose lifetime nobody had thought about.
pub struct Window {
    hwnd: Arc<AtomicIsize>,
    /// Set once the pump has been started, so a burst of `show` calls cannot
    /// start a second one.
    started: AtomicBool,
    /// The hand-off slot for a toast asked for before there is a window to post
    /// it to. Empty for the whole life of the process after the first `show`.
    pending: Arc<Pending>,
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

impl Window {
    pub fn new() -> Self {
        Self {
            hwnd: Arc::new(AtomicIsize::new(0)),
            started: AtomicBool::new(false),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// Show the panel, starting the pump thread on first use.
    ///
    /// Returns immediately. A toast that cannot be shown at all — no thread, no
    /// Direct2D — is a logged no-op rather than an error, because it confirms an
    /// action that has already happened.
    pub fn show(&self, title: &str, detail: &str) {
        if title.is_empty() && detail.is_empty() {
            return;
        }
        if let Some(hwnd) = self.handle() {
            // The steady state, and the whole life of the process after the
            // first toast: one `PostMessage` and no lock at all.
            post(hwnd, title, detail);
            return;
        }

        put_pending(&self.pending, title, detail);
        self.start();
        // The window may have come up between the check above and the store, in
        // which case the pump has already looked in the slot and found nothing.
        // Taking it back here closes that window; if the pump got there first
        // this is `None` and the pump is drawing the toast already.
        if let Some(hwnd) = self.handle() {
            if let Some((title, detail)) = take_pending(&self.pending) {
                post(hwnd, &title, &detail);
            }
        }
    }

    /// Hide the panel immediately.
    pub fn hide(&self) {
        let Some(hwnd) = self.handle() else {
            return;
        };
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_HIDE, WPARAM(0), LPARAM(0));
        }
    }

    fn handle(&self) -> Option<HWND> {
        let raw = self.hwnd.load(Ordering::Acquire);
        (raw != 0).then_some(HWND(raw as *mut core::ffi::c_void))
    }

    /// Start the pump thread if it is not already running.
    ///
    /// The thread is never joined: it lives as long as the process and its
    /// window is hidden between toasts. A `Toast` dropped while a toast is on
    /// screen leaves the pump running with nothing to show, which is the honest
    /// outcome — the window belongs to the process, not to the handle that
    /// happened to create it.
    fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let slot = Arc::clone(&self.hwnd);
        let pending = Arc::clone(&self.pending);
        let result = std::thread::Builder::new()
            .name("wb-toast".into())
            .spawn(move || {
                if let Err(e) = run(slot, pending) {
                    tracing::error!("toast window exited: {e}");
                }
            });
        if let Err(e) = result {
            // Reset so a later `show` may try again, rather than the toast
            // being permanently silent about a transient spawn failure.
            self.started.store(false, Ordering::Release);
            tracing::error!("could not start the toast thread: {e}");
        }
    }
}

/// Everything the pump thread owns.
struct Pump {
    hwnd: HWND,
    painter: Option<Painter>,
    surface: Option<LayeredSurface>,
    /// Screen position of the window, so a fade tick can present without
    /// recomputing the layout and a hide does not have to know it.
    at: (i32, i32),
    /// The frame as it was drawn, premultiplied BGRA, `stride` bytes per row.
    ///
    /// Kept so the fade can present it again at a lower alpha; see the module
    /// docs for why the fade cannot be a window attribute.
    frame: Vec<u8>,
    /// The faded copy handed to `present`. A member rather than a local so a
    /// fifteen-tick fade allocates once instead of fifteen times.
    faded: Vec<u8>,
    stride: usize,
    /// True while the window is on screen.
    visible: bool,
    /// Fade level: 255 is fully visible, 0 is the last step before hiding.
    alpha: u8,
}

impl Pump {
    /// Draw a toast and put it on screen, replacing whatever is showing and
    /// restarting the timers.
    fn show(&mut self, title: &str, detail: &str) {
        let Some(work) = primary_work_area() else {
            tracing::warn!("no primary monitor; the toast has nowhere to go");
            return;
        };
        // The window's own DPI, which for a window sitting on the primary
        // monitor is the primary monitor's DPI.
        let metrics = Metrics::new(unsafe { GetDpiForWindow(self.hwnd) });

        // Three separate borrow phases, in the only order that works: measure
        // through a shared borrow, resize the surface through `&mut self`, and
        // only then hold the painter and the surface together to draw. Merging
        // the first two would be a second mutable borrow of `self`.
        let measured = match self.painter.as_ref() {
            Some(painter) => painter.measure(title, detail, &metrics),
            None => {
                tracing::warn!("Direct2D is unavailable; the toast cannot be drawn");
                return;
            }
        };
        let Ok((panel, title, detail)) = measured else {
            tracing::warn!("could not measure the toast");
            return;
        };
        if !self.ensure_surface(&panel) {
            return;
        }
        let (x, y) = layout::origin(work, &panel, &metrics);
        self.at = (x, y);

        // The frame is copied out of the render target inside the render, so
        // the fade has the pixels without a second render.
        let mut frame = std::mem::take(&mut self.frame);
        let mut stride = 0usize;
        let (Some(painter), Some(surface)) = (self.painter.as_mut(), self.surface.as_mut()) else {
            // Both were just checked; there is nothing to draw on.
            self.frame = frame;
            return;
        };
        let presented = painter.render(&panel, &title, &detail, &metrics, |pixels, row_stride| {
            frame.clear();
            frame.extend_from_slice(pixels);
            stride = row_stride;
            surface.present(pixels, row_stride, x, y)
        });
        self.frame = frame;
        if let Err(e) = presented {
            tracing::warn!("toast frame failed: {e}");
            self.hide_now();
            return;
        }
        self.stride = stride;
        self.faded.clear();

        self.visible = true;
        self.alpha = OPAQUE;
        // `UpdateLayeredWindow` paints but never shows: without this the window
        // is positioned, has its pixels, and is invisible. `SW_SHOWNA` keeps it
        // from taking the foreground.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
        // Both timers restart, so a second toast gets its full life and the
        // fade of the one it replaced cannot take it down early.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_FADE);
            let _ = SetTimer(Some(self.hwnd), TIMER_HIDE, VISIBLE_MS, None);
        }
        tracing::debug!(title = %title, detail = %detail, x, y, "toast shown");
    }

    /// The visible time is up: start the fade, or hide outright.
    fn begin_fade(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_HIDE);
        }
        if !self.visible {
            return;
        }
        // The first step is taken now rather than a tick later, so the time the
        // toast is actually up matches `VISIBLE_MS` plus the fade.
        self.step_fade();
        if self.visible {
            unsafe {
                let _ = SetTimer(Some(self.hwnd), TIMER_FADE, FADE_STEP_MS, None);
            }
        }
    }

    /// One step of the fade.
    fn fade_tick(&mut self) {
        if !self.visible {
            // The window was taken down by something other than the ramp — a
            // `hide`, or a new toast replacing this one — so the ramp stops
            // here instead of presenting onto a window that is not showing.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_FADE);
            }
            return;
        }
        self.step_fade();
    }

    /// Lower the alpha by one step and present the result.
    fn step_fade(&mut self) {
        self.alpha = self.alpha.saturating_sub(FADE_ALPHA_STEP);
        if self.alpha == 0 {
            self.hide_now();
            return;
        }
        self.represent_faded();
    }

    /// Present the stored frame at the current fade level.
    ///
    /// Every channel is scaled, not just alpha. The buffer is *premultiplied*,
    /// so the colour channels are already multiplied by the alpha; scaling only
    /// the alpha would leave the colour too bright for it, which is the bright
    /// halo that a fading layered window shows when this is got wrong.
    fn represent_faded(&mut self) {
        let (Some(surface), false) = (self.surface.as_mut(), self.frame.is_empty()) else {
            return;
        };
        let scale = self.alpha as u16;
        self.faded.clear();
        self.faded.reserve(self.frame.len());
        self.faded.extend(
            self.frame
                .iter()
                .map(|channel| ((*channel as u16 * scale) / 255) as u8),
        );
        let (x, y) = self.at;
        if let Err(e) = surface.present(&self.faded, self.stride, x, y) {
            tracing::warn!("toast fade frame failed: {e}");
            // A frame that cannot be pushed is a window that would stay at
            // whatever alpha it last reached; taking it down is the safe end.
            self.hide_now();
        }
    }

    /// Take the window down now, whatever the fade was doing.
    ///
    /// Both timers are killed and the state is reset before the window is
    /// hidden, so the next toast cannot start from whatever the ramp happened
    /// to leave behind. This is the only place the window stops being visible,
    /// which is what makes "cannot be left stuck visible" a property of the
    /// control flow rather than of the timing.
    fn hide_now(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_HIDE);
            let _ = KillTimer(Some(self.hwnd), TIMER_FADE);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
        self.alpha = OPAQUE;
        // The frame is no larger than the panel, but it is the one allocation
        // here that scales with the text; a toast spends almost all of its life
        // hidden, so it does not get to keep it.
        self.frame = Vec::new();
        self.faded = Vec::new();
        self.stride = 0;
    }

    /// Make sure the layered surface is the panel's size.
    ///
    /// A resize invalidates the stored frame too: its stride was the old
    /// surface's, and a fade presenting it at the new size would shear the
    /// image rather than fade it.
    fn ensure_surface(&mut self, panel: &Panel) -> bool {
        let size = (panel.width as i32, panel.height as i32);
        if let Some(surface) = self.surface.as_ref() {
            if surface.width() == size.0 && surface.height() == size.1 {
                return true;
            }
        }
        match unsafe { LayeredSurface::new(self.hwnd, size.0, size.1) } {
            Ok(surface) => {
                self.surface = Some(surface);
                self.frame = Vec::new();
                self.faded = Vec::new();
                self.stride = 0;
                true
            }
            Err(e) => {
                tracing::warn!("could not create the toast surface: {e}");
                self.surface = None;
                false
            }
        }
    }
}

/// The primary monitor's work area.
///
/// `MONITOR_DEFAULTTOPRIMARY` rather than "the monitor the window is on": the
/// window is a hidden one-pixel popup and can be reported anywhere, and what is
/// wanted is the *primary* monitor's top-left corner, not whichever screen the
/// pointer last crossed.
fn primary_work_area() -> Option<Rect> {
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let work = info.rcWork;
    Some(Rect::new(work.left, work.top, work.right, work.bottom))
}

/// Run the pump until the process ends. Blocks the calling thread.
pub fn run(
    slot: Arc<AtomicIsize>,
    pending: Arc<Pending>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        // No cursor: the toast is click-through, so `WM_SETCURSOR` never
        // arrives for it and a class cursor would be one nobody can see.
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    // A second registration in the same process fails benignly.
    unsafe { RegisterClassExW(&class) };

    let style =
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(style.0),
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;

    let painter = match Painter::new() {
        Ok(painter) => Some(painter),
        Err(e) => {
            // The window is kept either way: `show` becomes a logged no-op
            // rather than a panic, which is what the API promises. The window
            // is never shown, so a toast that cannot draw costs nothing.
            tracing::error!("Direct2D is unavailable; the toast cannot be drawn: {e}");
            None
        }
    };
    PUMP.with(|cell| {
        *cell.borrow_mut() = Some(Pump {
            hwnd,
            painter,
            surface: None,
            at: (0, 0),
            frame: Vec::new(),
            faded: Vec::new(),
            stride: 0,
            visible: false,
            alpha: OPAQUE,
        })
    });
    slot.store(hwnd.0 as isize, Ordering::Release);

    // Deliver a toast that was asked for before this thread had a window to
    // post to — the first `show` of the process. Doing it here rather than
    // waiting for the caller to notice the handle means the first toast of a
    // run is not the one that goes missing.
    if let Some((title, detail)) = take_pending(&pending) {
        with_pump(|pump| pump.show(&title, &detail));
    }

    let mut message = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }
    }

    // Device resources go before the window does.
    PUMP.with(|cell| {
        if let Some(pump) = cell.borrow_mut().as_mut() {
            pump.surface = None;
            pump.painter = None;
        }
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    slot.store(0, Ordering::Release);
    PUMP.with(|cell| *cell.borrow_mut() = None);
    tracing::debug!("toast pump stopped");
    Ok(())
}

thread_local! {
    static PUMP: std::cell::RefCell<Option<Pump>> = const { std::cell::RefCell::new(None) };
}

/// Borrow the thread-local pump.
///
/// A re-entrant message — Windows sends several synchronously from inside a
/// handler like `ShowWindow` — falls through to `DefWindowProc` instead of
/// panicking: a panic inside a window procedure cannot unwind and would abort
/// the whole process.
fn with_pump<R>(f: impl FnOnce(&mut Pump) -> R) -> Option<R> {
    PUMP.with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        let pump = slot.as_mut()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(pump))) {
            Ok(result) => Some(result),
            Err(_) => {
                tracing::error!(
                    "the toast hit a bug while handling a message; that message was skipped"
                );
                None
            }
        }
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_SHOW => {
            // The box is freed exactly once on every path: a toast posted from a
            // hotkey handler would otherwise leak two `String`s per press.
            if let Some((title, detail)) = take_lines(wparam.0 as isize) {
                with_pump(|pump| pump.show(&title, &detail));
            }
            LRESULT(0)
        }
        WM_APP_HIDE => {
            with_pump(|pump| pump.hide_now());
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_HIDE => {
                    with_pump(|pump| pump.begin_fade());
                }
                TIMER_FADE => {
                    with_pump(|pump| pump.fade_tick());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => {
            // Belt and braces with `WS_EX_NOACTIVATE`: the default handler
            // answers `MA_ACTIVATE`, Windows then finds the window cannot be
            // activated, and the click that triggered it is discarded.
            // `MA_NOACTIVATE` keeps the click and still refuses the focus.
            LRESULT(MA_NOACTIVATE as isize)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boxed_pair_round_trips_through_the_message_payload() {
        let raw = boxed_lines("已切换到", "扬声器 (Realtek)");
        assert_eq!(
            take_lines(raw),
            Some(("已切换到".to_string(), "扬声器 (Realtek)".to_string()))
        );
    }

    #[test]
    fn an_empty_pair_round_trips_too() {
        // `show` refuses empty input, but the payload must not be mistaken for
        // a null one if a caller ever posts it directly.
        assert_eq!(
            take_lines(boxed_lines("", "")),
            Some((String::new(), String::new()))
        );
    }

    #[test]
    fn a_null_payload_is_not_dereferenced() {
        assert_eq!(take_lines(0), None);
    }

    #[test]
    fn the_first_toast_of_a_run_is_handed_over_rather_than_dropped() {
        // The bug this slot exists for: `show` cannot post to a window that a
        // thread it just spawned has not created yet, so without somewhere to
        // leave the lines the first toast of every run vanishes. The pump takes
        // them the moment it has a handle.
        let pending: Pending = Mutex::new(None);
        assert_eq!(take_pending(&pending), None, "nothing waiting yet");

        put_pending(&pending, "已切换到", "扬声器");
        assert_eq!(
            take_pending(&pending),
            Some(("已切换到".to_string(), "扬声器".to_string())),
            "the pump must find what the first caller left"
        );
    }

    #[test]
    fn the_hand_over_slot_delivers_exactly_once() {
        // Both sides race to claim the slot, and `take` is what makes the
        // outcome "exactly one of them" rather than "both" or "neither": a
        // toast drawn twice, or not at all, are both worse than a race.
        let pending: Pending = Mutex::new(None);
        put_pending(&pending, "标题", "详情");
        let first = take_pending(&pending);
        let second = take_pending(&pending);
        assert!(first.is_some(), "one side claims it");
        assert_eq!(second, None, "and the other finds nothing to claim");
    }

    #[test]
    fn a_later_hand_over_replaces_an_undelivered_one() {
        // Toast semantics are "replace", not "queue": a second switch arriving
        // before the window exists must not draw the first device's name.
        let pending: Pending = Mutex::new(None);
        put_pending(&pending, "第一次", "扬声器");
        put_pending(&pending, "第二次", "耳机");
        assert_eq!(
            take_pending(&pending),
            Some(("第二次".to_string(), "耳机".to_string()))
        );
    }

    #[test]
    fn a_poisoned_hand_over_slot_still_delivers() {
        // Nothing panics while holding the lock, so this cannot happen — but a
        // stranded toast is a worse failure than a recovered mutex, and the
        // recovery has to actually work rather than panic in turn.
        let pending: Pending = Mutex::new(None);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = pending.lock().expect("fresh lock");
            panic!("poison the lock");
        }));
        // `put` must survive the poisoned lock...
        put_pending(&pending, "标题", "详情");
        assert_eq!(
            take_pending(&pending),
            Some(("标题".to_string(), "详情".to_string()))
        );
    }

    #[test]
    fn the_fade_reaches_the_end_in_a_bounded_number_of_ticks() {
        // The ramp has to finish within a sensible budget, or the hide timer is
        // what actually takes the window down and the fade is decoration that
        // never completes.
        let ticks = OPAQUE.div_ceil(FADE_ALPHA_STEP) as u32;
        let duration = ticks * FADE_STEP_MS;
        assert!(
            (100..=400).contains(&duration),
            "a fade of {duration} ms is either invisible or dawdling"
        );
    }

    #[test]
    fn the_alpha_never_wraps_on_its_way_down() {
        // `saturating_sub` is what stops a step past zero from becoming 255 and
        // flashing the window back to fully opaque.
        assert_eq!(0u8.saturating_sub(FADE_ALPHA_STEP), 0);
        assert_eq!(FADE_ALPHA_STEP.saturating_sub(FADE_ALPHA_STEP), 0);
    }

    /// The premultiplied fade, as arithmetic rather than as a window.
    ///
    /// This is the one calculation in the module that can be wrong in a way
    /// nobody would notice from the outside — a fade that scales only alpha
    /// produces a visible halo — so it is checked directly.
    fn faded(frame: &[u8], alpha: u8) -> Vec<u8> {
        frame
            .iter()
            .map(|channel| ((*channel as u16 * alpha as u16) / 255) as u8)
            .collect()
    }

    #[test]
    fn a_full_alpha_step_leaves_the_frame_alone() {
        let frame = [10u8, 20, 30, 40, 200, 100, 50, 255];
        assert_eq!(faded(&frame, OPAQUE), frame.to_vec());
    }

    #[test]
    fn fading_scales_every_channel_not_only_alpha() {
        // Premultiplied means colour is already multiplied by alpha, so all four
        // channels scale together and the colour-to-alpha ratio is preserved.
        let frame = [64u8, 64, 64, 128];
        assert_eq!(faded(&frame, 128), vec![32, 32, 32, 64]);
    }

    #[test]
    fn the_colour_to_alpha_ratio_is_preserved_all_the_way_down() {
        // Premultiplication means each colour channel stands in a fixed
        // proportion to alpha, and a fade that broke that is exactly what shows
        // as a halo. The tolerance is the integer-quantization bound: at most
        // one level of error per channel, which relative to alpha is `1/alpha`.
        let frame = [100u8, 50, 25, 200];
        for alpha in (0..=255u8).step_by(17) {
            let out = faded(&frame, alpha);
            let visible = out[3];
            if visible == 0 {
                continue;
            }
            let tolerance = 1.0 / visible as f32 + 0.01;
            for (index, channel) in out[..3].iter().enumerate() {
                let faded_ratio = *channel as f32 / visible as f32;
                let original_ratio = frame[index] as f32 / frame[3] as f32;
                assert!(
                    (faded_ratio - original_ratio).abs() <= tolerance,
                    "at alpha {alpha} channel {index} drifted: {out:?}"
                );
            }
        }
    }

    #[test]
    fn the_faded_frame_never_has_colour_stronger_than_its_alpha() {
        // The premultiplied invariant itself, checked directly: a colour
        // channel above the alpha is precisely the over-bright edge that a
        // wrong fade produces.
        let frame = [200u8, 190, 180, 200, 255, 255, 255, 255];
        for alpha in (0..=255u8).step_by(17) {
            let out = faded(&frame, alpha);
            for pixel in out.chunks_exact(4) {
                for channel in &pixel[..3] {
                    assert!(
                        *channel <= pixel[3],
                        "at alpha {alpha} the pixel {pixel:?} is over-bright"
                    );
                }
            }
        }
    }

    #[test]
    fn a_transparent_frame_stays_transparent() {
        assert_eq!(faded(&[0, 0, 0, 0], 128), vec![0, 0, 0, 0]);
    }

    #[test]
    fn a_toast_stays_up_long_enough_to_read_and_not_much_longer() {
        assert!(
            (2000..=4000).contains(&VISIBLE_MS),
            "a toast that cannot be read is not a toast, and one that outstays the action is in the way"
        );
    }

    #[test]
    fn the_window_class_name_is_namespaced() {
        let expected: Vec<u16> = "WinBeautify.Toast\0".encode_utf16().collect();
        let actual = unsafe { std::slice::from_raw_parts(WINDOW_CLASS.as_ptr(), expected.len()) };
        assert_eq!(actual, expected.as_slice());
    }
}
