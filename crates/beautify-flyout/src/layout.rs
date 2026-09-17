//! Where everything goes in the panel.
//!
//! Pure arithmetic again: the rows come in as data and come out as rectangles,
//! so the painter and the hit tester read the same numbers and a row cannot be
//! drawn in one place and clicked in another.

use crate::paint::Icon;
use crate::{ClipRow, Tab, TodoRow};

use beautify_widget::layout::Rect;

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

    pub fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    /// Header height, which holds the tabs and the close button.
    pub fn header_height(&self) -> f32 {
        self.px(38.0)
    }

    /// One of the two tab buttons.
    pub fn tab_height(&self) -> f32 {
        self.px(26.0)
    }

    /// Width of a tab button.
    ///
    /// Fixed and equal rather than measured from the label: the two are equally
    /// important, equal widths look deliberate, and estimating a label's width
    /// from its character count is how the two came to overlap.
    pub fn tab_width(&self) -> f32 {
        self.px(88.0)
    }

    /// The search field on the clipboard tab, and the add field on the to-do
    /// tab.
    pub fn field_height(&self) -> f32 {
        self.px(30.0)
    }

    pub fn padding(&self) -> f32 {
        self.px(10.0)
    }

    pub fn gap(&self) -> f32 {
        self.px(8.0)
    }

    /// A clipboard row: a thumbnail, two lines of text, four buttons.
    pub fn clip_row_height(&self) -> f32 {
        self.px(48.0)
    }

    /// A task row, which has one line and two controls.
    pub fn todo_row_height(&self) -> f32 {
        self.px(34.0)
    }

    /// The thumbnail square on an image row.
    pub fn thumbnail(&self) -> f32 {
        self.px(38.0)
    }

    /// One of the row's action buttons.
    ///
    /// Deliberately generous: these are 20-pixel icons in a 30-pixel target,
    /// because a list of them is aimed at rather than read.
    pub fn button(&self) -> f32 {
        self.px(30.0)
    }

    pub fn button_gap(&self) -> f32 {
        self.px(4.0)
    }

    pub fn checkbox(&self) -> f32 {
        self.px(18.0)
    }

    pub fn title_size(&self) -> f32 {
        self.px(12.5)
    }

    pub fn small_size(&self) -> f32 {
        self.px(10.5)
    }

    pub fn line_spacing(&self) -> f32 {
        1.35
    }

    /// The footer strip on the clipboard tab.
    pub fn footer_height(&self) -> f32 {
        self.px(28.0)
    }

    pub fn close_button(&self) -> f32 {
        self.px(20.0)
    }
}

/// A positioned action button.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub icon: Icon,
    pub rect: Rect,
    /// Lit because the state it toggles is on — a favourite, or an image that is
    /// already on screen.
    pub active: bool,
}

/// A positioned row.
#[derive(Debug, Clone)]
pub struct Row {
    /// Index into the list this row came from, so a click can be turned back
    /// into the entry it belongs to.
    pub index: usize,
    pub rect: Rect,
    /// The check box on a task row.
    pub checkbox: Option<Rect>,
    /// The image thumbnail, on an image row.
    pub thumbnail: Option<Rect>,
    /// Where the badge is drawn when there is no thumbnail.
    pub badge: Rect,
    pub title: Rect,
    pub subtitle: Rect,
    pub buttons: Vec<Button>,
}

/// The whole panel, positioned.
///
/// Owns nothing but rectangles: the rows it was built from stay with the caller,
/// which is what lets the window keep a scene around for hit testing while the
/// painter reads the same rows separately.
#[derive(Debug, Clone)]
pub struct Scene {
    pub tab: Tab,
    pub window: Rect,
    pub header: Rect,
    pub tabs: Vec<(Tab, Rect)>,
    pub close: Rect,
    /// The search field (clipboard) or the add field (to-do).
    pub field: Rect,
    /// The scrolling list.
    pub list: Rect,
    pub rows: Vec<Row>,
    /// Height of the whole list, for the scrollbar.
    pub content_height: f32,
    pub scroll: f32,
    pub scroll_max: f32,
    pub scrollbar: Rect,
    pub footer: Rect,
    pub footer_button: Option<Rect>,
}

impl Scene {
    /// The row under a point.
    pub fn row_at(&self, x: f32, y: f32) -> Option<&Row> {
        if !self.list.contains(x, y) {
            return None;
        }
        self.rows.iter().find(|row| row.rect.contains(x, y))
    }

    /// Which tab is under a point, if any.
    pub fn tab_at(&self, x: f32, y: f32) -> Option<Tab> {
        self.tabs
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(tab, _)| *tab)
    }
}

/// What a click landed on, worked out before anything acts on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A row, by list index.
    Row(usize),
    /// A button on a row.
    RowButton { row: usize, icon: Icon },
    /// The check box of a task row.
    Checkbox(usize),
    Tab(Tab),
    Close,
    Field,
    FooterButton,
    Scrollbar(f32),
    /// Empty space: commits an edit and nothing else.
    Nothing,
}

/// Turn the data into rectangles.
///
/// `rows` and `todos` are exclusive: the caller passes the list that matches
/// `tab`, because the two have different heights.
#[allow(clippy::too_many_arguments)]
pub fn layout(
    window: Rect,
    metrics: &Metrics,
    tab: Tab,
    clips: &[ClipRow],
    todos: &[TodoRow],
    scroll: f32,
    stats: (i64, i64),
) -> Scene {
    let header = Rect::new(window.left, window.top, window.right, window.top + metrics.header_height());

    // Two tabs, left to right, of equal width.
    let mut tabs = Vec::new();
    let mut left = header.left + metrics.padding();
    for tab in Tab::ALL {
        let rect = Rect::new(
            left,
            header.center_y() - metrics.tab_height() * 0.5,
            left + metrics.tab_width(),
            header.center_y() + metrics.tab_height() * 0.5,
        );
        tabs.push((tab, rect));
        left += metrics.tab_width() + metrics.px(4.0);
    }
    let close = Rect::new(
        header.right - metrics.padding() - metrics.close_button(),
        header.center_y() - metrics.close_button() * 0.5,
        header.right - metrics.padding(),
        header.center_y() + metrics.close_button() * 0.5,
    );

    // The field sits under the header on both tabs: search on one, "add a task"
    // on the other. One control, two jobs, and the same place to look for it.
    let field = Rect::new(
        window.left + metrics.padding(),
        header.bottom + metrics.padding() * 0.5,
        window.right - metrics.padding(),
        header.bottom + metrics.padding() * 0.5 + metrics.field_height(),
    );

    let footer = Rect::new(
        window.left,
        window.bottom - metrics.footer_height(),
        window.right,
        window.bottom,
    );
    let footer_button = (tab == Tab::Clipboard && stats.0 > 0).then(|| {
        let width = metrics.px(84.0);
        Rect::new(
            footer.right - metrics.padding() - width,
            footer.center_y() - metrics.px(10.0),
            footer.right - metrics.padding(),
            footer.center_y() + metrics.px(10.0),
        )
    });

    let list = Rect::new(
        window.left,
        field.bottom + metrics.padding() * 0.5,
        window.right,
        footer.top,
    );

    let row_height = match tab {
        Tab::Clipboard => metrics.clip_row_height(),
        Tab::Todo => metrics.todo_row_height(),
    };
    let count = match tab {
        Tab::Clipboard => clips.len(),
        Tab::Todo => todos.len(),
    };
    let content_height = count as f32 * row_height;
    let scroll_max = (content_height - list.height()).max(0.0);
    let scroll = scroll.clamp(0.0, scroll_max);

    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let top = list.top + index as f32 * row_height - scroll;
        let rect = Rect::new(list.left, top, list.right, top + row_height);
        rows.push(build_row(metrics, tab, index, rect, clips));
    }

    let track = Rect::new(
        window.right - metrics.px(6.0),
        list.top,
        window.right,
        list.bottom,
    );
    let thumb_height = if content_height <= 0.0 {
        track.height()
    } else {
        (track.height() * (list.height() / content_height).min(1.0)).max(metrics.px(20.0))
    };
    let thumb_top = if scroll_max <= 0.0 {
        track.top
    } else {
        track.top + (track.height() - thumb_height) * (scroll / scroll_max)
    };
    let scrollbar = Rect::new(track.left, thumb_top, track.right, thumb_top + thumb_height);

    Scene {
        tab,
        window,
        header,
        tabs,
        close,
        field,
        list,
        rows,
        content_height,
        scroll,
        scroll_max,
        scrollbar,
        footer,
        footer_button,
    }
}

fn build_row(metrics: &Metrics, tab: Tab, index: usize, rect: Rect, clips: &[ClipRow]) -> Row {
    let padding = metrics.padding();
    let size = metrics.button();
    let gap = metrics.button_gap();

    // Buttons are laid out from the right edge inwards, in the order they are
    // read: delete is furthest out, because it is the one that must not be hit
    // by accident.
    let icons: &[Icon] = match tab {
        Tab::Clipboard => {
            if clips.get(index).is_some_and(|row| row.kind.is_image()) {
                &[Icon::Delete, Icon::Star, Icon::Pin, Icon::Copy]
            } else {
                &[Icon::Delete, Icon::Star, Icon::Copy]
            }
        }
        Tab::Todo => &[Icon::Delete],
    };
    let mut buttons = Vec::with_capacity(icons.len());
    let mut right = rect.right - padding;
    for icon in icons {
        let is_active = match (tab, icon) {
            (Tab::Clipboard, Icon::Star) => clips.get(index).is_some_and(|row| row.favourite),
            (Tab::Clipboard, Icon::Pin) => clips.get(index).is_some_and(|row| row.pinned_to_screen),
            _ => false,
        };
        buttons.push(Button {
            icon: *icon,
            rect: Rect::new(
                right - size,
                rect.center_y() - size * 0.5,
                right,
                rect.center_y() + size * 0.5,
            ),
            active: is_active,
        });
        right -= size + gap;
    }

    // A task row's left edge is its check box.
    let checkbox = (tab == Tab::Todo).then(|| {
        let box_size = metrics.checkbox();
        Rect::new(
            rect.left + padding,
            rect.center_y() - box_size * 0.5,
            rect.left + padding + box_size,
            rect.center_y() + box_size * 0.5,
        )
    });

    let thumb_size = metrics.thumbnail();
    let badge = Rect::new(
        rect.left + padding,
        rect.center_y() - thumb_size * 0.5,
        rect.left + padding + thumb_size,
        rect.center_y() + thumb_size * 0.5,
    );
    let thumbnail = clips
        .get(index)
        .filter(|row| row.kind.is_image() && !row.image_path.is_empty())
        .map(|_| badge);

    // Text starts after the badge (or the check box), and stops before the
    // buttons.
    let text_left = badge.right + metrics.gap();
    let text_right = right - gap;
    let title_top = match tab {
        Tab::Clipboard => rect.center_y() - metrics.title_size() * 0.62,
        Tab::Todo => rect.center_y() - metrics.title_size() * 0.62,
    };
    let title = Rect::new(
        text_left,
        title_top,
        text_right.max(text_left),
        title_top + metrics.title_size() * metrics.line_spacing(),
    );
    let subtitle = Rect::new(
        text_left,
        title.bottom,
        text_right.max(text_left),
        title.bottom + metrics.small_size() * metrics.line_spacing(),
    );

    Row {
        index,
        rect,
        checkbox,
        thumbnail,
        badge,
        title,
        subtitle,
        buttons,
    }
}

/// The default size of the panel, in logical pixels.
pub const DEFAULT_SIZE: (f32, f32) = (380.0, 480.0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipKind;

    fn metrics() -> Metrics {
        Metrics::new(96)
    }

    fn window() -> Rect {
        Rect::new(0.0, 0.0, 380.0, 480.0)
    }

    fn clips(count: usize) -> Vec<ClipRow> {
        (0..count)
            .map(|index| ClipRow {
                id: index as i64,
                kind: if index % 2 == 0 {
                    ClipKind::Image
                } else {
                    ClipKind::Text
                },
                title: format!("entry {index}"),
                subtitle: "刚刚".into(),
                image_path: if index % 2 == 0 {
                    format!("C:/clips/{index}.bmp")
                } else {
                    String::new()
                },
                favourite: index == 1,
                pinned_to_screen: index == 0,
            })
            .collect()
    }

    fn todos(count: usize) -> Vec<TodoRow> {
        (0..count)
            .map(|index| TodoRow {
                id: index as i64,
                title: format!("task {index}"),
                done: index % 2 == 0,
            })
            .collect()
    }

    #[test]
    fn the_panel_divides_into_header_field_list_and_footer() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(3), &[], 0.0, (3, 0));
        assert!(scene.header.bottom <= scene.field.top);
        assert!(scene.field.bottom <= scene.list.top);
        assert!(scene.list.bottom <= scene.footer.top);
        assert_eq!(scene.footer.bottom, 480.0);
    }

    #[test]
    fn rows_stack_without_overlap() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(6), &[], 0.0, (6, 0));
        for pair in scene.rows.windows(2) {
            assert_eq!(pair[0].rect.bottom, pair[1].rect.top);
        }
    }

    #[test]
    fn an_image_row_gets_a_thumbnail_and_a_pin_button() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        let image = &scene.rows[0];
        assert!(image.thumbnail.is_some());
        assert!(
            image.buttons.iter().any(|button| button.icon == Icon::Pin),
            "only an image can be pinned to the screen"
        );
        assert!(
            image.buttons.iter().find(|b| b.icon == Icon::Pin).unwrap().active,
            "the first entry is pinned, so its button is lit"
        );
        let text = &scene.rows[1];
        assert!(text.thumbnail.is_none());
        assert!(!text.buttons.iter().any(|button| button.icon == Icon::Pin));
    }

    #[test]
    fn a_favourite_is_lit_and_its_neighbour_is_not() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        let lit = |row: &Row| row.buttons.iter().find(|b| b.icon == Icon::Star).unwrap().active;
        assert!(lit(&scene.rows[1]), "entry 1 is a favourite");
        assert!(!lit(&scene.rows[0]));
    }

    #[test]
    fn buttons_stay_inside_their_row_and_do_not_overlap() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        for row in &scene.rows {
            for button in &row.buttons {
                assert!(button.rect.right <= row.rect.right);
                assert!(button.rect.right - button.rect.left > 8.0, "clickable");
            }
            for pair in row.buttons.windows(2) {
                assert!(pair[1].rect.right <= pair[0].rect.left, "buttons overlap");
            }
        }
    }

    #[test]
    fn the_text_column_does_not_run_under_the_buttons() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(4), &[], 0.0, (4, 0));
        for row in &scene.rows {
            let leftmost = row
                .buttons
                .iter()
                .map(|button| button.rect.left)
                .fold(f32::MAX, f32::min);
            assert!(
                row.title.right <= leftmost,
                "title {:?} runs under the buttons at {leftmost}",
                row.title
            );
        }
    }

    #[test]
    fn a_task_row_has_a_check_box_and_only_a_delete_button() {
        let scene = layout(window(), &metrics(), Tab::Todo, &[], &todos(3), 0.0, (0, 0));
        let row = &scene.rows[0];
        assert!(row.checkbox.is_some());
        assert_eq!(row.buttons.len(), 1);
        assert_eq!(row.buttons[0].icon, Icon::Delete);
        // The check box is at the left, where a list's check boxes live.
        assert!(row.checkbox.unwrap().left < row.title.left);
    }

    #[test]
    fn scrolling_moves_the_list_and_stops_at_the_end() {
        let short = layout(window(), &metrics(), Tab::Clipboard, &clips(3), &[], 0.0, (3, 0));
        assert_eq!(short.scroll_max, 0.0, "three rows fit");

        let long = layout(window(), &metrics(), Tab::Clipboard, &clips(40), &[], 0.0, (40, 0));
        assert!(long.scroll_max > 0.0);
        let scrolled = layout(window(), &metrics(), Tab::Clipboard, &clips(40), &[], 60.0, (40, 0));
        assert!(
            (long.rows[0].rect.top - scrolled.rows[0].rect.top - 60.0).abs() < 0.01,
            "scrolling by 60 moves the list by 60"
        );
        let over = layout(window(), &metrics(), Tab::Clipboard, &clips(40), &[], 100_000.0, (40, 0));
        assert_eq!(over.scroll, over.scroll_max, "the offset is clamped");
    }

    #[test]
    fn hit_testing_finds_the_row_and_the_button_under_a_point() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &clips(4), &[], 0.0, (4, 0));
        let row = &scene.rows[1];
        let centre = ((row.rect.left + row.rect.right) * 0.5, row.rect.center_y());
        assert_eq!(scene.row_at(centre.0, centre.1).map(|row| row.index), Some(1));

        let star = row.buttons.iter().find(|b| b.icon == Icon::Star).unwrap();
        let star_centre = ((star.rect.left + star.rect.right) * 0.5, star.rect.center_y());
        assert!(star.rect.contains(star_centre.0, star_centre.1));
        // A point over the footer is not a row.
        assert!(scene.row_at(centre.0, scene.footer.center_y()).is_none());
    }

    #[test]
    fn the_tab_buttons_are_inside_the_header_and_do_not_overlap() {
        let m = metrics();
        let scene = layout(window(), &m, Tab::Todo, &[], &todos(1), 0.0, (0, 0));
        assert_eq!(scene.tabs.len(), 2);
        for (_, rect) in &scene.tabs {
            assert!(rect.top >= scene.header.top && rect.bottom <= scene.header.bottom);
            // Wide enough for the longest label at the size it is drawn in.
            assert!(
                rect.width() >= m.tab_width(),
                "a tab narrower than its label overlaps the next one"
            );
        }
        assert!(
            scene.tabs[0].1.right <= scene.tabs[1].1.left,
            "the tabs must not overlap: {:?} {:?}",
            scene.tabs[0].1,
            scene.tabs[1].1
        );
        assert!(scene.close.left > scene.tabs[1].1.right);
        assert!(scene.close.right <= scene.window.right);
    }

    #[test]
    fn the_footer_button_only_appears_when_there_is_something_to_clear() {
        let empty = layout(window(), &metrics(), Tab::Clipboard, &[], &[], 0.0, (0, 0));
        assert!(empty.footer_button.is_none());
        let some = layout(window(), &metrics(), Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        assert!(some.footer_button.is_some());
        // The task tab has nothing to clear in the footer.
        let todo = layout(window(), &metrics(), Tab::Todo, &[], &todos(1), 0.0, (0, 0));
        assert!(todo.footer_button.is_none());
    }

    #[test]
    fn an_empty_list_lays_out_with_no_rows() {
        let scene = layout(window(), &metrics(), Tab::Clipboard, &[], &[], 0.0, (0, 0));
        assert!(scene.rows.is_empty());
        assert_eq!(scene.content_height, 0.0);
        assert!(scene.row_at(100.0, 200.0).is_none());
    }

    #[test]
    fn dpi_scales_the_rows() {
        let normal = layout(window(), &metrics(), Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        let scaled_metrics = Metrics::new(192);
        let big_window = Rect::new(0.0, 0.0, 760.0, 960.0);
        let scaled = layout(big_window, &scaled_metrics, Tab::Clipboard, &clips(2), &[], 0.0, (2, 0));
        let ratio = scaled.rows[0].rect.height() / normal.rows[0].rect.height();
        assert!((ratio - 2.0).abs() < 0.01, "got {ratio}");
    }
}
