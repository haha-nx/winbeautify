//! Layered-window surface for the widget bar.
//!
//! The pill has to be rounded and translucent over the taskbar, which means
//! per-pixel alpha. `WS_EX_LAYERED` + `UpdateLayeredWindow` is the oldest and
//! most reliable way to get that on Windows, and it needs no GPU compositor —
//! which matters, because the DWM/DirectComposition path is exactly what fails
//! on machines without a real display adapter.
//!
//! A useful side effect: **layered windows hit-test by alpha**, so the fully
//! transparent margin around the pill passes clicks straight through to the
//! taskbar. That is what lets the window be created once at its widest and
//! never resized while the bar animates.

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

/// A 32-bit top-down DIB that backs a layered window.
pub struct LayeredSurface {
    hwnd: HWND,
    width: i32,
    height: i32,
    screen_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    /// Start of the pixel buffer; `stride * height` bytes, BGRA premultiplied.
    bits: *mut u8,
    stride: usize,
}

impl LayeredSurface {
    /// Create a surface for `hwnd`.
    ///
    /// # Safety
    ///
    /// `hwnd` must be a valid top-level window created with `WS_EX_LAYERED` and
    /// must outlive the returned surface.
    pub unsafe fn new(hwnd: HWND, width: i32, height: i32) -> Result<Self, String> {
        let width = width.max(1);
        let height = height.max(1);
        let screen_dc = unsafe { GetDC(None) };
        if screen_dc.is_invalid() {
            return Err("GetDC failed".into());
        }
        let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        if memory_dc.is_invalid() {
            unsafe { ReleaseDC(None, screen_dc) };
            return Err("CreateCompatibleDC failed".into());
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height selects a top-down DIB, matching the row order
                // WIC hands back, so the copy below needs no flipping.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = match unsafe { CreateDIBSection(Some(memory_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                unsafe {
                    let _ = DeleteDC(memory_dc);
                    ReleaseDC(None, screen_dc);
                }
                return Err("CreateDIBSection failed".into());
            }
        };
        let previous = unsafe { SelectObject(memory_dc, bitmap.into()) };

        Ok(Self {
            hwnd,
            width,
            height,
            screen_dc,
            memory_dc,
            bitmap,
            previous,
            bits: bits as *mut u8,
            stride: width as usize * 4,
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Replace the pixel buffer and push it to the screen.
    ///
    /// `source` is premultiplied BGRA, top-down, `stride` bytes per row — which
    /// is exactly what a `32bppPBGRA` WIC bitmap locks as. The result is handed
    /// back rather than dropped: `UpdateLayeredWindow` failing is the difference
    /// between a bar that is drawn and one that is not on the screen at all, and
    /// nothing else in the process can tell.
    pub fn present(
        &mut self,
        source: &[u8],
        source_stride: usize,
        x: i32,
        y: i32,
    ) -> windows::core::Result<()> {
        let rows = self.height as usize;
        let copy_row = self.stride.min(source_stride);
        for row in 0..rows {
            let src = row * source_stride;
            let dst = row * self.stride;
            if src + copy_row > source.len() {
                break;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr().add(src),
                    self.bits.add(dst),
                    copy_row,
                );
            }
        }
        self.flush(x, y)
    }

    /// Clear the surface to fully transparent and push it.
    pub fn clear(&mut self, x: i32, y: i32) -> windows::core::Result<()> {
        unsafe {
            std::ptr::write_bytes(self.bits, 0, self.stride * self.height as usize);
        }
        self.flush(x, y)
    }

    fn flush(&self, x: i32, y: i32) -> windows::core::Result<()> {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen_dc),
                Some(&POINT { x, y }),
                Some(&SIZE {
                    cx: self.width,
                    cy: self.height,
                }),
                Some(self.memory_dc),
                Some(&POINT { x: 0, y: 0 }),
                // The key colour is only consulted for ULW_COLORKEY.
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory_dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory_dc);
            windows::Win32::Graphics::Gdi::ReleaseDC(None, self.screen_dc);
        }
    }
}

// The surface is owned by the widget's pump thread, and the raw pixel pointer
// is only ever touched from there.
unsafe impl Send for LayeredSurface {}
