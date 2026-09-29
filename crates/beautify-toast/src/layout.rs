//! Where the toast goes and how big it is.
//!
//! Pure arithmetic, in the same spirit as the widget bar's and the flyout's
//! layout modules: everything here takes measured text widths and a monitor
//! work area and returns numbers, so the sizing can be tested without a window,
//! a device, or a desktop. The painter and the window code only *consume* these
//! rectangles.
//!
//! # Why the panel is sized rather than fixed
//!
//! A toast that is always 400 px wide looks wrong for a two-word confirmation
//! and clips a device name that happens to be long. So the width follows the
//! text between a minimum that keeps a short line from becoming a stamp and a
//! maximum that keeps a long one from becoming a banner. Everything is a
//! *96-DPI design constant* scaled once through [`Metrics`], because the window
//! is created in physical pixels while the design is not.

use beautify_core::geometry::Rect;

/// Panel width bounds, in 96-DPI pixels.
///
/// Without a floor a two-character title produces a panel narrower than its own
/// corner radius; without a ceiling a device name with a long vendor prefix
/// would run most of the way across the screen and stop reading as a toast.
pub const MIN_WIDTH: f32 = 220.0;
pub const MAX_WIDTH: f32 = 460.0;

/// Space between the panel and the top-left corner of the work area, in 96-DPI
/// pixels.
pub const INSET: f32 = 24.0;

/// Space between the panel's edge and its text, in 96-DPI pixels.
pub const PADDING: f32 = 16.0;

/// Corner radius, in 96-DPI pixels.
pub const RADIUS: f32 = 10.0;

/// Gap between the title line and the detail line, in 96-DPI pixels.
///
/// Small on purpose: the two lines are one sentence split in two, not two
/// separate rows.
pub const LINE_GAP: f32 = 3.0;

/// Title and detail text sizes, in 96-DPI pixels.
pub const TITLE_PX: f32 = 15.0;
pub const DETAIL_PX: f32 = 13.0;

/// How tall one line of text is, as a multiple of its font size.
///
/// DirectWrite's own line height varies with the fallback font a string lands
/// in — a Chinese detail line and an English one differ — and a panel whose
/// height moved with the script would look like it was twitching. A fixed
/// multiplier is what makes two toasts of the same shape the same size.
pub const LINE_HEIGHT: f32 = 1.45;

/// The most the panel will ever be tall, in 96-DPI pixels.
///
/// The detail lines are truncated to the text width, so this is a guard rather
/// than a wrap limit: it is what stops a font with an unusual line height from
/// producing a panel taller than a toast has any business being.
pub const MAX_HEIGHT: f32 = 120.0;

/// DPI scaling, applied once to the design constants above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub scale: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        Self {
            // A DPI of zero arrives from a window that does not exist yet, and
            // would scale every constant to nothing.
            scale: dpi.max(96) as f32 / 96.0,
        }
    }

    pub fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    pub fn inset(&self) -> f32 {
        self.px(INSET)
    }

    pub fn padding(&self) -> f32 {
        self.px(PADDING)
    }

    pub fn radius(&self) -> f32 {
        self.px(RADIUS)
    }

    pub fn max_width(&self) -> f32 {
        self.px(MAX_WIDTH)
    }

    pub fn min_width(&self) -> f32 {
        self.px(MIN_WIDTH)
    }

    pub fn title_size(&self) -> f32 {
        self.px(TITLE_PX)
    }

    pub fn detail_size(&self) -> f32 {
        self.px(DETAIL_PX)
    }

    /// Height of one line at `size`, already scaled.
    pub fn line_height(&self, size: f32) -> f32 {
        size * LINE_HEIGHT
    }

    /// Widest a line of text may be inside the panel.
    ///
    /// What is left of the maximum panel width once both paddings are taken
    /// out. Both lines are truncated to it, so the panel can be sized from the
    /// wider of the two without either of them overflowing it.
    pub fn text_width(&self) -> f32 {
        self.max_width() - 2.0 * self.padding()
    }
}

/// What the panel is showing: a title line and its detail lines, already
/// truncated to fit.
///
/// A detail may itself be several lines (`\n`-separated — the "both sides
/// switched at once" toast reports one line per flow), so the count comes from
/// the string and the width from its widest line. The truncation itself needs
/// DirectWrite and therefore lives in the painter; this is the *result* of it,
/// so the sizing below is arithmetic on numbers rather than on strings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lines<'a> {
    pub title: &'a str,
    /// Empty when there is nothing under the title, in which case the panel has
    /// one line and is correspondingly shorter. `\n`-separated lines are drawn
    /// as separate rows.
    pub detail: &'a str,
    /// Measured width of `title`, in physical pixels.
    pub title_width: f32,
    /// Measured width of the *widest* detail line, in physical pixels, or zero
    /// when there is no detail.
    pub detail_width: f32,
}

/// The panel's size and the rectangles its lines are drawn in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Panel {
    pub width: f32,
    pub height: f32,
    /// The area the text is drawn in, in *panel-local* coordinates.
    pub text: Rect,
    /// Height of one title line; the detail lines sit below it, each in a row
    /// of its own.
    pub line_height: f32,
    /// How many detail lines there are — zero leaves a one-line panel.
    pub detail_lines: usize,
    /// True when there is at least one detail line to draw.
    pub has_detail: bool,
}

impl Panel {
    /// The panel rectangle in window-local coordinates, at the origin.
    ///
    /// Whole pixels, because the window and its DIB are sized in whole pixels:
    /// a fractional rect would leave a column of unpainted pixels on the right
    /// edge.
    pub fn rect(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.width.round() as i32,
            bottom: self.height.round() as i32,
        }
    }
}

/// Size the panel for the lines it will hold.
///
/// The width is the wider of the title and the widest detail line plus both
/// paddings, clamped; the height is one row per line plus the gaps between
/// them and both paddings. Integers come out because the window and its DIB
/// are sized in whole pixels, and a DIB one pixel narrower than the text would
/// clip the last glyph's antialiasing.
pub fn panel(lines: &Lines<'_>, metrics: &Metrics) -> Panel {
    let padding = metrics.padding();
    let widest = lines.title_width.max(if lines.detail.is_empty() {
        0.0
    } else {
        lines.detail_width
    });
    let width = (widest + 2.0 * padding).clamp(metrics.min_width(), metrics.max_width());

    let title_height = metrics.line_height(metrics.title_size());
    let detail_height = metrics.line_height(metrics.detail_size());
    let detail_lines = if lines.detail.is_empty() {
        0
    } else {
        lines.detail.lines().count()
    };
    let has_detail = detail_lines > 0;
    // One gap before each detail row — the first one separates it from the
    // title, the rest separate the detail rows from each other.
    let height = title_height
        + detail_lines as f32 * (metrics.px(LINE_GAP) + detail_height)
        + 2.0 * padding;

    Panel {
        width: width.round().max(1.0),
        height: height.round().min(metrics.px(MAX_HEIGHT)).max(1.0),
        text: Rect {
            left: padding.round() as i32,
            top: padding.round() as i32,
            right: (width - padding).round() as i32,
            bottom: (height - padding).round() as i32,
        },
        line_height: title_height,
        detail_lines,
        has_detail,
    }
}

/// Where the panel's window goes: `INSET` inside the top-left of `work`.
///
/// The window is the panel — a layered window hit-tests by alpha, and this one
/// is `WS_EX_TRANSPARENT` besides — so there is no transparent margin to
/// account for and the panel's origin *is* the window's origin.
///
/// The clamp is the part that matters. `rcWork` is not guaranteed to be bigger
/// than the panel: a work area left over by a docked toolbar can be a few
/// hundred pixels wide, and at 200% DPI the panel is twice the size it was
/// designed at. Without the clamp the panel would hang off the edge of the
/// screen — and a toast that reports an action is exactly the thing that must
/// not be half off-screen.
pub fn origin(work: Rect, panel: &Panel, metrics: &Metrics) -> (i32, i32) {
    let inset = metrics.inset().round() as i32;
    let max_x = work.right - panel.width.round() as i32;
    let max_y = work.bottom - panel.height.round() as i32;
    (
        (work.left + inset).min(max_x).max(work.left),
        (work.top + inset).min(max_y).max(work.top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scaling every other test reasons in: one logical pixel is one
    /// physical pixel.
    fn at_96() -> Metrics {
        Metrics::new(96)
    }

    fn lines<'a>(
        title: &'a str,
        title_width: f32,
        detail: &'a str,
        detail_width: f32,
    ) -> Lines<'a> {
        Lines {
            title,
            detail,
            title_width,
            detail_width,
        }
    }

    #[test]
    fn a_short_line_is_padded_up_to_the_minimum_width() {
        // A two-word confirmation must not produce a panel narrower than its
        // own corner radius.
        let sized = panel(&lines("已切换", 40.0, "", 0.0), &at_96());
        assert_eq!(sized.width, MIN_WIDTH);
        // Rounded to whole pixels, because that is what the window and its DIB
        // are sized in.
        assert_eq!(
            sized.height,
            (TITLE_PX * LINE_HEIGHT + 2.0 * PADDING).round()
        );
        assert!(!sized.has_detail);
    }

    #[test]
    fn a_long_line_is_capped_at_the_maximum_width() {
        let sized = panel(&lines("标题", 2000.0, "", 0.0), &at_96());
        assert_eq!(sized.width, MAX_WIDTH);
    }

    #[test]
    fn the_wider_of_the_two_lines_decides_the_width() {
        let title_wide = panel(&lines("标题", 300.0, "详情", 80.0), &at_96());
        let detail_wide = panel(&lines("标题", 80.0, "详情", 300.0), &at_96());
        assert_eq!(title_wide.width, detail_wide.width);
        assert_eq!(title_wide.width, 300.0 + 2.0 * PADDING);
    }

    #[test]
    fn an_empty_detail_leaves_a_one_line_panel() {
        let one = panel(&lines("标题", 100.0, "", 0.0), &at_96());
        let two = panel(&lines("标题", 100.0, "详情", 80.0), &at_96());
        assert!(!one.has_detail);
        assert!(two.has_detail);
        assert!(
            two.height > one.height,
            "a second line has to make the panel taller: {} vs {}",
            two.height,
            one.height
        );
        // And exactly by the detail line plus the gap between them.
        let expected = DETAIL_PX * LINE_HEIGHT + LINE_GAP;
        assert!((two.height - one.height - expected).abs() <= 1.0);
    }

    #[test]
    fn an_empty_detail_does_not_stretch_the_panel() {
        // An empty detail measures zero, but a naive `max` over "the wider of
        // the two" would still let it through if the measurement were garbage.
        // The title is wide enough to clear the minimum, so the answer comes
        // from the title alone rather than from that clamp.
        let sized = panel(&lines("标题", 300.0, "", 900.0), &at_96());
        assert_eq!(sized.width, 300.0 + 2.0 * PADDING);
    }

    #[test]
    fn every_detail_line_gets_its_own_row_of_height() {
        // The "both sides" toast reports one line per flow; a panel sized for
        // one detail line would clip the second — the exact bug this guards.
        let one = panel(&lines("标题", 100.0, "详情", 80.0), &at_96());
        let two = panel(&lines("标题", 100.0, "详情\n详情二", 80.0), &at_96());
        assert_eq!(one.detail_lines, 1);
        assert_eq!(two.detail_lines, 2);
        let expected = DETAIL_PX * LINE_HEIGHT + LINE_GAP;
        assert!(
            (two.height - one.height - expected).abs() <= 1.0,
            "a second detail line adds exactly one row: {} vs {}",
            two.height,
            one.height
        );
        // And the text box still ends inside the panel.
        assert!(two.text.bottom <= two.height as i32);
    }

    #[test]
    fn a_trailing_newline_does_not_add_an_empty_row() {
        // A detail built by joining lines always ends without `\n`, but a
        // hand-built one may not; a blank last row would read as a bug.
        let clean = panel(&lines("标题", 100.0, "详情", 80.0), &at_96());
        let trailing = panel(&lines("标题", 100.0, "详情\n", 80.0), &at_96());
        assert_eq!(clean.detail_lines, 1);
        assert_eq!(trailing.detail_lines, 1);
        assert_eq!(clean.height, trailing.height);
    }

    #[test]
    fn the_text_box_is_inside_the_panel_by_one_padding() {
        let sized = panel(&lines("标题", 200.0, "详情", 120.0), &at_96());
        assert_eq!(sized.text.left, PADDING as i32);
        assert_eq!(sized.text.top, PADDING as i32);
        assert!(sized.text.right <= sized.width as i32);
        assert!(sized.text.bottom <= sized.height as i32);
    }

    #[test]
    fn dpi_scales_every_dimension_together() {
        let at_one = panel(&lines("标题", 300.0, "详情", 200.0), &Metrics::new(96));
        let at_two = panel(&lines("标题", 600.0, "详情", 400.0), &Metrics::new(192));
        // The measured widths double too, so the whole panel scales by two.
        assert!((at_two.width - at_one.width * 2.0).abs() <= 1.0);
        assert!((at_two.height - at_one.height * 2.0).abs() <= 1.0);
        assert_eq!(Metrics::new(192).inset(), INSET * 2.0);
    }

    #[test]
    fn a_zero_dpi_does_not_scale_the_panel_away() {
        // `GetDpiForWindow` answers zero for a window that does not exist yet.
        assert_eq!(Metrics::new(0).scale, 1.0);
        assert_eq!(Metrics::new(0).padding(), PADDING);
    }

    #[test]
    fn the_panel_sits_one_inset_inside_the_work_area() {
        let work = Rect::new(0, 0, 1920, 1040);
        let sized = panel(&lines("标题", 200.0, "详情", 160.0), &at_96());
        assert_eq!(origin(work, &sized, &at_96()), (24, 24));
    }

    #[test]
    fn a_work_area_that_is_not_at_the_origin_is_respected() {
        // A secondary monitor's work area starts wherever that monitor does.
        let work = Rect::new(-1920, -200, 0, 880);
        let sized = panel(&lines("标题", 200.0, "", 0.0), &at_96());
        let (x, y) = origin(work, &sized, &at_96());
        assert_eq!(x, -1920 + 24);
        assert_eq!(y, -200 + 24);
    }

    #[test]
    fn dpi_moves_the_inset_inward() {
        let work = Rect::new(0, 0, 3840, 2080);
        let sized = panel(&lines("标题", 400.0, "详情", 300.0), &Metrics::new(192));
        assert_eq!(origin(work, &sized, &Metrics::new(192)), (48, 48));
    }

    #[test]
    fn a_work_area_barely_larger_than_the_panel_clamps_instead_of_overflowing() {
        // The clamp case the code exists for: the inset would put the panel's
        // far edge past the work area, so the inset has to give way.
        let sized = panel(&lines("标题", 1000.0, "详情", 900.0), &at_96());
        let work = Rect::new(
            0,
            0,
            sized.width.round() as i32 + 10,
            sized.height.round() as i32 + 4,
        );
        let (x, y) = origin(work, &sized, &at_96());
        assert_eq!(x, 10, "the inset gave way to the work area's right edge");
        assert_eq!(y, 4, "and to its bottom edge");
        assert!(x + sized.width.round() as i32 <= work.right);
        assert!(y + sized.height.round() as i32 <= work.bottom);
    }

    #[test]
    fn the_clamp_never_pushes_the_panel_before_the_work_area() {
        // The pathological case: the panel is wider than the whole work area,
        // so no position can keep it inside. Both clamps fire and the result
        // must still be the work area's own origin rather than a negative
        // offset that would put the panel off the top-left of the screen.
        let work = Rect::new(100, 100, 200, 160);
        let sized = panel(&lines("标题", 1000.0, "详情", 900.0), &at_96());
        let (x, y) = origin(work, &sized, &at_96());
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn the_panel_rect_is_whole_pixels() {
        // The DIB and the window are sized in whole pixels; a fractional rect
        // would leave a column of unpainted pixels on the right edge.
        let sized = panel(&lines("标题", 213.7, "详情", 100.9), &at_96());
        let rect = sized.rect();
        assert_eq!(rect.left, 0);
        assert_eq!(rect.top, 0);
        assert_eq!(rect.right, sized.width as i32);
        assert_eq!(rect.height(), sized.height as i32);
    }

    #[test]
    fn the_panel_is_never_taller_than_a_toast_should_be() {
        let huge = panel(&lines("标题", 100.0, "详情", 100.0), &Metrics::new(480));
        assert!(huge.height <= 480.0 / 96.0 * MAX_HEIGHT + 1.0);
    }
}
