//! Drawing the toast.
//!
//! The same technique as the widget bar, and for the same reason: the panel is
//! rounded and translucent over whatever is behind it, so it needs per-pixel
//! alpha, and the frame is rendered into a `32bppPBGRA` WIC bitmap whose buffer
//! goes straight to `UpdateLayeredWindow`. A `ID2D1DCRenderTarget` would be the
//! obvious alternative and does not reliably write the alpha channel — and
//! alpha is the whole point of a floating panel.
//!
//! Everything is drawn in *physical pixels*: the render target is pinned at
//! 96 DPI and the DPI scale is applied to the design constants through
//! [`crate::layout::Metrics`] instead. One coordinate space, no hidden scaling.
//!
//! # Why the frame is rebuilt per show rather than kept
//!
//! The panel's size follows its text, and a toast spends its whole life
//! invisible between shows. Holding a device-sized bitmap and a render target
//! for a window that is on screen for two seconds every few minutes is memory
//! doing nothing; the frame is created when a toast starts and dropped when it
//! ends.

use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICBitmapLock, IWICImagingFactory,
    WICBitmapCacheOnLoad, WICBitmapLockRead,
};

use beautify_widget::canvas::{Canvas, TextEngine};
use beautify_widget::layout::Rect;
use beautify_widget::theme::Rgba;

use crate::layout::{self, Lines, Metrics, Panel};

/// Refuse to allocate a frame larger than this, so a bad measurement cannot
/// exhaust memory. Far larger than any toast: this is a guard, not a limit.
const MAX_FRAME_PIXELS: i64 = 4096 * 1024;

/// The panel's background.
///
/// The same dark as the flyout's panel and the default taskbar accent, a little
/// more opaque: a toast sits over the middle of whatever the user is looking at
/// rather than over the desktop, so it has to hold its own contrast without a
/// system blur behind it.
const BACKGROUND: Rgba = Rgba::new(
    0x14 as f32 / 255.0,
    0x16 as f32 / 255.0,
    0x1C as f32 / 255.0,
    0.94,
);

/// A hairline around the panel, so it still reads against a bright wallpaper.
const BORDER: Rgba = Rgba::new(1.0, 1.0, 1.0, 0.10);

const TITLE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

/// The detail line, dimmed rather than a second colour: it reads as "quieter
/// than the title" without turning into a different hue.
const DETAIL: Rgba = Rgba::new(1.0, 1.0, 1.0, 0.72);

/// Text measurement and device-bound drawing for the toast.
///
/// One object rather than two so the strings that are sized are the strings
/// that are drawn: the truncation here and the layout in [`crate::layout`] go
/// through the same [`TextEngine`], so the panel can never be sized for a line
/// it then renders differently.
pub struct Painter {
    d2d: ID2D1Factory,
    wic: IWICImagingFactory,
    text: TextEngine,
    frame: Option<Frame>,
}

struct Frame {
    bitmap: IWICBitmap,
    target: ID2D1RenderTarget,
}

impl Painter {
    /// Create the painter. Must run on a COM-initialised thread.
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            Ok(Self {
                d2d,
                wic: beautify_widget::images::create_wic_factory()?,
                text: TextEngine::new()?,
                frame: None,
            })
        }
    }

    /// Fit the lines to the panel's text width and size the panel around
    /// them, without drawing anything.
    ///
    /// Split out from [`Self::render`] because the caller needs the panel's
    /// size *before* it renders: the layered surface is sized from it, and the
    /// surface has to exist before the frame's pixels have anywhere to go. The
    /// measurement is done once and the result is handed to `render`, so the
    /// two can never disagree about how big the panel is.
    ///
    /// The detail may be several `\n`-separated lines; each is truncated and
    /// measured on its own, and the panel is sized from the widest of them.
    /// Truncation needs DirectWrite metrics, which is why it happens here
    /// rather than in the pure layout module — and the *truncated* widths are
    /// what the panel is sized from. Sizing from the untruncated measurement
    /// would leave a panel too wide for the text that actually lands in it.
    pub fn measure(
        &self,
        title: &str,
        detail: &str,
        metrics: &Metrics,
    ) -> Result<(Panel, String, String)> {
        let title_format = self
            .text
            .format(metrics.title_size(), DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
        let detail_format = self
            .text
            .format(metrics.detail_size(), DWRITE_FONT_WEIGHT_NORMAL)?;

        let text_width = metrics.text_width();
        let title = self.text.fit(title, &title_format, text_width);
        let title_width = self.text.measure(&title, &title_format, 4096.0);
        // Each detail row is truncated on its own: a long first line must not
        // steal width (or an ellipsis) from a short second one.
        let mut fitted = Vec::new();
        let mut widest: f32 = 0.0;
        for line in detail.lines() {
            let line = self.text.fit(line, &detail_format, text_width);
            widest = widest.max(self.text.measure(&line, &detail_format, 4096.0));
            fitted.push(line);
        }
        let detail = fitted.join("\n");

        let panel = layout::panel(
            &Lines {
                title: &title,
                detail: &detail,
                title_width,
                detail_width: widest,
            },
            metrics,
        );
        Ok((panel, title, detail))
    }

    /// Draw one already-measured toast, handing the premultiplied BGRA buffer
    /// to `out`.
    ///
    /// `out` receives `(pixels, stride)`; the borrow ends when it returns, which
    /// is what lets the caller feed `UpdateLayeredWindow` with no extra copy.
    ///
    /// # Panics
    ///
    /// Does not panic. `panel` and the two lines must be the ones
    /// [`Self::measure`] produced — passing anything else draws a panel that
    /// does not match its own size, which is a caller bug rather than an
    /// input-dependent failure.
    pub fn render(
        &mut self,
        panel: &Panel,
        title: &str,
        detail: &str,
        metrics: &Metrics,
        out: impl FnOnce(&[u8], usize) -> Result<()>,
    ) -> Result<()> {
        let title_format = self
            .text
            .format(metrics.title_size(), DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
        let detail_format = self
            .text
            .format(metrics.detail_size(), DWRITE_FONT_WEIGHT_NORMAL)?;

        self.ensure_frame(panel.width as u32, panel.height as u32)?;
        let frame = self.frame.as_ref().expect("just ensured");
        let canvas = Canvas::new(&frame.target)?;
        canvas.begin();
        canvas.clear(Rgba::TRANSPARENT);
        // The panel is drawn *inside* the window with a one-pixel margin, so the
        // antialiased corner pixels have somewhere to land: a rounded rectangle
        // filling its target exactly gets its four corners clipped by the edge
        // of the DIB and reads as a square with nibbled corners.
        let inset = Rect::new(0.5, 0.5, panel.width - 0.5, panel.height - 0.5);
        let radius = metrics.radius();
        canvas.fill_rounded(inset, radius, BACKGROUND);
        canvas.stroke_rounded(inset, radius, BORDER, 1.0);

        let title_rect = Rect::new(
            panel.text.left as f32,
            panel.text.top as f32,
            panel.text.right as f32,
            panel.text.top as f32 + panel.line_height,
        );
        canvas.text(
            title,
            &title_format,
            title_rect,
            TITLE,
            self.text.vertical_correction(&title_format),
        );
        if panel.has_detail {
            // One rect per detail row, in the same arithmetic `panel` sized
            // them with: a gap before each row, starting under the title.
            let detail_line = metrics.line_height(metrics.detail_size());
            let row = detail_line + metrics.px(layout::LINE_GAP);
            for (index, line) in detail.lines().take(panel.detail_lines).enumerate() {
                let top = panel.text.top as f32 + panel.line_height + metrics.px(layout::LINE_GAP)
                    + index as f32 * row;
                let rect = Rect::new(
                    panel.text.left as f32,
                    top,
                    panel.text.right as f32,
                    top + detail_line,
                );
                canvas.text(
                    line,
                    &detail_format,
                    rect,
                    DETAIL,
                    self.text.vertical_correction(&detail_format),
                );
            }
        }
        canvas.end()?;

        let lock: IWICBitmapLock = unsafe {
            frame
                .bitmap
                .Lock(std::ptr::null(), WICBitmapLockRead.0 as u32)?
        };
        let stride = unsafe { lock.GetStride()? } as usize;
        let mut pointer = std::ptr::null_mut();
        let mut length = 0u32;
        unsafe { lock.GetDataPointer(&mut length, &mut pointer)? };
        let pixels = unsafe { std::slice::from_raw_parts(pointer, length as usize) };
        // Presenting is part of the frame: drawing into a buffer nothing looks
        // at is not a rendered frame, and the caller has to hear about the
        // difference.
        out(pixels, stride)
    }

    fn ensure_frame(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if (width as i64) * (height as i64) > MAX_FRAME_PIXELS {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "toast panel size out of range",
            ));
        }
        if let Some(frame) = self.frame.as_ref() {
            let current = unsafe { frame.target.GetPixelSize() };
            if current.width == width && current.height == height {
                return Ok(());
            }
        }

        let bitmap = unsafe {
            self.wic.CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )?
        };
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // Pinned at 96 so drawing coordinates are physical pixels.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = unsafe { self.d2d.CreateWicBitmapRenderTarget(&bitmap, &properties)? };
        self.frame = Some(Frame { bitmap, target });
        Ok(())
    }

    /// Drop the device-bound frame.
    ///
    /// Called when a toast ends: the bitmap is the size of the panel and would
    /// otherwise stay allocated for the life of the process to serve a window
    /// that is not on screen.
    pub fn release(&mut self) {
        self.frame = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two colour decisions below are compile-time facts about constants, so
    // they are checked in const blocks rather than at run time: the assertion
    // either holds for every build or the crate does not compile, and there is
    // nothing for a test run to discover.

    /// 0x14, 0x16, 0x1c — the flyout's dark panel and the default taskbar
    /// accent. A toast that picked its own dark would look like a different
    /// application's notification.
    #[test]
    fn the_panel_colour_is_the_dark_panel_every_other_surface_uses() {
        const { assert!(BACKGROUND.r == 0x14 as f32 / 255.0) };
        const { assert!(BACKGROUND.g == 0x16 as f32 / 255.0) };
        const { assert!(BACKGROUND.b == 0x1C as f32 / 255.0) };
        // A toast sits over whatever the user is looking at rather than over
        // the desktop, so it has no system blur behind it to lean on.
        const { assert!(BACKGROUND.a > 0.9) };
    }

    /// The detail line is dimmed rather than recoloured, so it reads as quieter
    /// than the title without turning into a second hue.
    #[test]
    fn the_detail_line_is_quieter_than_the_title_without_changing_hue() {
        const { assert!(DETAIL.r == TITLE.r) };
        const { assert!(DETAIL.g == TITLE.g) };
        const { assert!(DETAIL.b == TITLE.b) };
        const { assert!(DETAIL.a < TITLE.a) };
    }
}
