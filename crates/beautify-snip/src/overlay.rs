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
    GetStockObject, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetStretchBltMode,
    SetTextColor, StretchDIBits, BACKGROUND_MODE, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    COLORONCOLOR, DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, LoadCursorW, PostMessageW,
    PostQuitMessage,
    RegisterClassExW, SetCursor, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    SetWindowsHookExW, ShowWindow, SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx,
    CS_DBLCLKS, GWLP_USERDATA, HC_ACTION, HWND_TOPMOST, IDC_CROSS, KBDLLHOOKSTRUCT, MSG,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SWP_SHOWWINDOW, SW_SHOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WH_KEYBOARD_LL, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR,
    WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
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

    #[cfg(test)]
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    const EMPTY: Self = Self {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };

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

    /// The bounding box of two areas, for repainting only what moved.
    ///
    /// An empty operand is ignored, so "nothing here" can be unioned with the
    /// real rectangles without dragging the result towards the origin.
    fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
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

/// A memory DC with a top-down DIB selected into it.
///
/// Top-down to match how [`Shot`] stores its rows, which is what lets
/// `StretchDIBits` move a region from the shot to the frame with no flipping.
struct Frame {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    /// Base of the pixel buffer, kept so a test can read back what a blit
    /// actually wrote.
    #[cfg(test)]
    bits: *mut u8,
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
        // The magnifier scales a patch of the capture into the frame, and a
        // screenshot tool is expected to show the pixel grid rather than a
        // smoothed guess at it.
        unsafe { SetStretchBltMode(dc, COLORONCOLOR) };
        Some(Self {
            dc,
            bitmap,
            previous,
            #[cfg(test)]
            bits: bits as *mut u8,
        })
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
    /// The foreground window from before the hotkey, restored on the way out.
    previous_foreground: HWND,
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
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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

    let session = Box::new(Session {
        hwnd,
        // `Shot::dim` premultiplies, so a plain SRCCOPY blit of the result is
        // already the darkened pixel: no alpha blending anywhere.
        dimmed: screen.dim(options.dim),
        screen,
        origin: (rect.left, rect.top),
        frame,
        anchor: (0, 0),
        selection: None,
        dragging: false,
        cursor: None,
        previous_foreground: unsafe { GetForegroundWindow() },
        font: message_font(),
        options,
        result: None,
        finished: false,
    });
    // The whole frame is composed before the window is shown, or the first paint
    // would blit an uninitialised (black) buffer.
    compose(
        &session,
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
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            width,
            height,
            SWP_SHOWWINDOW,
        );
        // Best effort: refused whenever the hotkey was pressed while another
        // application held the foreground, which is the normal case. Nothing
        // depends on it — the mouse is captured explicitly, and Escape is caught
        // by the hook below.
        let _ = SetForegroundWindow(hwnd);
        SetCapture(hwnd);
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
        repaint(session, full);
    });
    HOOK_TARGET.store(hwnd.0 as isize, Ordering::Release);
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) }.ok();

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
    let previous_foreground = session.previous_foreground;
    // The crop happens while the session still owns the undimmed pixels.
    let shot = area.and_then(|area| {
        session
            .screen
            .crop(area.left, area.top, area.width(), area.height())
    });
    let pin_at = area.map(|area| (origin.0 + area.left, origin.1 + area.top));
    drop(session);

    if !previous_foreground.is_invalid() {
        unsafe {
            let _ = SetForegroundWindow(previous_foreground);
        }
    }

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

/// Swallow exactly one key — Escape — while a session is up.
///
/// The overlay usually cannot take the foreground, so this is the only way
/// Escape reaches it. See the module comment for why that is.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_KEYDOWN {
        let event = lparam.0 as *const KBDLLHOOKSTRUCT;
        if !event.is_null() && unsafe { (*event).vkCode } == VK_ESCAPE.0 as u32 {
            let target = HOOK_TARGET.load(Ordering::Acquire);
            if target != 0 {
                // Posted rather than handled here: the hook runs inside another
                // message's dispatch, and destroying the window from there would
                // unwind back through the hook.
                let hwnd = HWND(target as *mut core::ffi::c_void);
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                // Swallow it: while the overlay is up, Escape belongs to us.
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Compose `area` of the frame, then ask for exactly that much to be blitted.
fn repaint(session: &mut Session, area: Area) {
    let (width, height) = (session.screen.width, session.screen.height);
    let area = area.clamped(width, height);
    if area.width() <= 0 || area.height() <= 0 {
        return;
    }
    compose(session, area);
    let rect = area.to_rect();
    unsafe {
        let _ = InvalidateRect(Some(session.hwnd), Some(&rect), false);
    }
}

/// Draw `area` of the frame: the dimmed screen, the bright selection over it,
/// then the selection's frame.
fn compose(session: &Session, area: Area) {
    let dc = session.frame.dc;
    // 1. The darkened screen, so anything the selection no longer covers goes
    //    back to reading as "not taken".
    blit_dib(dc, &session.dimmed, area);

    // 2. The selection, if there is one: the same rectangle at full brightness,
    //    so the part that will be taken stands out from the part that will not.
    if let Some(selection) = session.selection {
        let bright = Area {
            left: area.left.max(selection.left),
            top: area.top.max(selection.top),
            right: area.right.min(selection.right),
            bottom: area.bottom.min(selection.bottom),
        };
        if bright.width() > 0 && bright.height() > 0 {
            blit_dib(dc, &session.screen, bright);
        }

        // A thin frame around it, drawn as four bands so the pixels inside stay
        // exactly as captured.
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
        draw_magnifier(session, dc, area, cursor);
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

/// The bounding box of everything in [`furniture`], for invalidating it.
fn furniture_bounds(session: &Session) -> Area {
    furniture(session)
        .into_iter()
        .fold(Area::EMPTY, Area::union)
        .clamped(session.screen.width, session.screen.height)
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

/// The magnifier: the pixels around the pointer, blown up, with the pixel under
/// it marked and its colour written underneath.
///
/// This is the part of a screenshot tool that makes a selection land on the
/// pixel you meant, which guessing at full-screen zoom never does.
fn draw_magnifier(session: &Session, dc: HDC, area: Area, cursor: (i32, i32)) {
    let plate = magnifier_area(session, cursor);
    if !intersects(plate, area.to_rect()) {
        return;
    }
    let patch = Area {
        left: plate.left,
        top: plate.top,
        right: plate.right,
        bottom: plate.bottom - 18,
    };
    let zoom = patch.width() / MAGNIFIER_SOURCE;
    // The source rectangle, relative to the pointer and clamped to the picture
    // so a corner does not read outside it.
    let half = MAGNIFIER_SOURCE / 2;
    let source_x = (cursor.0 - half).clamp(0, (session.screen.width - MAGNIFIER_SOURCE).max(0));
    let source_y = (cursor.1 - half).clamp(0, (session.screen.height - MAGNIFIER_SOURCE).max(0));

    blit_dib_scaled(
        dc,
        &session.screen,
        Area {
            left: source_x,
            top: source_y,
            right: source_x + MAGNIFIER_SOURCE,
            bottom: source_y + MAGNIFIER_SOURCE,
        },
        patch,
    );

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

/// Copy a region of a shot into `destination`, scaled to fit it.
fn blit_dib_scaled(dc: HDC, shot: &Shot, source: Area, destination: Area) {
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
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: shot.width,
            biHeight: -shot.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        StretchDIBits(
            dc,
            destination.left,
            destination.top,
            destination.width(),
            destination.height(),
            source.left,
            source.top,
            source.width(),
            source.height(),
            Some(shot.bgra.as_ptr() as *const core::ffi::c_void),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
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

/// Copy the part of `shot` that `area` covers into the frame at the same place.
fn blit_dib(dc: HDC, shot: &Shot, area: Area) {
    if area.width() <= 0 || area.height() <= 0 {
        return;
    }
    if area.left < 0 || area.top < 0 || area.right > shot.width || area.bottom > shot.height {
        return;
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: shot.width,
            biHeight: -shot.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        StretchDIBits(
            dc,
            area.left,
            area.top,
            area.width(),
            area.height(),
            area.left,
            area.top,
            area.width(),
            area.height(),
            Some(shot.bgra.as_ptr() as *const core::ffi::c_void),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
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
                // has to be repainted along with the new selection.
                let prompt = furniture_bounds(session);
                session.dragging = true;
                session.anchor = (x, y);
                session.cursor = Some((x, y));
                // Wipe the previous selection's frame even if the new drag never
                // grows: the first frame of a drag has no area yet.
                let here = Area {
                    left: x,
                    top: y,
                    right: x,
                    bottom: y,
                };
                let (width, height) = (session.screen.width, session.screen.height);
                let dirty = match session.selection.take() {
                    Some(previous) => previous.inflated(8, width, height).union(here),
                    None => here,
                };
                repaint(session, dirty.union(prompt).union(furniture_bounds(session)));
            });
            unsafe { SetCapture(hwnd) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_of(lparam);
            with_session(hwnd, |session| {
                // The crosshair, the magnifier and the prompt all follow the
                // pointer, so what they covered a moment ago has to be repainted
                // along with what they cover now.
                let stale = furniture_bounds(session);
                session.cursor = Some((x, y));
                let fresh = furniture_bounds(session);

                if !session.dragging {
                    repaint(session, stale.union(fresh));
                    return;
                }
                let (width, height) = (session.screen.width, session.screen.height);
                let next = Area::from_drag(session.anchor, (x, y)).clamped(width, height);
                let previous = session.selection.replace(next);
                // Only what changed is recomposed: this runs per mouse event, and
                // a full frame is tens of megabytes.
                let dirty = match previous {
                    Some(previous) => previous
                        .inflated(8, width, height)
                        .union(next.inflated(8, width, height)),
                    None => next.inflated(8, width, height),
                };
                repaint(session, dirty.union(stale).union(fresh));
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
        WM_SETCURSOR => {
            // Answering without deferring keeps Windows from resetting the
            // crosshair to the class arrow whenever the mouse moves.
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_CROSS) {
                    SetCursor(Some(cursor));
                }
            }
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

    /// A region blit has to land on the rows that were asked for.
    ///
    /// This is the check for the worst bug this window has had: the overlay's
    /// dimmed background and its bright selection are both *sub-rectangles* of
    /// one captured DIB, and if the source rectangle of such a blit is
    /// interpreted against the wrong end of the bitmap, the picture the user
    /// drags over has nothing to do with the picture underneath it — while the
    /// file that comes out (a plain crop of the buffer) is perfectly correct.
    #[test]
    #[ignore = "needs a desktop session (a screen DC)"]
    fn a_region_blit_lands_on_the_rows_it_was_asked_for() {
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

        let window_dc = unsafe { GetDC(None) };
        let frame = Frame::new(window_dc, W, H).expect("a frame");
        let area = Area {
            left: 0,
            top: 4,
            right: W,
            bottom: 12,
        };
        blit_dib(frame.dc, &shot, area);
        unsafe { ReleaseDC(None, window_dc) };

        let blue = |y: i32| unsafe { *frame.bits.add((y * W * 4) as usize) };
        for y in area.top..area.bottom {
            assert_eq!(
                blue(y),
                y as u8,
                "row {y} of the frame holds row {} of the shot: the source                  rectangle is being read from the wrong end",
                blue(y)
            );
        }
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

    #[test]
    fn the_union_covers_both_rectangles() {
        let a = Area {
            left: 10,
            top: 10,
            right: 20,
            bottom: 20,
        };
        let b = Area {
            left: 50,
            top: 5,
            right: 60,
            bottom: 40,
        };
        let union = a.union(b);
        assert_eq!(
            (union.left, union.top, union.right, union.bottom),
            (10, 5, 60, 40)
        );
    }
}
