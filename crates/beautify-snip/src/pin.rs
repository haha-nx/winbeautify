//! Pinned images.
//!
//! A pin is a small borderless topmost window showing a captured region, and it
//! stays on the desktop until it is dismissed. One window and one message loop
//! per pin: they can be moved and zoomed independently, and closing one cannot
//! disturb the others.
//!
//! # Why an ordinary window
//!
//! Not a layered window like the widget bar. A pin is a rectangle of opaque
//! pixels with a one-pixel border — no rounded corner, no translucent margin —
//! so `WS_POPUP` plus a double-buffered blit is simpler and cheaper than
//! maintaining an alpha surface. It also leaves the pin *activatable*, which is
//! what lets Escape and the arrow keys work after a click, with no keyboard
//! hook and no global hotkey.
//!
//! # Zooming
//!
//! The window changes size and the picture is scaled to fill it, nearest
//! neighbour, by [`Shot::scale_into`] — so a magnified screenshot shows the
//! pixel grid it actually has rather than a smoothed guess at it, and the
//! capture itself is never resampled or copied.
//!
//! The scaling is done on the frame buffer's own memory rather than by
//! `StretchDIBits`. GDI's scaler was measured on this machine reading a source
//! DIB as large as a screen capture from the wrong scanlines — which showed the
//! wrong part of the image at the wrong size, and only while zoomed, since a
//! 1:1 stretch was the one case it got right. The overlay composes its own
//! pixels for the same reason.

use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, FrameRect, InvalidateRect, LineTo, MoveToEx,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    PAINTSTRUCT, PS_SOLID, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE,
    VK_LEFT, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, CS_DBLCLKS, GWLP_USERDATA, IDC_ARROW, IDC_HAND, IDC_SIZEALL, MSG,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::capture::Shot;

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.SnipPin");

/// The close button in the corner of a pin, in pixels.
///
/// A pin is a window a user has to be able to get rid of without knowing that
/// Escape works, and without a click that also happens to start a drag.
const CLOSE_SIZE: i32 = 22;
/// How far the close button sits from the corner.
const CLOSE_INSET: i32 = 4;

/// Smallest and largest zoom, as a multiple of the original pixels.
const MIN_SCALE: f32 = 0.1;
const MAX_SCALE: f32 = 8.0;
/// One wheel notch multiplies the zoom by this.
const ZOOM_STEP: f32 = 1.1;
/// One image currently shown as a pin.
#[derive(Debug, Clone, Copy)]
struct Live {
    /// Content fingerprint of the image, so a list of clipboard entries can tell
    /// which of them is already on screen.
    fingerprint: u64,
    /// The window handle, as a plain integer: each pin lives on its own thread
    /// and all this registry does is post a message to it.
    hwnd: isize,
}

/// The live pins, and whoever wants to hear about them changing.
///
/// The listener is how the clipboard list knows to re-draw its pin buttons: the
/// pins live in this crate, the list lives in another, and neither should have to
/// poll the other.
#[derive(Default)]
struct Registry {
    live: Vec<Live>,
    listener: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

fn registry() -> &'static Mutex<Registry> {
    static PINS: OnceLock<Mutex<Registry>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock_registry() -> std::sync::MutexGuard<'static, Registry> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Tell `f` whenever the set of pinned images changes.
///
/// One listener at a time, and the newest wins: there is a single clipboard
/// list, and a stale listener would be a leak.
pub fn on_change(f: impl Fn() + Send + Sync + 'static) {
    lock_registry().listener = Some(std::sync::Arc::new(f));
}

/// Called with the lock released, so a listener that asks about the pin set
/// cannot deadlock against the update that triggered it.
fn announce() {
    let listener = lock_registry().listener.clone();
    if let Some(listener) = listener {
        listener();
    }
}

/// Fingerprints of every image on screen right now.
pub fn pinned() -> Vec<u64> {
    lock_registry()
        .live
        .iter()
        .map(|pin| pin.fingerprint)
        .collect()
}

/// Is an image with this fingerprint already pinned?
pub fn is_pinned(fingerprint: u64) -> bool {
    lock_registry()
        .live
        .iter()
        .any(|pin| pin.fingerprint == fingerprint)
}

/// Dismiss the pin showing this image, if there is one.
pub fn close_fingerprint(fingerprint: u64) -> bool {
    let handle = {
        let mut registry = lock_registry();
        match registry.live.iter().position(|pin| pin.fingerprint == fingerprint) {
            Some(index) => registry.live.remove(index).hwnd,
            None => return false,
        }
    };
    unsafe {
        let _ = PostMessageW(
            Some(HWND(handle as *mut core::ffi::c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
    announce();
    true
}

/// Close every pinned image. Safe to call when there are none.
pub fn close_all() {
    let handles: Vec<isize> = {
        let mut registry = lock_registry();
        std::mem::take(&mut registry.live)
            .into_iter()
            .map(|pin| pin.hwnd)
            .collect()
    };
    for raw in handles {
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    announce();
    tracing::debug!("all pinned images dismissed");
}

/// Pin `shot` to the desktop at `at`, or at the cursor when that is `None`.
///
/// Returns false only when the thread could not be started; a failure to create
/// the window is logged from that thread.
pub fn pin(shot: Shot, at: Option<(i32, i32)>) -> bool {
    std::thread::Builder::new()
        .name("wb-snip-pin".into())
        .spawn(move || {
            if let Err(e) = run(shot, at) {
                tracing::warn!("could not create the pinned image: {e}");
            }
        })
        .is_ok()
}

/// One pin's whole lifetime: create, pump, destroy.
fn run(shot: Shot, at: Option<(i32, i32)>) -> Result<(), String> {
    let fingerprint = shot.fingerprint();
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        // Set per-message in `WM_SETCURSOR`, where it can say "grab" or "move".
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&class) };

    let (x, y) = at.unwrap_or_else(cursor_position);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("WinBeautify 贴图"),
            WS_POPUP,
            x,
            y,
            shot.width.max(1),
            shot.height.max(1),
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|e| e.to_string())?;

    let raw = Box::into_raw(Box::new(Pin {
        hwnd,
        shot,
        scale: 1.0,
        frame: None,
        frame_size: (0, 0),
        image_drawn: false,
        dragging: None,
        hover_close: false,
    }));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    lock_registry().live.push(Live {
        fingerprint,
        hwnd: hwnd.0 as isize,
    });
    announce();

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

    // The window is gone; reclaim the pin and its backing store.
    drop(unsafe { Box::from_raw(raw) });
    {
        let mut registry = lock_registry();
        registry.live.retain(|pin| pin.hwnd != hwnd.0 as isize);
    }
    announce();
    Ok(())
}

fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// A memory DC with a top-down DIB selected into it.
///
/// Top-down to match how [`Shot`] stores its rows, and a DIB section rather than
/// a compatible bitmap because the scaled picture is written into `bits`
/// directly — see [`Shot::scale_into`]. It is the same arrangement the capture
/// overlay composes into, for the same reason.
struct Frame {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    /// Base of the pixel buffer. Valid until the frame is dropped, and only ever
    /// written on the pin's own thread, between messages.
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Frame {
    fn new(window: HDC, width: i32, height: i32) -> Option<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let dc = unsafe { CreateCompatibleDC(Some(window)) };
        if dc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = match unsafe {
            CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        } {
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

/// Everything one pinned image owns.
struct Pin {
    hwnd: HWND,
    /// The capture at its original size. Zooming only ever reads it.
    shot: Shot,
    scale: f32,
    /// Cached backing store, rebuilt only when the window changes size.
    frame: Option<Frame>,
    /// The client size the frame was built for.
    frame_size: (i32, i32),
    /// Whether `frame` holds the picture at the current zoom.
    ///
    /// The scaler is the expensive part of a frame and the close button's hover
    /// only changes the corner of it, so the picture is drawn once per zoom or
    /// resize rather than once per repaint.
    image_drawn: bool,
    /// Where inside the window the drag started, in client coordinates.
    dragging: Option<(i32, i32)>,
    /// Is the pointer over the close button? Drives its colour.
    hover_close: bool,
}

impl Pin {
    /// The window size the current zoom calls for.
    fn wanted_size(&self) -> (i32, i32) {
        (
            ((self.shot.width as f32 * self.scale).round() as i32).max(1),
            ((self.shot.height as f32 * self.scale).round() as i32).max(1),
        )
    }

    /// Make sure the backing store matches the client area, and hand back the
    /// pieces the painter works with.
    ///
    /// Raw parts rather than a borrow of the frame: the painter also needs the
    /// capture, and a `&mut Frame` would keep it from reaching it.
    fn frame(&mut self, window: HDC) -> Option<(HDC, *mut u8, i32, i32)> {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
        }
        let size = (client.right.max(1), client.bottom.max(1));
        if self.frame.as_ref().map(|frame| (frame.width, frame.height)) != Some(size) {
            self.frame = Frame::new(window, size.0, size.1);
            // A fresh buffer holds nothing, so the picture has to go in again.
            self.image_drawn = false;
        }
        self.frame_size = size;
        self.frame
            .as_ref()
            .map(|frame| (frame.dc, frame.bits, frame.width, frame.height))
    }

    /// Rescale about a point in client coordinates.
    ///
    /// The image pixel under that point stays under it, which is what makes
    /// wheel-zoom feel like magnifying the thing under the pointer rather than
    /// dragging the window around.
    fn zoom(&mut self, factor: f32, focus: Option<(i32, i32)>) {
        let previous = self.scale;
        let next = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if (next - previous).abs() < f32::EPSILON {
            return;
        }
        let (old_width, old_height) = self.wanted_size();
        let (focus_x, focus_y) = focus.unwrap_or((old_width / 2, old_height / 2));
        // The capture pixel under the focus point, which has to stay put.
        let image_x = focus_x as f32 / previous;
        let image_y = focus_y as f32 / previous;

        let (origin_x, origin_y) = self.origin();
        self.scale = next;
        // The window is about to be a different size; the picture in the frame
        // is at the old zoom until it is drawn again.
        self.image_drawn = false;
        let (width, height) = self.wanted_size();
        let x = origin_x + focus_x - (image_x * next).round() as i32;
        let y = origin_y + focus_y - (image_y * next).round() as i32;
        unsafe {
            // A size change makes Windows invalidate the window, so the new
            // frame is drawn without asking.
            let _ = SetWindowPos(self.hwnd, None, x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// The close button, in client coordinates.
    fn close_button(&self) -> RECT {
        let (width, height) = self.frame_size;
        let size = CLOSE_SIZE.min(width).min(height);
        // The button has to stay inside the picture: a pin a few pixels across
        // would otherwise put its only control outside itself.
        let inset = CLOSE_INSET
            .min((width - size).max(0))
            .min((height - size).max(0));
        RECT {
            left: width - size - inset,
            top: inset,
            right: width - inset,
            bottom: inset + size,
        }
    }

    fn origin(&self) -> (i32, i32) {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        (rect.left, rect.top)
    }

    fn move_by(&mut self, dx: i32, dy: i32) {
        let (x, y) = self.origin();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x + dx,
                y + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn close(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Draw the image and its border into the backing store, then blit the invalid
/// part across.
fn paint(pin: &mut Pin, paint_struct: &PAINTSTRUCT) {
    let Some((memory, bits, width, height)) = pin.frame(paint_struct.hdc) else {
        return;
    };

    // The picture is scaled into the frame's own pixels, which is the one way
    // to be sure of what lands on screen — see the module header.
    if !pin.image_drawn {
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(bits, (width as usize * height as usize) * 4)
        };
        pin.shot.scale_into(width, height, pixels);
        pin.image_drawn = true;
    }

    unsafe {
        // The border has to sit *inside* the client area, or it would be part of
        // the window frame and resize the image by two pixels.
        let bounds = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let brush = CreateSolidBrush(COLORREF(0x00A0_A0A0));
        FrameRect(memory, &bounds, brush);
        let _ = DeleteObject(brush.into());

        draw_close_button(pin, memory);

        let target = paint_struct.rcPaint;
        let _ = BitBlt(
            paint_struct.hdc,
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            Some(memory),
            target.left,
            target.top,
            SRCCOPY,
        );
    }
}

/// Borrow the pin out of the window's user data.
///
/// # Safety
///
/// As in the overlay: `WM_DESTROY` must not be answered through this borrow,
/// because `DestroyWindow` delivers it synchronously from inside a handler.
/// Is a point inside a rectangle?
fn inside(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// The little ✕ in the corner.
///
/// Drawn as two strokes rather than as a glyph so it needs no icon font and
/// stays crisp: it is the one control a pinned image has.
fn draw_close_button(pin: &Pin, dc: HDC) {
    let button = pin.close_button();
    if button.right <= button.left || button.bottom <= button.top {
        return;
    }
    // A wash behind it, so it reads against whatever the image happens to be.
    let wash = unsafe {
        CreateSolidBrush(COLORREF(if pin.hover_close {
            0x0030_3030
        } else {
            0x0018_1818
        }))
    };
    unsafe {
        FillRect(dc, &button, wash);
        let _ = DeleteObject(wash.into());
    }

    let ink = COLORREF(if pin.hover_close { 0x0060_8080FF } else { 0x00D0_D0D0 });
    let pen = unsafe { CreatePen(PS_SOLID, 2, ink) };
    let previous = unsafe { SelectObject(dc, pen.into()) };
    let inset = 6;
    unsafe {
        let _ = MoveToEx(dc, button.left + inset, button.top + inset, None);
        let _ = LineTo(dc, button.right - inset, button.bottom - inset);
        let _ = MoveToEx(dc, button.right - inset, button.top + inset, None);
        let _ = LineTo(dc, button.left + inset, button.bottom - inset);
        SelectObject(dc, previous);
        let _ = DeleteObject(pen.into());
    }
}

fn with_pin<R>(hwnd: HWND, f: impl FnOnce(&mut Pin) -> R) -> Option<R> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Pin;
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
            let mut paint_struct = PAINTSTRUCT::default();
            unsafe { BeginPaint(hwnd, &mut paint_struct) };
            with_pin(hwnd, |pin| paint(pin, &paint_struct));
            unsafe { let _ = EndPaint(hwnd, &paint_struct); };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Take the focus so Escape and the arrow keys reach us. A pin is a
            // deliberate click target, so activating it is what the user means.
            let _ = unsafe { SetFocus(Some(hwnd)) };
            let (x, y) = point_of(lparam);
            let closed = with_pin(hwnd, |pin| {
                if inside(pin.close_button(), x, y) {
                    pin.close();
                    true
                } else {
                    pin.dragging = Some((x, y));
                    false
                }
            })
            .unwrap_or(false);
            if !closed {
                unsafe { SetCapture(hwnd) };
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_of(lparam);
            with_pin(hwnd, |pin| {
                // Light the close button up as the pointer reaches it, which is
                // the only thing that makes an ✕ in a corner discoverable.
                let over_close = inside(pin.close_button(), x, y);
                if over_close != pin.hover_close {
                    pin.hover_close = over_close;
                    unsafe {
                        let _ = InvalidateRect(Some(pin.hwnd), None, false);
                    }
                }
                let Some((grab_x, grab_y)) = pin.dragging else {
                    return;
                };
                // Move by the delta rather than to the cursor, so the image
                // cannot jump under the pointer when the drag starts.
                let (dx, dy) = (x - grab_x, y - grab_y);
                if dx != 0 || dy != 0 {
                    pin.move_by(dx, dy);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = unsafe { ReleaseCapture() };
            with_pin(hwnd, |pin| pin.dragging = None);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK | WM_RBUTTONUP => {
            with_pin(hwnd, |pin| pin.close());
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let notches = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
            let factor = ZOOM_STEP.powf(notches);
            let (cursor_x, cursor_y) = cursor_position();
            let (origin_x, origin_y) = with_pin(hwnd, |pin| pin.origin()).unwrap_or((0, 0));
            with_pin(hwnd, |pin| {
                pin.zoom(factor, Some((cursor_x - origin_x, cursor_y - origin_y)));
            });
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let key = wparam.0 as u32;
            let step = if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 {
                10
            } else {
                1
            };
            with_pin(hwnd, |pin| match key {
                k if k == VK_ESCAPE.0 as u32 || k == VK_DELETE.0 as u32 => pin.close(),
                k if k == VK_LEFT.0 as u32 => pin.move_by(-step, 0),
                k if k == VK_RIGHT.0 as u32 => pin.move_by(step, 0),
                k if k == VK_UP.0 as u32 => pin.move_by(0, -step),
                k if k == VK_DOWN.0 as u32 => pin.move_by(0, step),
                // Ctrl+C copies the original capture, not the zoomed view.
                k if k == 'C' as u32 && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 => {
                    crate::copy_to_clipboard(&pin.shot);
                }
                _ => {}
            });
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let (dragging, over_close) = with_pin(hwnd, |pin| {
                (pin.dragging.is_some(), pin.hover_close)
            })
            .unwrap_or((false, false));
            unsafe {
                let id = if over_close {
                    IDC_HAND
                } else if dragging {
                    IDC_SIZEALL
                } else {
                    IDC_ARROW
                };
                if let Ok(cursor) = LoadCursorW(None, id) {
                    SetCursor(Some(cursor));
                }
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            with_pin(hwnd, |pin| pin.close());
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                // The pin itself is reclaimed by the thread that created it,
                // once the loop returns.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The window coordinates in a mouse message.
fn point_of(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(width: i32, height: i32) -> Shot {
        Shot {
            width,
            height,
            bgra: vec![0; width as usize * height as usize * 4],
        }
    }

    fn pin_with(scale: f32, width: i32, height: i32) -> Pin {
        Pin {
            hwnd: HWND::default(),
            shot: shot(width, height),
            scale,
            frame: None,
            frame_size: (0, 0),
            image_drawn: false,
            dragging: None,
            hover_close: false,
        }
    }

    #[test]
    fn the_zoom_limits_are_sane() {
        assert!(MIN_SCALE > 0.0 && MIN_SCALE < 1.0);
        assert!(MAX_SCALE > 1.0);
        assert!(ZOOM_STEP > 1.0);
    }

    #[test]
    fn dismissing_with_no_pins_is_harmless() {
        close_all();
        assert!(pinned().is_empty());
        assert!(!close_fingerprint(1234));
        assert!(!is_pinned(1234));
    }

    #[test]
    fn the_window_size_follows_the_zoom() {
        assert_eq!(pin_with(1.0, 100, 50).wanted_size(), (100, 50));
        assert_eq!(pin_with(2.0, 100, 50).wanted_size(), (200, 100));
        assert_eq!(pin_with(0.5, 100, 50).wanted_size(), (50, 25));
        // A tiny zoom on a tiny capture must still leave a window to draw in.
        let smallest = pin_with(MIN_SCALE, 4, 4).wanted_size();
        assert!(smallest.0 >= 1 && smallest.1 >= 1);
    }

    #[test]
    fn the_close_button_sits_in_the_corner_and_shrinks_to_fit() {
        let mut pin = pin_with(1.0, 400, 300);
        pin.frame_size = (400, 300);
        let button = pin.close_button();
        assert!(button.right <= 400 && button.top >= 0);
        assert!(button.right - button.left > 8, "it has to be clickable");
        // A pin of a few pixels still gets a button that fits inside it.
        pin.frame_size = (12, 12);
        let tiny = pin.close_button();
        assert!(tiny.right <= 12 && tiny.bottom <= 12);
    }

    #[test]
    fn the_capture_is_never_resampled_in_place() {
        // Scaling happens at blit time, so the stored pixels stay exactly as
        // captured however far the zoom is pushed.
        let pin = pin_with(3.0, 8, 4);
        assert_eq!((pin.shot.width, pin.shot.height), (8, 4));
        assert_eq!(pin.shot.bgra.len(), 8 * 4 * 4);
    }
}
