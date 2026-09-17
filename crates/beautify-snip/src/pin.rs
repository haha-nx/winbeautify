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
//! `StretchDIBits` scales straight out of the original capture, so zooming never
//! reallocates or resamples a buffer: only the window changes size. The stretch
//! mode is `COLORONCOLOR` (nearest neighbour), which is what a screenshot tool
//! wants — a magnified screenshot should show the pixel grid it actually has,
//! not a smoothed guess at it.

use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FrameRect, SelectObject, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT,
    SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE,
    VK_LEFT, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    CS_DBLCLKS, GWLP_USERDATA, IDC_ARROW, IDC_SIZEALL, MSG, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_NOZORDER, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::Shot;

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.SnipPin");

/// Smallest and largest zoom, as a multiple of the original pixels.
const MIN_SCALE: f32 = 0.1;
const MAX_SCALE: f32 = 8.0;
/// One wheel notch multiplies the zoom by this.
const ZOOM_STEP: f32 = 1.1;
/// The live pins, so the whole set can be dismissed at shutdown.
///
/// Handles rather than window objects: each pin lives on its own thread, and all
/// this registry does is post a close message to each.
fn registry() -> &'static Mutex<Vec<isize>> {
    static PINS: OnceLock<Mutex<Vec<isize>>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock_registry() -> std::sync::MutexGuard<'static, Vec<isize>> {
    registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Close every pinned image. Safe to call when there are none.
pub fn close_all() {
    let handles: Vec<isize> = std::mem::take(&mut lock_registry());
    for raw in handles {
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
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
        dragging: None,
    }));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    lock_registry().push(hwnd.0 as isize);

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
    lock_registry().retain(|entry| *entry != hwnd.0 as isize);
    Ok(())
}

fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// Everything one pinned image owns.
struct Pin {
    hwnd: HWND,
    /// The capture at its original size. Scaling happens inside
    /// `StretchDIBits`, so this buffer is never resampled or copied.
    shot: Shot,
    scale: f32,
    /// Cached backing store, rebuilt only when the window changes size.
    frame: Option<(HDC, HBITMAP, HGDIOBJ)>,
    /// The client size `frame` was built for.
    frame_size: (i32, i32),
    /// Where inside the window the drag started, in client coordinates.
    dragging: Option<(i32, i32)>,
}

impl Pin {
    /// The window size the current zoom calls for.
    fn wanted_size(&self) -> (i32, i32) {
        (
            ((self.shot.width as f32 * self.scale).round() as i32).max(1),
            ((self.shot.height as f32 * self.scale).round() as i32).max(1),
        )
    }

    /// Make sure the backing store matches the client area, and return it.
    fn frame(&mut self, window: HDC) -> Option<(HDC, HBITMAP)> {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
        }
        let size = (client.right.max(1), client.bottom.max(1));
        if self.frame.is_none() || self.frame_size != size {
            if let Some((dc, bitmap, previous)) = self.frame.take() {
                unsafe {
                    SelectObject(dc, previous);
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(dc);
                }
            }
            let dc = unsafe { CreateCompatibleDC(Some(window)) };
            if dc.is_invalid() {
                return None;
            }
            let bitmap = unsafe { CreateCompatibleBitmap(window, size.0, size.1) };
            if bitmap.0.is_null() {
                unsafe { let _ = DeleteDC(dc); };
                return None;
            }
            let previous = unsafe { SelectObject(dc, bitmap.into()) };
            // Nearest neighbour: a magnified screenshot should show the pixels
            // it actually has rather than a smoothed guess.
            unsafe { SetStretchBltMode(dc, COLORONCOLOR) };
            self.frame = Some((dc, bitmap, previous));
            self.frame_size = size;
        }
        self.frame.as_ref().map(|(dc, bitmap, _)| (*dc, *bitmap))
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
        let (width, height) = self.wanted_size();
        let x = origin_x + focus_x - (image_x * next).round() as i32;
        let y = origin_y + focus_y - (image_y * next).round() as i32;
        unsafe {
            // A size change makes Windows invalidate the window, so the new
            // frame is drawn without asking.
            let _ = SetWindowPos(self.hwnd, None, x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
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

impl Drop for Pin {
    fn drop(&mut self) {
        if let Some((dc, bitmap, previous)) = self.frame.take() {
            unsafe {
                SelectObject(dc, previous);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
            }
        }
    }
}

/// Draw the image and its border into the backing store, then blit the invalid
/// part across.
fn paint(pin: &mut Pin, paint_struct: &PAINTSTRUCT) {
    let Some((memory, _bitmap)) = pin.frame(paint_struct.hdc) else {
        return;
    };
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: pin.shot.width,
            biHeight: -pin.shot.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let (width, height) = pin.frame_size;
    unsafe {
        StretchDIBits(
            memory,
            0,
            0,
            width,
            height,
            0,
            0,
            pin.shot.width,
            pin.shot.height,
            Some(pin.shot.bgra.as_ptr() as *const core::ffi::c_void),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
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
            with_pin(hwnd, |pin| pin.dragging = Some((x, y)));
            unsafe { SetCapture(hwnd) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_of(lparam);
            with_pin(hwnd, |pin| {
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
            let dragging = with_pin(hwnd, |pin| pin.dragging.is_some()).unwrap_or(false);
            unsafe {
                let id = if dragging { IDC_SIZEALL } else { IDC_ARROW };
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
            dragging: None,
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
    fn the_capture_is_never_resampled_in_place() {
        // Scaling happens at blit time, so the stored pixels stay exactly as
        // captured however far the zoom is pushed.
        let pin = pin_with(3.0, 8, 4);
        assert_eq!((pin.shot.width, pin.shot.height), (8, 4));
        assert_eq!(pin.shot.bgra.len(), 8 * 4 * 4);
    }
}
