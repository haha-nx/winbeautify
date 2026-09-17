//! Drawing the panel.
//!
//! Same division as the settings window: everything positional comes from
//! [`crate::layout`], and this module only decides what a shape looks like given
//! its rectangle and its state.

use windows::core::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U;
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1Layer, ID2D1SolidColorBrush,
    D2D1CreateFactory, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_TEXT_ALIGNMENT_CENTER,
};
use windows::Win32::Graphics::Imaging::IWICImagingFactory;

use beautify_widget::canvas::{Canvas, TextEngine};
use beautify_widget::layout::Rect;
use beautify_widget::theme::Rgba;

use crate::layout::{Heading, Metrics, Row, RowTarget, Scene, LINE_SPACING};
use crate::{ClipKind, ClipRow, Tab, TodoRow};

const LABEL_WEIGHT: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_NORMAL;
const TITLE_WEIGHT: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_SEMI_BOLD;

/// How many thumbnails to keep decoded at once.
///
/// A panel shows a handful of rows; keeping every image the user has ever copied
/// decoded would be hundreds of megabytes.
const THUMBNAIL_CACHE: usize = 24;

/// An icon drawn as geometry, so no icon font is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Star,
    Pin,
    Delete,
    /// What a clipboard entry is, drawn on its badge.
    ///
    /// Drawn rather than lettered: three letters in a 20-pixel square do not fit
    /// ("FILE" was wider than its own badge), and what a reader recognises here
    /// is the shape, not the word.
    Kind(ClipKind),
}

/// What the pointer and keyboard are doing.
#[derive(Debug, Clone, Default)]
pub struct Interaction {
    /// The row under the pointer.
    pub hover_row: Option<RowTarget>,
    /// The icon under the pointer, with the row it belongs to.
    pub hover_button: Option<(RowTarget, Icon)>,
    pub hover_tab: Option<Tab>,
    pub hover_close: bool,
    pub hover_footer: bool,
    /// True while the pointer is over the check box of a task row.
    pub hover_checkbox: Option<usize>,
    /// The field has focus and is being typed into.
    pub editing: bool,
    pub editing_text: String,
    /// A row's title is being edited in place.
    pub editing_row: Option<RowTarget>,
}

/// The colours, resolved from the app's theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub panel: Rgba,
    pub header: Rgba,
    pub text: Rgba,
    pub text_dim: Rgba,
    pub text_faint: Rgba,
    pub accent: Rgba,
    pub on_accent: Rgba,
    pub control: Rgba,
    pub control_border: Rgba,
    pub hover: Rgba,
    pub divider: Rgba,
    pub favourite: Rgba,
    pub danger: Rgba,
    pub light: bool,
}

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Rgba {
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

impl Palette {
    /// Resolve for the configured theme.
    ///
    /// The panel's own colour comes from the theme, not from the widget
    /// background: it is a panel over the desktop, and the webview version this
    /// replaces had exactly these two values baked into its stylesheet. The
    /// accent is the configured one, so a recoloured app looks recoloured.
    pub fn resolve(accent: beautify_core::geometry::Color, light: bool) -> Self {
        let accent = Rgba::from_color(accent, 1.0);
        if light {
            Self {
                // The window is translucent (DWM backdrop), so the panel colour
                // carries an alpha and the blur shows through it.
                panel: rgba(0xFF, 0xFF, 0xFF, 0.78),
                header: rgba(0x00, 0x00, 0x00, 0.04),
                text: rgba(0x1B, 0x1F, 0x2A, 1.0),
                text_dim: rgba(0x5A, 0x63, 0x76, 1.0),
                text_faint: rgba(0x8B, 0x93, 0xA5, 1.0),
                accent,
                on_accent: rgba(0xFF, 0xFF, 0xFF, 1.0),
                control: rgba(0xFF, 0xFF, 0xFF, 0.85),
                control_border: rgba(0x00, 0x00, 0x00, 0.14),
                hover: rgba(0x00, 0x00, 0x00, 0.07),
                divider: rgba(0x00, 0x00, 0x00, 0.07),
                favourite: rgba(0xE0, 0xA5, 0x1B, 1.0),
                danger: rgba(0xD1, 0x3B, 0x3B, 1.0),
                light: true,
            }
        } else {
            Self {
                panel: rgba(0x14, 0x16, 0x1C, 0.72),
                header: rgba(0xFF, 0xFF, 0xFF, 0.05),
                text: rgba(0xE9, 0xEB, 0xF2, 1.0),
                text_dim: rgba(0x9B, 0xA3, 0xB7, 1.0),
                text_faint: rgba(0x6E, 0x76, 0x88, 1.0),
                accent,
                on_accent: rgba(0xFF, 0xFF, 0xFF, 1.0),
                control: rgba(0xFF, 0xFF, 0xFF, 0.08),
                control_border: rgba(0xFF, 0xFF, 0xFF, 0.14),
                hover: rgba(0xFF, 0xFF, 0xFF, 0.09),
                divider: rgba(0xFF, 0xFF, 0xFF, 0.07),
                favourite: rgba(0xE8, 0xB3, 0x3B, 1.0),
                danger: rgba(0xE0, 0x5A, 0x5A, 1.0),
                light: false,
            }
        }
    }
}

/// Device-bound drawing state, rebuilt when the window is.
pub struct Painter {
    factory: ID2D1Factory,
    wic: IWICImagingFactory,
    target: Option<ID2D1HwndRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    layer: Option<ID2D1Layer>,
    pub text: TextEngine,
    /// Decoded thumbnails, oldest first, dropped when the device is rebuilt.
    thumbnails: Vec<(String, ID2D1Bitmap)>,
}

impl Painter {
    pub fn new() -> Result<Self> {
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        Ok(Self {
            factory,
            wic: beautify_widget::images::create_wic_factory()?,
            target: None,
            brush: None,
            layer: None,
            text: TextEngine::new()?,
            thumbnails: Vec::new(),
        })
    }

    /// Attach to a window, or resize the existing target.
    pub fn attach(&mut self, hwnd: HWND, width: u32, height: u32) -> Result<()> {
        if let Some(target) = &self.target {
            let current = unsafe { target.GetPixelSize() };
            if current.width == width && current.height == height {
                return Ok(());
            }
            unsafe { target.Resize(&D2D_SIZE_U { width, height }) }?;
            return Ok(());
        }

        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            // The layout is already in physical pixels.
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let hwnd_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width, height },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = unsafe { self.factory.CreateHwndRenderTarget(&properties, &hwnd_properties)? };
        let brush = unsafe { target.CreateSolidColorBrush(&Rgba::TRANSPARENT.to_d2d(), None)? };
        self.target = Some(target);
        self.brush = Some(brush);
        self.layer = None;
        // A bitmap belongs to the device that made it.
        self.thumbnails.clear();
        Ok(())
    }

    pub fn detach(&mut self) {
        self.target = None;
        self.brush = None;
        self.layer = None;
        self.thumbnails.clear();
    }

    /// Draw one frame.
    ///
    /// The rows are passed alongside the scene rather than inside it: the scene
    /// is rectangles only, so the window can keep it for hit testing.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        scene: &Scene,
        clips: &[ClipRow],
        todos: &[TodoRow],
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        placeholder: &str,
        stats: (i64, i64),
        empty_text: &str,
    ) -> Result<()> {
        let (Some(target), Some(_brush)) = (self.target.clone(), self.brush.clone()) else {
            return Ok(());
        };
        let canvas = Canvas::new(&target)?;
        // Direct2D only presents a frame when these two match; drawing outside
        // them is silently discarded.
        canvas.begin();
        let drawn = self.draw(
            &canvas,
            scene,
            clips,
            todos,
            metrics,
            palette,
            interaction,
            placeholder,
            stats,
            empty_text,
        );
        let presented = canvas.end();
        drawn?;
        presented
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        clips: &[ClipRow],
        todos: &[TodoRow],
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        placeholder: &str,
        stats: (i64, i64),
        empty_text: &str,
    ) -> Result<()> {
        canvas.clear(palette.panel);
        canvas.fill_rect(scene.header, palette.header);

        self.draw_header(canvas, scene, metrics, palette, interaction)?;
        self.draw_field(canvas, scene, metrics, palette, interaction, placeholder);

        canvas.clipped(scene.list, || {
            for row in &scene.rows {
                self.draw_row(
                    canvas,
                    scene,
                    clips,
                    todos,
                    row,
                    metrics,
                    palette,
                    interaction,
                );
            }
            if scene.rows.is_empty() {
                let format = self.text.format_aligned(
                    metrics.title_size(),
                    LABEL_WEIGHT,
                    DWRITE_TEXT_ALIGNMENT_CENTER,
                )?;
                let padded = scene
                    .list
                    .inset_by(metrics.padding(), metrics.padding());
                self.text_in(canvas, padded, empty_text, &format, palette.text_faint);
            }
            Ok::<(), windows::core::Error>(())
        })?;

        self.draw_footer(canvas, scene, metrics, palette, interaction, stats);
        self.draw_scrollbar(canvas, scene, palette);
        Ok(())
    }

    fn draw_header(
        &self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
    ) -> Result<()> {
        // Centred in their buttons: a tab is a button, and the text inside a
        // drawn box belongs in the middle of it.
        let format = self.text.format_aligned(
            metrics.title_size(),
            TITLE_WEIGHT,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        )?;
        for (tab, rect) in &scene.tabs {
            let active = *tab == scene.tab;
            if active {
                canvas.fill_rounded(*rect, metrics.px(6.0), palette.accent.with_alpha(0.22));
            } else if interaction.hover_tab == Some(*tab) {
                canvas.fill_rounded(*rect, metrics.px(6.0), palette.hover);
            }
            let colour = if active { palette.accent } else { palette.text_dim };
            self.text_in(canvas, *rect, tab.label(), &format, colour);
        }
        // The close button, with the same cross as the settings title bar.
        if interaction.hover_close {
            canvas.fill_rounded(scene.close, metrics.px(4.0), palette.danger);
        }
        self.draw_cross(
            canvas,
            scene.close,
            if interaction.hover_close {
                palette.on_accent
            } else {
                palette.text_dim
            },
            metrics.px(4.5),
            metrics.px(1.4),
        )?;
        Ok(())
    }

    fn draw_field(
        &self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        placeholder: &str,
    ) {
        let format = self.text.format(metrics.title_size(), LABEL_WEIGHT);
        canvas.fill_rounded(scene.field, metrics.px(6.0), palette.control);
        canvas.stroke_rounded(
            scene.field,
            metrics.px(6.0),
            if interaction.editing {
                palette.accent
            } else {
                palette.control_border
            },
            if interaction.editing { 2.0 } else { 1.0 },
        );
        let inner = scene.field.inset_by(metrics.px(9.0), 0.0);
        let Ok(format) = format else {
            return;
        };
        let (text, colour) = if interaction.editing || !interaction.editing_text.is_empty() {
            (interaction.editing_text.as_str(), palette.text)
        } else {
            (placeholder, palette.text_faint)
        };
        self.text_in(canvas, inner, text, &format, colour);

        // A caret, since these fields are drawn rather than being child
        // controls: there is no system caret to show where typing goes.
        if interaction.editing {
            let typed = self
                .text
                .measure(text, &format, inner.width())
                .min(inner.width());
            let x = (inner.left + typed).min(inner.right - metrics.px(1.0));
            canvas.fill_rect(
                Rect::new(
                    x,
                    inner.top + metrics.px(4.0),
                    x + metrics.px(1.5),
                    inner.bottom - metrics.px(4.0),
                ),
                palette.accent,
            );
        }
    }

    /// A section title: a quiet label over the rows it introduces.
    #[allow(clippy::too_many_arguments)]
    fn draw_heading(
        &self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        row: &Row,
        heading: Heading,
        todos: &[TodoRow],
        metrics: &Metrics,
        palette: &Palette,
    ) {
        self.divider(canvas, scene, row, metrics, palette);
        let Ok(format) = self.text.format(metrics.meta_size(), TITLE_WEIGHT) else {
            return;
        };
        // Counted from the tasks rather than from the visible rows: the number
        // is the section's size, not how much of it fits on screen.
        let count = todos
            .iter()
            .filter(|task| task.done == (heading == Heading::Done))
            .count();
        let label = format!("{} · {count}", heading.label());
        self.text_in(canvas, row.title, &label, &format, palette.text_faint);
    }

    /// The hairline between two rows, inset so the list reads as a list rather
    /// than as a table.
    fn divider(
        &self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
    ) {
        if row.rect.top <= scene.list.top + 1.0 {
            return;
        }
        canvas.fill_rect(
            Rect::new(
                row.rect.left + metrics.padding(),
                row.rect.top,
                row.rect.right - metrics.padding(),
                row.rect.top + 1.0,
            ),
            palette.divider,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        &mut self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        clips: &[ClipRow],
        todos: &[TodoRow],
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
    ) {
        if let RowTarget::Heading(heading) = row.target {
            self.draw_heading(canvas, scene, row, heading, todos, metrics, palette);
            return;
        }
        if interaction.hover_row == Some(row.target) {
            canvas.fill_rect(row.rect, palette.hover);
        }
        self.divider(canvas, scene, row, metrics, palette);

        match row.target {
            RowTarget::Todo(index) => {
                self.draw_checkbox(canvas, row, todos.get(index), metrics, palette)
            }
            RowTarget::Clip(index) => {
                if let Some(badge) = row.badge {
                    let kind = clips.get(index).map(|entry| entry.kind);
                    if let Some(kind) = kind {
                        self.draw_badge(canvas, badge, kind, metrics, palette);
                    }
                }
                if let Some(entry) = clips.get(index) {
                    self.draw_thumbnail(canvas, row, entry, metrics, palette);
                }
            }
            RowTarget::Heading(_) => {}
        }

        if let Err(e) = self.draw_row_text(canvas, clips, todos, row, metrics, palette, interaction)
        {
            tracing::debug!("a row could not be drawn: {e}");
        }

        for button in &row.buttons {
            let lit = interaction.hover_button == Some((row.target, button.icon));
            if lit {
                canvas.fill_rounded(button.rect, metrics.px(5.0), palette.hover);
            }
            let colour = match (button.icon, button.active) {
                (Icon::Kind(_), _) => palette.text_dim,
                (Icon::Star, true) => palette.favourite,
                (Icon::Pin, true) => palette.accent,
                (_, _) if lit => palette.text,
                // Always visible, always quiet: these were hover-only in the
                // webview panel, which made them something to find rather than
                // something to use.
                _ => palette.text_faint,
            };
            let _ = self.draw_icon(canvas, button.icon, button.rect, colour, metrics, button.active);
        }
    }

    /// The tick box of a task row.
    fn draw_checkbox(
        &self,
        canvas: &Canvas<'_>,
        row: &Row,
        task: Option<&TodoRow>,
        metrics: &Metrics,
        palette: &Palette,
    ) {
        let Some(box_rect) = row.checkbox else {
            return;
        };
        let done = task.is_some_and(|task| task.done);
        if done {
            canvas.fill_rounded(box_rect, metrics.px(4.0), palette.accent);
            // A tick, not just a filled box: "done" has to be readable at a
            // glance down a list.
            let tick = [
                (
                    box_rect.left + box_rect.width() * 0.24,
                    box_rect.center_y(),
                ),
                (
                    box_rect.left + box_rect.width() * 0.44,
                    box_rect.bottom - box_rect.height() * 0.28,
                ),
                (
                    box_rect.right - box_rect.width() * 0.22,
                    box_rect.top + box_rect.height() * 0.3,
                ),
            ];
            let _ = canvas.stroke_polyline(
                &self.factory,
                &tick,
                (0.0, 0.0),
                palette.on_accent,
                metrics.px(1.8),
            );
        } else {
            canvas.fill_rounded(box_rect, metrics.px(4.0), palette.control);
            canvas.stroke_rounded(box_rect, metrics.px(4.0), palette.control_border, 1.0);
        }
    }

    /// An image's thumbnail, inside the box the layout made for it.
    fn draw_thumbnail(
        &mut self,
        canvas: &Canvas<'_>,
        row: &Row,
        entry: &ClipRow,
        metrics: &Metrics,
        palette: &Palette,
    ) {
        let Some(bounds) = row.thumbnail else {
            return;
        };
        let radius = metrics.px(5.0);
        canvas.fill_rounded(bounds, radius, palette.control);
        if let Some(bitmap) = self.thumbnail(&entry.image_path) {
            let size = unsafe { bitmap.GetSize() };
            let fitted = fit_into((size.width, size.height), bounds);
            // The image is letterboxed inside the box and clipped to it, so a
            // portrait screenshot reads as a picture rather than a smear.
            let _ = canvas.clipped(bounds, || self.draw_bitmap(canvas, &bitmap, fitted, bounds, radius));
        }
        canvas.stroke_rounded(bounds, radius, palette.control_border, 1.0);
    }

    /// The title, the recognised text and the meta line — or the in-place editor
    /// when a task is being renamed.
    #[allow(clippy::too_many_arguments)]
    fn draw_row_text(
        &mut self,
        canvas: &Canvas<'_>,
        clips: &[ClipRow],
        todos: &[TodoRow],
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
    ) -> Result<()> {
        let editing = interaction.editing_row == Some(row.target);
        match row.target {
            RowTarget::Heading(_) => Ok(()),
            RowTarget::Todo(index) => {
                let task = todos.get(index);
                let done = task.is_some_and(|task| task.done);
                let (text, colour) = if editing {
                    (interaction.editing_text.as_str(), palette.accent)
                } else {
                    (
                        task.map(|task| task.title.as_str()).unwrap_or_default(),
                        if done { palette.text_faint } else { palette.text },
                    )
                };
                self.text_block(
                    canvas,
                    row.title,
                    text,
                    metrics.todo_size(),
                    LABEL_WEIGHT,
                    colour,
                );
                Ok(())
            }
            RowTarget::Clip(index) => {
                let Some(entry) = clips.get(index) else {
                    return Ok(());
                };
                self.text_block(
                    canvas,
                    row.title,
                    &entry.title,
                    metrics.title_size(),
                    LABEL_WEIGHT,
                    palette.text,
                );
                if let Some(rect) = row.ocr {
                    self.text_block(
                        canvas,
                        rect,
                        &entry.ocr,
                        metrics.ocr_size(),
                        LABEL_WEIGHT,
                        palette.text_dim,
                    );
                }
                if let Some(rect) = row.meta {
                    let format = self.text.format(metrics.meta_size(), LABEL_WEIGHT)?;
                    self.text_in(canvas, rect, &entry.meta, &format, palette.text_faint);
                }
                Ok(())
            }
        }
    }

    /// The height a wrapped block of `text` needs, clamped to `max_lines`.
    ///
    /// The counterpart of [`Self::text_block`], and the reason the two cannot
    /// disagree: a block is either one line or every line the row allows, so the
    /// measurement is a single width comparison rather than a line-by-line wrap.
    /// That matters because the whole list is measured to size the scrollbar,
    /// and only the rows on screen are ever drawn.
    pub fn text_height(&self, text: &str, width: f32, px: f32, max_lines: usize) -> f32 {
        if text.is_empty() || width <= 0.0 {
            return 0.0;
        }
        let max = max_lines.max(1);
        // A hard line break is a line whatever the width says.
        let hard = text.matches('\n').count() + 1;
        let lines = if hard > 1 {
            hard
        } else {
            match self.text.format(px, DWRITE_FONT_WEIGHT_NORMAL) {
                Ok(format) if self.text.measure(text, &format, 4096.0) <= width => 1,
                _ => max,
            }
        };
        lines.min(max).max(1) as f32 * px * LINE_SPACING
    }

    /// A block of text that may take more than one line.
    ///
    /// Drawn with a wrapping format of its own rather than joined with newlines:
    /// word wrapping is the only thing that breaks Chinese text, which has no
    /// spaces to break at, and the block is top-aligned so a line too many is
    /// clipped cleanly at the bottom instead of at both ends.
    ///
    /// No trimming: the rectangle is exactly as tall as the lines the layout
    /// paid for, so anything past `max_lines` falls outside it and is dropped by
    /// the clip rather than drawn half a glyph high.
    #[allow(clippy::too_many_arguments)]
    fn text_block(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        text: &str,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
        colour: Rgba,
    ) {
        if text.is_empty() || rect.is_empty() {
            return;
        }
        let Ok(format) = self.text.block_format(size, weight) else {
            return;
        };
        canvas.text(text, &format, rect, colour, 0.0);
    }

    /// The small badge that says what kind of entry a row is.
    fn draw_badge(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        kind: ClipKind,
        metrics: &Metrics,
        palette: &Palette,
    ) {
        canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
        canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
        let _ = self.draw_icon(canvas, Icon::Kind(kind), rect, palette.text_dim, metrics, false);
    }

    fn draw_footer(
        &self,
        canvas: &Canvas<'_>,
        scene: &Scene,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        stats: (i64, i64),
    ) {
        let Ok(format) = self.text.format(metrics.meta_size(), LABEL_WEIGHT) else {
            return;
        };
        let text = match scene.tab {
            Tab::Clipboard => format!("{} 条 · 收藏 {}", stats.0, stats.1),
            Tab::Todo => String::new(),
        };
        self.text_in(
            canvas,
            Rect::new(
                scene.footer.left + metrics.padding(),
                scene.footer.top,
                scene.footer.right - metrics.px(100.0),
                scene.footer.bottom,
            ),
            &text,
            &format,
            palette.text_faint,
        );

        if let Some(button) = scene.footer_button {
            let lit = interaction.hover_footer;
            canvas.fill_rounded(
                button,
                metrics.px(5.0),
                palette.danger.with_alpha(if lit { 0.22 } else { 0.12 }),
            );
            // Centred in its button: text in a drawn box belongs in the middle.
            if let Ok(centred) = self.text.format_aligned(
                metrics.meta_size(),
                LABEL_WEIGHT,
                DWRITE_TEXT_ALIGNMENT_CENTER,
            ) {
                self.text_in(canvas, button, "清空未收藏", &centred, palette.danger);
            }
        }
    }

    fn draw_scrollbar(&self, canvas: &Canvas<'_>, scene: &Scene, palette: &Palette) {
        if scene.scroll_max <= 0.0 {
            return;
        }
        canvas.fill_rounded(
            scene.scrollbar,
            scene.scrollbar.width() * 0.5,
            palette.text_faint.with_alpha(0.5),
        );
    }

    /// One icon, drawn with the primitives the canvas already has.
    fn draw_icon(
        &self,
        canvas: &Canvas<'_>,
        icon: Icon,
        rect: Rect,
        colour: Rgba,
        metrics: &Metrics,
        filled: bool,
    ) -> Result<()> {
        let centre = ((rect.left + rect.right) * 0.5, rect.center_y());
        let arm = metrics.px(6.0);
        let thin = metrics.px(1.5);
        match icon {
            Icon::Delete => self.draw_cross(canvas, rect, colour, metrics.px(5.0), thin)?,
            Icon::Star => {
                let points = star_points(centre, arm, arm * 0.42);
                if filled {
                    canvas.fill_polygon(&self.factory, &points, (0.0, 0.0), colour)?;
                } else {
                    canvas.stroke_polyline(&self.factory, &points, (0.0, 0.0), colour, thin)?;
                }
            }
            Icon::Pin => {
                // A ball on a stem, leaning the way a pin leans.
                let head_radius = arm * 0.42;
                let head = Rect::new(
                    centre.0 - head_radius,
                    centre.1 - arm * 0.9,
                    centre.0 + head_radius,
                    centre.1 - arm * 0.9 + head_radius * 2.0,
                );
                canvas.fill_rounded(head, head_radius, colour);
                canvas.fill_rect(
                    Rect::new(
                        centre.0 - thin * 0.5,
                        head.bottom - thin * 0.5,
                        centre.0 + thin * 0.5,
                        centre.1 + arm * 0.8,
                    ),
                    colour,
                );
            }
            Icon::Kind(kind) => self.draw_kind(canvas, kind, rect, colour, metrics)?,
        }
        Ok(())
    }

    /// The kind glyph, inside the badge it is drawn on.
    fn draw_kind(
        &self,
        canvas: &Canvas<'_>,
        kind: ClipKind,
        rect: Rect,
        colour: Rgba,
        metrics: &Metrics,
    ) -> Result<()> {
        let inner = rect.inset_by(metrics.px(5.5), metrics.px(5.5));
        let bar = (inner.height() * 0.16).max(1.0);
        match kind {
            // Three lines of text.
            ClipKind::Text => {
                for (index, width) in [1.0f32, 1.0, 0.6].into_iter().enumerate() {
                    let top = inner.top + inner.height() * (0.08 + 0.42 * index as f32);
                    canvas.fill_rect(
                        Rect::new(
                            inner.left,
                            top,
                            inner.left + inner.width() * width,
                            top + bar,
                        ),
                        colour,
                    );
                }
            }
            // A diagonal arrow leaving the corner: what a link does.
            ClipKind::Link => {
                let thickness = bar * 1.5;
                canvas.fill_polygon(
                    &self.factory,
                    &bar_points(
                        ((inner.left + inner.right) * 0.5, inner.center_y()),
                        inner.width() * 0.44,
                        thickness,
                        -1.0,
                    ),
                    (0.0, 0.0),
                    colour,
                )?;
                canvas.fill_polygon(
                    &self.factory,
                    &[
                        (inner.right, inner.top),
                        (inner.right - inner.width() * 0.60, inner.top),
                        (inner.right, inner.top + inner.height() * 0.60),
                    ],
                    (0.0, 0.0),
                    colour,
                )?;
            }
            // A folder: a tab on the left, an outlined body under it.
            ClipKind::Files => {
                canvas.fill_rounded(
                    Rect::new(
                        inner.left,
                        inner.top + inner.height() * 0.08,
                        inner.left + inner.width() * 0.46,
                        inner.top + inner.height() * 0.36,
                    ),
                    metrics.px(1.5),
                    colour,
                );
                canvas.stroke_rounded(
                    Rect::new(
                        inner.left,
                        inner.top + inner.height() * 0.28,
                        inner.right,
                        inner.bottom,
                    ),
                    metrics.px(1.5),
                    colour,
                    bar * 0.7,
                );
            }
            // A picture: a frame, a sun and a hill.
            ClipKind::Image => {
                canvas.stroke_rounded(inner, metrics.px(1.5), colour, bar * 0.8);
                let sun = bar * 1.6;
                canvas.fill_rounded(
                    Rect::new(
                        inner.left + inner.width() * 0.16,
                        inner.top + inner.height() * 0.16,
                        inner.left + inner.width() * 0.16 + sun,
                        inner.top + inner.height() * 0.16 + sun,
                    ),
                    sun * 0.5,
                    colour,
                );
                canvas.fill_polygon(
                    &self.factory,
                    &[
                        (inner.left + bar, inner.bottom - bar),
                        (inner.left + inner.width() * 0.44, inner.top + inner.height() * 0.46),
                        (inner.right - bar, inner.bottom - bar),
                    ],
                    (0.0, 0.0),
                    colour,
                )?;
            }
        }
        Ok(())
    }

    /// A cross made of two rotated bars.
    fn draw_cross(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        colour: Rgba,
        arm: f32,
        thickness: f32,
    ) -> Result<()> {
        let centre = ((rect.left + rect.right) * 0.5, rect.center_y());
        for tilt in [-1.0f32, 1.0] {
            canvas.fill_polygon(
                &self.factory,
                &bar_points(centre, arm, thickness, tilt),
                (0.0, 0.0),
                colour,
            )?;
        }
        Ok(())
    }

    fn text_in(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        text: &str,
        format: &IDWriteTextFormat,
        colour: Rgba,
    ) {
        if text.is_empty() {
            return;
        }
        let offset = self.text.vertical_correction(format);
        canvas.text(text, format, rect, colour, offset);
    }

    /// The decoded thumbnail for a stored image, loading it on first use.
    ///
    /// The cache is dropped when the render target is rebuilt, because a
    /// Direct2D bitmap belongs to the device that created it.
    fn thumbnail(&mut self, path: &str) -> Option<ID2D1Bitmap> {
        if path.is_empty() {
            return None;
        }
        if let Some((_, bitmap)) = self.thumbnails.iter().find(|(key, _)| key == path) {
            return Some(bitmap.clone());
        }
        let target = self.target.clone()?;
        let bytes = std::fs::read(path).ok()?;
        // Stored clipboard images are `.bmp`, which WIC decodes directly.
        let bitmap = unsafe {
            beautify_widget::images::bitmap_from_bytes(&target, &self.wic, &bytes)
        }?;
        if self.thumbnails.len() >= THUMBNAIL_CACHE {
            self.thumbnails.remove(0);
        }
        self.thumbnails.push((path.to_string(), bitmap.clone()));
        Some(bitmap)
    }

    /// Draw a bitmap into `rect`, clipped to a rounded `bounds`.
    fn draw_bitmap(
        &mut self,
        canvas: &Canvas<'_>,
        bitmap: &ID2D1Bitmap,
        rect: Rect,
        bounds: Rect,
        radius: f32,
    ) -> Result<()> {
        if self.layer.is_none() {
            // Layers are created by the render target, not the factory.
            let target = self
                .target
                .clone()
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            self.layer = Some(unsafe { target.CreateLayer(None) }?);
        }
        let Some(layer) = self.layer.clone() else {
            return Ok(());
        };
        canvas.image_rounded(&self.factory, &layer, bitmap, rect, radius, bounds)
    }
}

/// A five-pointed star, as a closed polygon.
fn star_points(centre: (f32, f32), outer: f32, inner: f32) -> Vec<(f32, f32)> {
    let mut points = Vec::with_capacity(10);
    for step in 0..10 {
        // Start at the top and alternate outer and inner radii.
        let angle = -std::f32::consts::FRAC_PI_2 + step as f32 * std::f32::consts::PI / 5.0;
        let radius = if step % 2 == 0 { outer } else { inner };
        points.push((
            centre.0 + radius * angle.cos(),
            centre.1 + radius * angle.sin(),
        ));
    }
    points
}

/// The four corners of a thin bar through `centre`, tilted by `tilt`.
fn bar_points(centre: (f32, f32), arm: f32, thickness: f32, tilt: f32) -> [(f32, f32); 4] {
    let half = thickness * 0.5;
    let (ax, ay) = (1.0, tilt);
    let length = (ax * ax + ay * ay).sqrt();
    let (ux, uy) = (ax / length * arm, ay / length * arm);
    let (px, py) = (-ay / length * half, ax / length * half);
    [
        (centre.0 - ux + px, centre.1 - uy + py),
        (centre.0 + ux + px, centre.1 + uy + py),
        (centre.0 + ux - px, centre.1 + uy - py),
        (centre.0 - ux - px, centre.1 - uy - py),
    ]
}

/// Fit a `source` size inside `box`, keeping its aspect ratio.
fn fit_into(source: (f32, f32), container: Rect) -> Rect {
    if source.0 <= 0.0 || source.1 <= 0.0 {
        return container;
    }
    let scale = (container.width() / source.0).min(container.height() / source.1);
    let (width, height) = (source.0 * scale, source.1 * scale);
    let left = container.left + (container.width() - width) * 0.5;
    let top = container.top + (container.height() - height) * 0.5;
    Rect::new(left, top, left + width, top + height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_image_is_letterboxed_rather_than_squashed() {
        let box_rect = Rect::new(0.0, 0.0, 40.0, 40.0);
        let fitted = fit_into((400.0, 200.0), box_rect);
        assert!((fitted.width() - 40.0).abs() < 0.01);
        assert!((fitted.height() - 20.0).abs() < 0.01, "the aspect ratio is kept");
        assert!((fitted.center_y() - box_rect.center_y()).abs() < 0.01, "centred");
        // A tall image fills the height instead.
        let tall = fit_into((100.0, 400.0), box_rect);
        assert!((tall.height() - 40.0).abs() < 0.01);
        assert!((tall.width() - 10.0).abs() < 0.01);
    }

    #[test]
    fn a_star_has_ten_corners_alternating_radius() {
        let points = star_points((0.0, 0.0), 10.0, 4.0);
        assert_eq!(points.len(), 10);
        let radius = |(x, y): (f32, f32)| (x * x + y * y).sqrt();
        for (index, point) in points.iter().enumerate() {
            let expected = if index % 2 == 0 { 10.0 } else { 4.0 };
            assert!((radius(*point) - expected).abs() < 0.01);
        }
        // The first point is straight up.
        assert!((points[0].0).abs() < 0.01);
        assert!(points[0].1 < 0.0);
    }

    #[test]
    fn a_bar_is_as_thick_as_it_is_asked_to_be() {
        let points = bar_points((0.0, 0.0), 10.0, 2.0, 0.0);
        let width = points.iter().map(|p| p.0).fold(f32::MIN, f32::max)
            - points.iter().map(|p| p.0).fold(f32::MAX, f32::min);
        let height = points.iter().map(|p| p.1).fold(f32::MIN, f32::max)
            - points.iter().map(|p| p.1).fold(f32::MAX, f32::min);
        assert!((width - 20.0).abs() < 0.01, "armed ±10");
        assert!((height - 2.0).abs() < 0.01, "thickness");
    }
}
