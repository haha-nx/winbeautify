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
    DWRITE_TEXT_ALIGNMENT_CENTER,
};

use beautify_widget::canvas::{Canvas, TextEngine};
use beautify_widget::theme::Rgba;

use crate::controls::{self, Part};
use crate::geom::Rect;
use crate::layout::{Layout, Metrics, Row};
use crate::palette::Palette;
use crate::schema::{Field, InfoKey, Kind, StatusKind};

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
    /// The row whose colour palette is open.
    pub open_color: Option<usize>,
    /// The palette cell under the pointer, as `(row, column)`.
    pub color_highlight: Option<(usize, usize)>,
    /// The row whose text box has focus.
    pub focused_row: Option<usize>,
    /// Text being typed into the box that has focus.
    pub editing: String,
    /// The row whose hotkey field is recording, with what has been pressed so
    /// far ("Ctrl+Alt+"), so the field can show the combination as it is built.
    pub recording: Option<usize>,
    pub recording_text: String,
    /// Label of the currently active sidebar entry, for hover.
    pub hover_nav: Option<usize>,
    /// The title-bar button under the pointer, if any.
    pub hover_window: Option<WindowButton>,
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
    /// Live values for the 关于 page's read-only rows.
    pub info: Vec<(InfoKey, String)>,
}

impl StatusText {
    /// The value a 关于 row shows, or an empty string when the host had nothing
    /// to say — a missing value is not worth a placeholder in the middle of a
    /// list of facts.
    pub fn info(&self, key: InfoKey) -> &str {
        self.info
            .iter()
            .find(|(entry, _)| *entry == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }

    /// Record one value, replacing any earlier one for the same key.
    pub fn set_info(&mut self, key: InfoKey, value: impl Into<String>) {
        let value = value.into();
        match self.info.iter_mut().find(|(entry, _)| *entry == key) {
            Some((_, slot)) => *slot = value,
            None => self.info.push((key, value)),
        }
    }
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
        // Direct2D only presents a frame when `BeginDraw` and `EndDraw` are
        // matched; every call between them is buffered and any call outside is
        // silently discarded. Getting this wrong produces a window that is
        // created, painted and completely blank.
        canvas.begin();
        let drawn = self.draw_frame(&canvas, layout, metrics, palette, config, interaction, status);
        // `EndDraw` runs even when a shape failed: leaving the target in a
        // drawing state makes it refuse every later frame.
        let presented = canvas.end();
        drawn?;
        presented
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_frame(
        &self,
        canvas: &Canvas<'_>,
        layout: &Layout,
        metrics: &Metrics,
        palette: &Palette,
        config: &beautify_core::config::Config,
        interaction: &Interaction,
        status: &StatusText,
    ) -> Result<()> {
        canvas.clear(palette.window);

        let sidebar = layout.sidebar;
        canvas.fill_rect(sidebar, palette.sidebar);
        canvas.fill_rect(layout.titlebar, palette.titlebar);

        self.draw_nav(canvas, layout, metrics, palette, interaction)?;
        self.draw_titlebar(canvas, layout, metrics, palette, interaction)?;

        // The page is clipped to its viewport so a row scrolling up disappears
        // under the header instead of over it.
        canvas.clipped(layout.viewport, || {
            self.draw_content(canvas, layout, metrics, palette, config, interaction, status)
        })?;

        self.draw_scrollbar(canvas, layout, metrics, palette)?;

        // The dropdown floats above everything, including the scrollbar.
        if let Some(row) = layout
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .nth(interaction.open_dropdown.unwrap_or(usize::MAX))
        {
            // The row's current value, so the list can show which entry that
            // is: a dropdown that only marks what the pointer is over leaves the
            // user guessing what they have chosen.
            let current = crate::access::read(config, row.field.path);
            let current = current.as_ref().and_then(|v| v.as_text()).unwrap_or("");
            self.draw_dropdown(canvas, row, metrics, palette, interaction, current)?;
        }

        // A colour palette is the same kind of thing and goes in the same place.
        if let Some(row) = layout
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .nth(interaction.open_color.unwrap_or(usize::MAX))
        {
            let current = crate::access::read(config, row.field.path)
                .and_then(|value| value.as_text().map(str::to_string))
                .and_then(|text| text.parse::<beautify_core::geometry::Color>().ok());
            self.draw_color_popup(canvas, row, metrics, palette, interaction, current)?;
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

    /// The text being typed into `row`, when it is the focused one.
    fn typing<'a>(&self, interaction: &'a Interaction, row: usize) -> Option<&'a str> {
        (interaction.focused_row == Some(row)).then_some(interaction.editing.as_str())
    }

    /// Draw the contents of an editable box, with a caret while it has focus.
    ///
    /// The caret is drawn rather than delegated to Windows: these are not child
    /// `EDIT` controls (they would not composite into the Direct2D surface), so
    /// there is no system caret, and a text field with no visible insertion
    /// point is unusable. Measuring the string is the only way to know where the
    /// end of it is, since no text layout is retained between frames.
    #[allow(clippy::too_many_arguments)]
    fn draw_box_text(
        &self,
        canvas: &Canvas<'_>,
        rect: Rect,
        stored: &str,
        placeholder: &str,
        format: &IDWriteTextFormat,
        palette: &Palette,
        metrics: &Metrics,
        typing: Option<&str>,
    ) {
        let inner = rect.inset_by(metrics.px(9.0), 0.0);
        let (text, colour) = match typing {
            Some(typed) => (typed, palette.text),
            None if stored.is_empty() => (placeholder, palette.text_faint),
            None => (stored, palette.text),
        };
        self.text_in(canvas, inner, text, format, colour);

        if typing.is_none() {
            return;
        }
        let typed = self.text.measure(text, format, inner.width()).min(inner.width());
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
        interaction: &Interaction,
    ) -> Result<()> {
        let format = self.text.format(metrics.label_size(), TITLE_WEIGHT)?;
        let rect = Rect::new(
            layout.titlebar.left + metrics.content_padding(),
            layout.titlebar.top,
            layout.titlebar.right - metrics.window_buttons_width(),
            layout.titlebar.bottom,
        );
        self.text_in(canvas, rect, "WinBeautify 设置", &format, palette.text);
        // The window buttons. The glyphs are drawn as geometry rather than text
        // so they stay crisp at any DPI and do not depend on an icon font that
        // may not be installed.
        for button in window_buttons(layout, metrics) {
            let hovered = interaction.hover_window == Some(button.kind);
            let (wash, ink) = match button.kind {
                WindowButton::Close if hovered => (palette.danger, palette.on_accent),
                WindowButton::Close => (Rgba::TRANSPARENT, palette.text_dim),
                _ if hovered => (palette.hover, palette.text),
                _ => (Rgba::TRANSPARENT, palette.text_dim),
            };
            if wash.a > 0.0 {
                canvas.fill_rounded(button.rect, metrics.px(4.0), wash);
            }
            self.draw_window_glyph(canvas, &button, metrics, ink)?;
        }
        Ok(())
    }

    /// The stroke inside a window button: a bar for minimise, a cross for close.
    fn draw_window_glyph(
        &self,
        canvas: &Canvas<'_>,
        button: &PositionedButton,
        metrics: &Metrics,
        ink: Rgba,
    ) -> Result<()> {
        let centre = (
            (button.rect.left + button.rect.right) * 0.5,
            button.rect.center_y(),
        );
        let arm = metrics.px(5.0);
        let thickness = metrics.px(1.4);
        match button.kind {
            WindowButton::Minimize => canvas.fill_rect(
                Rect::new(
                    centre.0 - arm,
                    centre.1 - thickness * 0.5,
                    centre.0 + arm,
                    centre.1 + thickness * 0.5,
                ),
                ink,
            ),
            WindowButton::Close => {
                // Two strokes, each a thin rotated bar. A cross made of two
                // rectangles reads correctly even at 10 px.
                for tilt in [-1.0f32, 1.0] {
                    self.polygon(
                        canvas,
                        &bar_points(centre, arm, thickness, tilt),
                        ink,
                    )?;
                }
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
        // Text that sits inside a drawn box is centred in it; text in a column
        // (labels, values, list entries) is left-aligned.
        let box_format = self
            .text
            .format_aligned(metrics.hint_size(), LABEL_WEIGHT, DWRITE_TEXT_ALIGNMENT_CENTER)?;

        // The section's own heading, where the layout put it — which is a little
        // below the top of the page, not on it. This used to be recomputed here
        // from the viewport's top edge (with a padding that cancelled itself
        // out), so the air the layout reserved above the title was reserved and
        // then ignored: every page's title sat flush against the top, looking
        // clipped.
        self.text_in(
            canvas,
            layout.content.title,
            current_section_title(layout),
            &title_format,
            palette.text,
        );

        // The section's own explanation, in the space the layout reserved for
        // it between the heading and the first card.
        if !layout.content.description.is_empty() {
            self.draw_wrapped(
                canvas,
                layout.content.description,
                current_section_description(layout),
                &hint_format,
                palette.text_faint,
                metrics,
            );
        }

        let mut row_index = 0usize;
        for card in &layout.content.cards {
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
                    &box_format,
                    &label_format,
                )?;
            }
        }
        Ok(())
    }

    /// Draw text wrapped to the box.
    ///
    /// Every line is drawn, because the layout reserved exactly the height they
    /// need: it measures with the same wrap and the same line spacing. Capping
    /// this at a fixed number of lines instead would leave visible gaps wherever
    /// a hint ran longer.
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
        let line_height = metrics.description_size() * crate::layout::LINE_SPACING;
        for (index, line) in lines.iter().enumerate() {
            let top = rect.top + index as f32 * line_height;
            // A hair of slack: the reserved height is a float multiple of the
            // line height, so the last line can land a fraction past it.
            if top + line_height > rect.bottom + 1.0 {
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
        centred: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
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
                self.text_in(
                    canvas,
                    readout.inset_by(metrics.px(6.0), 0.0),
                    &text,
                    small,
                    palette.text_dim,
                );
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
                let stored = value.as_ref().and_then(|v| v.as_text()).unwrap_or("");
                let colour = stored.parse::<beautify_core::geometry::Color>().ok();
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
                self.draw_box_text(
                    canvas,
                    hex,
                    stored,
                    "",
                    small,
                    palette,
                    metrics,
                    self.typing(interaction, row_index),
                );
            }
            Kind::Number { suffix, .. } => {
                let rect = parts.boxes[0];
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
                let number = value.as_ref().and_then(|v| v.as_number()).unwrap_or(0.0);
                let stored = format!("{} {suffix}", number.round() as i64);
                self.draw_box_text(
                    canvas,
                    rect,
                    &stored,
                    "",
                    small,
                    palette,
                    metrics,
                    self.typing(interaction, row_index),
                );
            }
            Kind::Text { placeholder } => {
                let rect = parts.boxes[0];
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                canvas.stroke_rounded(rect, metrics.px(5.0), palette.control_border, 1.0);
                let stored = value.as_ref().and_then(|v| v.as_text()).unwrap_or("");
                self.draw_box_text(
                    canvas,
                    rect,
                    stored,
                    placeholder,
                    small,
                    palette,
                    metrics,
                    self.typing(interaction, row_index),
                );
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
                self.text_in(canvas, pill, text, centred, colour);
            }
            Kind::Hotkey => {
                let rect = parts.boxes[0];
                let recording = interaction.recording == Some(row_index);
                canvas.fill_rounded(rect, metrics.px(5.0), palette.control);
                // The border is the whole affordance a recorder has: it says
                // whether the field is listening.
                canvas.stroke_rounded(
                    rect,
                    metrics.px(5.0),
                    if recording {
                        palette.accent
                    } else {
                        palette.control_border
                    },
                    if recording { 2.0 } else { 1.0 },
                );
                let stored = value.as_ref().and_then(|v| v.as_text()).unwrap_or("");
                let (text, colour) = if recording {
                    (interaction.recording_text.as_str(), palette.accent)
                } else if stored.is_empty() {
                    ("点击后按组合键", palette.text_faint)
                } else {
                    (stored, palette.text)
                };
                self.text_in(canvas, rect.inset_by(metrics.px(9.0), 0.0), text, small, colour);
            }
            // A read-only value. The label column already says what it is, so
            // this is just the text, and it is deliberately not styled like a
            // control: nothing here can be clicked or typed into.
            Kind::Info(key) => {
                self.text_in(canvas, row.control, status.info(key), small, palette.text_dim);
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
                    let lit = hovered && interaction.hover_part == Some(Part::Button(index));
                    canvas.fill_rounded(
                        rect,
                        metrics.px(5.0),
                        colour.with_alpha(if lit { 0.24 } else { 0.14 }),
                    );
                    self.text_in(canvas, rect, button.label, centred, colour);
                }
            }
        }
        let _ = label;
        Ok(())
    }

    /// The open colour palette, drawn over the page.
    ///
    /// A grid of swatches rather than a wheel or a pair of gradient sliders:
    /// every colour it offers is one click, it needs no extra Direct2D
    /// primitives, and the hex box beside the swatch already covers the exact
    /// value when the grid is not precise enough.
    fn draw_color_popup(
        &self,
        canvas: &Canvas<'_>,
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        current: Option<beautify_core::geometry::Color>,
    ) -> Result<()> {
        use crate::schema::{self, SWATCH_COLUMNS, SWATCH_ROWS};

        let Some(rect) = color_popup_rect(row, metrics) else {
            return Ok(());
        };
        canvas.fill_rounded(rect, metrics.px(6.0), palette.card);
        canvas.stroke_rounded(rect, metrics.px(6.0), palette.control_border, 1.0);

        for grid_row in 0..SWATCH_ROWS {
            for column in 0..SWATCH_COLUMNS {
                let colour = schema::swatch_color(grid_row, column);
                let cell = swatch_cell_rect(rect, grid_row, column, metrics);
                canvas.fill_rounded(cell, metrics.px(3.0), Rgba::from_color(colour, 1.0));
                // Every cell gets an edge: without one, the white end of the
                // grayscale row disappears into a light card.
                let marker = if interaction.color_highlight == Some((grid_row, column)) {
                    Some(2.0)
                } else if current == Some(colour) {
                    Some(1.5)
                } else {
                    None
                };
                let (border, width) = match marker {
                    Some(width) => (palette.text, width),
                    None => (palette.card_border, 1.0),
                };
                canvas.stroke_rounded(cell, metrics.px(3.0), border, width);
            }
        }
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
    #[allow(clippy::too_many_arguments)]
    fn draw_dropdown(
        &self,
        canvas: &Canvas<'_>,
        row: &Row,
        metrics: &Metrics,
        palette: &Palette,
        interaction: &Interaction,
        current: &str,
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
            // The entry under the pointer, and — separately — the entry the
            // setting is actually set to. They are different questions and a list
            // that answers only the first is the reason "which one am I on" had
            // to be worked out by remembering what the box said.
            let chosen = choice.value == current;
            if interaction.dropdown_highlight == index {
                canvas.fill_rect(
                    entry.inset_by(metrics.px(3.0), 0.0),
                    palette.accent.with_alpha(0.18),
                );
            }
            if chosen {
                canvas.fill_rect(
                    entry.inset_by(metrics.px(3.0), 0.0),
                    palette.accent.with_alpha(0.10),
                );
            }
            self.text_in(
                canvas,
                entry.inset_by(metrics.px(10.0), 0.0),
                choice.label,
                &small,
                if chosen { palette.accent } else { palette.text },
            );
            if chosen {
                // A tick, because the accent alone would also be a hover colour
                // as soon as the pointer is on another entry.
                let size = metrics.px(4.0);
                let cx = entry.right - metrics.px(14.0);
                let cy = entry.center_y();
                let _ = canvas.stroke_polyline(
                    &self.factory,
                    &[
                        (cx - size * 0.8, cy),
                        (cx - size * 0.15, cy + size * 0.7),
                        (cx + size, cy - size * 0.8),
                    ],
                    (0.0, 0.0),
                    palette.accent,
                    metrics.px(1.6),
                );
            }
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

/// Where a colour swatch's palette appears.
///
/// Anchored to the trailing edge of the control column and grown leftwards, the
/// way every other value on the page is: the grid is wider than the column, and
/// growing right from the swatch would push it off the window.
pub fn color_popup_rect(row: &Row, metrics: &Metrics) -> Option<Rect> {
    let (width, height) = color_popup_size(metrics);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let below = row.control.bottom + metrics.px(2.0);
    let above = row.control.top - metrics.px(2.0) - height;
    let top = if row.control.top > height {
        above
    } else {
        below
    };
    let right = row.control.right;
    Some(Rect::new(
        (right - width).max(0.0),
        top,
        right,
        top + height,
    ))
}

/// The size of the palette, from the grid it holds.
pub fn color_popup_size(metrics: &Metrics) -> (f32, f32) {
    use crate::schema::{SWATCH_COLUMNS, SWATCH_ROWS};
    let pad = metrics.popup_padding();
    let cell = metrics.swatch_cell_size();
    let gap = metrics.swatch_cell_gap();
    let width = SWATCH_COLUMNS as f32 * cell + (SWATCH_COLUMNS - 1) as f32 * gap + pad * 2.0;
    let height = SWATCH_ROWS as f32 * cell + (SWATCH_ROWS - 1) as f32 * gap + pad * 2.0;
    (width, height)
}

/// The rectangle of one palette cell.
///
/// The single place the cells are positioned, so the painter and the hit tester
/// cannot disagree about where a colour is — the arrangement this crate is built
/// around.
pub fn swatch_cell_rect(popup: Rect, row: usize, column: usize, metrics: &Metrics) -> Rect {
    let pad = metrics.popup_padding();
    let cell = metrics.swatch_cell_size();
    let stride = cell + metrics.swatch_cell_gap();
    let left = popup.left + pad + column as f32 * stride;
    let top = popup.top + pad + row as f32 * stride;
    Rect::new(left, top, left + cell, top + cell)
}

/// Which palette cell `(x, y)` is on, if any.
///
/// The gaps between cells are not cells: a click that lands in one is a miss
/// rather than a guess at the nearest colour.
pub fn swatch_at(popup: Rect, x: f32, y: f32, metrics: &Metrics) -> Option<(usize, usize)> {
    use crate::schema::{SWATCH_COLUMNS, SWATCH_ROWS};
    let pad = metrics.popup_padding();
    let cell = metrics.swatch_cell_size();
    let stride = cell + metrics.swatch_cell_gap();
    let offset_x = x - (popup.left + pad);
    let offset_y = y - (popup.top + pad);
    if offset_x < 0.0 || offset_y < 0.0 {
        return None;
    }
    let column = (offset_x / stride) as usize;
    let row = (offset_y / stride) as usize;
    if column >= SWATCH_COLUMNS || row >= SWATCH_ROWS {
        return None;
    }
    if offset_x - column as f32 * stride > cell || offset_y - row as f32 * stride > cell {
        return None;
    }
    Some((row, column))
}

/// The four corners of a thin bar through `centre`, tilted by `tilt` (a slope).
fn bar_points(centre: (f32, f32), arm: f32, thickness: f32, tilt: f32) -> [(f32, f32); 4] {
    let half = thickness * 0.5;
    // A unit vector along the bar, and its perpendicular.
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

/// The active section's own explanation, drawn under the page heading.
fn current_section_description(layout: &Layout) -> &'static str {
    layout
        .nav
        .iter()
        .find(|item| item.active)
        .map(|item| item.section.description)
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

    /// The palette has to be clickable where it is drawn, which is the whole
    /// class of bug that made the swatch feel dead: the hit test and the paint
    /// have to read the same rectangles.
    #[test]
    fn every_swatch_is_hit_testable_where_it_is_drawn() {
        use crate::schema::{SWATCH_COLUMNS, SWATCH_ROWS};
        let metrics = Metrics::new(96);
        let layout = crate::layout::layout(
            Rect::new(0.0, 0.0, 900.0, 640.0),
            &metrics,
            crate::schema::section("appearance").unwrap(),
            &beautify_core::config::Config::default(),
            0.0,
            &|_, _| 14.0,
        );
        let row = layout
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .find(|row| matches!(row.field.kind, Kind::Color))
            .expect("a colour row");
        let popup = color_popup_rect(row, &metrics).expect("a palette");

        for grid_row in 0..SWATCH_ROWS {
            for column in 0..SWATCH_COLUMNS {
                let cell = swatch_cell_rect(popup, grid_row, column, &metrics);
                assert!(cell.width() > 0.0 && cell.height() > 0.0);
                assert!(
                    popup.contains(cell.center_x(), cell.center_y()),
                    "cell ({grid_row}, {column}) is drawn outside the palette"
                );
                assert_eq!(
                    swatch_at(popup, cell.center_x(), cell.center_y(), &metrics),
                    Some((grid_row, column)),
                    "cell ({grid_row}, {column}) is not where it is drawn"
                );
            }
        }

        // The gaps and the padding are misses, not a guess at the nearest
        // colour — otherwise clicking the border would silently pick something.
        assert_eq!(
            swatch_at(popup, popup.left + 1.0, popup.top + 1.0, &metrics),
            None
        );
        let first = swatch_cell_rect(popup, 0, 0, &metrics);
        let second = swatch_cell_rect(popup, 0, 1, &metrics);
        assert_eq!(
            swatch_at(
                popup,
                (first.right + second.left) * 0.5,
                first.center_y(),
                &metrics
            ),
            None
        );
        assert_eq!(
            swatch_at(popup, popup.right - 1.0, popup.bottom - 1.0, &metrics),
            None
        );
    }

    /// The palette is wider than the control column, so it grows leftwards from
    /// the column's trailing edge; growing rightwards would push it off the
    /// window it is drawn in.
    #[test]
    fn the_palette_stays_inside_the_window_it_floats_over() {
        let metrics = Metrics::new(96);
        let window = Rect::new(0.0, 0.0, 880.0, 620.0);
        let layout = crate::layout::layout(
            window,
            &metrics,
            crate::schema::section("appearance").unwrap(),
            &beautify_core::config::Config::default(),
            0.0,
            &|_, _| 14.0,
        );
        for row in layout
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
        {
            let Some(popup) = color_popup_rect(row, &metrics) else {
                continue;
            };
            assert!(popup.left >= 0.0, "the palette ran off the left edge");
            assert!(
                popup.right <= window.right + 0.01,
                "the palette ran off the right edge: {popup:?}"
            );
        }
    }
}
