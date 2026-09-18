//! The full-screen region selector.
//!
//! A borderless window covering the whole virtual desktop, showing a darkened
//! copy of the screen. Dragging punches the original pixels back through, so the
//! selection reads as "the part that will be taken".
//!
//! # Why GDI
//!
//! The widget bar draws with Direct2D because it needs per-pixel alpha and text
//! shaping. This window needs neither: it blits two already-decoded DIBs and a
//! few rectangles. `StretchDIBits` does that with no device objects to create
//! and destroy per frame, and the composed frame can be pushed through
//! `WM_PAINT`'s own invalidation region — so a mouse move only touches the
//! strips that changed. Recomposing a 4K frame per mouse event would be 33 MB of
//! copying each time.
//!
//! # How a session ends
//!
//! The button coming up, Enter, Escape, or a right-click — and all of them
//! funnel into [`Session::finish`], which records the answer and destroys the
//! window. The message loop then returns and the caller, which still owns the
//! undimmed shot, crops it. Cropping *after* the window is gone is what
//! guarantees the overlay is never part of the picture it delivers.
//!
//! # Why Escape needs a keyboard hook
//!
//! The overlay is opened by a global hotkey, so the foreground belongs to
//! whatever application the user was in, and `SetForegroundWindow` is refused
//! for a process that is not already foreground. Keyboard messages follow the
//! *focus*, not the mouse, so without help Escape would go to that other
//! application and the session could only be ended with the mouse. A low-level
//! keyboard hook is installed for the life of the session instead, and swallows
//! exactly one key: Escape. It is removed as soon as the session ends, so the
//! system-wide input path is only ever affected while the overlay is up.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC,
    GetStockObject, IntersectClipRect, ReleaseDC, RestoreDC, SaveDC, SelectObject, SetBkMode,
    SetTextColor, BACKGROUND_MODE, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetWindow, GetWindowLongPtrW, LoadCursorW,
    PostMessageW, PostQuitMessage,
    RegisterClassExW, SetCursor, SetWindowLongPtrW, SetTimer, SetWindowPos,
    SetWindowsHookExW, ShowWindow, SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx,
    CS_DBLCLKS, GWLP_USERDATA, GW_HWNDPREV, HC_ACTION, HWND_TOPMOST, IDC_CROSS, KBDLLHOOKSTRUCT,
    MSG, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOREDRAW, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WM_TIMER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WH_KEYBOARD_LL, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR,
    WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::{self, Shot};
use crate::Options;

/// Only one selector at a time: a second window would dim the first one's dimmed
/// screen and the two would fight over the mouse capture.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The window the keyboard hook should cancel, or 0 when there is no session.
///
/// A hook procedure is a bare `extern "system"` function with no user data, so
/// the target has to live beside it.
static HOOK_TARGET: AtomicIsize = AtomicIsize::new(0);

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.SnipOverlay");

/// How often the mask re-asserts that it is on top.
///
/// Not only per message: the shell raises the taskbar above our windows on its
/// own, and if that happens while the user is not moving the mouse there is no
/// message to hang the check off — so the taskbar and the widget bar sit on top
/// of the mask, undimmed, looking like a patch of screen that was never part of
/// the picture. A quarter of a second keeps the whole screen consistently the
/// picture the user is choosing from.
const TIMER_ASSERT: usize = 1;
const ASSERT_MS: u32 = 250;

/// A rectangle dragged out on screen, already normalised.
///
/// Its own type rather than `RECT` so the arithmetic — which way the drag went,
/// whether it is big enough to be a selection rather than a stray click, where
/// the badge fits — is testable without a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Area {
    /// The rectangle between two drag points, whichever way round they are.
    fn from_drag(from: (i32, i32), to: (i32, i32)) -> Self {
        Self {
            left: from.0.min(to.0),
            top: from.1.min(to.1),
            right: from.0.max(to.0),
            bottom: from.1.max(to.1),
        }
    }

    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// A click with no drag is not a selection, and neither is a one-pixel
    /// sliver — both are what a cancelled attempt looks like.
    fn is_usable(&self) -> bool {
        self.width() >= 2 && self.height() >= 2
    }

    /// "No selection here", which `union`-style arithmetic can be handed.
    const EMPTY: Self = Self {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };

    #[cfg(test)]
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Does this cover no pixels at all?
    fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    /// Clamp into a `width` by `height` box, keeping it non-inverted.
    fn clamped(self, width: i32, height: i32) -> Self {
        let left = self.left.clamp(0, width);
        let top = self.top.clamp(0, height);
        Self {
            left,
            top,
            right: self.right.clamp(left, width),
            bottom: self.bottom.clamp(top, height),
        }
    }

    fn to_rect(self) -> RECT {
        RECT {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }

    /// Grow by `n` on every side, clamped to the box.
    fn inflated(self, n: i32, width: i32, height: i32) -> Self {
        Self {
            left: self.left - n,
            top: self.top - n,
            right: self.right + n,
            bottom: self.bottom + n,
        }
        .clamped(width, height)
    }
}

/// The strips where one selection differs from another.
///
/// Dragging a corner moves two of a selection's four edges; everything inside
/// both rectangles is the same bright screen as it was a moment ago, so nothing
/// there has to be composed again. Recomposing the whole rectangle instead is
/// what made the mask fall behind the pointer: a 1400×900 selection is two and a
/// half million pixels of blitting *per mouse event*, and the picture the user
/// is dragging over then lags their mouse by however long the backlog takes.
///
/// The symmetric difference of two rectangles is at most four strips — the two
/// vertical bands between the pairs of left and right edges, and the two
/// horizontal ones between the pairs of top and bottom — and each is as thin as
/// the pointer actually moved. `pad` grows every strip by the width of the
/// selection's own frame, which is drawn outside the selection and so has to be
/// repainted on both the old and the new position.
fn changed_strips(previous: Area, next: Area, pad: i32, width: i32, height: i32) -> Vec<Area> {
    // Nothing to compare against: the whole of the other one changed.
    if previous.is_empty() {
        return vec![next.inflated(pad, width, height)];
    }
    if next.is_empty() {
        return vec![previous.inflated(pad, width, height)];
    }

    let left = previous.left.min(next.left);
    let right = previous.right.max(next.right);
    let top = previous.top.min(next.top);
    let bottom = previous.bottom.max(next.bottom);
    let band = |l: i32, t: i32, r: i32, b: i32| Area {
        left: l,
        top: t,
        right: r,
        bottom: b,
    };
    [
        band(left, top, previous.left.max(next.left), bottom),
        band(previous.right.min(next.right), top, right, bottom),
        band(left, top, right, previous.top.max(next.top)),
        band(left, previous.bottom.min(next.bottom), right, bottom),
    ]
    .into_iter()
    // A strip of no thickness is a pair of edges that did not move: there is
    // nothing to compose, and growing it would only compose a band that never
    // changed.
    .filter(|strip| !strip.is_empty())
    .map(|strip| strip.inflated(pad, width, height))
    .filter(|strip| !strip.is_empty())
    .collect()
}

/// A memory DC with a top-down DIB selected into it.
///
/// Top-down to match how [`Shot`] stores its rows, which is what lets the
/// composers move a region from the shot to the frame with no flipping.
struct Frame {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    /// Base of the pixel buffer. Composition writes this memory directly —
    /// see `copy_rows` and `expand` for why this does not go through GDI.
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Frame {
    fn new(window: HDC, width: i32, height: i32) -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(Some(window)) };
        if dc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width.max(1),
                biHeight: -height.max(1),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = match unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                unsafe { let _ = DeleteDC(dc); };
                return None;
            }
        };
        let previous = unsafe { SelectObject(dc, bitmap.into()) };
        Some(Self {
            dc,
            bitmap,
            previous,
            bits: bits as *mut u8,
            width,
            height,
        })
    }

    /// The frame's pixel buffer as a slice, one row of `width` pixels.
    ///
    /// # Safety
    ///
    /// `bits` stays valid for the frame's lifetime, and the frame is only ever
    /// composed on its own window's thread between messages.
    unsafe fn pixels(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.bits,
                (self.width as usize * self.height as usize) * 4,
            )
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Everything one selection session owns.
struct Session {
    hwnd: HWND,
    /// The screen as captured, at full brightness.
    screen: Shot,
    /// The same screen, darkened, for everything not selected.
    dimmed: Shot,
    /// Where the window sits in virtual-screen coordinates.
    origin: (i32, i32),
    frame: Frame,
    /// Where the drag started, in window coordinates.
    anchor: (i32, i32),
    selection: Option<Area>,
    dragging: bool,
    /// Where the pointer is, in window coordinates. The crosshair, the magnifier
    /// and the prompt all hang off it.
    cursor: Option<(i32, i32)>,
    font: HGDIOBJ,
    options: Options,
    /// What ended the session: the chosen area, or `None` for a cancel.
    result: Option<Area>,
    finished: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font);
        }
    }
}

impl Session {
    /// Record how the session ended and take the window down.
    fn finish(&mut self, selection: Option<Area>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.result = selection;
        let _ = unsafe { ReleaseCapture() };
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// What a finished session produced: the crop, and whether it should also be
/// pinned back where it came from.
pub(crate) struct Captured {
    pub shot: Shot,
    pub pin_at: Option<(i32, i32)>,
}

/// How a request to capture ended.
pub(crate) enum Finished {
    /// A session was already running, so this request did nothing at all.
    Refused,
    /// The user backed out.
    Cancelled,
    /// The screen could not be captured or the window could not be created.
    Failed,
    Captured(Captured),
}

/// Run one selection session. Blocks the calling thread.
pub(crate) fn run(options: Options) -> Finished {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        tracing::debug!("a capture is already in progress; ignoring the request");
        return Finished::Refused;
    }
    let result = session(options);
    ACTIVE.store(false, Ordering::Release);
    result
}

/// Why a selection ended without a capture.
enum Failure {
    /// The user backed out.
    Cancelled,
    /// The screen could not be captured, or the window could not be created.
    /// Reported separately from a cancel so a real failure is not filed as the
    /// user changing their mind.
    Unavailable,
}

fn session(options: Options) -> Finished {
    match select(options) {
        Ok(captured) => Finished::Captured(captured),
        Err(Failure::Cancelled) => Finished::Cancelled,
        Err(Failure::Unavailable) => Finished::Failed,
    }
}

fn select(options: Options) -> Result<Captured, Failure> {
    unsafe {
        // Per-monitor v2 so the window covers the virtual desktop in physical
        // pixels. The widget bar has usually set this process-wide already, and
        // the "already set" failure is not interesting.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Capture *before* any window of ours exists, so nothing of ours can be in
    // the picture.
    let rect = capture::virtual_screen_rect().ok_or(Failure::Unavailable)?;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let screen = capture::grab(rect.left, rect.top, width, height).ok_or(Failure::Unavailable)?;

    let instance = unsafe { GetModuleHandleW(None) }.map_err(|_| Failure::Unavailable)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_CROSS) }.unwrap_or_default(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    // A second registration in the same process fails benignly.
    unsafe { RegisterClassExW(&class) };

    let hwnd = unsafe {
        CreateWindowExW(
            // `WS_EX_NOACTIVATE` is load-bearing, not decoration: a full-screen
            // window that also takes the foreground is what the shell calls a
            // full-screen application, and it reacts by switching the desktop
            // into its full-screen state — the pointer turns into the busy ring
            // for as long as the mask is up, the taskbar is reconfigured, and
            // our own taskbar module resets the taskbar's appearance, which is
            // what makes the shell re-raise the taskbar over our windows. The
            // mask needs none of that: the mouse is captured explicitly and the
            // two keys that end a session come through the keyboard hook.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            WINDOW_CLASS,
            w!("WinBeautify 截图"),
            WS_POPUP,
            rect.left,
            rect.top,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|_| Failure::Unavailable)?;

    let window_dc = unsafe { GetDC(Some(hwnd)) };
    let frame = Frame::new(window_dc, width, height);
    unsafe { ReleaseDC(Some(hwnd), window_dc) };
    let Some(frame) = frame else {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(Failure::Unavailable);
    };

    // The whole frame is composed before the window is shown, or the first paint
    // would blit an uninitialised (black) buffer.
    let mut session = Box::new(Session {
        hwnd,
        // `Shot::dim` premultiplies, so a plain blit of the result is already
        // the darkened pixel: no alpha blending anywhere.
        dimmed: screen.dim(options.dim),
        screen,
        origin: (rect.left, rect.top),
        frame,
        anchor: (0, 0),
        selection: None,
        dragging: false,
        cursor: None,
        font: message_font(),
        options,
        result: None,
        finished: false,
    });
    compose(
        &mut session,
        Area {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
    );

    let raw = Box::into_raw(session);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            width,
            height,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
        SetCapture(hwnd);
        show_crosshair();
    }
    with_session(hwnd, |session| {
        let mut point = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
        }
        session.cursor = Some((point.x - session.origin.0, point.y - session.origin.1));
        let full = Area {
            left: 0,
            top: 0,
            right: session.screen.width,
            bottom: session.screen.height,
        };
        repaint(session, &[full]);
    });
    unsafe { SetTimer(Some(hwnd), TIMER_ASSERT, ASSERT_MS, None) };
    HOOK_TARGET.store(hwnd.0 as isize, Ordering::Release);
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) }.ok();

    let mut message = MSG::default();
    let mut was_covered = false;
    loop {
        // Before blocking for the next message, not after: whatever is in front
        // of the mask has to be put back whether or not the mouse has moved.
        // Reported on the way in only: a mouse move is one message each, and one
        // line per repaired message would bury the log during a drag.
        let covered = assert_topmost(hwnd);
        if covered && !was_covered {
            tracing::debug!("something was in front of the capture mask; put it back on top");
        }
        was_covered = covered;
        let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }
    }

    // The session is over, so stop watching the keyboard straight away rather
    // than at some later point when this function happens to return.
    HOOK_TARGET.store(0, Ordering::Release);
    if let Some(hook) = hook {
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
    }

    // The window is gone; reclaim the session and read the outcome out of it.
    let session = unsafe { Box::from_raw(raw) };
    let area = session.result;
    let origin = session.origin;
    // The crop happens while the session still owns the undimmed pixels.
    let shot = area.and_then(|area| {
        session
            .screen
            .crop(area.left, area.top, area.width(), area.height())
    });
    let pin_at = area.map(|area| (origin.0 + area.left, origin.1 + area.top));
    drop(session);

    let mut shot = shot.ok_or(Failure::Cancelled)?;
    // A screen DC has no alpha channel, so every captured pixel came back with
    // `a = 0`. Everything that presents this — the clipboard, the pin window's
    // scaler — needs it set.
    shot.opaque();
    Ok(Captured {
        shot,
        pin_at: options.auto_pin.then_some(pin_at).flatten(),
    })
}

/// Show the crosshair.
///
/// Called from every path that can leave the pointer showing something else.
/// While a window holds the mouse capture, Windows stops sending it
/// `WM_SETCURSOR` — the capturing window is responsible for its own cursor — so
/// whatever the pointer happened to be showing when the capture was taken stays
/// on screen for the whole session. That is how the mask came to be drawn with
/// the busy ring: the desktop was momentarily busy as it appeared, and nothing
/// ever corrected it.
fn show_crosshair() {
    unsafe {
        if let Ok(cursor) = LoadCursorW(None, IDC_CROSS) {
            SetCursor(Some(cursor));
        }
    }
}

/// Put the mask back on top of everything.
///
/// A capture overlay is modal: the whole point is that the screen it shows *is*
/// what will be taken, dimmed, with the selection punched out of it. Any window
/// in front of it — and on this machine the shell reorders topmost windows on
/// its own, which is what makes the widget bar disappear behind the taskbar —
/// is a patch of undimmed, unselected screen that the user reads as part of the
/// picture. What they then drag a box around is not what they are looking at.
///
/// So the invariant is asserted rather than assumed, and only while a session
/// is running: outside one, the overlay is not on screen at all.
fn assert_topmost(hwnd: HWND) -> bool {
    // Nothing in front means nothing to repair. `GW_HWNDPREV` walks the whole
    // desktop, and everything in front of a topmost window is topmost too.
    if unsafe { GetWindow(hwnd, GW_HWNDPREV) }.is_err() {
        return false;
    }
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            // `SWP_NOREDRAW`: the mask's pixels are ours to push, and a window
            // manager redraw every time something gets in front would be a
            // flicker for no gain.
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
    true
}

/// Swallow the two keys that end a session — Escape and Enter.
///
/// The overlay deliberately does not take the foreground (see the window style
/// where it is created), so keyboard messages follow the focus to whatever
/// application the user was in. This is how the mask hears them anyway: it is
/// installed for the life of the session and swallows exactly the keys the mask
/// owns while it is up.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_KEYDOWN {
        let event = lparam.0 as *const KBDLLHOOKSTRUCT;
        let owned = !event.is_null()
            && matches!(
                unsafe { (*event).vkCode },
                k if k == VK_ESCAPE.0 as u32 || k == VK_RETURN.0 as u32
            );
        if owned {
            let target = HOOK_TARGET.load(Ordering::Acquire);
            if target != 0 {
                // Posted rather than handled here: the hook runs inside another
                // message's dispatch, and destroying the window from there would
                // unwind back through the hook.
                let hwnd = HWND(target as *mut core::ffi::c_void);
                let key = unsafe { (*event).vkCode };
                unsafe {
                    if key == VK_ESCAPE.0 as u32 {
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    } else {
                        // Enter takes the selection as it stands, which the
                        // window procedure already knows how to do.
                        let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(key as usize), LPARAM(0));
                    }
                }
                // Swallow it: while the overlay is up, these two are ours.
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Compose and push a set of areas, one at a time.
///
/// # Why a set and not a bounding box
///
/// This used to union everything that changed into one rectangle and hand that
/// to `InvalidateRect`. The crosshair alone runs the width and the height of the
/// monitor, so the union of "the crosshair moved" is the whole screen: every
/// mouse move recomposed and re-pushed ~30 MB. That starves the message loop
/// badly enough that Windows decides the window is not responding — the pointer
/// turns into the busy ring while the mask is up — and the picture falls behind
/// the pointer by however long the backlog takes to drain.
///
/// The strips are what actually changed. Composing and pushing them individually
/// costs about a megabyte a move, and the loop stays responsive.
fn repaint(session: &mut Session, areas: &[Area]) {
    let started = std::time::Instant::now();
    let (width, height) = (session.screen.width, session.screen.height);
    let dc = unsafe { GetDC(Some(session.hwnd)) };
    if dc.is_invalid() {
        return;
    }
    let mut pixels = 0i64;
    let mut pushed = 0usize;
    for area in areas {
        let area = area.clamped(width, height);
        if area.is_empty() {
            continue;
        }
        compose(session, area);
        pixels += area.width() as i64 * area.height() as i64;
        pushed += 1;
        // Straight to the window rather than through `InvalidateRect`: WM_PAINT
        // reports the *bounding box* of the update region, which would put the
        // whole screen back in play — and would blit parts of the frame nothing
        // had composed.
        unsafe {
            let _ = BitBlt(
                dc,
                area.left,
                area.top,
                area.width(),
                area.height(),
                Some(session.frame.dc),
                area.left,
                area.top,
                SRCCOPY,
            );
        }
    }
    unsafe { ReleaseDC(Some(session.hwnd), dc) };
    tracing::debug!(
        strips = pushed,
        pixels,
        micros = started.elapsed().as_micros() as u64,
        "overlay repaint"
    );
}

/// Draw `area` of the frame: the dimmed screen, the bright selection over it,
/// then the selection's frame.
fn compose(session: &mut Session, area: Area) {
    let dc = session.frame.dc;
    // Everything below draws into the frame buffer, which is the size of the
    // whole desktop and knows nothing about `area`. Clipping here is what makes
    // "composed" and "pushed" the same set of pixels: a fill or a frame that
    // crossed the edge of the area would otherwise be composed but never shown,
    // leaving the window with an older frame in those pixels.
    let saved = unsafe { SaveDC(dc) };
    unsafe {
        let _ = IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
    }
    compose_inner(session, dc, area);
    unsafe {
        let _ = RestoreDC(dc, saved);
    }
}

fn compose_inner(session: &mut Session, dc: HDC, area: Area) {
    // 1. The darkened screen, so anything the selection no longer covers goes
    //    back to reading as "not taken", and the selection at full brightness.
    //    Both are plain memory writes into the frame's DIB — see `copy_rows`
    //    for why they do not go through GDI.
    {
        let width = session.frame.width;
        let pixels = unsafe { session.frame.pixels() };
        copy_rows(&session.dimmed, area, pixels, width, area);
        if let Some(selection) = session.selection {
            if let Some(bright) = overlap(selection, area) {
                copy_rows(&session.screen, bright, pixels, width, area);
            }
        }
    }

    // 2. A thin frame around the selection, drawn as four bands so the pixels
    //    inside stay exactly as captured.
    if let Some(selection) = session.selection {
        let thickness = (session.screen.width / 1000).clamp(1, 3);
        let brush = unsafe { CreateSolidBrush(COLORREF(session.options.accent.to_bgr_u32())) };
        for band in border_bands(selection, thickness) {
            let clipped = Area {
                left: band.left.max(area.left),
                top: band.top.max(area.top),
                right: band.right.min(area.right),
                bottom: band.bottom.min(area.bottom),
            };
            if clipped.width() > 0 && clipped.height() > 0 {
                let rect = clipped.to_rect();
                unsafe { FillRect(dc, &rect, brush) };
            }
        }
        unsafe { let _ = DeleteObject(brush.into()); };
    }

    // 3. The pointer furniture, on top of the picture: where the pointer is,
    //    what is under it, and how big the selection is. Deliberately outside
    //    the block above — an early return for "no selection yet" is exactly
    //    what left the crosshair and the magnifier missing.
    if let Some(cursor) = session.cursor {
        draw_crosshair(session, dc, area, cursor);
        if session.selection.is_none() && !session.dragging {
            draw_hint(session, dc, area, cursor);
        }
        // The magnified patch is pixels, not fills, so it is written before the
        // chrome around it is drawn over it.
        {
            let (source, patch) = magnifier_source_and_patch(session, cursor);
            let width = session.frame.width;
            let pixels = unsafe { session.frame.pixels() };
            expand(&session.screen, source, patch, pixels, width, area);
        }
        draw_magnifier_chrome(session, dc, area, cursor);
    }
    draw_badge(session, dc, area);
}

/// Everything drawn on top of the screen picture, anchored to the pointer or to
/// the selection.
///
/// These live in the frame buffer rather than being painted straight to the
/// window, so a strip repaint restores exactly the pixels that were there —
/// including erasing furniture that has moved away.
fn furniture(session: &Session) -> Vec<Area> {
    let mut areas = Vec::new();

    if let Some(cursor) = session.cursor {
        // The crosshair: one line across the monitor's width and one down its
        // height, so it reads as a ruler rather than as a mark on the picture.
        let monitor = session.cursor_monitor();
        areas.push(Area {
            left: monitor.left - session.origin.0,
            top: cursor.1,
            right: monitor.right - session.origin.0,
            bottom: cursor.1 + CROSSHAIR,
        });
        areas.push(Area {
            left: cursor.0,
            top: monitor.top - session.origin.1,
            right: cursor.0 + CROSSHAIR,
            bottom: monitor.bottom - session.origin.1,
        });
        // The prompt, while there is nothing to describe yet, and the magnifier
        // for as long as the session lasts.
        if session.selection.is_none() && !session.dragging {
            areas.push(hint_area(session, cursor));
        }
        areas.push(magnifier_area(session, cursor));
    }

    if let Some(selection) = session.selection {
        let (badge, _) = badge_plate(session, selection);
        areas.push(badge);
    }

    areas.into_iter().filter(|area| !area.is_empty()).collect()
}

impl Session {
    /// The monitor the pointer is on.
    fn cursor_monitor(&self) -> RECT {
        let point = self
            .cursor
            .map(|(x, y)| (self.origin.0 + x, self.origin.1 + y))
            .unwrap_or(self.origin);
        capture::monitor_rect_at(point.0, point.1).unwrap_or(RECT {
            left: self.origin.0,
            top: self.origin.1,
            right: self.origin.0 + self.screen.width,
            bottom: self.origin.1 + self.screen.height,
        })
    }
}

/// Draw the crosshair through the pointer.
fn draw_crosshair(session: &Session, dc: HDC, area: Area, cursor: (i32, i32)) {
    let monitor = session.cursor_monitor();
    let ink = COLORREF(0x00FF_FFFF);
    let brush = unsafe { CreateSolidBrush(ink) };
    for line in [
        Area {
            left: monitor.left - session.origin.0,
            top: cursor.1,
            right: monitor.right - session.origin.0,
            bottom: cursor.1 + CROSSHAIR,
        },
        Area {
            left: cursor.0,
            top: monitor.top - session.origin.1,
            right: cursor.0 + CROSSHAIR,
            bottom: monitor.bottom - session.origin.1,
        },
    ] {
        let clipped = Area {
            left: line.left.max(area.left),
            top: line.top.max(area.top),
            right: line.right.min(area.right),
            bottom: line.bottom.min(area.bottom),
        };
        if !clipped.is_empty() {
            let rect = clipped.to_rect();
            unsafe { FillRect(dc, &rect, brush) };
        }
    }
    unsafe { let _ = DeleteObject(brush.into()); };
}

/// How much of the surrounding screen the magnifier shows.
const MAGNIFIER_SOURCE: i32 = 21;
/// How far each source pixel is blown up.
const MAGNIFIER_ZOOM: i32 = 8;
/// Thickness of the crosshair lines.
const CROSSHAIR: i32 = 1;

/// Where the magnifier patch goes: next to the pointer, flipped near an edge.
fn magnifier_area(session: &Session, cursor: (i32, i32)) -> Area {
    let monitor = session.cursor_monitor();
    let (width, height) = (session.screen.width, session.screen.height);
    let gap = 18;
    let plate = (MAGNIFIER_SOURCE * MAGNIFIER_ZOOM, MAGNIFIER_SOURCE * MAGNIFIER_ZOOM + 18);

    // Prefer down-right of the pointer, which is where a right-handed user is
    // not about to drag a selection.
    let mut left = cursor.0 + gap;
    let mut top = cursor.1 + gap;
    if left + plate.0 > monitor.right - session.origin.0 {
        left = cursor.0 - gap - plate.0;
    }
    if top + plate.1 > monitor.bottom - session.origin.1 {
        top = cursor.1 - gap - plate.1;
    }
    Area {
        left: left.max(monitor.left - session.origin.0),
        top: top.max(monitor.top - session.origin.1),
        right: left.max(monitor.left - session.origin.0) + plate.0,
        bottom: top.max(monitor.top - session.origin.1) + plate.1,
    }
    .clamped(width, height)
}

/// Where the magnifier samples from, and where its patch goes.
///
/// The patch is next to the pointer, flipped near an edge; the source is the
/// 21×21 patch around the pointer, clamped to the picture so a corner does not
/// read outside it.
fn magnifier_source_and_patch(session: &Session, cursor: (i32, i32)) -> (Area, Area) {
    let plate = magnifier_area(session, cursor);
    let patch = Area {
        left: plate.left,
        top: plate.top,
        right: plate.right,
        bottom: plate.bottom - 18,
    };
    let half = MAGNIFIER_SOURCE / 2;
    let source_x = (cursor.0 - half).clamp(0, (session.screen.width - MAGNIFIER_SOURCE).max(0));
    let source_y = (cursor.1 - half).clamp(0, (session.screen.height - MAGNIFIER_SOURCE).max(0));
    let source = Area {
        left: source_x,
        top: source_y,
        right: source_x + MAGNIFIER_SOURCE,
        bottom: source_y + MAGNIFIER_SOURCE,
    };
    (source, patch)
}

/// The magnifier's chrome: the pixel under the pointer outlined inside the
/// patch, a border around the whole plate, and the coordinate and colour
/// read-out underneath. The magnified pixels themselves are written by
/// `expand` before this runs.
fn draw_magnifier_chrome(session: &Session, dc: HDC, area: Area, cursor: (i32, i32)) {
    let plate = magnifier_area(session, cursor);
    if !intersects(plate, area.to_rect()) {
        return;
    }
    let (_, patch) = magnifier_source_and_patch(session, cursor);
    let zoom = patch.width() / MAGNIFIER_SOURCE;
    let half = MAGNIFIER_SOURCE / 2;
    let source_x = (cursor.0 - half).clamp(0, (session.screen.width - MAGNIFIER_SOURCE).max(0));
    let source_y = (cursor.1 - half).clamp(0, (session.screen.height - MAGNIFIER_SOURCE).max(0));

    // The pixel the pointer is on, outlined inside the patch.
    let centre_x = patch.left + (cursor.0 - source_x) * zoom;
    let centre_y = patch.top + (cursor.1 - source_y) * zoom;
    let cell = Area {
        left: centre_x,
        top: centre_y,
        right: centre_x + zoom,
        bottom: centre_y + zoom,
    };
    let outline = cell.to_rect();
    let brush = unsafe { CreateSolidBrush(COLORREF(0x00FF_FFFF)) };
    unsafe {
        FrameRect(dc, &outline, brush);
        // …and a border around the whole patch, which also separates it from a
        // bright or busy background.
        let frame = plate.to_rect();
        FrameRect(dc, &frame, brush);
        let _ = DeleteObject(brush.into());
    }

    // The coordinate and colour read-out, in the strip under the patch.
    let (red, green, blue) = pixel_at(&session.screen, cursor.0, cursor.1);
    let label = format!(
        "{} × {}   #{:02X}{:02X}{:02X}",
        session.origin.0 + cursor.0,
        session.origin.1 + cursor.1,
        red,
        green,
        blue
    );
    let strip = Area {
        left: plate.left,
        top: patch.bottom,
        right: plate.right,
        bottom: plate.bottom,
    };
    let plate_brush = unsafe { CreateSolidBrush(COLORREF(0x0010_1010)) };
    let strip_rect = strip.to_rect();
    unsafe {
        FillRect(dc, &strip_rect, plate_brush);
        let _ = DeleteObject(plate_brush.into());
    }
    let mut bounds = strip.to_rect();
    let mut wide: Vec<u16> = label.encode_utf16().collect();
    unsafe {
        let font = SelectObject(dc, session.font);
        let colour = SetTextColor(dc, COLORREF(0x00FF_FFFF));
        let mode = SetBkMode(dc, TRANSPARENT);
        DrawTextW(
            dc,
            &mut wide,
            &mut bounds,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SetBkMode(dc, BACKGROUND_MODE(mode as u32));
        SetTextColor(dc, colour);
        SelectObject(dc, font);
    }
}

/// The colour of one pixel of a shot, as `(r, g, b)`.
fn pixel_at(shot: &Shot, x: i32, y: i32) -> (u8, u8, u8) {
    if x < 0 || y < 0 || x >= shot.width || y >= shot.height {
        return (0, 0, 0);
    }
    let index = y as usize * shot.stride() + x as usize * 4;
    // Stored BGRA.
    (shot.bgra[index + 2], shot.bgra[index + 1], shot.bgra[index])
}

/// Where the "drag to select" prompt goes, anchored to the pointer.
///
/// To the *left* of the pointer, because the magnifier takes the right: the two
/// would otherwise be drawn on top of each other, and the magnifier wins.
fn hint_area(session: &Session, cursor: (i32, i32)) -> Area {
    let monitor = session.cursor_monitor();
    let plate = (360, 30);
    let gap = 22;
    let mut left = cursor.0 - gap - plate.0;
    if left < monitor.left - session.origin.0 {
        // Not enough room on the left; the magnifier flips in that case too, so
        // above the pointer is the one place that is always free.
        left = cursor.0 - plate.0 / 2;
    }
    let left = left
        .max(monitor.left - session.origin.0)
        .min(monitor.right - session.origin.0 - plate.0)
        .max(0);
    let top = (cursor.1 + gap).min(monitor.bottom - session.origin.1 - plate.1);
    Area {
        left,
        top: top.max(monitor.top - session.origin.1),
        right: left + plate.0,
        bottom: top.max(monitor.top - session.origin.1) + plate.1,
    }
    .clamped(session.screen.width, session.screen.height)
}

/// Draw the prompt next to the pointer.
fn draw_hint(session: &Session, dc: HDC, area: Area, cursor: (i32, i32)) {
    let hint = hint_area(session, cursor);
    if !intersects(hint, area.to_rect()) {
        return;
    }
    let brush = unsafe { CreateSolidBrush(COLORREF(0x0018_1818)) };
    let bounds = hint.to_rect();
    unsafe {
        FillRect(dc, &bounds, brush);
        let _ = DeleteObject(brush.into());
    }
    draw_centered(dc, session.font, "拖动选择区域 · Esc 或右键取消", bounds);
}

/// The intersection of two areas, or `None` when they do not overlap.
fn overlap(area: Area, clip: Area) -> Option<Area> {
    let clipped = Area {
        left: area.left.max(clip.left),
        top: area.top.max(clip.top),
        right: area.right.min(clip.right),
        bottom: area.bottom.min(clip.bottom),
    };
    (!clipped.is_empty()).then_some(clipped)
}

/// Copy `area` of `shot` into a top-down BGRA buffer `width` pixels wide,
/// limited to `clip`.
///
/// This used to be a `StretchDIBits` call, and the magnifier's scaled variant
/// beside it. Measured on this machine, GDI does not honour either reliably
/// when the source DIB is the size of the whole desktop: a 1:1 copy drew
/// correct pixels only while source and destination rectangles coincided, and
/// the 21×21 → 168×168 magnifier patch drew whatever neighbouring scanlines
/// the driver's banding pass felt like — the frame buffer held the right
/// pixels, the screen showed the wrong ones. Writing the buffer directly has
/// exactly one behaviour on every machine, and it is testable without a
/// desktop.
fn copy_rows(shot: &Shot, area: Area, bits: &mut [u8], width: i32, clip: Area) {
    let Some(area) = overlap(area.clamped(shot.width, shot.height), clip) else {
        return;
    };
    let source_stride = shot.stride();
    let target_stride = width as usize * 4;
    for row in area.top..area.bottom {
        let source = (row as usize * source_stride) + area.left as usize * 4;
        let target = (row as usize * target_stride) + area.left as usize * 4;
        let bytes = area.width() as usize * 4;
        bits[target..target + bytes]
            .copy_from_slice(&shot.bgra[source..source + bytes]);
    }
}

/// Blow `source` up into `destination` on a top-down BGRA buffer `width` wide,
/// limited to `clip`.
///
/// Nearest-neighbour by hand rather than through `StretchDIBits`, for the same
/// reason as [`copy_rows`] — and because a screenshot tool is expected to show
/// the pixel grid rather than a smoothed guess at it.
fn expand(
    shot: &Shot,
    source: Area,
    destination: Area,
    bits: &mut [u8],
    width: i32,
    clip: Area,
) {
    if source.is_empty() || destination.is_empty() {
        return;
    }
    if source.left < 0
        || source.top < 0
        || source.right > shot.width
        || source.bottom > shot.height
    {
        return;
    }
    let Some(area) = overlap(destination, clip) else {
        return;
    };
    let source_stride = shot.stride();
    let target_stride = width as usize * 4;
    for row in area.top..area.bottom {
        // Sample positions come from the unclipped `destination`, so a clip at
        // the top or left edge shows the part of the picture that belongs
        // there rather than the top-left corner of the whole patch.
        let source_row = source.top
            + ((row - destination.top) * source.height()) / destination.height();
        let source_base = source_row as usize * source_stride;
        let target_base = row as usize * target_stride;
        for column in area.left..area.right {
            let source_column = source.left
                + ((column - destination.left) * source.width()) / destination.width();
            let from = source_base + source_column as usize * 4;
            let to = target_base + column as usize * 4;
            bits[to..to + 4].copy_from_slice(&shot.bgra[from..from + 4]);
        }
    }
}

/// Where the badge goes: above the selection, or below when there is no room.
///
/// Also used to keep it inside the window horizontally, so a selection at the
/// screen edge does not push it out of sight.
fn badge_origin(selection: Area, badge: (i32, i32), window: (i32, i32)) -> (i32, i32) {
    let gap = 6;
    let above = selection.top - gap - badge.1;
    let top = if above >= 0 {
        above
    } else {
        selection.bottom + gap
    };
    let mut left = selection.left;
    // Flip to the left of the selection's right edge rather than run off screen.
    if left + badge.0 > window.0 {
        left = selection.right - badge.0;
    }
    (
        left.clamp(0, (window.0 - badge.0).max(0)),
        top.clamp(0, (window.1 - badge.1).max(0)),
    )
}

/// Where the size badge goes, and the plate it is drawn on.
fn badge_plate(session: &Session, selection: Area) -> (Area, String) {
    let label = format!("{} × {}", selection.width(), selection.height());
    // Measured with the font it is drawn in, so a four-digit pair of numbers
    // cannot overflow the plate.
    let (text_width, text_height) = measure_text(session.frame.dc, session.font, &label);
    let plate = (text_width + 16, text_height + 10);
    let origin = badge_origin(selection, plate, (session.screen.width, session.screen.height));
    let area = Area {
        left: origin.0,
        top: origin.1,
        right: origin.0 + plate.0,
        bottom: origin.1 + plate.1,
    };
    (area, label)
}

/// The four bands of a selection's frame.
///
/// Drawn *outside* the selection, so the bright area in the overlay is exactly
/// the region that will be captured. Drawn inside — as this first did — the part
/// that looks selected is smaller than the part that is taken by the width of the
/// frame, which is a discrepancy the user cannot see and therefore cannot
/// explain: "what is shown is not what I get".
fn border_bands(selection: Area, thickness: i32) -> [Area; 4] {
    let outer = Area {
        left: selection.left - thickness,
        top: selection.top - thickness,
        right: selection.right + thickness,
        bottom: selection.bottom + thickness,
    };
    [
        Area {
            left: outer.left,
            top: outer.top,
            right: outer.right,
            bottom: selection.top,
        },
        Area {
            left: outer.left,
            top: selection.bottom,
            right: outer.right,
            bottom: outer.bottom,
        },
        Area {
            left: outer.left,
            top: selection.top,
            right: selection.left,
            bottom: selection.bottom,
        },
        Area {
            left: selection.right,
            top: selection.top,
            right: outer.right,
            bottom: selection.bottom,
        },
    ]
}

/// Borrow the session out of the window's user data.
///
/// # Safety
///
/// The handler must never let a single message reach this twice: the borrow is a
/// `&mut` derived from a raw pointer, so nesting would alias it. The window
/// procedure therefore answers `WM_DESTROY` (which `DestroyWindow` delivers
/// synchronously) *without* the session — see the arm further down.
fn with_session<R>(hwnd: HWND, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Session;
    if raw.is_null() {
        return None;
    }
    Some(f(unsafe { &mut *raw }))
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            with_session(hwnd, |session| {
                let target = paint.rcPaint;
                // The frame buffer is only composed where it was pushed, so an
                // area the system exposes has to be composed here before it can
                // be copied out of it.
                compose(
                    session,
                    Area {
                        left: target.left,
                        top: target.top,
                        right: target.right,
                        bottom: target.bottom,
                    },
                );
                unsafe {
                    let _ = BitBlt(
                        dc,
                        target.left,
                        target.top,
                        target.right - target.left,
                        target.bottom - target.top,
                        Some(session.frame.dc),
                        target.left,
                        target.top,
                        SRCCOPY,
                    );
                }
            });
            unsafe { let _ = EndPaint(hwnd, &paint); };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = point_of(lparam);
            with_session(hwnd, |session| {
                // The prompt only shows before the first drag, so where it was
                // has to be repainted along with the new selection. The pointer
                // furniture moves with the click, too.
                let mut dirty = furniture(session);
                session.dragging = true;
                session.anchor = (x, y);
                session.cursor = Some((x, y));
                dirty.extend(furniture(session));
                // Wipe the previous selection's frame even if the new drag never
                // grows: the first frame of a drag has no area yet.
                let (width, height) = (session.screen.width, session.screen.height);
                if let Some(previous) = session.selection.take() {
                    // No new selection yet, so the difference is all of it.
                    dirty.extend(changed_strips(previous, Area::EMPTY, 8, width, height));
                }
                dirty.push(Area {
                    left: x,
                    top: y,
                    right: x,
                    bottom: y,
                });
                repaint(session, &dirty);
            });
            unsafe { SetCapture(hwnd) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_of(lparam);
            show_crosshair();
            with_session(hwnd, |session| {
                // The crosshair, the magnifier and the prompt all follow the
                // pointer, so the strips they covered a moment ago are pushed
                // along with the strips they cover now — as strips, not as the
                // box around them, which is the whole screen.
                let stale = furniture(session);
                session.cursor = Some((x, y));
                let mut dirty = stale;
                dirty.extend(furniture(session));

                if session.dragging {
                    let (width, height) = (session.screen.width, session.screen.height);
                    let next = Area::from_drag(session.anchor, (x, y)).clamped(width, height);
                    let previous = session.selection.replace(next);
                    // Only where the two differ: the inside of the selection is
                    // the same bright screen it was a moment ago, and a big
                    // selection is a megapixel that would otherwise be composed
                    // again on every mouse event.
                    let strips = changed_strips(previous.unwrap_or(Area::EMPTY), next, 8, width, height);
                    tracing::debug!(?previous, ?next, strips = strips.len(), "drag");
                    dirty.extend(strips);
                }
                repaint(session, &dirty);
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = unsafe { ReleaseCapture() };
            with_session(hwnd, |session| {
                if !session.dragging {
                    return;
                }
                session.dragging = false;
                let selection = session.selection.filter(|area| area.is_usable());
                session.finish(selection);
            });
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            with_session(hwnd, |session| session.finish(None));
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let key = wparam.0 as u32;
            with_session(hwnd, |session| match key {
                // Enter takes the selection as it stands; Escape abandons it.
                k if k == VK_RETURN.0 as u32 => {
                    let selection = session.selection.filter(|area| area.is_usable());
                    session.finish(selection);
                }
                k if k == VK_ESCAPE.0 as u32 => session.finish(None),
                _ => {}
            });
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ASSERT {
                assert_topmost(hwnd);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Answering without deferring keeps Windows from resetting the
            // crosshair to the class arrow whenever the mouse moves.
            show_crosshair();
            LRESULT(1)
        }
        WM_CLOSE => {
            with_session(hwnd, |session| session.finish(None));
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                // The session itself is reclaimed by the thread that created it,
                // after the loop returns; clearing the slot just makes every
                // message that arrives in between a no-op.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Draw the `320 × 240` badge beside the selection.
fn draw_badge(session: &Session, dc: HDC, area: Area) {
    let Some(selection) = session.selection else {
        return;
    };
    let (badge, label) = badge_plate(session, selection);
    if !intersects(badge, area.to_rect()) {
        return;
    }
    // A dark plate under light text, which stays legible over any wallpaper.
    let brush = unsafe { CreateSolidBrush(COLORREF(0x0018_1818)) };
    let bounds = badge.to_rect();
    unsafe {
        FillRect(dc, &bounds, brush);
        let _ = DeleteObject(brush.into());
    }
    draw_centered(dc, session.font, &label, bounds);
}

/// Draw one line of white text, centred in `bounds`.
fn draw_centered(dc: HDC, font: HGDIOBJ, text: &str, mut bounds: RECT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let previous_font = SelectObject(dc, font);
        let previous_colour = SetTextColor(dc, COLORREF(0x00FF_FFFF));
        // `SetBkMode` hands back the mode it replaced, as a plain integer.
        let previous_mode = SetBkMode(dc, TRANSPARENT);
        DrawTextW(
            dc,
            &mut wide,
            &mut bounds,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SetBkMode(dc, BACKGROUND_MODE(previous_mode as u32));
        SetTextColor(dc, previous_colour);
        SelectObject(dc, previous_font);
    }
}

/// The size `text` needs, in device pixels.
fn measure_text(dc: HDC, font: HGDIOBJ, text: &str) -> (i32, i32) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut bounds = RECT::default();
    unsafe {
        let previous = SelectObject(dc, font);
        DrawTextW(
            dc,
            &mut wide,
            &mut bounds,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc, previous);
    }
    (bounds.right - bounds.left, (bounds.bottom - bounds.top).max(14))
}

fn intersects(area: Area, target: RECT) -> bool {
    area.right > target.left
        && area.left < target.right
        && area.bottom > target.top
        && area.top < target.bottom
}

/// The window coordinates in a mouse message.
fn point_of(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

/// The shell's own message font.
///
/// Taken from `SPI_GETNONCLIENTMETRICS` rather than named outright: the labels
/// here are Chinese, and the shell font is the one face guaranteed to carry the
/// current locale's glyphs — a hard-coded "Segoe UI" would render boxes on a
/// system without the CJK fonts installed.
fn message_font() -> HGDIOBJ {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            Some(&mut metrics as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_ok() {
        let font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
        if !font.is_invalid() {
            return font.into();
        }
    }
    // Stock fonts are not DPI-scaled, but a legible label beats no label.
    unsafe { GetStockObject(DEFAULT_GUI_FONT) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A region copy has to land on the rows that were asked for.
    ///
    /// This is the check for the worst bug this window has had: the overlay's
    /// dimmed background and its bright selection are both *sub-rectangles* of
    /// one captured DIB. The copies used to go through `StretchDIBits`, which
    /// on this machine mangled exactly those sub-rectangle reads — the frame
    /// buffer then held scanlines from elsewhere in the shot, which is why the
    /// magnifier showed the wrong part of the screen. The writers are plain
    /// memory moves now, and this asserts their one behaviour.
    #[test]
    fn a_region_copy_lands_on_the_rows_it_was_asked_for() {
        const W: i32 = 8;
        const H: i32 = 16;
        // Every row carries its own index in the blue channel.
        let mut shot = Shot {
            width: W,
            height: H,
            bgra: vec![0; (W * H * 4) as usize],
        };
        for y in 0..H {
            for x in 0..W {
                let at = ((y * W + x) * 4) as usize;
                shot.bgra[at] = y as u8;
                shot.bgra[at + 3] = 0xFF;
            }
        }

        let mut frame = vec![0u8; (W * H * 4) as usize];
        let area = Area {
            left: 0,
            top: 4,
            right: W,
            bottom: 12,
        };
        copy_rows(&shot, area, &mut frame, W, Area {
            left: 0,
            top: 0,
            right: W,
            bottom: H,
        });

        let blue = |y: i32| frame[(y * W * 4) as usize];
        for y in area.top..area.bottom {
            assert_eq!(
                blue(y),
                y as u8,
                "row {y} of the frame holds row {} of the shot",
                blue(y)
            );
        }
        // Rows the copy did not ask for stay untouched.
        assert_eq!(blue(0), 0);
        assert_eq!(blue(15), 0);
    }

    /// A copy clipped to a strip only touches that strip.
    #[test]
    fn a_copy_stays_inside_its_clip() {
        let shot = Shot {
            width: 4,
            height: 4,
            bgra: vec![0xAA; (4 * 4 * 4) as usize],
        };
        let mut frame = vec![0u8; (4 * 4 * 4) as usize];
        copy_rows(
            &shot,
            Area {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            },
            &mut frame,
            4,
            Area {
                left: 1,
                top: 2,
                right: 3,
                bottom: 4,
            },
        );
        let at = |x: i32, y: i32| frame[((y * 4 + x) * 4) as usize];
        assert_eq!(at(0, 0), 0, "outside the clip");
        assert_eq!(at(1, 2), 0xAA, "inside the clip");
        assert_eq!(at(2, 2), 0xAA);
        assert_eq!(at(3, 3), 0, "the clip's right edge is exclusive");
        assert_eq!(at(0, 3), 0);
    }

    /// The magnifier blows up exactly the pixels it was asked for, in order.
    #[test]
    fn the_magnifier_expands_the_sampled_patch_in_order() {
        const W: i32 = 4;
        let mut shot = Shot {
            width: W,
            height: W,
            bgra: vec![0; (W * W * 4) as usize],
        };
        for y in 0..W {
            for x in 0..W {
                let at = ((y * W + x) * 4) as usize;
                shot.bgra[at] = y as u8;
                shot.bgra[at + 1] = x as u8;
                shot.bgra[at + 3] = 0xFF;
            }
        }
        // 2×2 source (1,1)..(3,3) blown up 4× into an 8×8 frame.
        let mut frame = vec![0u8; (8 * 8 * 4) as usize];
        expand(
            &shot,
            Area {
                left: 1,
                top: 1,
                right: 3,
                bottom: 3,
            },
            Area {
                left: 0,
                top: 0,
                right: 8,
                bottom: 8,
            },
            &mut frame,
            8,
            Area {
                left: 0,
                top: 0,
                right: 8,
                bottom: 8,
            },
        );
        let at = |x: i32, y: i32| (frame[((y * 8 + x) * 4) as usize], frame[((y * 8 + x) * 4 + 1) as usize]);
        assert_eq!(at(0, 0), (1, 1), "top-left is the source's top-left");
        assert_eq!(at(7, 0), (1, 2), "top-right is the source's top-right");
        assert_eq!(at(0, 7), (2, 1), "bottom-left is the source's bottom-left");
        assert_eq!(at(7, 7), (2, 2));
        assert_eq!(at(4, 6), (2, 2), "the middle switches where the samples do");
    }

    /// A magnifier patch clipped by the strip it is being composed into shows
    /// the part of the picture that belongs at those coordinates, not the
    /// patch's own top-left corner.
    #[test]
    fn an_expansion_clipped_at_the_top_keeps_its_sample_mapping() {
        const W: i32 = 2;
        let mut shot = Shot {
            width: W,
            height: W,
            bgra: vec![0; (W * W * 4) as usize],
        };
        for y in 0..W {
            for x in 0..W {
                let at = ((y * W + x) * 4) as usize;
                shot.bgra[at] = y as u8;
                shot.bgra[at + 1] = x as u8;
            }
        }
        // 2×2 source into a 4×4 destination, but only the destination's bottom
        // half is composed.
        let mut frame = vec![0u8; (4 * 4 * 4) as usize];
        expand(
            &shot,
            Area {
                left: 0,
                top: 0,
                right: 2,
                bottom: 2,
            },
            Area {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            },
            &mut frame,
            4,
            Area {
                left: 0,
                top: 2,
                right: 4,
                bottom: 4,
            },
        );
        let at = |x: i32, y: i32| (frame[((y * 4 + x) * 4) as usize], frame[((y * 4 + x) * 4 + 1) as usize]);
        assert_eq!(at(0, 2), (1, 0), "destination row 2 samples source row 1");
        assert_eq!(at(2, 3), (1, 1));
        assert_eq!(at(0, 0), (0, 0), "the clipped-off half stays untouched");
    }

    /// Every pixel of `a` and `b` that belongs to only one of them has to be
    /// inside one of the strips.
    fn covered_by(strips: &[Area], a: Area, b: Area) -> usize {
        let mut missed = 0;
        for y in 0..40 {
            for x in 0..40 {
                let in_a = a.contains(x, y);
                let in_b = b.contains(x, y);
                if in_a == in_b {
                    continue;
                }
                if !strips.iter().any(|s| s.contains(x, y)) {
                    missed += 1;
                }
            }
        }
        missed
    }

    fn box_at(l: i32, t: i32, r: i32, b: i32) -> Area {
        Area {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    #[test]
    fn the_strips_cover_where_two_selections_differ() {
        let cases = [
            // Growing down-right, one step at a time.
            (box_at(10, 10, 20, 20), box_at(10, 10, 22, 22)),
            // Growing up-left (the drag went the other way).
            (box_at(10, 10, 20, 20), box_at(6, 7, 20, 20)),
            // Shrinking back.
            (box_at(6, 7, 20, 20), box_at(10, 10, 20, 20)),
            // Disjoint: a drag that jumped.
            (box_at(2, 2, 8, 8), box_at(20, 20, 30, 30)),
            // Unchanged: nothing to compose.
            (box_at(10, 10, 20, 20), box_at(10, 10, 20, 20)),
            // One of them empty, both ways.
            (box_at(10, 10, 20, 20), Area::EMPTY),
            (Area::EMPTY, box_at(10, 10, 20, 20)),
        ];
        for (a, b) in cases {
            // No padding: the question is only about the difference itself.
            let strips = changed_strips(a, b, 0, 100, 100);
            assert_eq!(
                covered_by(&strips, a, b),
                0,
                "{a:?} -> {b:?} left pixels uncovered: {strips:?}"
            );
        }
    }

    #[test]
    fn a_small_drag_only_composes_the_edge_that_moved() {
        // The point of the exercise: a corner drag on a big selection used to
        // compose the whole thing, twice, on every mouse event.
        let big = box_at(0, 0, 1400, 900);
        let nudged = box_at(0, 0, 1410, 900);
        let strips = changed_strips(big, nudged, 8, 4000, 2000);
        let composed: i64 = strips
            .iter()
            .map(|s| s.width() as i64 * s.height() as i64)
            .sum();
        let whole = big.width() as i64 * big.height() as i64;
        assert!(
            composed * 20 < whole,
            "a ten-pixel nudge composed {composed} of {whole} pixels"
        );
    }

    #[test]
    fn an_unchanged_selection_composes_nothing() {
        let same = box_at(10, 10, 200, 200);
        assert!(changed_strips(same, same, 8, 1000, 1000).is_empty());
    }

    #[test]
    fn a_drag_normalises_whichever_way_it_went() {
        let down = Area::from_drag((100, 80), (20, 10));
        let up = Area::from_drag((20, 10), (100, 80));
        assert_eq!(down, up);
        assert_eq!(
            (down.left, down.top, down.right, down.bottom),
            (20, 10, 100, 80)
        );
        assert_eq!((down.width(), down.height()), (80, 70));
    }

    #[test]
    fn a_click_without_a_drag_is_not_a_selection() {
        assert!(!Area::from_drag((50, 50), (50, 50)).is_usable());
        assert!(!Area::from_drag((50, 50), (51, 50)).is_usable());
        assert!(Area::from_drag((50, 50), (52, 52)).is_usable());
    }

    #[test]
    fn a_drag_past_the_edge_is_clamped_not_inverted() {
        let area = Area::from_drag((10, 10), (-40, -40)).clamped(200, 100);
        assert_eq!((area.left, area.top, area.right, area.bottom), (0, 0, 10, 10));
        let area = Area::from_drag((190, 90), (900, 900)).clamped(200, 100);
        assert_eq!(
            (area.left, area.top, area.right, area.bottom),
            (190, 90, 200, 100)
        );
    }

    #[test]
    fn the_frame_is_four_bands_that_leave_the_middle_alone() {
        let selection = Area {
            left: 10,
            top: 20,
            right: 110,
            bottom: 90,
        };
        let bands = border_bands(selection, 2);
        // The middle of the selection is in none of them, so the captured
        // pixels there are never overdrawn by our own chrome.
        assert!(bands.iter().all(|band| !band.contains(60, 50)));
        // …but every edge is: the frame is drawn outside the selection, so the
        // bright area equals what will be captured.
        assert!(bands.iter().any(|band| band.contains(8, 50)), "left band");
        assert!(bands.iter().any(|band| band.contains(111, 50)), "right band");
        assert!(bands.iter().any(|band| band.contains(60, 18)), "top band");
        assert!(bands.iter().any(|band| band.contains(60, 91)), "bottom band");
        assert!(
            bands.iter().all(|band| {
                band.right <= selection.left
                    || band.left >= selection.right
                    || band.bottom <= selection.top
                    || band.top >= selection.bottom
            }),
            "no band may cover a pixel of the selection"
        );
    }

    #[test]
    fn the_badge_flips_above_or_below_whichever_fits() {
        let roomy = Area {
            left: 100,
            top: 300,
            right: 400,
            bottom: 500,
        };
        let (_, top) = badge_origin(roomy, (70, 24), (1920, 1080));
        assert!(top < roomy.top, "there is room above, so it goes there");

        let at_the_top = Area {
            left: 100,
            top: 4,
            right: 400,
            bottom: 200,
        };
        let (_, top) = badge_origin(at_the_top, (70, 24), (1920, 1080));
        assert!(top > at_the_top.bottom, "no room above, so it flips below");
    }

    #[test]
    fn the_badge_stays_inside_the_window() {
        let at_the_edge = Area {
            left: 1880,
            top: 100,
            right: 1920,
            bottom: 200,
        };
        let (left, _) = badge_origin(at_the_edge, (70, 24), (1920, 1080));
        assert!(left + 70 <= 1920, "the badge must not run off the right edge");
        assert!(left >= 0);
    }

}
