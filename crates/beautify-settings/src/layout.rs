//! Where everything goes.
//!
//! Pure arithmetic: it takes the window size, the DPI, the active section and the
//! scroll offset, and returns rectangles. Nothing here touches a device, which is
//! what lets the layout be tested on its own and keeps the painter and the hit
//! tester from ever disagreeing — they both read this.
//!
//! # Text measurement
//!
//! Row heights depend on how many lines a hint wraps to, which needs real font
//! metrics. Rather than reach for a device, the caller passes a measuring
//! closure; the window supplies one backed by DirectWrite and the tests supply a
//! fake. Everything else is arithmetic.

use beautify_core::config::Config;

use crate::geom::{clamp, Rect};
use crate::schema::{Card as CardSpec, Field, Kind, Section, SECTIONS};

/// Font size to line height, for every wrapped run of text on the page.
///
/// Shared with the painter: the layout reserves `lines * size * LINE_SPACING`
/// and the painter draws the lines at that pitch, so the two cannot drift into
/// a box that is a line too short.
pub const LINE_SPACING: f32 = 1.35;

/// Design constants, in 96-DPI units. Scaled once, here.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub scale: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        Self {
            scale: dpi as f32 / 96.0,
        }
    }

    pub(crate) fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    pub fn sidebar_width(&self) -> f32 {
        self.px(176.0)
    }

    pub fn titlebar_height(&self) -> f32 {
        self.px(38.0)
    }

    pub fn content_padding(&self) -> f32 {
        self.px(22.0)
    }

    pub fn nav_item_height(&self) -> f32 {
        self.px(32.0)
    }

    pub fn nav_item_gap(&self) -> f32 {
        self.px(2.0)
    }

    /// Gap before the 关于 entry, which sits at the bottom of the sidebar.
    pub fn nav_spacer_height(&self) -> f32 {
        self.px(10.0)
    }

    pub fn section_title_size(&self) -> f32 {
        self.px(19.0)
    }

    pub fn section_title_gap(&self) -> f32 {
        self.px(8.0)
    }

    pub fn description_size(&self) -> f32 {
        self.px(11.5)
    }

    pub fn description_gap(&self) -> f32 {
        self.px(16.0)
    }

    pub fn card_gap(&self) -> f32 {
        self.px(14.0)
    }

    pub fn card_radius(&self) -> f32 {
        self.px(8.0)
    }

    pub fn card_padding(&self) -> f32 {
        self.px(14.0)
    }

    pub fn card_title_size(&self) -> f32 {
        self.px(12.0)
    }

    pub fn card_title_gap(&self) -> f32 {
        self.px(6.0)
    }

    pub fn row_padding_v(&self) -> f32 {
        self.px(9.0)
    }

    pub fn row_padding_h(&self) -> f32 {
        self.px(12.0)
    }

    pub fn label_size(&self) -> f32 {
        self.px(12.5)
    }

    pub fn hint_size(&self) -> f32 {
        self.px(11.0)
    }

    pub fn hint_gap(&self) -> f32 {
        self.px(3.0)
    }

    /// Minimum height of a row, so rows without hints match rows with a short one.
    pub fn row_min_height(&self) -> f32 {
        self.px(38.0)
    }

    /// Width of the control column. Fixed so labels and controls line up down
    /// the whole page, which is most of what makes it look deliberate.
    pub fn control_width(&self) -> f32 {
        self.px(226.0)
    }

    pub fn control_gap(&self) -> f32 {
        self.px(16.0)
    }

    pub fn switch_width(&self) -> f32 {
        self.px(42.0)
    }

    pub fn switch_height(&self) -> f32 {
        self.px(23.0)
    }

    pub fn control_height(&self) -> f32 {
        self.px(28.0)
    }

    pub fn button_height(&self) -> f32 {
        self.px(28.0)
    }

    pub fn button_gap(&self) -> f32 {
        self.px(8.0)
    }

    pub fn scrollbar_width(&self) -> f32 {
        self.px(8.0)
    }

    /// Room reserved for a slider's numeric read-out.
    pub fn slider_readout_width(&self) -> f32 {
        self.px(56.0)
    }

    /// Width of a colour swatch.
    pub fn color_swatch_width(&self) -> f32 {
        self.px(46.0)
    }

    /// Width of an action button.
    pub fn button_width(&self) -> f32 {
        self.px(112.0)
    }

    /// Height of one entry in an open dropdown.
    pub fn dropdown_row_height(&self) -> f32 {
        self.px(26.0)
    }

    /// Room taken by the minimize and close buttons.
    pub fn window_buttons_width(&self) -> f32 {
        self.px(58.0)
    }
}

/// One row, positioned.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// The schema entry this came from, so the painter knows the control kind
    /// without being told separately.
    pub field: &'static Field,
    /// The whole row, including padding.
    pub rect: Rect,
    /// Where the label goes. Empty for status and action rows.
    pub label: Rect,
    /// Where the hint goes. Empty when the field has none.
    pub hint: Rect,
    /// Where the control goes.
    pub control: Rect,
    /// Draw a divider above this row (false for the first row of a card).
    pub divider: bool,
}

impl Row {
    /// Is any of this row inside `viewport`?
    pub fn visible(&self, viewport: Rect) -> bool {
        !self.rect.clipped_to(viewport).is_empty()
    }
}

/// A card, positioned.
#[derive(Debug, Clone)]
pub struct Card {
    pub spec: &'static CardSpec,
    pub rect: Rect,
    pub title: Rect,
    pub rows: Vec<Row>,
}

/// The scrollable page.
#[derive(Debug, Clone)]
pub struct Content {
    pub cards: Vec<Card>,
    /// Height of the whole page, used to size the scrollbar.
    pub height: f32,
    /// Where the section's own explanation goes, between the heading and the
    /// first card. Empty when the section has none.
    pub description: Rect,
}

/// One sidebar entry.
#[derive(Debug, Clone, Copy)]
pub struct NavItem {
    pub section: &'static Section,
    pub rect: Rect,
    pub active: bool,
}

/// The full layout.
#[derive(Debug, Clone)]
pub struct Layout {
    pub nav: Vec<NavItem>,
    /// Gap that pushes 关于 to the bottom of the sidebar.
    pub nav_spacer: Rect,
    pub sidebar: Rect,
    pub titlebar: Rect,
    /// The part of the window the page scrolls inside.
    pub viewport: Rect,
    pub content: Content,
    pub scrollbar: Rect,
    pub scroll_max: f32,
    /// The scroll offset this layout was built with, **after** clamping.
    ///
    /// Reported rather than assumed: a wheel event can ask for an offset past
    /// the end, and the caller needs to know what it actually got before it
    /// draws a scrollbar thumb that disagrees with the page.
    pub scroll: f32,
}

impl Layout {
    /// Which row is under `(x, y)`, if any.
    ///
    /// Clipped to the viewport, so a row scrolled out from under the header is
    /// not clickable through it.
    pub fn row_at(&self, x: f32, y: f32) -> Option<&Row> {
        if !self.viewport.contains(x, y) {
            return None;
        }
        self.content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .find(|row| row.rect.contains(x, y))
    }

    /// Which sidebar entry is under `(x, y)`.
    pub fn nav_at(&self, x: f32, y: f32) -> Option<&NavItem> {
        self.nav.iter().find(|item| item.rect.contains(x, y))
    }
}

/// Build the layout for one section.
///
/// `measure_hint(text, width)` returns the height the hint needs when wrapped
/// into `width`.
pub fn layout(
    window: Rect,
    metrics: &Metrics,
    section: &'static Section,
    config: &Config,
    scroll: f32,
    measure_hint: &dyn Fn(&str, f32) -> f32,
) -> Layout {
    let titlebar = window.take_top(metrics.titlebar_height());
    let body = Rect::new(
        window.left,
        titlebar.bottom,
        window.right,
        window.bottom,
    );
    let sidebar = body.take_left(metrics.sidebar_width());

    // The scrollbar lives in the content gutter, so the text never runs under it.
    let gutter = metrics.scrollbar_width() + metrics.content_padding() * 0.5;
    let viewport = Rect::new(
        sidebar.right,
        body.top,
        (body.right - gutter).max(sidebar.right),
        body.bottom,
    );

    let nav = layout_nav(sidebar, metrics, section);
    let nav_spacer = spacer_between(&nav);
    let content = layout_content(viewport, metrics, section, config, scroll, measure_hint);

    let scroll_max = (content.height - viewport.height()).max(0.0);
    let scroll = clamp(scroll, 0.0, scroll_max);
    let track = Rect::new(window.right - metrics.scrollbar_width(), body.top, window.right, body.bottom);
    let thumb_height = if content.height <= 0.0 {
        track.height()
    } else {
        (track.height() * (viewport.height() / content.height).min(1.0)).max(metrics.px(24.0))
    };
    let thumb_top = if scroll_max <= 0.0 {
        track.top
    } else {
        track.top + (track.height() - thumb_height) * (scroll / scroll_max)
    };
    let scrollbar = Rect::new(track.left, thumb_top, track.right, thumb_top + thumb_height);

    Layout {
        nav,
        nav_spacer,
        sidebar,
        titlebar,
        viewport,
        content,
        scrollbar,
        scroll_max,
        scroll,
    }
}

/// Sidebar entries, stacked from the top with 关于 pinned to the bottom.
fn layout_nav(sidebar: Rect, metrics: &Metrics, active: &Section) -> Vec<NavItem> {
    let height = metrics.nav_item_height();
    let gap = metrics.nav_item_gap();
    let mut top = sidebar.top + metrics.content_padding();

    SECTIONS
        .iter()
        .map(|section| {
            // The spacer is inserted before 关于, so everything after it is
            // pushed down; simplest is to account for it when reaching that item.
            if section.is_about {
                top += metrics.nav_spacer_height();
            }
            let rect = Rect::new(
                sidebar.left + metrics.content_padding() * 0.5,
                top,
                sidebar.right - metrics.content_padding() * 0.5,
                top + height,
            );
            top += height + gap;
            NavItem {
                section,
                rect,
                // Compared by id rather than by address: `SECTIONS` is a
                // `const`, so the array can be inlined separately at each use
                // site and two references to the same logical section need not
                // share an address.
                active: section.id == active.id,
            }
        })
        .collect()
}

/// The gap that pushes 关于 to the bottom, derived from what was laid out so
/// the two cannot drift apart.
fn spacer_between(nav: &[NavItem]) -> Rect {
    let last_form = nav.iter().rev().find(|item| !item.section.is_about);
    let about = nav.iter().find(|item| item.section.is_about);
    match (last_form, about) {
        (Some(above), Some(below)) if below.rect.top > above.rect.bottom => Rect::new(
            above.rect.left,
            above.rect.bottom,
            above.rect.right,
            below.rect.top,
        ),
        _ => Rect::EMPTY,
    }
}

/// The page itself, laid out at `scroll`.
fn layout_content(
    viewport: Rect,
    metrics: &Metrics,
    section: &'static Section,
    config: &Config,
    scroll: f32,
    measure_hint: &dyn Fn(&str, f32) -> f32,
) -> Content {
    let padding = metrics.content_padding();
    let left = viewport.left + padding;
    let right = viewport.right - padding;
    let text_width = right - left;
    let control_width = metrics.control_width();
    // The label column ends where the control column begins. Measured against
    // the *inner* width — the part left after the row padding on both sides —
    // or the two columns overlap by exactly one padding.
    let inner_width = (text_width - 2.0 * metrics.row_padding_h()).max(0.0);
    let label_width = (inner_width - control_width - metrics.control_gap()).max(0.0);

    // Lay out from the top of the page, then shift by the scroll offset, so the
    // scroll position does not change any of the internal arithmetic.
    let mut cursor = viewport.top;
    let mut cards = Vec::new();

    let title_rect = Rect::new(left, cursor, right, cursor + metrics.section_title_size());
    cursor = title_rect.bottom + metrics.section_title_gap();

    let mut description_rect = Rect::EMPTY;
    if !section.description.is_empty() {
        // Measured with the same wrap the painter draws with, so the reserved
        // height is the height the text actually takes.
        let height = measure_hint(section.description, text_width);
        description_rect = Rect::new(left, cursor, right, cursor + height);
        cursor = description_rect.bottom;
    }
    cursor += metrics.description_gap();

    for spec in section.cards {
        let card_top = cursor;
        let card_padding = metrics.card_padding();
        let mut inner = card_top + card_padding;
        if spec.title.is_some() {
            inner += metrics.card_title_size() + metrics.card_title_gap();
        }

        let mut rows = Vec::new();
        for field in spec.fields.iter().filter(|f| section.shows(f, config)) {
            let row = layout_row(
                field,
                metrics,
                left,
                right,
                inner,
                label_width,
                control_width,
                measure_hint,
                rows.is_empty(),
            );
            inner = row.rect.bottom;
            rows.push(row);
        }

        let card_bottom = inner + card_padding;
        let card_rect = Rect::new(left, card_top, right, card_bottom);
        let title = Rect::new(
            left + card_padding,
            card_top + card_padding * 0.75,
            right - card_padding,
            card_top + card_padding * 0.75 + metrics.card_title_size(),
        );
        cards.push(Card {
            spec,
            rect: card_rect,
            title,
            rows,
        });
        cursor = card_bottom + metrics.card_gap();
    }

    let height = (cursor - viewport.top).max(0.0);

    // Shift everything by the scroll offset now that the page height is known.
    let shift = |rect: Rect| rect.shifted(-scroll);
    description_rect = shift(description_rect);
    for card in &mut cards {
        card.rect = shift(card.rect);
        card.title = shift(card.title);
        for row in &mut card.rows {
            row.rect = shift(row.rect);
            row.label = shift(row.label);
            row.hint = shift(row.hint);
            row.control = shift(row.control);
        }
    }

    Content {
        cards,
        height,
        description: description_rect,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_row(
    field: &'static Field,
    metrics: &Metrics,
    left: f32,
    right: f32,
    top: f32,
    label_width: f32,
    control_width: f32,
    measure_hint: &dyn Fn(&str, f32) -> f32,
    is_first: bool,
) -> Row {
    let padding_h = metrics.row_padding_h();
    let padding_v = metrics.row_padding_v();
    let label_size = metrics.label_size();
    // The text is drawn centred in its rectangle, so the rectangle has to be one
    // *line* tall: handing it the whole row would centre the label against a
    // box several lines high and drop it onto the hint below.
    let label_line = label_size * LINE_SPACING;

    let mut text_height = label_line;
    let mut hint_height = 0.0;
    if let Some(hint) = field.hint {
        hint_height = measure_hint(hint, label_width) + metrics.hint_gap();
        text_height += hint_height;
    }
    let control_height = control_height_for(&field.kind, metrics);
    let content_height = text_height.max(control_height);
    let row_height = (content_height + padding_v * 2.0).max(metrics.row_min_height());
    let rect = Rect::new(left, top, right, top + row_height);

    // Status and action rows put their control in the label column: there is no
    // value to the right of the label to describe.
    let (label, control) = match field.kind {
        Kind::Status(_) => (Rect::EMPTY, Rect::new(left + padding_h, rect.top + padding_v, right - padding_h, rect.bottom - padding_v)),
        Kind::Action(_) => (Rect::EMPTY, Rect::new(left + padding_h, rect.top + padding_v, right - padding_h, rect.bottom - padding_v)),
        _ => {
            let label = Rect::new(
                left + padding_h,
                rect.top + padding_v,
                left + padding_h + label_width,
                rect.top + padding_v + label_line,
            );
            let control = Rect::new(
                right - padding_h - control_width,
                rect.top + padding_v,
                right - padding_h,
                rect.top + padding_v + control_height,
            );
            (label, control)
        }
    };

    // A hint sits directly under the label: the label occupies the first line
    // of the text column, the hint the ones after it.
    let hint = if hint_height > 0.0 {
        Rect::new(
            label.left,
            label.top + label_line + metrics.hint_gap(),
            label.right,
            label.top + label_line + hint_height,
        )
    } else {
        Rect::EMPTY
    };

    Row {
        field,
        rect,
        label,
        hint,
        control,
        divider: !is_first,
    }
}

/// How tall the control of `kind` wants to be.
fn control_height_for(kind: &Kind, metrics: &Metrics) -> f32 {
    match kind {
        Kind::Switch => metrics.switch_height(),
        Kind::Action(_) => metrics.button_height(),
        _ => metrics.control_height(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for font metrics: one line per 20 characters, so wrapping is
    /// observable without a font.
    fn fake_measure(text: &str, width: f32) -> f32 {
        let per_line = ((width / 6.0) as usize).max(1);
        let lines = text.chars().count().div_ceil(per_line).max(1);
        lines as f32 * 14.0
    }

    fn window() -> Rect {
        Rect::new(0.0, 0.0, 900.0, 640.0)
    }

    fn build(section: &str, config: &Config, scroll: f32) -> Layout {
        let metrics = Metrics::new(96);
        layout(
            window(),
            &metrics,
            crate::schema::section(section).unwrap(),
            config,
            scroll,
            &fake_measure,
        )
    }

    #[test]
    fn every_section_lays_out_without_panicking() {
        for section in SECTIONS {
            let metrics = Metrics::new(96);
            let layout = layout(
                window(),
                &metrics,
                section,
                &Config::default(),
                0.0,
                &fake_measure,
            );
            assert_eq!(layout.nav.len(), SECTIONS.len());
            if !section.is_about {
                assert!(
                    !layout.content.cards.is_empty(),
                    "{} produced no cards",
                    section.id
                );
            }
        }
    }

    #[test]
    fn the_sidebar_keeps_the_about_entry_at_the_bottom() {
        let layout = build("appearance", &Config::default(), 0.0);
        let about = layout
            .nav
            .iter()
            .find(|item| item.section.is_about)
            .expect("about entry");
        let others: Vec<&NavItem> = layout.nav.iter().filter(|i| !i.section.is_about).collect();
        assert!(
            others.iter().all(|item| item.rect.bottom < about.rect.top),
            "关于 should sit below the spacer, not inline with the rest"
        );
        assert!(!layout.nav_spacer.is_empty(), "the spacer should have height");
    }

    #[test]
    fn rows_stack_without_gaps_or_overlap() {
        let layout = build("taskbar", &Config::default(), 0.0);
        for card in &layout.content.cards {
            for pair in card.rows.windows(2) {
                assert_eq!(
                    pair[0].rect.bottom, pair[1].rect.top,
                    "rows must meet exactly, or the divider lands in the wrong place"
                );
            }
            if let (Some(first), Some(last)) = (card.rows.first(), card.rows.last()) {
                assert!(first.rect.top >= card.rect.top);
                assert!(last.rect.bottom <= card.rect.bottom, "rows overflow their card");
            }
        }
    }

    #[test]
    fn cards_do_not_overlap_and_leave_a_gap() {
        let metrics = Metrics::new(96);
        let layout = build("widget", &Config::default(), 0.0);
        for pair in layout.content.cards.windows(2) {
            assert_eq!(pair[0].rect.bottom + metrics.card_gap(), pair[1].rect.top);
        }
    }

    #[test]
    fn the_label_column_ends_where_the_control_column_begins() {
        let layout = build("appearance", &Config::default(), 0.0);
        for card in &layout.content.cards {
            for row in &card.rows {
                if row.label.is_empty() || row.control.is_empty() {
                    continue;
                }
                assert!(
                    row.label.right < row.control.left,
                    "label {:?} collides with control {:?}",
                    row.label,
                    row.control
                );
                assert!(row.control.right <= card.rect.right);
            }
        }
    }

    #[test]
    fn a_row_with_a_hint_is_taller_than_one_without() {
        let config = Config::default();
        let layout = build("appearance", &config, 0.0);
        let rows: Vec<&Row> = layout
            .content
            .cards
            .iter()
            .flat_map(|c| c.rows.iter())
            .collect();
        let with_hint = rows.iter().find(|r| !r.hint.is_empty()).expect("a hinted row");
        let without = rows.iter().find(|r| r.hint.is_empty()).expect("an unhinted row");
        assert!(with_hint.rect.height() > without.rect.height());
    }

    /// The label's box has to be exactly one line tall.
    ///
    /// Text is drawn centred in its rectangle, so a label handed the whole row
    /// gets centred against a box several lines high — which puts it on top of
    /// its own hint. This is the assertion that catches that.
    #[test]
    fn a_label_gets_one_line_and_its_hint_the_next() {
        let metrics = Metrics::new(96);
        let layout = build("appearance", &Config::default(), 0.0);
        let line = metrics.label_size() * LINE_SPACING;
        let rows: Vec<&Row> = layout
            .content
            .cards
            .iter()
            .flat_map(|c| c.rows.iter())
            .collect();
        for row in rows.iter().filter(|row| !row.hint.is_empty()) {
            assert!(
                (row.label.height() - line).abs() < 0.01,
                "{:?} label box is {} tall, not one line ({line})",
                row.field.label,
                row.label.height()
            );
            assert!(
                (row.hint.top - row.label.bottom - metrics.hint_gap()).abs() < 0.01,
                "{:?} hint is not one gap below the label",
                row.field.label
            );
        }
    }

    #[test]
    fn status_and_action_rows_use_the_whole_width() {
        let layout = build("clipboard", &Config::default(), 0.0);
        let status = layout
            .content
            .cards
            .iter()
            .flat_map(|c| c.rows.iter())
            .find(|r| matches!(r.field.kind, Kind::Status(_)))
            .expect("a status row");
        assert!(status.label.is_empty(), "status rows have no label column");
        assert!(status.control.width() > 400.0, "the control spans the row");
    }

    #[test]
    fn scrolling_moves_the_page_and_bounds_itself() {
        let config = Config::default();
        let top = build("media", &config, 0.0);
        assert!(top.scroll_max > 0.0, "the media page should overflow");

        let scrolled = build("media", &config, 60.0);
        let first_top = top.content.cards[0].rows[0].rect.top;
        let first_scrolled = scrolled.content.cards[0].rows[0].rect.top;
        assert!(
            (first_top - first_scrolled - 60.0).abs() < 0.01,
            "scrolling by 60 must move the page by exactly 60"
        );
        assert_eq!(
            scrolled.content.height, top.content.height,
            "the page height does not depend on the scroll offset"
        );
        assert_eq!(scrolled.scroll, 60.0, "an offset inside the range is kept");

        // Past the end it clamps, so the scrollbar cannot be dragged off.
        let over = build("media", &config, 100_000.0);
        let last = over.content.cards.last().unwrap().rows.last().unwrap();
        assert!(
            last.rect.bottom <= over.viewport.bottom + 0.01,
            "clamped scrolling should stop at the end of the page"
        );
        assert_eq!(
            over.scroll, over.scroll_max,
            "the layout has to report the offset it clamped to, or the thumb lies"
        );
    }

    #[test]
    fn the_scrollbar_thumb_shrinks_with_a_longer_page() {
        let config = Config::default();
        let short = build("appearance", &config, 0.0);
        let long = build("media", &config, 0.0);
        assert!(long.content.height > short.content.height);
        assert!(
            long.scrollbar.height() <= short.scrollbar.height(),
            "a longer page gets a shorter thumb"
        );
        assert!(short.scrollbar.height() > 0.0);
    }

    #[test]
    fn a_narrow_window_does_not_produce_negative_columns() {
        let metrics = Metrics::new(96);
        let narrow = Rect::new(0.0, 0.0, 260.0, 400.0);
        let layout = layout(
            narrow,
            &metrics,
            crate::schema::section("widget").unwrap(),
            &Config::default(),
            0.0,
            &fake_measure,
        );
        for card in &layout.content.cards {
            assert!(card.rect.width() >= 0.0);
            for row in &card.rows {
                assert!(row.label.width() >= 0.0);
                assert!(row.control.width() >= 0.0);
            }
        }
    }

    #[test]
    fn hit_testing_respects_the_viewport() {
        let layout = build("media", &Config::default(), 0.0);
        let first = layout.content.cards[0].rows[0];
        let center = (
            (first.rect.left + first.rect.right) * 0.5,
            (first.rect.top + first.rect.bottom) * 0.5,
        );
        assert!(layout.row_at(center.0, center.1).is_some());

        // A point over the sidebar is not a row, however the rows are laid out.
        assert!(layout.row_at(10.0, center.1).is_none());
        // Neither is one above the viewport.
        assert!(layout.row_at(center.0, layout.viewport.top - 5.0).is_none());
    }

    #[test]
    fn hit_testing_finds_the_active_nav_entry() {
        let layout = build("media", &Config::default(), 0.0);
        let active = layout.nav.iter().find(|item| item.active).unwrap();
        assert_eq!(active.section.id, "media");
        let center = (
            (active.rect.left + active.rect.right) * 0.5,
            (active.rect.top + active.rect.bottom) * 0.5,
        );
        assert_eq!(layout.nav_at(center.0, center.1).unwrap().section.id, "media");
    }

    #[test]
    fn dpi_scales_every_dimension() {
        let config = Config::default();
        let normal = build("appearance", &config, 0.0);
        let metrics = Metrics::new(192);
        let scaled = layout(
            Rect::new(0.0, 0.0, 1800.0, 1280.0),
            &metrics,
            crate::schema::section("appearance").unwrap(),
            &config,
            0.0,
            &fake_measure,
        );
        let ratio = scaled.titlebar.height() / normal.titlebar.height();
        assert!((ratio - 2.0).abs() < 0.01, "150%/200% DPI doubles the chrome");
        assert!(scaled.sidebar.width() > normal.sidebar.width());
    }

    #[test]
    fn hidden_rows_take_no_space() {
        let mut config = Config::default();
        config.taskbar.enabled = true;
        let expanded = build("taskbar", &config, 0.0);
        config.taskbar.enabled = false;
        let collapsed = build("taskbar", &config, 0.0);
        assert!(
            collapsed.content.height < expanded.content.height,
            "hiding rows must shorten the page"
        );
    }
}
