//! Grabbing pixels off the screen, and the pixel shuffling around it.
//!
//! A screen DC has no alpha channel — every pixel comes back opaque — so the
//! captured buffer is BGRA with `a = 0`. Anything presenting it through a layered
//! window has to fix that up, which is what [`opaque`] and [`dim`] are for.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};

/// A rectangle of pixels, top-down, BGRA.
#[derive(Clone)]
pub struct Shot {
    pub width: i32,
    pub height: i32,
    pub bgra: Vec<u8>,
}

impl Shot {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// A rectangle `(x, y, w, h)` inside this shot, clamped to its bounds.
    ///
    /// Clamping rather than refusing: a selection dragged past the edge of the
    /// screen is a normal thing to do, and the user means "as far as it goes".
    pub fn crop(&self, x: i32, y: i32, width: i32, height: i32) -> Option<Shot> {
        let left = x.clamp(0, self.width);
        let top = y.clamp(0, self.height);
        let right = (x + width).clamp(left, self.width);
        let bottom = (y + height).clamp(top, self.height);
        let (w, h) = (right - left, bottom - top);
        if w <= 0 || h <= 0 {
            return None;
        }

        let stride = self.stride();
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        for row in 0..h as usize {
            let src = (top as usize + row) * stride + left as usize * 4;
            let dst = row * w as usize * 4;
            bgra[dst..dst + w as usize * 4].copy_from_slice(&self.bgra[src..src + w as usize * 4]);
        }
        Some(Shot {
            width: w,
            height: h,
            bgra,
        })
    }

    /// Force every pixel opaque.
    pub fn opaque(&mut self) {
        for pixel in self.bgra.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
    }

    /// Premultiplied `(colour * factor, alpha = factor)`, for dimming.
    ///
    /// Premultiplied because that is what `UpdateLayeredWindow` expects: writing
    /// a straight colour with a low alpha produces a bright halo instead of a
    /// darker pixel.
    pub fn dim(&self, factor: f32) -> Shot {
        let factor = factor.clamp(0.0, 1.0);
        let alpha = (factor * 255.0).round() as u32;
        let mut bgra = self.bgra.clone();
        for pixel in bgra.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * alpha) / 255) as u8;
            }
            pixel[3] = alpha as u8;
        }
        Shot {
            width: self.width,
            height: self.height,
            bgra,
        }
    }

    /// Copy `patch` into this shot at `(x, y)`, replacing what is there.
    ///
    /// Used to punch the selection out of the dimmed overlay: the patch is the
    /// snapshot of the same region at full brightness.
    pub fn blit(&mut self, patch: &Shot, x: i32, y: i32) {
        let stride = self.stride();
        for row in 0..patch.height {
            let dst_y = y + row;
            if dst_y < 0 || dst_y >= self.height {
                continue;
            }
            let dst = dst_y as usize * stride + x.max(0) as usize * 4;
            let src = row as usize * patch.stride();
            let width = patch.width as usize * 4;
            if dst + width > self.bgra.len() || src + width > patch.bgra.len() {
                continue;
            }
            self.bgra[dst..dst + width].copy_from_slice(&patch.bgra[src..src + width]);
        }
    }

    /// Nearest-neighbour scale, used by the pin window's zoom.
    ///
    /// Nearest rather than a filter: the source is pixel art as often as it is a
    /// photograph, and a pixel that is a copy of its neighbour is what the user
    /// expects when they zoom into a screenshot to read small text.
    pub fn scaled(&self, factor: f32) -> Shot {
        let factor = factor.max(0.05);
        let width = ((self.width as f32 * factor).round() as i32).max(1);
        let height = ((self.height as f32 * factor).round() as i32).max(1);
        let stride = self.stride();
        let mut bgra = vec![0u8; width as usize * height as usize * 4];

        for row in 0..height {
            // Clamp so a slight downscale cannot read past the last row.
            let src_y = ((row as f32 / factor) as usize).min(self.height as usize - 1);
            for column in 0..width {
                let src_x = ((column as f32 / factor) as usize).min(self.width as usize - 1);
                let src = src_y * stride + src_x * 4;
                let dst = (row as usize * width as usize + column as usize) * 4;
                bgra[dst..dst + 4].copy_from_slice(&self.bgra[src..src + 4]);
            }
        }
        Shot { width, height, bgra }
    }

    /// Encode as a BMP: a `BITMAPFILEHEADER` in front of a `CF_DIB`.
    ///
    /// Built on top of [`beautify_clipboard::dib`] rather than duplicating the
    /// header layout, and top-down (negative height) to match how the pixels are
    /// already stored, so nothing has to be flipped.
    pub fn to_bmp(&self) -> Option<Vec<u8>> {
        let mut dib = Vec::with_capacity(40 + self.bgra.len());
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: self.width,
            biHeight: -self.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: self.bgra.len() as u32,
            ..Default::default()
        };
        dib.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const BITMAPINFOHEADER as *const u8,
                std::mem::size_of::<BITMAPINFOHEADER>(),
            )
        });
        dib.extend_from_slice(&self.bgra);
        beautify_clipboard::dib::dib_to_bmp(&dib).map(|(bmp, _, _)| bmp)
    }

    /// Encode as a `CF_DIB` payload ready for the clipboard.
    ///
    /// Bottom-up (positive `biHeight`), unlike [`Self::to_bmp`]: `CF_DIB` is a
    /// Win32 interchange format and plenty of consumers still assume the
    /// original bottom-up layout. The rows are written in reverse rather than
    /// flipping the buffer, so the stored shot is untouched.
    pub fn to_dib(&self) -> Vec<u8> {
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: self.width,
            biHeight: self.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: self.bgra.len() as u32,
            ..Default::default()
        };
        let mut dib = Vec::with_capacity(40 + self.bgra.len());
        dib.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                &header as *const BITMAPINFOHEADER as *const u8,
                std::mem::size_of::<BITMAPINFOHEADER>(),
            )
        });
        let stride = self.stride();
        for row in (0..self.height as usize).rev() {
            dib.extend_from_slice(&self.bgra[row * stride..(row + 1) * stride]);
        }
        dib
    }
}

/// Grab a rectangle of the screen.
///
/// `x`/`y` are in virtual-screen coordinates, so a secondary monitor at a
/// negative origin works. `None` when the DC could not be acquired or the
/// rectangle is empty.
pub fn grab(x: i32, y: i32, width: i32, height: i32) -> Option<Shot> {
    if width <= 0 || height <= 0 {
        return None;
    }

    unsafe {
        let screen: HDC = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let memory = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            ReleaseDC(None, screen);
            return None;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height for a top-down DIB, matching the layered
                // window buffers this ends up in.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap: HBITMAP =
            match CreateDIBSection(Some(memory), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) if !bits.is_null() => bitmap,
                _ => {
                    let _ = DeleteDC(memory);
                    ReleaseDC(None, screen);
                    return None;
                }
            };
        let previous: HGDIOBJ = SelectObject(memory, bitmap.into());

        // `CAPTUREBLT` is deliberately not set: it would pull in layered windows
        // (our own widget bar and pins) drawn on top of whatever is being
        // captured.
        let copied = BitBlt(memory, 0, 0, width, height, Some(screen), x, y, SRCCOPY).is_ok();

        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        if copied {
            std::ptr::copy_nonoverlapping(
                bits as *const u8,
                bgra.as_mut_ptr(),
                bgra.len(),
            );
        }

        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);

        if !copied {
            return None;
        }
        Some(Shot {
            width,
            height,
            bgra,
        })
    }
}

/// The bounding box of every monitor, in virtual-screen coordinates.
pub fn virtual_screen() -> Option<Shot> {
    let rect = virtual_screen_rect()?;
    grab(rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top)
}

/// `(left, top, right, bottom)` of the whole desktop.
pub fn virtual_screen_rect() -> Option<RECT> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let (left, top) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(width: i32, height: i32, fill: u8) -> Shot {
        Shot {
            width,
            height,
            bgra: vec![fill; width as usize * height as usize * 4],
        }
    }

    #[test]
    fn cropping_takes_the_requested_corner() {
        let mut source = shot(4, 3, 0);
        // Mark the pixel at (1, 1) so the crop can be checked.
        let stride = source.stride();
        source.bgra[stride + 4] = 0xAB;

        let cropped = source.crop(1, 1, 2, 2).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(cropped.bgra[0], 0xAB, "top-left of the crop is (1,1)");
        assert_eq!(cropped.stride(), 8);
    }

    #[test]
    fn a_selection_dragged_off_screen_is_clamped() {
        let source = shot(4, 3, 7);
        let cropped = source.crop(-2, -1, 4, 4).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 3));
        assert!(source.crop(4, 0, 5, 5).is_none(), "fully outside");
        assert!(source.crop(0, 0, 0, 5).is_none(), "empty");
    }

    #[test]
    fn dimming_is_premultiplied() {
        let source = shot(1, 1, 200);
        let dimmed = source.dim(0.5);
        // Colour and alpha both come down, so the composite is a darker pixel
        // rather than a bright ghost.
        assert_eq!(dimmed.bgra[3], 128);
        assert!(dimmed.bgra[0] <= 101 && dimmed.bgra[0] >= 99, "got {}", dimmed.bgra[0]);
    }

    #[test]
    fn scaling_doubles_and_keeps_the_corners() {
        let mut source = shot(2, 2, 0);
        let stride = source.stride();
        source.bgra[0] = 0x11; // (0,0)
        source.bgra[4] = 0x22; // (1,0)
        source.bgra[stride] = 0x33; // (0,1)

        let doubled = source.scaled(2.0);
        assert_eq!((doubled.width, doubled.height), (4, 4));
        let at = |x: usize, y: usize| doubled.bgra[y * doubled.stride() + x * 4];
        assert_eq!(at(0, 0), 0x11);
        assert_eq!(at(1, 1), 0x11, "nearest-neighbour repeats the source pixel");
        assert_eq!(at(2, 0), 0x22);
        assert_eq!(at(0, 2), 0x33);
    }

    #[test]
    fn scaling_never_produces_an_empty_image() {
        let scaled = shot(10, 10, 1).scaled(0.01);
        assert!(scaled.width >= 1 && scaled.height >= 1);
    }

    #[test]
    fn opaque_fills_the_alpha_channel() {
        let mut source = shot(2, 2, 0);
        source.opaque();
        assert!(source.bgra.chunks_exact(4).all(|p| p[3] == 0xFF));
    }

    #[test]
    fn blitting_replaces_a_region() {
        let mut base = shot(4, 4, 10);
        let patch = shot(2, 2, 99);
        base.blit(&patch, 1, 1);

        let stride = base.stride();
        let at = |x: usize, y: usize| base.bgra[y * stride + x * 4];
        assert_eq!(at(0, 0), 10, "outside the patch");
        assert_eq!(at(1, 1), 99, "inside the patch");
        assert_eq!(at(2, 2), 99);
        assert_eq!(at(3, 3), 10);
    }

    #[test]
    fn the_bmp_header_describes_a_top_down_image() {
        let source = shot(3, 2, 0x40);
        let bmp = source.to_bmp().expect("a valid BMP");
        assert_eq!(&bmp[0..2], b"BM");
        // biHeight sits 8 bytes into the DIB header, which starts at offset 14.
        let height = i32::from_le_bytes(bmp[14 + 8..14 + 12].try_into().unwrap());
        assert_eq!(height, -2, "negative height means top-down");
        let width = i32::from_le_bytes(bmp[14 + 4..14 + 8].try_into().unwrap());
        assert_eq!(width, 3);
    }

    #[test]
    fn the_clipboard_dib_is_bottom_up() {
        let mut source = shot(2, 2, 0);
        let stride = source.stride();
        // Mark the top row and the bottom row differently.
        for byte in &mut source.bgra[0..stride] {
            *byte = 0x11;
        }
        for byte in &mut source.bgra[stride..2 * stride] {
            *byte = 0x22;
        }

        let dib = source.to_dib();
        let height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
        assert_eq!(height, 2, "CF_DIB is stored bottom-up");
        assert_eq!(dib[40], 0x22, "the first row in the file is the bottom one");
        assert_eq!(dib[40 + stride], 0x11);
        // The header is the only thing in front of the pixels.
        assert_eq!(dib.len(), 40 + source.bgra.len());
    }

    /// Touches the real desktop. Excluded from the default run only because it
    /// needs an interactive session; it is safe and read-only.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn the_desktop_can_be_captured() {
        let rect = virtual_screen_rect().expect("a virtual screen");
        let screen = grab(rect.left, rect.top, 320, 240).expect("a capture");
        assert_eq!((screen.width, screen.height), (320, 240));
        assert_eq!(screen.bgra.len(), 320 * 240 * 4);
        assert!(screen.to_bmp().is_some());
    }
}
