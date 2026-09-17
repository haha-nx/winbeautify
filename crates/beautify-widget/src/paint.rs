//! Direct2D painting of the widget bar.
//!
//! The frame is rendered into a `32bppPBGRA` WIC bitmap rather than onto a DC.
//! A `ID2D1DCRenderTarget` would be the obvious choice, but DC render targets
//! are built for GDI interop and do not reliably write the alpha channel — and
//! alpha is the entire point of a floating pill. A WIC bitmap render target
//! does support premultiplied alpha, and its buffer can be handed straight to
//! `UpdateLayeredWindow`.
//!
//! Everything is drawn in *physical pixels*: the render target is pinned at
//! 96 DPI and the DPI scale is applied to the design constants instead. One
//! coordinate space, no hidden scaling.
//!
//! # Shape of this module
//!
//! [`Painter`] owns the device-bound resources and performs every `&mut self`
//! step inside [`Painter::render`] *before* drawing begins. Drawing itself is
//! free functions over borrowed resources, which keeps the borrow checker happy
//! and makes the draw code obviously read-only.
//!
//! Brush allocation is deliberately one per frame: `SetColor` mutates it
//! between fills. Creating a brush per shape would allocate hundreds of COM
//! objects a second for no visual benefit.

use std::mem::ManuallyDrop;

use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    IWICBitmap, IWICBitmapLock, IWICImagingFactory, WICBitmapCacheOnLoad, WICBitmapLockRead,
    GUID_WICPixelFormat32bppPBGRA,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::canvas::{self, TextEngine};
use crate::images::{self, ArtworkCache};
use crate::layout::{self, Align, AudioLayout, Content, Hit, Layout, Metrics, Rect, Transport};
use crate::state::WidgetState;
use crate::theme::{Rgba, Theme};

/// Refuse to allocate a DIB larger than this, so a bad config cannot exhaust
/// memory: far wider and taller than any real taskbar.
const MAX_WINDOW_PIXELS: i64 = 4096 * 256;

/// Text sizes from the design, in 96-DPI pixels.
const LYRIC_PX: f32 = 12.0;
const BADGE_PX: f32 = 9.0;
/// Corner radius of the album-art tile, in 96-DPI pixels.
const COVER_RADIUS: f32 = 5.0;
/// Corner radius of the launcher's hover highlight.
const LAUNCHER_RADIUS: f32 = 8.0;

pub struct Painter {
    d2d: ID2D1Factory,
    wic: IWICImagingFactory,
    frame: Option<Frame>,
    /// Bumped whenever the frame is rebuilt, to invalidate device-dependent
    /// resources such as the album-art bitmap.
    generation: u64,
    artwork: ArtworkCache,
    glyphs: Option<Glyphs>,
    text: TextEngine,
    badge_format: Option<IDWriteTextFormat>,
    badge_scale: f32,
    fitted: Option<(String, f32, i32, String)>,
    /// The single brush every shape recolours before filling. Held across
    /// frames so a 30 fps spectrum does not allocate a COM object per frame.
    brush: Option<ID2D1SolidColorBrush>,
}

struct Frame {
    width: u32,
    height: u32,
    bitmap: IWICBitmap,
    target: ID2D1RenderTarget,
    layer: ID2D1Layer,
}

/// Transport-control outlines, built once per DPI.
#[derive(Clone)]
struct Glyphs {
    scale: f32,
    previous: ID2D1PathGeometry,
    next: ID2D1PathGeometry,
    play: ID2D1PathGeometry,
    pause: ID2D1PathGeometry,
}

/// Everything a frame needs, with no way back to `&mut Painter`.
struct Scene<'a> {
    frame: &'a Frame,
    brush: &'a ID2D1SolidColorBrush,
    factory: &'a ID2D1Factory,
    glyphs: &'a Glyphs,
    artwork: Option<&'a ID2D1Bitmap>,
    badge_format: Option<&'a IDWriteTextFormat>,
    lyric_format: &'a IDWriteTextFormat,
    layout: &'a Layout,
    content: &'a Content,
    metrics: &'a Metrics,
    theme: &'a Theme,
    state: &'a WidgetState,
    /// Already truncated to the slot width.
    line: &'a str,
    line_is_fallback: bool,
    hover: Option<Hit>,
    corner_radius: f32,
    /// Optical centring corrections, measured from the font's line metrics.
    lyric_optical_offset: f32,
    badge_optical_offset: f32,
}

impl Painter {
    /// Create the painter. Must run on a COM-initialised thread.
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let wic = images::create_wic_factory()
                .map_err(|e| windows::core::Error::new(e.code(), "WIC factory unavailable"))?;
            Ok(Self {
                d2d,
                wic,
                frame: None,
                generation: 0,
                artwork: ArtworkCache::new(),
                glyphs: None,
                text: TextEngine::new()?,
                badge_format: None,
                badge_scale: 0.0,
                fitted: None,
                brush: None,
            })
        }
    }

    /// The shared brush, created on first use and reused thereafter.
    ///
    /// Borrows the target separately from `self.brush` so the two do not
    /// conflict: both live in `self`, but the brush is only created when the
    /// cache is empty, at which point nothing else is borrowed.
    fn ensure_brush(&mut self) -> Result<()> {
        if self.brush.is_some() {
            return Ok(());
        }
        let target = self.frame.as_ref().expect("frame ensured").target.clone();
        self.brush = Some(unsafe { target.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)? });
        Ok(())
    }

    /// Render one frame and hand the premultiplied BGRA buffer to `out`.
    ///
    /// `out` receives `(pixels, stride)`; the borrow ends when it returns, which
    /// is what lets the caller feed `UpdateLayeredWindow` with no extra copy.
    // The parameters mirror the frame's inputs one-for-one; grouping them into
    // a struct would only move the same list somewhere else.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        state: &WidgetState,
        size: (u32, u32),
        dpi: u32,
        bar_width: f32,
        align: Align,
        hover: Option<Hit>,
        out: impl FnOnce(&[u8], usize),
    ) -> Result<()> {
        self.ensure_frame(size)?;
        let metrics = Metrics::new(dpi);
        self.ensure_badge_format(metrics.scale);

        // Every `&mut self` step happens here, before drawing borrows the frame.
        // Interfaces are refcounted, so cloning one is cheap.
        let glyphs = self.glyphs(dpi)?.clone();
        let content = state.content();
        let theme = Theme::resolve(&state.config);
        let (line, line_is_fallback) = state.display_line();
        let lyric_format = self.lyric_format(metrics.scale)?;

        let (window, generation) = {
            let frame = self.frame.as_ref().expect("just ensured");
            (
                Rect::new(0.0, 0.0, frame.width as f32, frame.height as f32),
                self.generation,
            )
        };
        let layout =
            layout::layout(window, bar_width, align, &content, &metrics);

        // Truncation needs DirectWrite metrics, i.e. `&self`, so it happens here
        // rather than in the draw code.
        let fitted = match layout.audio.as_ref() {
            Some(audio) => self
                .fit_cached(&line, &lyric_format, metrics.scale, audio.slot.width())
                .to_string(),
            None => String::new(),
        };

        let artwork = {
            let frame = self.frame.as_ref().expect("just ensured");
            let url = state.media.artwork.clone();
            self.artwork
                .get(&frame.target, &self.wic, generation, &url)
                .cloned()
        };

        let corner_radius = state.config.widget.corner_radius;
        self.ensure_brush()?;
        let brush = self.brush.as_ref().expect("just ensured").clone();
        {
            let frame = self.frame.as_ref().expect("just ensured");
            let scene = Scene {
                brush: &brush,
                frame,
                factory: &self.d2d,
                glyphs: &glyphs,
                artwork: artwork.as_ref(),
                badge_format: self.badge_format.as_ref(),
                lyric_format: &lyric_format,
                layout: &layout,
                content: &content,
                metrics: &metrics,
                theme: &theme,
                state,
                line: &fitted,
                line_is_fallback,
                hover,
                corner_radius,
                lyric_optical_offset: self.text.vertical_correction(&lyric_format),
                badge_optical_offset: self
                    .badge_format
                    .as_ref()
                    .map(|f| self.text.vertical_correction(f))
                    .unwrap_or(0.0),
            };
            draw_scene(&scene)?;
        }

        let frame = self.frame.as_ref().expect("just ensured");
        let lock: IWICBitmapLock =
            unsafe { frame.bitmap.Lock(std::ptr::null(), WICBitmapLockRead.0 as u32)? };
        let stride = unsafe { lock.GetStride()? } as usize;
        let mut pointer = std::ptr::null_mut();
        let mut length = 0u32;
        unsafe { lock.GetDataPointer(&mut length, &mut pointer)? };
        let pixels = unsafe { std::slice::from_raw_parts(pointer, length as usize) };
        out(pixels, stride);
        Ok(())
    }

    fn ensure_frame(&mut self, size: (u32, u32)) -> Result<()> {
        let width = size.0.max(1);
        let height = size.1.max(1);
        if (width as i64) * (height as i64) > MAX_WINDOW_PIXELS {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "widget window size out of range",
            ));
        }
        if let Some(frame) = self.frame.as_ref() {
            if frame.width == width && frame.height == height {
                return Ok(());
            }
        }

        // A Direct2D bitmap belongs to the device that made it, so a resize
        // invalidates the album art too.
        self.artwork.clear();
        self.generation = self.generation.wrapping_add(1);
        self.fitted = None;
        // The brush belongs to the old render target.
        self.brush = None;

        let bitmap = unsafe {
            self.wic
                .CreateBitmap(width, height, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?
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
        let layer = unsafe { target.CreateLayer(None)? };

        self.frame = Some(Frame {
            width,
            height,
            bitmap,
            target,
            layer,
        });
        Ok(())
    }

    // -- text ---------------------------------------------------------------

    /// "Segoe UI Variable Text" is the Windows 11 face; plain "Segoe UI" is the
    /// fallback on 10. DirectWrite handles CJK fallback on its own.
    /// Width of a lyric line at `scale`, for the caller to feed the layout.
    ///
    /// Goes through the same format and the same DirectWrite metrics as the
    /// draw path, so the bar can never be sized for a string it then renders
    /// differently.
    pub fn measure_line(&self, text: &str, scale: f32) -> f32 {
        match self.lyric_format(scale) {
            Ok(format) => self.text.measure(text, &format, 4096.0),
            Err(_) => text.chars().count() as f32 * 8.0 * scale,
        }
    }

    /// The lyric format for `scale`.
    ///
    /// Horizontally centred: the slot is sized to the lyric but never narrower
    /// than the transport controls, so a short line has slack on both sides and
    /// belongs in the middle of it rather than against the cover.
    fn lyric_format(&self, scale: f32) -> Result<IDWriteTextFormat> {
        self.text.format_aligned(
            (LYRIC_PX * scale).max(1.0),
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )
    }

    /// [`TextEngine::fit`] memoised on the string and the width it must fit in.
    fn fit_cached<'a>(
        &'a mut self,
        text: &str,
        format: &IDWriteTextFormat,
        scale: f32,
        max_width: f32,
    ) -> &'a str {
        // Rounded so sub-pixel jitter during the width animation does not miss
        // the cache on every frame.
        let key = max_width.round() as i32;
        let hit = matches!(
            self.fitted.as_ref(),
            Some((cached, cached_scale, cached_key, _))
                if cached == text
                    && (cached_scale - scale).abs() < f32::EPSILON
                    && *cached_key == key
        );
        if !hit {
            let fitted = self.text.fit(text, format, max_width);
            self.fitted = Some((text.to_string(), scale, key, fitted));
        }
        &self.fitted.as_ref().expect("just filled").3
    }

    // -- cached resources ---------------------------------------------------

    fn ensure_badge_format(&mut self, scale: f32) {
        if self.badge_format.is_some() && (self.badge_scale - scale).abs() < f32::EPSILON {
            return;
        }
        // The badge is a centred pill of digits.
        let format = self
            .text
            .format_aligned(
                (BADGE_PX * scale).max(1.0),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_TEXT_ALIGNMENT_CENTER,
            )
            .ok();
        self.badge_format = format;
        self.badge_scale = scale;
    }

    fn glyphs(&mut self, dpi: u32) -> Result<&Glyphs> {
        let scale = Metrics::new(dpi).scale;
        let rebuild = match self.glyphs.as_ref() {
            Some(glyphs) => (glyphs.scale - scale).abs() > f32::EPSILON,
            None => true,
        };
        if rebuild {
            self.glyphs = Some(Glyphs::build(&self.d2d, scale)?);
        }
        Ok(self.glyphs.as_ref().expect("just built"))
    }
}

// ---------------------------------------------------------------------------
// Frame drawing
// ---------------------------------------------------------------------------

fn draw_scene(scene: &Scene<'_>) -> Result<()> {
    let target = &scene.frame.target;
    let brush = scene.brush;

    unsafe { target.BeginDraw() };
    unsafe { target.Clear(Some(&Rgba::TRANSPARENT.to_d2d())) };

    draw_pill(target, brush, scene);
    draw_launcher(target, brush, scene);

    if let Some(audio) = scene.layout.audio.as_ref() {
        draw_cover(target, brush, scene, audio)?;
        draw_slot(target, brush, scene, audio)?;
        if scene.content.show_spectrum && audio.spectrum.width() >= 2.0 {
            let bands = scene.state.spectrum_bars();
            draw_spectrum(
                target,
                brush,
                audio.spectrum,
                &bands,
                scene.theme.spectrum,
                scene.metrics,
            );
        }
    }

    unsafe { target.EndDraw(None, None)? };
    Ok(())
}


fn draw_pill(target: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, scene: &Scene<'_>) {
    let theme = scene.theme;
    if theme.pill.a <= 0.0 {
        // Switched off entirely — drawing a zero-alpha fill would be a no-op
        // anyway, and the stroke must not survive it.
        return;
    }

    let shape = canvas::rounded(scene.layout.pill, scene.corner_radius);
    canvas::set_brush(brush, theme.pill);
    unsafe { target.FillRoundedRectangle(&shape, brush) };

    // A hairline stroke keeps the pill legible against a wallpaper of the same
    // tone as its fill. Its alpha already tracks the pill's, so a translucent
    // pill cannot end up outlined by a stroke stronger than the fill.
    canvas::set_brush(brush, theme.pill_border);
    unsafe { target.DrawRoundedRectangle(&shape, brush, 1.0, None) };
}

fn draw_launcher(target: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, scene: &Scene<'_>) {
    let rect = scene.layout.launcher;
    let theme = scene.theme;
    let radius = LAUNCHER_RADIUS * scene.metrics.scale;
    let open = scene.state.flyout_open;

    // Hover gets a neutral wash, the open state gets *no* fill at all: a filled
    // highlight reads as a heavy accent-coloured block, which is far too loud on
    // a transparent bar. The glyph colour carries that state instead.
    if !open && scene.hover == Some(Hit::Launcher) {
        canvas::set_brush(brush, theme.hover);
        unsafe { target.FillRoundedRectangle(&canvas::rounded(rect, radius), brush) };
    }

    let foreground = if open { theme.active } else { theme.foreground };
    let span = 16.0 * scene.metrics.scale;
    let bounds = Rect::new(
        rect.left + (rect.width() - span) * 0.5,
        rect.center_y() - span * 0.5,
        rect.left + (rect.width() + span) * 0.5,
        rect.center_y() + span * 0.5,
    );
    draw_menu_glyph(target, brush, bounds, foreground, scene.metrics.scale);

    let todo = &scene.state.config.todo;
    if todo.enabled && todo.show_badge && scene.state.open_tasks > 0 {
        draw_badge(target, brush, scene, rect);
    }
}

/// The three stacked bars of the launcher icon, drawn as shapes rather than
/// text so the widget never depends on an icon font being installed.
fn draw_menu_glyph(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    bounds: Rect,
    color: Rgba,
    scale: f32,
) {
    let unit = bounds.width() / 16.0;
    for (top, height, alpha) in [(2.5f32, 3.2f32, 1.0f32), (6.9, 3.2, 0.62), (11.3, 2.2, 0.38)] {
        let rect = Rect::new(
            bounds.left + 1.5 * unit,
            bounds.top + top * scale,
            bounds.left + 14.5 * unit,
            bounds.top + (top + height) * scale,
        );
        canvas::set_brush(brush, color.with_alpha(color.a * alpha));
        unsafe { target.FillRoundedRectangle(&canvas::rounded(rect, height * scale * 0.5), brush) };
    }
}

fn draw_badge(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    scene: &Scene<'_>,
    launcher: Rect,
) {
    let scale = scene.metrics.scale;
    let count = scene.state.open_tasks;
    let text = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };

    let height = 14.0 * scale;
    let width = (height + text.chars().count() as f32 * 5.0 * scale).max(height);
    let rect = Rect::new(
        launcher.right - width + 2.0 * scale,
        launcher.top - 3.0 * scale,
        launcher.right + 2.0 * scale,
        launcher.top - 3.0 * scale + height,
    );

    canvas::set_brush(brush, scene.theme.accent);
    unsafe { target.FillRoundedRectangle(&canvas::rounded(rect, height * 0.5), brush) };

    let Some(format) = scene.badge_format else {
        return;
    };
    canvas::set_brush(brush, scene.theme.on_accent);
    // Same optical correction as the lyric: a 14 px pill is small enough that a
    // line-box-centred digit looks visibly high.
    let centred = Rect::new(
        rect.left,
        rect.top + scene.badge_optical_offset,
        rect.right,
        rect.bottom + scene.badge_optical_offset,
    );
    unsafe {
        target.DrawText(
            &text.encode_utf16().collect::<Vec<u16>>(),
            format,
            &canvas::rect_f(centred),
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        )
    };
}

/// Album art, or a quaver tile when the track has none.
fn draw_cover(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    scene: &Scene<'_>,
    audio: &AudioLayout,
) -> Result<()> {
    let rect = audio.cover;
    let radius = (COVER_RADIUS * scene.metrics.scale).min(scene.corner_radius.max(2.0));
    let shape = canvas::rounded(rect, radius);

    match scene.artwork {
        Some(bitmap) => {
            // Clip the artwork to the rounded tile. `geometry` is a local, so it
            // outlives the layer and the `ManuallyDrop` in the parameters struct
            // cannot leak it.
            let geometry = unsafe { scene.factory.CreateRoundedRectangleGeometry(&shape)? };
            let params = D2D1_LAYER_PARAMETERS {
                contentBounds: D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: scene.frame.width as f32,
                    bottom: scene.frame.height as f32,
                },
                geometricMask: ManuallyDrop::new(Some(geometry.cast()?)),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS_NONE,
            };
            unsafe { target.PushLayer(&params, &scene.frame.layer) };
            unsafe {
                target.DrawBitmap(
                    bitmap,
                    Some(&canvas::rect_f(rect)),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                )
            };
            unsafe { target.PopLayer() };
        }
        None => {
            canvas::set_brush(brush, scene.theme.accent);
            unsafe { target.FillRoundedRectangle(&shape, brush) };
            draw_quaver(target, brush, rect, scene.theme.on_accent);
        }
    }
    Ok(())
}

/// The lyric, or the transport controls when the pointer is over them.
fn draw_slot(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    scene: &Scene<'_>,
    audio: &AudioLayout,
) -> Result<()> {
    if scene.hover.is_some_and(Hit::is_audio) {
        return draw_controls(target, brush, scene);
    }

    if scene.line.is_empty() || audio.slot.width() <= 0.0 {
        return Ok(());
    }
    let color = if scene.line_is_fallback {
        scene.theme.foreground_dim
    } else {
        scene.theme.foreground
    };
    canvas::set_brush(brush, color);
    // Paragraph centring positions the line box; the glyph ink sits above that
    // centre, by ~2 px at this size. Shift the run so the text *looks* centred.
    let slot = Rect::new(
        audio.slot.left,
        audio.slot.top + scene.lyric_optical_offset,
        audio.slot.right,
        audio.slot.bottom + scene.lyric_optical_offset,
    );
    unsafe {
        target.DrawText(
            &scene.line.encode_utf16().collect::<Vec<u16>>(),
            scene.lyric_format,
            &canvas::rect_f(slot),
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        )
    };
    Ok(())
}

fn draw_controls(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    scene: &Scene<'_>,
) -> Result<()> {
    let Some(controls) = scene.layout.controls(scene.metrics) else {
        return Ok(());
    };
    let state = scene.state;
    let scale = scene.metrics.scale;

    let enabled = |transport: Transport| match transport {
        Transport::Previous => state.media.can_skip_previous,
        Transport::Next => state.media.can_skip_next,
        Transport::Toggle => {
            if state.is_playing() {
                state.media.can_pause
            } else {
                state.media.can_play
            }
        }
    };
    let color = |transport: Transport| {
        if enabled(transport) {
            scene.theme.foreground
        } else {
            scene.theme.foreground.with_alpha(0.3)
        }
    };

    for (transport, rect) in [
        (Transport::Previous, controls.previous),
        (Transport::Toggle, controls.toggle),
        (Transport::Next, controls.next),
    ] {
        if scene.hover == Some(Hit::Transport(transport)) {
            canvas::set_brush(brush, scene.theme.hover);
            unsafe { target.FillRoundedRectangle(&canvas::rounded(rect, 6.0 * scale), brush) };
        }
    }

    let draw_glyph = |geometry: &ID2D1PathGeometry, rect: Rect, tint: Rgba| {
        // The geometry is authored with its origin at the centre of a 16-unit
        // box, so one translate puts it on any button.
        let half = 8.0 * scale;
        let translation = Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: rect.left + rect.width() * 0.5 - half,
            M32: rect.center_y() - half,
        };
        canvas::set_brush(brush, tint);
        unsafe {
            target.SetTransform(&translation);
            target.FillGeometry(geometry, brush, None);
            target.SetTransform(&Matrix3x2::identity());
        }
    };

    draw_glyph(
        &scene.glyphs.previous,
        controls.previous,
        color(Transport::Previous),
    );
    draw_glyph(&scene.glyphs.next, controls.next, color(Transport::Next));
    let toggle = if state.is_playing() {
        &scene.glyphs.pause
    } else {
        &scene.glyphs.play
    };
    draw_glyph(toggle, controls.toggle, color(Transport::Toggle));
    Ok(())
}

fn draw_spectrum(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    area: Rect,
    bands: &[f32],
    color: Rgba,
    metrics: &Metrics,
) {
    let count = bands.len();
    if count == 0 || area.width() < 2.0 || area.height() < 2.0 {
        return;
    }
    // The band geometry comes from the same constants the layout reserved
    // space with, so the drawn bars fill that space exactly.
    let (bar, gap) = crate::layout::spectrum_band(metrics);
    let radius = (bar * 0.5).min(1.5 * metrics.scale);

    for (index, value) in bands.iter().enumerate() {
        let level = value.clamp(0.0, 1.0);
        let height = (level * area.height()).max(bar * 0.75);
        let left = area.left + index as f32 * (bar + gap);
        let rect = Rect::new(left, area.bottom - height, left + bar, area.bottom);
        // Quiet bars stay faint, so the display does not read as stuck noise.
        canvas::set_brush(brush, color.with_alpha(0.28 + level * 0.72));
        unsafe { target.FillRoundedRectangle(&canvas::rounded(rect, radius), brush) };
    }
}

/// The no-artwork tile: a quaver built from an ellipse, a stem and a flag.
fn draw_quaver(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    bounds: Rect,
    color: Rgba,
) {
    let unit = bounds.width() / 24.0;
    canvas::set_brush(brush, color);

    let head = D2D1_ELLIPSE {
        point: Vector2 {
            X: bounds.left + 9.6 * unit,
            Y: bounds.top + 16.4 * unit,
        },
        radiusX: 3.6 * unit,
        radiusY: 3.0 * unit,
    };
    unsafe { target.FillEllipse(&head, brush) };

    let stem = Rect::new(
        bounds.left + 12.2 * unit,
        bounds.top + 5.6 * unit,
        bounds.left + 13.8 * unit,
        bounds.top + 16.6 * unit,
    );
    unsafe { target.FillRectangle(&canvas::rect_f(stem), brush) };

    let flag = Rect::new(
        bounds.left + 12.4 * unit,
        bounds.top + 5.2 * unit,
        bounds.left + 18.6 * unit,
        bounds.top + 10.2 * unit,
    );
    unsafe { target.FillRoundedRectangle(&canvas::rounded(flag, 1.8 * unit), brush) };
}

/// Merge several geometries into one path, so the caller fills a single shape.
fn combine(factory: &ID2D1Factory, parts: &[ID2D1PathGeometry]) -> Result<ID2D1PathGeometry> {
    let merged = unsafe { factory.CreatePathGeometry()? };
    let sink = unsafe { merged.Open()? };
    for part in parts {
        unsafe { part.Stream(&sink)? };
    }
    unsafe { sink.Close()? };
    Ok(merged)
}

impl Glyphs {
    fn build(factory: &ID2D1Factory, scale: f32) -> Result<Self> {
        let s = |v: f32| v * scale;
        let previous_bar = canvas::build_path(
            factory,
            &[(s(4.0), s(3.0)), (s(5.6), s(3.0)), (s(5.6), s(13.0)), (s(4.0), s(13.0))],
            (0.0, 0.0),
        )?;
        let previous_triangle =
            canvas::build_path(factory, &[(s(13.0), s(3.4)), (s(13.0), s(12.6)), (s(6.4), s(8.0))], (0.0, 0.0))?;
        let next_bar = canvas::build_path(
            factory,
            &[(s(10.4), s(3.0)), (s(12.0), s(3.0)), (s(12.0), s(13.0)), (s(10.4), s(13.0))],
            (0.0, 0.0),
        )?;
        let next_triangle =
            canvas::build_path(factory, &[(s(3.0), s(3.4)), (s(3.0), s(12.6)), (s(9.6), s(8.0))], (0.0, 0.0))?;
        let play = canvas::build_path(
            factory,
            &[(s(4.6), s(3.1)), (s(12.4), s(8.0)), (s(4.6), s(12.9))],
            (0.0, 0.0),
        )?;
        let pause_left = canvas::build_path(
            factory,
            &[(s(4.4), s(3.0)), (s(6.8), s(3.0)), (s(6.8), s(13.0)), (s(4.4), s(13.0))],
            (0.0, 0.0),
        )?;
        let pause_right = canvas::build_path(
            factory,
            &[(s(9.2), s(3.0)), (s(11.6), s(3.0)), (s(11.6), s(13.0)), (s(9.2), s(13.0))],
            (0.0, 0.0),
        )?;

        Ok(Self {
            scale,
            previous: combine(factory, &[previous_bar, previous_triangle])?,
            next: combine(factory, &[next_bar, next_triangle])?,
            play,
            pause: combine(factory, &[pause_left, pause_right])?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_rect_carries_the_corner_radius() {
        let shape = canvas::rounded(Rect::new(1.0, 2.0, 11.0, 12.0), 3.0);
        assert_eq!(shape.radiusX, 3.0);
        assert_eq!(shape.rect.right, 11.0);
    }

    #[test]
    fn a_negative_radius_is_clamped_rather_than_producing_broken_geometry() {
        let shape = canvas::rounded(Rect::new(0.0, 0.0, 10.0, 10.0), -4.0);
        assert_eq!(shape.radiusX, 0.0);
    }

    #[test]
    fn rect_conversion_is_lossless() {
        let source = Rect::new(1.5, 2.5, 3.5, 4.5);
        let converted = canvas::rect_f(source);
        assert_eq!(converted.left, 1.5);
        assert_eq!(converted.bottom, 4.5);
    }
}
