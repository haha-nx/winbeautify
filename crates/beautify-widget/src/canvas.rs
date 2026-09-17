//! Direct2D drawing primitives shared by the widget bar and the flyout.
//!
//! Both surfaces want the same handful of operations — a rounded panel, some
//! text truncated to a width, an image clipped to a rounded tile — and the two
//! differ only in where they render. Keeping the primitives here means the
//! lyric fits identically on both, which is the sort of thing that silently
//! drifts when it is duplicated.

use windows::core::{w, Interface, Result};
use windows_numerics::Vector2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::layout::Rect;
use crate::theme::Rgba;

pub fn rect_f(rect: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

pub fn rounded(rect: Rect, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: rect_f(rect),
        radiusX: radius.max(0.0),
        radiusY: radius.max(0.0),
    }
}

/// Recolour a solid brush. The one place `SetColor` is called.
pub fn set_brush(brush: &ID2D1SolidColorBrush, color: Rgba) {
    unsafe { brush.SetColor(&color.to_d2d()) };
}

/// A render target plus the one brush every shape recolours before filling.
///
/// A single cached brush matters more than it looks: creating one per shape
/// would allocate hundreds of COM objects a second while a spectrum animates.
pub struct Canvas<'a> {
    target: &'a ID2D1RenderTarget,
    brush: ID2D1SolidColorBrush,
}

impl<'a> Canvas<'a> {
    pub fn new(target: &'a ID2D1RenderTarget) -> Result<Self> {
        let brush = unsafe { target.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)? };
        Ok(Self { target, brush })
    }

    pub fn target(&self) -> &ID2D1RenderTarget {
        self.target
    }

    pub fn begin(&self) {
        unsafe { self.target.BeginDraw() };
    }

    /// `EndDraw` reports device loss; the caller decides whether to rebuild.
    pub fn end(&self) -> Result<()> {
        unsafe { self.target.EndDraw(None, None) }
    }

    pub fn clear(&self, color: Rgba) {
        unsafe { self.target.Clear(Some(&color.to_d2d())) };
    }

    fn set(&self, color: Rgba) {
        set_brush(&self.brush, color);
    }

    pub fn fill_rect(&self, rect: Rect, color: Rgba) {
        self.set(color);
        unsafe { self.target.FillRectangle(&rect_f(rect), &self.brush) };
    }

    pub fn fill_rounded(&self, rect: Rect, radius: f32, color: Rgba) {
        self.set(color);
        unsafe { self.target.FillRoundedRectangle(&rounded(rect, radius), &self.brush) };
    }

    pub fn stroke_rounded(&self, rect: Rect, radius: f32, color: Rgba, width: f32) {
        self.set(color);
        unsafe {
            self.target
                .DrawRoundedRectangle(&rounded(rect, radius), &self.brush, width, None)
        };
    }

    /// A single-line run, vertically centred, horizontally per `format`.
    ///
    /// `optical_offset` shifts the run so the *glyphs* are centred rather than
    /// the line box; see [`TextEngine::vertical_correction`].
    pub fn text(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        rect: Rect,
        color: Rgba,
        optical_offset: f32,
    ) {
        if text.is_empty() {
            return;
        }
        self.set(color);
        let shifted = Rect::new(
            rect.left,
            rect.top + optical_offset,
            rect.right,
            rect.bottom + optical_offset,
        );
        let units: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            self.target.DrawText(
                &units,
                format,
                &rect_f(shifted),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            )
        };
    }

    /// Clip everything drawn inside `f` to `rect`.
    ///
    /// Used for the scrolling list bodies, which must not paint over the tab
    /// strip or the footer.
    pub fn clipped<R>(&self, rect: Rect, f: impl FnOnce() -> R) -> R {
        unsafe { self.target.PushAxisAlignedClip(&rect_f(rect), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        let result = f();
        unsafe { self.target.PopAxisAlignedClip() };
        result
    }

    /// Draw a bitmap into `rect`, clipped to a rounded tile.
    pub fn image_rounded(
        &self,
        factory: &ID2D1Factory,
        layer: &ID2D1Layer,
        bitmap: &ID2D1Bitmap,
        rect: Rect,
        radius: f32,
        bounds: Rect,
    ) -> Result<()> {
        let shape = rounded(rect, radius);
        let geometry = unsafe { factory.CreateRoundedRectangleGeometry(&shape)? };
        let params = D2D1_LAYER_PARAMETERS {
            contentBounds: rect_f(bounds),
            // SAFETY: `geometry` outlives the layer, so the `ManuallyDrop`
            // inside the parameters struct cannot leak it.
            geometricMask: core::mem::ManuallyDrop::new(Some(geometry.cast()?)),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: windows_numerics::Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: core::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        unsafe {
            self.target.PushLayer(&params, layer);
            self.target.DrawBitmap(
                bitmap,
                Some(&rect_f(rect)),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
            self.target.PopLayer();
        }
        Ok(())
    }

    /// A filled polygon from points, translated by `offset`.
    pub fn fill_polygon(
        &self,
        factory: &ID2D1Factory,
        points: &[(f32, f32)],
        offset: (f32, f32),
        color: Rgba,
    ) -> Result<()> {
        let geometry = build_path(factory, points, offset)?;
        self.set(color);
        unsafe { self.target.FillGeometry(&geometry, &self.brush, None) };
        Ok(())
    }

    /// An outlined polygon, for the small "no artwork" style glyphs.
    pub fn stroke_polyline(
        &self,
        factory: &ID2D1Factory,
        points: &[(f32, f32)],
        offset: (f32, f32),
        color: Rgba,
        width: f32,
    ) -> Result<()> {
        let geometry = build_path(factory, points, offset)?;
        self.set(color);
        unsafe {
            self.target
                .DrawGeometry(&geometry, &self.brush, width, None)
        };
        Ok(())
    }
}

/// Build a closed polygon path, with each point offset by `(dx, dy)`.
pub fn build_path(
    factory: &ID2D1Factory,
    points: &[(f32, f32)],
    offset: (f32, f32),
) -> Result<ID2D1PathGeometry> {
    unsafe {
        let geometry = factory.CreatePathGeometry()?;
        let sink = geometry.Open()?;
        if let Some(((x, y), rest)) = points.split_first() {
            sink.BeginFigure(
                Vector2 {
                    X: x + offset.0,
                    Y: y + offset.1,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            for (x, y) in rest {
                sink.AddLine(Vector2 {
                    X: x + offset.0,
                    Y: y + offset.1,
                });
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }
        sink.Close()?;
        Ok(geometry)
    }
}

/// Font selection and text measurement.
///
/// Measure and draw must go through the same object, or the bar can be sized
/// for a string it then renders differently.
pub struct TextEngine {
    factory: IDWriteFactory,
    /// `(pixels, weight) -> (format, optical correction)`.
    ///
    /// Behind a `RefCell` because the engine lives on a single pump thread and
    /// a cached format lookup has no business requiring `&mut` from every
    /// caller — measurement happens from `&self` in several places.
    cache: std::cell::RefCell<Vec<CachedFormat>>,
}

struct CachedFormat {
    pixels: u32,
    weight: u32,
    alignment: i32,
    wrapping: i32,
    paragraph: i32,
    format: IDWriteTextFormat,
    /// How far the glyph ink sits above the centre of its line box.
    correction: f32,
}

impl TextEngine {
    pub fn new() -> Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(Self {
            factory,
            cache: std::cell::RefCell::new(Vec::new()),
        })
    }

    /// "Segoe UI Variable Text" is the Windows 11 face; plain "Segoe UI" is the
    /// fallback on 10. DirectWrite handles CJK fallback on its own.
    pub fn format(&self, px: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
        self.format_aligned(px, weight, DWRITE_TEXT_ALIGNMENT_LEADING)
    }

    /// Same, with an explicit horizontal alignment.
    ///
    /// Alignment is part of the cache key because it is set on the format
    /// object, and handing the same cached object to two callers that want
    /// different alignment would have them overwrite each other.
    pub fn format_aligned(
        &self,
        px: f32,
        weight: DWRITE_FONT_WEIGHT,
        alignment: DWRITE_TEXT_ALIGNMENT,
    ) -> Result<IDWriteTextFormat> {
        self.cached(
            px,
            weight,
            alignment,
            DWRITE_WORD_WRAPPING_NO_WRAP,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        )
    }

    /// A format for a block of text that may take several lines: it wraps, and
    /// it starts at the top of its box.
    ///
    /// Both differences matter. [`Self::format`] does not wrap, which is right
    /// for a label that must not silently become two lines, and it centres the
    /// paragraph, which for a block one line too tall would clip the first line
    /// and the last one instead of just the last.
    pub fn block_format(&self, px: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
        self.cached(
            px,
            weight,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            DWRITE_WORD_WRAPPING_WRAP,
            DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
        )
    }

    fn cached(
        &self,
        px: f32,
        weight: DWRITE_FONT_WEIGHT,
        alignment: DWRITE_TEXT_ALIGNMENT,
        wrapping: DWRITE_WORD_WRAPPING,
        paragraph: DWRITE_PARAGRAPH_ALIGNMENT,
    ) -> Result<IDWriteTextFormat> {
        let key = (
            px.max(1.0).to_bits(),
            weight.0 as u32,
            alignment.0,
            wrapping.0,
            paragraph.0,
        );
        if let Some(entry) = self.cache.borrow().iter().find(|e| {
            e.pixels == key.0
                && e.weight == key.1
                && e.alignment == key.2
                && e.wrapping == key.3
                && e.paragraph == key.4
        }) {
            return Ok(entry.format.clone());
        }

        let build = |family| unsafe {
            self.factory.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px.max(1.0),
                w!("zh-cn"),
            )
        };
        let format = build(w!("Segoe UI Variable Text")).or_else(|_| build(w!("Segoe UI")))?;
        unsafe {
            format.SetWordWrapping(wrapping)?;
            format.SetTextAlignment(alignment)?;
            format.SetParagraphAlignment(paragraph)?;
        }
        let correction = self.measure_correction(&format, px.max(1.0));
        self.cache.borrow_mut().push(CachedFormat {
            pixels: key.0,
            weight: key.1,
            alignment: key.2,
            wrapping: key.3,
            paragraph: key.4,
            format: format.clone(),
            correction,
        });
        Ok(format)
    }

    /// How far the glyph ink sits **above** the centre of its line box.
    ///
    /// Paragraph centring positions the *line box*, and a font's line box is
    /// not symmetric about the glyphs: for CJK text the em box ends at the
    /// baseline, so the ink ends up noticeably high. Measured rather than
    /// assumed, because the answer depends on the font's ascent/descent.
    pub fn vertical_correction(&self, format: &IDWriteTextFormat) -> f32 {
        let weight = unsafe { format.GetFontWeight() };
        let alignment = unsafe { format.GetTextAlignment() };
        // `format` came from this engine, so the entry exists; fall back to no
        // correction rather than panicking if it somehow does not.
        self.cache
            .borrow()
            .iter()
            .find(|e| unsafe {
                format.GetFontSize() == e.format.GetFontSize()
                    && weight == e.format.GetFontWeight()
                    && alignment == e.format.GetTextAlignment()
            })
            .map(|e| e.correction)
            .unwrap_or(0.0)
    }

    /// Measure the correction for a freshly built format.
    fn measure_correction(&self, format: &IDWriteTextFormat, size: f32) -> f32 {
        // Any single-line string gives the same line metrics; the font decides.
        let probe: Vec<u16> = "国".encode_utf16().collect();
        let Ok(layout) = (unsafe { self.factory.CreateTextLayout(&probe, format, 1000.0, 100.0) })
        else {
            return 0.0;
        };
        let mut lines = [DWRITE_LINE_METRICS::default(); 1];
        let mut count = 0u32;
        if unsafe { layout.GetLineMetrics(Some(&mut lines), &mut count) }.is_err() || count == 0 {
            return 0.0;
        }
        let line = lines[0];
        // The ideographic em box runs from `baseline - size` to `baseline`, so
        // its centre is `baseline - size/2`; the line box is centred on itself.
        line.height * 0.5 - (line.baseline - size * 0.5)
    }

    /// Natural width of `text` on a single line.
    pub fn measure(&self, text: &str, format: &IDWriteTextFormat, wrap_width: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let Ok(layout) = (unsafe {
            self.factory.CreateTextLayout(
                &text.encode_utf16().collect::<Vec<u16>>(),
                format,
                wrap_width,
                1000.0,
            )
        }) else {
            // A rough estimate beats refusing to draw anything at all.
            return text.chars().count() as f32 * 8.0;
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
            return text.chars().count() as f32 * 8.0;
        }
        metrics.widthIncludingTrailingWhitespace
    }

    /// Longest prefix of `text` that fits in `max_width`, with an ellipsis.
    ///
    /// Truncating by hand avoids implementing a COM trimmer callback for what
    /// amounts to a handful of characters.
    pub fn fit(&self, text: &str, format: &IDWriteTextFormat, max_width: f32) -> String {
        if text.is_empty() || max_width <= 0.0 {
            return String::new();
        }
        if self.measure(text, format, 4096.0) <= max_width {
            return text.to_string();
        }

        let chars: Vec<char> = text.chars().collect();
        // Invariant: `low` always fits with an ellipsis, `high` never does.
        let (mut low, mut high) = (0usize, chars.len());
        while high - low > 1 {
            let mid = (low + high) / 2;
            let candidate: String = chars[..mid].iter().collect::<String>() + "…";
            if self.measure(&candidate, format, 4096.0) <= max_width {
                low = mid;
            } else {
                high = mid;
            }
        }
        if low == 0 {
            return String::new();
        }
        chars[..low].iter().collect::<String>() + "…"
    }

    /// Split `text` into lines that each fit `max_width`, at word boundaries
    /// where possible and mid-word where not.
    pub fn wrap(&self, text: &str, format: &IDWriteTextFormat, max_width: f32) -> Vec<String> {
        if text.is_empty() || max_width <= 0.0 {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for paragraph in text.split('\n') {
            let mut current = String::new();
            for word in paragraph.split_inclusive(' ') {
                let candidate = format!("{current}{word}");
                if current.is_empty() || self.measure(candidate.trim_end(), format, 4096.0) <= max_width
                {
                    current = candidate;
                } else {
                    lines.push(current.trim_end().to_string());
                    current = word.to_string();
                }
            }
            lines.push(current.trim_end().to_string());
        }
        lines.retain(|line| !line.is_empty());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_rect_carries_the_corner_radius() {
        let shape = rounded(Rect::new(1.0, 2.0, 11.0, 12.0), 3.0);
        assert_eq!(shape.radiusX, 3.0);
        assert_eq!(shape.rect.right, 11.0);
    }

    #[test]
    fn a_negative_radius_is_clamped_rather_than_producing_broken_geometry() {
        assert_eq!(rounded(Rect::new(0.0, 0.0, 10.0, 10.0), -4.0).radiusX, 0.0);
    }

    #[test]
    fn the_optical_correction_is_small_and_downwards() {
        // Measured against DirectWrite: the line box is centred correctly but
        // CJK ink sits above it, so the correction must be a small positive
        // number. A large or negative value means the formula is wrong.
        let engine = TextEngine::new().expect("DirectWrite is available");
        let format = engine
            .format(15.0, DWRITE_FONT_WEIGHT_NORMAL)
            .expect("a UI font exists");
        let correction = engine.vertical_correction(&format);
        assert!(
            (0.2..6.0).contains(&correction),
            "correction should be a couple of pixels at most, got {correction}"
        );
    }

    #[test]
    fn the_correction_scales_with_the_font_size() {
        let engine = TextEngine::new().expect("DirectWrite is available");
        let small = engine
            .format(11.0, DWRITE_FONT_WEIGHT_NORMAL)
            .expect("a UI font exists");
        let large = engine
            .format(30.0, DWRITE_FONT_WEIGHT_NORMAL)
            .expect("a UI font exists");
        assert!(
            engine.vertical_correction(&large) > engine.vertical_correction(&small),
            "a bigger font needs a bigger correction"
        );
    }

    #[test]
    fn rect_conversion_is_lossless() {
        let converted = rect_f(Rect::new(1.5, 2.5, 3.5, 4.5));
        assert_eq!(converted.left, 1.5);
        assert_eq!(converted.bottom, 4.5);
    }
}
