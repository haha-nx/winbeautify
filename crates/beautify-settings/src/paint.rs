//! Drawing the settings page.
//!
//! Everything positional comes from [`crate::layout`], and everything
//! interactive from [`crate::controls`], so this module only decides what a
//! control *looks* like given its rectangle and state.
//!
//! # Why a window render target
//!
//! The widget bar is a layered window with a WIC bitmap, because it floats over
//! the taskbar with per-pixel alpha and must not be a real window. Settings is an
//! ordinary window: `ID2D1HwndRenderTarget` draws straight to it, which is less
//! code, gives proper text antialiasing against the window background, and — the
//! deciding factor — allows real Win32 child controls for the text boxes. Child
//! windows do not composite into a layered surface.

use windows::core::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
};

use beautify_widget::canvas::{Canvas, TextEngine};
use beautify_widget::theme::Rgba;

use crate::controls::{self, Part};
use crate::geom::Rect;
use crate::layout::{Layout, Metrics, Row};
use crate::palette::Palette;
use crate::schema::{Field, Kind, StatusKind};

/// Font weights, named for what they are used for.
const LABEL_WEIGHT: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_NORMAL;
const TITLE_WEIGHT: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT_SEMI_BOLD;

/// What the pointer and keyboard are doing, which is all the painter needs to
/// know about interaction.
#[derive(Debug, Clone, Default)]
pub struct Interaction {
    /// The row under the pointer.
    pub hover_row: Option<usize>,
    /// The part of that row under the pointer.
    pub hover_part: Option<Part>,
    /// The row a slider is being dragged on, if any.
    pub dragging_row: Option<usize>,
    /// The row whose dropdown is open.
    pub open_dropdown: Option<usize>,
    /// Index of the highlighted entry in that dropdown.
    pub dropdown_highlight: usize,
    /// The row whose text box has focus.
    pub focused_row: Option<usize>,
    /// Text being typed into the box that has focus.
    pub editing: String,
    /// Label of the currently active sidebar entry, for hover.
    pub hover_nav: Option<usize>,
}

/// Live values the status rows show.
#[derive(Debug, Clone, Default)]
pub struct StatusText {
    pub taskbar: String,
    pub taskbar_tone: Tone,
    pub spectrum: String,
    pub spectrum_tone: Tone,
    pub clipboard: String,
    pub clipboard_tone: Tone,
}

/// What a status pill's colour says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Off,
    On,
    Warn,
}

/// Device-bound drawing state, rebuilt when the window is.
pub struct Painter {
    factory: ID2D1Factory,
    target: Option<ID2D1HwndRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    pub text: TextEngine,
}

impl Painter {
    pub fn new() -> Result<Self> {
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        Ok(Self {
            factory,
            target: None,
            brush: None,
            text: TextEngine::new()?,
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
            // The layout is already in physical pixels, so the target must not
            // scale anything itself.
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
        Ok(())
    }

    /// Drop the device resources, e.g. after the window is recreated.
    pub fn detach(&mut self) {
        self.target = None;
        self.brush = None;
    }

    /// Draw one frame.
    pub fn render(
        &mut self,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
        config: &beautify_core::config::Config,
        interaction: &Interaction,
        status: &StatusText,
    ) -> Result<()> {
        let (Some(target), Some(_brush)) = (self.target.clone(), self.brush.clone()) else {
            return Ok(());
        };
        let canvas = Canvas::new(&target)?;
        canvas.clear(palette.window);

        let sidebar = layout.sidebar;
        canvas.fill_rect(sidebar, palette.sidebar);
        canvas.fill_rect(layout.titlebar, palette.titlebar);

        self.draw_nav(&canvas, layout, metrics, palette, interaction)?;
        self.draw_titlebar(&canvas, layout, metrics, palette)?;

        // The page is clipped to its viewport so a row scrolling up disappears
        // under the header instead of over it.
        canvas.clipped(layout.viewport, || {
            self.draw_content(&canvas, layout, metrics, palette, config, interaction, status)
        })?;

        self.draw_scrollbar(&canvas, layout, metrics, palette)?;

        // The dropdown floats above everything, including the scrollbar.
        if let Some(row) = layout
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .nth(interaction.open_dropdown.unwrap_or(usize::MAX))
        {
            self.draw_dropdown(&canvas, row, metrics, palette, interaction)?;
        }
        Ok(())
    }

    /// Draw text, applying the font's optical vertical correction.
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

    /// Fill a polygon given in absolute coordinates.
    fn polygon(&self, canvas: &Canvas<'_>, points: &[(f32, f32)], colour: Rgba) -> Result<()> {
        canvas.fill_polygon(&self.factory, points, (0.0, 0.0), colour)
    }

    fn draw_nav(
        &self,
        canvas: &Canvas<'_>,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
    ) -> Result<()> {
        let size = metrics.label_size() + 0.5;
        let format = self.text.format(size, TITLE_WEIGHT)?;
        for (index, item) in layout.nav.iter().enumerate() {
            let rect = item.rect.inset_by(metrics.content_padding() * 0.25, 0.0);
            if item.active {
                canvas.fill_rounded(rect, metrics.px(6.0), palette.accent.with_alpha(0.18));
            } else if interaction.hover_nav == Some(index) {
                canvas.fill_rounded(rect, metrics.px(6.0), palette.hover);
            }
            let colour = if item.active {
                palette.accent
            } else {
                palette.text_dim
            };
            let text_rect = Rect::new(
                rect.left + metrics.px(12.0),
                rect.top,
                rect.right - metrics.px(6.0),
                rect.bottom,
            );
            self.text_in(canvas, text_rect, item.section.title, &format, colour);
        }
        Ok(())
    }

    fn draw_titlebar(
        &self,
        canvas: &Canvas<'_>,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
    ) -> Result<()> {
        let format = self.text.format(metrics.label_size(), TITLE_WEIGHT)?;
        let rect = Rect::new(
            layout.titlebar.left + metrics.content_padding(),
            layout.titlebar.top,
            layout.titlebar.right - metrics.window_buttons_width(),
            layout.titlebar.bottom,
        );
        self.text_in(canvas, rect, "WinBeautify 设置", &format, palette.text);
        // Two window buttons, drawn as glyphs so no icon font is needed.
        for button in window_buttons(layout, metrics) {
            if button.kind == WindowButton::Close {
                canvas.fill_rounded(button.rect, metrics.px(4.0), palette.danger.with_alpha(0.9));
            } else {
                canvas.fill_rounded(button.rect, metrics.px(4.0), palette.hover);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_content(
        &self,
        canvas: &Canvas<'_>,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
        config: &beautify_core::config::Config,
        interaction: &Interaction,
        status: &StatusText,
    ) -> Result<()> {
        let title_format = self.text.format(metrics.section_title_size(), TITLE_WEIGHT)?;
        let hint_format = self.text.format(metrics.description_size(), LABEL_WEIGHT)?;
        let label_format = self.text.format(metrics.label_size(), LABEL_WEIGHT)?;
        let small_format = self.text.format(metrics.hint_size(), LABEL_WEIGHT)?;

        let mut row_index = 0usize;
        for card in &layout.content.cards {
            // Section heading sits above the first card.
            if layout.content.cards.first().is_some_and(|first| std::ptr::eq(card, first)) {
                let heading = Rect::new(
                    card.rect.left,
                    layout.viewport.top + metrics.content_padding()
                        - metrics.content_padding(),
                    card.rect.right,
                    card.rect.top,
                );
                self.text_in(canvas, Rect::new(heading.left, heading.top, heading.right, heading.top + metrics.section_title_size()), current_section_title(layout), &title_format, palette.text);
                let _ = heading;
            }

            canvas.fill_rounded(card.rect, metrics.card_radius(), palette.card);
            canvas.stroke_rounded(card.rect, metrics.card_radius(), palette.card_border, 1.0);

            if let Some(title) = card.spec.title {
                self.text_in(canvas, card.title, title, &small_format, palette.text_dim);
            }

            for row in &card.rows {
                let current = row_index;
                row_index += 1;
                if row.divider {
                    let line = Rect::new(
                        row.rect.left + metrics.card_padding(),
                        row.rect.top,
                        row.rect.right - metrics.card_padding(),
                        row.rect.top + 1.0,
                    );
                    canvas.fill_rect(line, palette.divider);
                }

                let hovered = interaction.hover_row == Some(current);
                if !row.label.is_empty() {
                    self.text_in(canvas, row.label, row.field.label, &label_format, palette.text);
                }
                if !row.hint.is_empty() {
                    if let Some(hint) = row.field.hint {
                        self.draw_wrapped(
                            canvas,
                            row.hint,
                            hint,
                            &hint_format,
                            palette.text_faint,
                            metrics,
                        );
                    }
                }
                self.draw_control(
                    canvas,
                    row,
                    metrics,
                    palette,
                    config,
                    hovered,
                    interaction,
                    status,
                    current,
                    &small_format,
                    &label_format,
                )?;
            }
        }
        Ok(())
    }

    /// Draw text wrapped to the box, at most two lines.
    fn draw_wrapped(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        text: &str,
        format: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
        colour: Rgba,
        metrics: &Metrics,
    ) {
        let lines = self.text.wrap(text, format, rect.width());
        let line_height = metrics.hint_size() * 1.35;
        for (index, line) in lines.iter().take(2).enumerate() {
            let top = rect.top + index as f32 * line_height;
            if top + line_height > rect.bottom + line_height {
                break;
            }
            let line_rect = Rect::new(rect.left, top, rect.right, top + line_height);
            self.text_in(canvas, line_rect, line, format, colour);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_control(
        &self,
        canvas: &Canvas<'_>,
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        config: &beautify_core::config::Config,
        hovered: bool,
        interaction: &Interaction,
        status: &StatusText,
        row_index: usize,
        small: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
        label: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    ) -> Result<()> {
        let parts = controls::parts(row.field, row.control, metrics);
        let value = if row.field.path.is_empty() {
            None
        } else {
            crate::access::read(config, row.field.path)
        };

        match row.field.kind {
            Kind::Switch => {
                let rect = parts.boxes[0];
                let on = value.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                let radius = rect.height() * 0.5;
                let track = if on {
                    palette.accent
                } else {
                    palette.control
                };
                canvas.fill_rounded(rect, radius, track);
                if !on {
                    canvas.stroke_rounded(rect, radius, palette.control_border, 1.0);
                }
                // Knob: a circle, drawn as a tightly rounded rectangle so the
                // canvas keeps one primitive.
                let knob = rect.height() - metrics.px(5.0);
                let knob_left = if on {
                    rect.right - metrics.px(2.5) - knob
                } else {
                    rect.left + metrics.px(2.5)
                };
                let knob_rect = Rect::new(
                    knob_left,
                    rect.top + metrics.px(2.5),
                    knob_left + knob,
                    rect.bottom - metrics.px(2.5),
                );
                canvas.fill_rounded(knob_rect, knob * 0.5, palette.on_accent);
            }
            Kind::Slider(slider) => {
                let Some(track) = parts.slider else {
                    return Ok(());
                };
                let current = value
                    .as_ref()
                    .and_then(|v| v.as_number())
                    .unwrap_or(slider.min);
                let span = (slider.max - slider.min).max(f64::EPSILON);
                let fraction = ((current - slider.min) / span).clamp(0.0, 1.0) as f32;
                let line_height = metrics.px(4.0);
                let line = Rect::new(
                    track.left,
                    track.center_y() - line_height * 0.5,
                    track.right,
                    track.center_y() + line_height * 0.5,
                );
                canvas.fill_rounded(line, line_height * 0.5, palette.control);
                let filled = Rect::new(
                    line.left,
                    line.top,
                    line.left + line.width() * fraction,
                    line.bottom,
                );
                canvas.fill_rounded(filled, line_height * 0.5, palette.accent);

                let knob = metrics.px(11.0);
                let knob_center = line.left + line.width() * fraction;
                let knob_rect = Rect::new(
                    knob_center - knob * 0.5,
                    track.center_y() - knob * 0.5,
                    knob_center + knob * 0.5,
                    track.center_y() + knob * 0.5,
                );
                canvas.fill_rounded(knob_rect, knob * 0.5, palette.accent);
                canvas.stroke_rounded(knob_rect, knob * 0.5, palette.card, 1.5);

                let readout = parts.boxes[0];
                let text = slider.format.render(current);
                self.text_in(canvas, readout, &text, small, palette.text_dim);
            }
            Kind::Select(choices) => {
                let rect = parts.boxes[0];
                let selected = value.as_ref().and_then(|v| v.as_text()).unwrap_or("");
                let label_text = choices
                    .iter()
                    .find(|choice| choice.value == selected)
                    .map(|choice| choice.label)
                    .unwrap_or(selected);
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
                let text_rect = Rect::new(
                    rect.left + metrics.px(9.0),
                    rect.top,
                    rect.right - metrics.px(20.0),
                    rect.bottom,
                );
                self.text_in(canvas, text_rect, label_text, small, palette.text);
                // Chevron.
                let size = metrics.px(4.0);
                let cx = rect.right - metrics.px(11.0);
                let cy = rect.center_y();
                self.polygon(
                    canvas,
                    &[
                        (cx - size, cy - size * 0.4),
                        (cx + size, cy - size * 0.4),
                        (cx, cy + size * 0.7),
                    ],
                    palette.text_dim,
                )?;
            }
            Kind::Color => {
                let swatch = parts.boxes[0];
                let hex = parts.boxes[1];
                let colour = value
                    .as_ref()
                    .and_then(|v| v.as_text())
                    .and_then(|text| text.parse::<beautify_core::geometry::Color>().ok());
                canvas.fill_rounded(swatch, metrics.px(5.0), palette.control);
                if let Some(colour) = colour {
                    canvas.fill_rounded(
                        swatch.inset_by(metrics.px(3.0), metrics.px(3.0)),
                        metrics.px(3.0),
                        Rgba::from_color(colour, 1.0),
                    );
                }
                canvas.stroke_rounded(swatch, metrics.px(5.0), palette.control_border, 1.0);
                canvas.fill_rounded(hex, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(hex, metrics.px(5.0), palette.control_border, 1.0);
                self.text_in(canvas, hex.inset_by(metrics.px(9.0), 0.0), value.as_ref().and_then(|v| v.as_text()).unwrap_or(""), small, palette.text);
            }
            Kind::Number { suffix, .. } => {
                let rect = parts.boxes[0];
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
                let number = value.as_ref().and_then(|v| v.as_number()).unwrap_or(0.0);
                let text = format!("{} {suffix}", number.round() as i64);
                self.text_in(canvas, rect.inset_by(metrics.px(9.0), 0.0), &text, small, palette.text);
            }
            Kind::Text { placeholder } => {
                let rect = parts.boxes[0];
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
                let editing = interaction.focused_row == Some(row_index);
                let text = if editing {
                    interaction.editing.clone()
                } else {
                    value.as_ref().and_then(|v| v.as_text()).unwrap_or("").to_string()
                };
                let (content, colour) = if text.is_empty() {
                    (placeholder.to_string(), palette.text_faint)
                } else {
                    (text, palette.text)
                };
                self.text_in(canvas, rect.inset_by(metrics.px(9.0), 0.0), &content, small, colour);
            }
            Kind::Status(kind) => {
                let slot = parts.boxes[0];
                let (text, tone) = match kind {
                    StatusKind::Taskbar => (&status.taskbar, status.taskbar_tone),
                    StatusKind::Spectrum => (&status.spectrum, status.spectrum_tone),
                    StatusKind::Clipboard => (&status.clipboard, status.clipboard_tone),
                };
                let colour = match tone {
                    Tone::On => palette.ok,
                    Tone::Warn => palette.warn,
                    Tone::Off => palette.text_dim,
                };
                // Sized to the text, but never wider than its slot.
                let width = (self
                    .text
                    .measure(text, small, slot.width())
                    .max(0.0)
                    + metrics.px(20.0))
                .min(slot.width());
                let pill = Rect::new(slot.left, slot.top, slot.left + width, slot.bottom);
                canvas.fill_rounded(pill, pill.height() * 0.5, colour.with_alpha(0.16));
                self.text_in(canvas, Rect::new(pill.left + metrics.px(10.0), pill.top, pill.right, pill.bottom), text, small, colour);
            }
            Kind::Action(buttons) => {
                for (index, button) in buttons.iter().enumerate() {
                    let Some(rect) = parts.buttons.get(index).copied() else {
                        break;
                    };
                    let colour = if button.danger {
                        palette.danger
                    } else {
                        palette.accent
                    };
                    let hovered = hovered
                        && interaction.hover_part == Some(Part::Button(index));
                    canvas.fill_rounded(
                        rect,
                        metrics.px(5.0),
                        colour.with_alpha(if hovered { 0.24 } else { 0.14 }),
                    );
                    self.text_in(canvas, rect, button.label, small, colour);
                }
            }
        }
        let _ = label;
        Ok(())
    }

    fn draw_scrollbar(
        &self,
        canvas: &Canvas<'_>,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
    ) -> Result<()> {
        if layout.scroll_max <= 0.0 {
            return Ok(());
        }
        let _ = metrics;
        canvas.fill_rounded(layout.scrollbar, layout.scrollbar.width() * 0.5, palette.text_faint.with_alpha(0.5));
        Ok(())
    }

    /// The open dropdown, drawn over the page.
    fn draw_dropdown(
        &self,
        canvas: &Canvas<'_>,
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
    ) -> Result<()> {
        let Kind::Select(choices) = row.field.kind else {
            return Ok(());
        };
        let Some(rect) = dropdown_rect(row, choices.len(), metrics) else {
            return Ok(());
        };
        let small = self.text.format(metrics.hint_size(), LABEL_WEIGHT)?;
        canvas.fill_rounded(rect, metrics.px(6.0), palette.card);
        canvas.stroke_rounded(rect, metrics.px(6.0), palette.control_border, 1.0);
        let row_height = metrics.dropdown_row_height();
        for (index, choice) in choices.iter().enumerate() {
            let entry = Rect::new(
                rect.left,
                rect.top + index as f32 * row_height,
                rect.right,
                rect.top + (index + 1) as f32 * row_height,
            );
            if interaction.dropdown_highlight == index {
                canvas.fill_rect(entry.inset_by(metrics.px(3.0), 0.0), palette.accent.with_alpha(0.18));
            }
            self.text_in(canvas, entry.inset_by(metrics.px(10.0), 0.0), choice.label, &small, palette.text);
        }
        Ok(())
    }
}

/// Where a dropdown's list appears, and how tall it is.
///
/// Placed below the box, or above it when there is not enough room — the same
/// flip the widget bar's flyout does.
pub fn dropdown_rect(
    row: &Row,
    entries: usize,
    metrics: &Metrics,
) -> Option<Rect> {
    let height = entries as f32 * metrics.dropdown_row_height() + metrics.px(6.0);
    if height <= 0.0 {
        return None;
    }
    let below = row.control.bottom + metrics.px(2.0);
    let above = row.control.top - metrics.px(2.0) - height;
    // The window is not known here, so "above" is only chosen when the row is
    // low enough that the list would clearly not fit; the window clamps later.
    let top = if row.control.top > height { above } else { below };
    Some(Rect::new(
        row.control.left,
        top,
        row.control.right,
        top + height,
    ))
}

/// The sidebar's window buttons, in the title bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowButton {
    Minimize,
    Close,
}

/// A positioned window button.
#[derive(Debug, Clone, Copy)]
pub struct PositionedButton {
    pub kind: WindowButton,
    pub rect: Rect,
}

/// Lay out the minimize and close buttons.
pub fn window_buttons(layout: &Layout, metrics: &Metrics) -> Vec<PositionedButton> {
    let size = metrics.px(22.0);
    let gap = metrics.px(4.0);
    let top = layout.titlebar.center_y() - size * 0.5;
    let right = layout.titlebar.right - metrics.px(10.0);
    vec![
        // Order is right to left: close sits at the very corner.
        PositionedButton {
            kind: WindowButton::Close,
            rect: Rect::new(right - size, top, right, top + size),
        },
        PositionedButton {
            kind: WindowButton::Minimize,
            rect: Rect::new(right - size * 2.0 - gap, top, right - size - gap, top + size),
        },
    ]
}

/// The title of the section being shown, for the page heading.
fn current_section_title(layout: &Layout) -> &'static str {
    layout
        .nav
        .iter()
        .find(|item| item.active)
        .map(|item| item.section.title)
        .unwrap_or("")
}

/// The field a row belongs to, for callers that only have the row index.
pub fn field_of(layout: &Layout, row_index: usize) -> Option<&'static Field> {
    layout
        .content
        .cards
        .iter()
        .flat_map(|card| card.rows.iter())
        .nth(row_index)
        .map(|row| row.field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Metrics;

    #[test]
    fn window_buttons_are_ordered_close_last() {
        let metrics = Metrics::new(96);
        let layout = crate::layout::layout(
            Rect::new(0.0, 0.0, 900.0, 640.0),
            &metrics,
            crate::schema::section("appearance").unwrap(),
            &beautify_core::config::Config::default(),
            0.0,
            &|_, _| 14.0,
        );
        let buttons = window_buttons(&layout, &metrics);
        assert_eq!(buttons.len(), 2);
        let close = buttons
            .iter()
            .find(|b| b.kind == WindowButton::Close)
            .unwrap();
        let minimize = buttons
            .iter()
            .find(|b| b.kind == WindowButton::Minimize)
            .unwrap();
        assert!(close.rect.left > minimize.rect.right, "close is at the corner");
        assert!(close.rect.right <= layout.titlebar.right);
        assert!(minimize.rect.left > 0.0);
    }
}
