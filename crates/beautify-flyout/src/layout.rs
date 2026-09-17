//! Where everything goes in the panel.
//!
//! Pure arithmetic again: the rows come in as data and come out as rectangles,
//! so the painter and the hit tester read the same numbers and a row cannot be
//! drawn in one place and clicked in another.
//!
//! # Row heights come from the text
//!
//! A clipboard row is not a fixed shape: an image carries a thumbnail as tall as
//! its own aspect ratio makes it, and both the preview and the recognised text
//! may take two lines. So the heights cannot be constants, and the layout asks
//! for a measurement through [`Rows::text_height`] instead — the same engine the
//! painter draws with, so the box reserved and the text that lands in it cannot
//! disagree.
//!
//! What this module owns is every decision *around* the measurement: which
//! fields a row has, how wide the text column is, and how many lines each field
//! may take ([`CLIP_TITLE_LINES`] and friends).

use crate::paint::Icon;
use crate::{ClipFilter, ClipRow, Tab, TodoPage, TodoRow};

use beautify_widget::layout::Rect;

/// Shared with the painter: a text block of `n` lines at `size` pixels is
/// `n * size * LINE_SPACING` tall, in the layout and on the screen.
pub const LINE_SPACING: f32 = 1.35;

/// How many lines a clipboard row's preview may take.
///
/// Clamped rather than free: one screenshot of a document would otherwise push
/// every other entry off the list.
pub const CLIP_TITLE_LINES: usize = 2;
/// …and the text recognised inside an image.
pub const CLIP_OCR_LINES: usize = 2;
/// A task title is the row, so it gets one line more than a preview.
pub const TODO_TITLE_LINES: usize = 3;
/// Gap between the segmented buttons under the field.
const CHIP_GAP: f32 = 4.0;

/// The default size of the panel, in logical pixels.
pub const DEFAULT_SIZE: (f32, f32) = (380.0, 480.0);

/// Keep at most `max` lines, marking the last kept one as truncated.
///
/// Used by both the layout (to size the box) and the painter (to fill it), so
/// the ellipsis is counted as a line rather than discovered after the fact.
pub fn clamp_lines(lines: &mut Vec<String>, max: usize) {
    let max = max.max(1);
    if lines.len() <= max {
        return;
    }
    lines.truncate(max);
    if let Some(last) = lines.last_mut() {
        last.push('…');
    }
}

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

    /// Padding between the window edge and its contents.
    pub fn padding(&self) -> f32 {
        self.px(10.0)
    }

    pub fn gap(&self) -> f32 {
        self.px(8.0)
    }

    /// Padding inside a list row, on every side.
    pub fn row_padding(&self) -> f32 {
        self.px(8.0)
    }

    /// The shortest a list row may be, so a row with almost no content still
    /// reads as a row rather than as a seam.
    pub fn row_min_height(&self) -> f32 {
        self.px(30.0)
    }

    /// The kind badge down the left of a clipboard row.
    pub fn badge(&self) -> f32 {
        self.px(20.0)
    }

    /// Gap between the badge and the text column, and between the text column
    /// and the buttons.
    pub fn body_gap(&self) -> f32 {
        self.px(9.0)
    }

    /// Tallest an image thumbnail may be. The width is the text column's.
    pub fn thumbnail_max(&self) -> f32 {
        self.px(96.0)
    }

    /// Gap above the thumbnail, above the recognised text, and above the meta
    /// line — the three blocks that follow one another down a row.
    pub fn thumbnail_gap(&self) -> f32 {
        self.px(6.0)
    }

    pub fn ocr_gap(&self) -> f32 {
        self.px(4.0)
    }

    pub fn meta_gap(&self) -> f32 {
        self.px(3.0)
    }

    /// The row of segmented buttons under the field: the clipboard's kinds, or
    /// the task list's two pages.
    pub fn chip_height(&self) -> f32 {
        self.px(24.0)
    }

    /// One chip of the clipboard's kind row. The labels are two characters
    /// each, so one width fits all of them.
    pub fn chip_width(&self) -> f32 {
        self.px(46.0)
    }

    /// One of the task list's two page buttons, which carry one character more.
    pub fn page_width(&self) -> f32 {
        self.px(62.0)
    }

    /// One of a clipboard row's action buttons.
    ///
    /// The webview original showed these only on hover; at 22 pixels and
    /// invisible they were hard to find, so they are drawn always but dimmed,
    /// and 26 rather than 22 — bigger without becoming the row's main feature.
    pub fn button(&self) -> f32 {
        self.px(26.0)
    }

    pub fn button_gap(&self) -> f32 {
        self.px(2.0)
    }

    /// The delete button on a task row, which is one small target rather than a
    /// column of them and so does not need the clipboard row's size.
    pub fn small_button(&self) -> f32 {
        self.px(22.0)
    }

    pub fn checkbox(&self) -> f32 {
        self.px(16.0)
    }

    /// Gap between a task's check box and its title.
    pub fn checkbox_gap(&self) -> f32 {
        self.px(8.0)
    }

    /// The preview line of a clipboard row, and the tab labels.
    pub fn title_size(&self) -> f32 {
        self.px(12.5)
    }

    /// The recognised text inside an image.
    pub fn ocr_size(&self) -> f32 {
        self.px(11.0)
    }

    /// Time and size, under everything else.
    pub fn meta_size(&self) -> f32 {
        self.px(10.5)
    }

    /// A task title, one size up from a clipboard preview.
    pub fn todo_size(&self) -> f32 {
        self.px(13.0)
    }

    /// How far one wheel notch scrolls, when there is no row to measure.
    pub fn nominal_row(&self) -> f32 {
        self.px(48.0)
    }

    /// The footer strip on the clipboard tab.
    pub fn footer_height(&self) -> f32 {
        self.px(28.0)
    }

    pub fn close_button(&self) -> f32 {
        self.px(20.0)
    }

    /// One line of `size`-pixel text.
    pub fn line(&self, size: f32) -> f32 {
        size * LINE_SPACING
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

/// What a row stands for.
///
/// The identity of a row is what it points at rather than where it happens to
/// sit: rows scrolled out of view are not built at all, and a click has to find
/// the entry it came from either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTarget {
    /// Index into the clipboard list the frame was built from.
    Clip(usize),
    /// Index into the task list the frame was built from.
    Todo(usize),
}

/// A positioned row.
#[derive(Debug, Clone)]
pub struct Row {
    pub target: RowTarget,
    pub rect: Rect,
    /// The check box on a task row.
    pub checkbox: Option<Rect>,
    /// The kind badge down the left of a clipboard row.
    pub badge: Option<Rect>,
    /// The image thumbnail, above the text.
    pub thumbnail: Option<Rect>,
    /// The preview, the task title, or a heading's label.
    pub title: Rect,
    /// The text recognised inside an image, under the preview.
    pub ocr: Option<Rect>,
    /// Time and the per-kind detail.
    pub meta: Option<Rect>,
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
    /// Which clipboard kinds the list is showing.
    pub filter: ClipFilter,
    /// Which half of the task list the list is showing.
    pub page: TodoPage,
    pub window: Rect,
    pub header: Rect,
    pub tabs: Vec<(Tab, Rect)>,
    pub close: Rect,
    /// The search field (clipboard) or the add field (to-do).
    pub field: Rect,
    /// The scrolling list.
    pub list: Rect,
    /// The segmented buttons under the field.
    pub segments: Vec<(Segment, Rect)>,
    /// The rows that are on screen, in order. Rows scrolled out of view are not
    /// built: an off-screen row is one nobody can see, click or hover, and
    /// wrapping its text would be work done for nothing.
    pub rows: Vec<Row>,
    /// Height of the whole list, for the scrollbar.
    pub content_height: f32,
    pub scroll: f32,
    pub scroll_max: f32,
    pub scrollbar: Rect,
    pub footer: Rect,
    /// The footer's action button, when this tab has one.
    pub footer_button: Option<(FooterAction, Rect)>,
}

/// What the footer's button does. One button per tab, so it is drawn from the
/// action rather than from the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterAction {
    /// Delete every clipboard entry that is not a favourite.
    ClearUnpinnedClips,
    /// Delete the tasks on the "done" page.
    ClearCompletedTodos,
}

impl Scene {
    /// The row under a point.
    pub fn row_at(&self, x: f32, y: f32) -> Option<&Row> {
        if !self.list.contains(x, y) {
            return None;
        }
        self.rows.iter().find(|row| row.rect.contains(x, y))
    }

    /// The row standing for `target`.
    pub fn row(&self, target: RowTarget) -> Option<&Row> {
        self.rows.iter().find(|row| row.target == target)
    }

    /// Which tab is under a point, if any.
    pub fn tab_at(&self, x: f32, y: f32) -> Option<Tab> {
        self.tabs
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(tab, _)| *tab)
    }
}

/// One button of the row that sits under the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// A clipboard kind.
    Clip(ClipFilter),
    /// A page of the task list.
    Todo(TodoPage),
}

/// What a click landed on, worked out before anything acts on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A row.
    Row(RowTarget),
    /// A button on a row.
    RowButton { target: RowTarget, icon: Icon },
    /// The check box of a task row, by index into the task list.
    Checkbox(usize),
    Tab(Tab),
    Close,
    Field,
    /// One of the segmented buttons under the field.
    Segment(Segment),
    FooterButton,
    Scrollbar(f32),
    /// Empty space: commits an edit and nothing else.
    Nothing,
}

/// What the list is built from.
///
/// `clips` and `todos` are exclusive: the caller passes the list that matches
/// `tab`, because the two have their own shapes.
pub struct Rows<'a> {
    pub tab: Tab,
    pub clips: &'a [ClipRow],
    pub todos: &'a [TodoRow],
    /// Which clipboard kinds the list shows.
    pub filter: ClipFilter,
    /// Which half of the task list the list shows.
    pub page: TodoPage,
    pub stats: (i64, i64),
    /// The wrapped height of `text` at `width`, in `px`-pixel type, clamped to
    /// `max_lines`. Zero for empty text. Comes from the real font; see the
    /// module comment.
    pub text_height: &'a dyn Fn(&str, f32, f32, usize) -> f32,
}

impl<'a> Rows<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tab: Tab,
        clips: &'a [ClipRow],
        todos: &'a [TodoRow],
        filter: ClipFilter,
        page: TodoPage,
        stats: (i64, i64),
        text_height: &'a dyn Fn(&str, f32, f32, usize) -> f32,
    ) -> Self {
        Self {
            tab,
            clips,
            todos,
            filter,
            page,
            stats,
            text_height,
        }
    }

    /// A measurer that gives every text block exactly one line.
    ///
    /// For the callers that only care about geometry, where a real font would
    /// make the arithmetic untestable rather than more correct.
    pub fn one_line(text: &str, _width: f32, px: f32, _max: usize) -> f32 {
        if text.is_empty() {
            0.0
        } else {
            px * LINE_SPACING
        }
    }

    /// How tall a text block is.
    ///
    /// Never zero for text that is there: a measurer that returns nothing for a
    /// string it could not lay out would collapse the row around it.
    fn block(&self, text: &str, width: f32, px: f32, max: usize) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let line = px * LINE_SPACING;
        (self.text_height)(text, width, px, max)
            .max(line)
            .min(max.max(1) as f32 * line)
    }
}

/// One row's height, before it has a place on the screen.
struct Measured {
    target: RowTarget,
    height: f32,
}

/// The rows the list holds, in order, with their heights.
///
/// `width` is the window's width, so the text is wrapped the way it will be
/// drawn rather than against some nominal panel size.
fn measure(rows: &Rows<'_>, metrics: &Metrics, width: f32) -> Vec<Measured> {
    match rows.tab {
        Tab::Clipboard => rows
            .clips
            .iter()
            .enumerate()
            .map(|(index, entry)| Measured {
                target: RowTarget::Clip(index),
                height: clip_height(metrics, entry, rows, width),
            })
            .collect(),
        Tab::Todo => {
            // One page at a time, which is what replaces the two headings this
            // used to draw: the two are buttons under the field now, and the
            // page's own empty state says which one you are looking at.
            rows.todos
                .iter()
                .enumerate()
                .filter(|(_, task)| task.done == rows.page.is_done())
                .map(|(index, task)| Measured {
                    target: RowTarget::Todo(index),
                    height: todo_height(metrics, task, rows, width),
                })
                .collect()
        }
    }
}

/// The width of a clipboard row's action column.
fn action_column(metrics: &Metrics, buttons: usize) -> f32 {
    if buttons == 0 {
        return 0.0;
    }
    buttons as f32 * metrics.button() + (buttons as f32 - 1.0) * metrics.button_gap()
}

/// How many buttons a clipboard row has. Only an image can go on the screen.
fn clip_buttons(entry: &ClipRow) -> usize {
    if entry.kind.is_image() {
        3
    } else {
        2
    }
}

/// The width of the text column of a clipboard row: inside the padding, past
/// the badge, and short of the buttons.
fn clip_text_width(metrics: &Metrics, width: f32, entry: &ClipRow) -> f32 {
    (width
        - 2.0 * metrics.row_padding()
        - metrics.badge()
        - 2.0 * metrics.body_gap()
        - action_column(metrics, clip_buttons(entry)))
    .max(1.0)
}

/// The width of a task row's title: between its check box and its delete button.
fn todo_text_width(metrics: &Metrics, width: f32) -> f32 {
    (width
        - 2.0 * metrics.row_padding()
        - metrics.checkbox()
        - metrics.checkbox_gap()
        - metrics.small_button()
        - metrics.body_gap())
    .max(1.0)
}

/// The box an image's thumbnail gets: the text column wide, as tall as its own
/// aspect ratio asks for, and never taller than the cap.
fn thumbnail_box(metrics: &Metrics, width: f32, entry: &ClipRow) -> Option<(f32, f32)> {
    if !entry.kind.is_image() || entry.image_path.is_empty() {
        return None;
    }
    let cap = metrics.thumbnail_max();
    let (w, h) = (entry.image_width as f32, entry.image_height as f32);
    let height = if w > 0.0 && h > 0.0 {
        width * h / w
    } else {
        // An entry whose pixel size was never recorded still gets a
        // picture-shaped box rather than a sliver.
        cap * 0.75
    };
    Some((width, height.min(cap).max(metrics.px(24.0))))
}

fn clip_height(metrics: &Metrics, entry: &ClipRow, rows: &Rows<'_>, width: f32) -> f32 {
    let text_width = clip_text_width(metrics, width, entry);
    // The buttons are a column of fixed-height targets down the right edge, so
    // the row is at least as tall as they are — however short the text is.
    let actions = action_column(metrics, clip_buttons(entry));
    let mut body = 0.0;
    if let Some((_, height)) = thumbnail_box(metrics, text_width, entry) {
        body += height + metrics.thumbnail_gap();
    }
    body += rows.block(
        &entry.title,
        text_width,
        metrics.title_size(),
        CLIP_TITLE_LINES,
    );
    if !entry.ocr.is_empty() {
        body += metrics.ocr_gap()
            + rows.block(&entry.ocr, text_width, metrics.ocr_size(), CLIP_OCR_LINES);
    }
    if !entry.meta.is_empty() {
        body += metrics.meta_gap() + metrics.line(metrics.meta_size());
    }
    // The badge is a fixed-height column of its own; a row is never shorter than
    // the tallest thing in it, whatever the text says.
    let badge = metrics.badge() + metrics.px(1.0);
    (body.max(badge).max(actions) + 2.0 * metrics.row_padding()).max(metrics.row_min_height())
}

fn todo_height(metrics: &Metrics, task: &TodoRow, rows: &Rows<'_>, width: f32) -> f32 {
    let text = rows.block(
        &task.title,
        todo_text_width(metrics, width),
        metrics.todo_size(),
        TODO_TITLE_LINES,
    );
    let tallest = text.max(metrics.checkbox()).max(metrics.small_button());
    (tallest + 2.0 * metrics.row_padding()).max(metrics.row_min_height())
}

/// Turn the data into rectangles.
pub fn layout(window: Rect, metrics: &Metrics, rows: &Rows<'_>, scroll: f32) -> Scene {
    let header = Rect::new(
        window.left,
        window.top,
        window.right,
        window.top + metrics.header_height(),
    );

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
    // One footer button per tab, and what it does depends on the page.
    let footer_action = match rows.tab {
        Tab::Clipboard if rows.stats.0 > 0 => Some(FooterAction::ClearUnpinnedClips),
        Tab::Todo if rows.page.is_done() && rows.todos.iter().any(|task| task.done) => {
            Some(FooterAction::ClearCompletedTodos)
        }
        _ => None,
    };
    let footer_button = footer_action.map(|action| {
        let width = metrics.px(84.0);
        (
            action,
            Rect::new(
                footer.right - metrics.padding() - width,
                footer.center_y() - metrics.px(10.0),
                footer.right - metrics.padding(),
                footer.center_y() + metrics.px(10.0),
            ),
        )
    });

    // The segmented row sits between the field and the list: the clipboard's
    // kinds, or the task list's two pages.
    let mut segments = Vec::new();
    let mut left = window.left + metrics.padding();
    let top = field.bottom + metrics.padding() * 0.5;
    let bottom = top + metrics.chip_height();
    match rows.tab {
        Tab::Clipboard => {
            for filter in ClipFilter::ALL {
                segments.push((
                    Segment::Clip(filter),
                    Rect::new(left, top, left + metrics.chip_width(), bottom),
                ));
                left += metrics.chip_width() + metrics.px(CHIP_GAP);
            }
        }
        Tab::Todo => {
            for page in TodoPage::ALL {
                segments.push((
                    Segment::Todo(page),
                    Rect::new(left, top, left + metrics.page_width(), bottom),
                ));
                left += metrics.page_width() + metrics.px(CHIP_GAP);
            }
        }
    }

    let list = Rect::new(
        window.left,
        bottom + metrics.padding() * 0.5,
        window.right,
        footer.top,
    );

    let measured = measure(rows, metrics, window.width());
    let content_height: f32 = measured.iter().map(|row| row.height).sum();
    let scroll_max = (content_height - list.height()).max(0.0);
    let scroll = scroll.clamp(0.0, scroll_max);

    let mut built = Vec::new();
    let mut top = list.top - scroll;
    for item in &measured {
        let rect = Rect::new(list.left, top, list.right, top + item.height);
        top += item.height;
        // Only what the clip window can show is built.
        if rect.bottom <= list.top || rect.top >= list.bottom {
            continue;
        }
        built.push(build_row(metrics, rows, item.target, rect));
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
        tab: rows.tab,
        filter: rows.filter,
        page: rows.page,
        window,
        header,
        tabs,
        close,
        field,
        list,
        segments,
        rows: built,
        content_height,
        scroll,
        scroll_max,
        scrollbar,
        footer,
        footer_button,
    }
}

/// Place one row's contents inside `rect`.
fn build_row(metrics: &Metrics, rows: &Rows<'_>, target: RowTarget, rect: Rect) -> Row {
    match target {
        RowTarget::Todo(index) => {
            let task = &rows.todos[index];
            let padding = metrics.row_padding();
            let side = metrics.checkbox();
            let checkbox = Rect::new(
                rect.left + padding,
                rect.top + padding,
                rect.left + padding + side,
                rect.top + padding + side,
            );
            let size = metrics.small_button();
            let button = Rect::new(
                rect.right - padding - size,
                rect.top + padding,
                rect.right - padding,
                rect.top + padding + size,
            );
            let text_left = checkbox.right + metrics.checkbox_gap();
            let title = Rect::new(
                text_left,
                rect.top + padding,
                (button.left - metrics.body_gap()).max(text_left),
                rect.top
                    + padding
                    + rows.block(
                        &task.title,
                        todo_text_width(metrics, rect.width()),
                        metrics.todo_size(),
                        TODO_TITLE_LINES,
                    ),
            );
            Row {
                target,
                rect,
                checkbox: Some(checkbox),
                badge: None,
                thumbnail: None,
                title,
                ocr: None,
                meta: None,
                buttons: vec![Button {
                    icon: Icon::Delete,
                    rect: button,
                    active: false,
                }],
            }
        }
        RowTarget::Clip(index) => {
            let entry = &rows.clips[index];
            let padding = metrics.row_padding();
            let gap = metrics.body_gap();
            let badge_size = metrics.badge();
            let badge = Rect::new(
                rect.left + padding,
                rect.top + padding + metrics.px(1.0),
                rect.left + padding + badge_size,
                rect.top + padding + metrics.px(1.0) + badge_size,
            );

            // Buttons stack down the right edge in reading order: pinning on
            // top (images only), then the star, then the one action that cannot
            // be taken back — delete, furthest from where the pointer arrives.
            let icons: &[Icon] = if entry.kind.is_image() {
                &[Icon::Pin, Icon::Star, Icon::Delete]
            } else {
                &[Icon::Star, Icon::Delete]
            };
            let size = metrics.button();
            let mut buttons = Vec::with_capacity(icons.len());
            let mut top = rect.top + padding;
            for icon in icons {
                let active = match icon {
                    Icon::Star => entry.favourite,
                    Icon::Pin => entry.pinned_to_screen,
                    _ => false,
                };
                buttons.push(Button {
                    icon: *icon,
                    rect: Rect::new(
                        rect.right - padding - size,
                        top,
                        rect.right - padding,
                        top + size,
                    ),
                    active,
                });
                top += size + metrics.button_gap();
            }
            let buttons_left = rect.right - padding - action_column(metrics, icons.len());

            let text_left = badge.right + gap;
            let text_right = (buttons_left - gap).max(text_left);
            let text_width = (text_right - text_left).max(1.0);
            let mut cursor = rect.top + padding;

            let thumbnail = thumbnail_box(metrics, text_width, entry).map(|(w, h)| {
                let box_rect = Rect::new(text_left, cursor, text_left + w, cursor + h);
                cursor += h + metrics.thumbnail_gap();
                box_rect
            });

            let title_height = rows.block(
                &entry.title,
                text_width,
                metrics.title_size(),
                CLIP_TITLE_LINES,
            );
            let title = Rect::new(text_left, cursor, text_right, cursor + title_height);
            cursor += title_height;

            let ocr = (!entry.ocr.is_empty()).then(|| {
                cursor += metrics.ocr_gap();
                let height = rows.block(
                    &entry.ocr,
                    text_width,
                    metrics.ocr_size(),
                    CLIP_OCR_LINES,
                );
                let rect = Rect::new(text_left, cursor, text_right, cursor + height);
                cursor += height;
                rect
            });

            let meta = (!entry.meta.is_empty()).then(|| {
                cursor += metrics.meta_gap();
                let height = metrics.line(metrics.meta_size());
                Rect::new(text_left, cursor, text_right, cursor + height)
            });

            Row {
                target,
                rect,
                checkbox: None,
                badge: Some(badge),
                thumbnail,
                title,
                ocr,
                meta,
                buttons,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipKind;

    fn metrics() -> Metrics {
        Metrics::new(96)
    }

    /// A measurer with no font behind it: one line per 30 characters, so a long
    /// string still wraps. The real font is exercised by the window's own tests
    /// and by hand.
    fn flat(text: &str, _width: f32, px: f32, max: usize) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let lines = (text.chars().count() as f32 / 30.0).ceil().max(1.0) as usize;
        lines.min(max) as f32 * px * LINE_SPACING
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
                ocr: String::new(),
                meta: "刚刚".into(),
                image_path: if index % 2 == 0 {
                    format!("C:/clips/{index}.bmp")
                } else {
                    String::new()
                },
                image_width: if index % 2 == 0 { 800 } else { 0 },
                image_height: if index % 2 == 0 { 600 } else { 0 },
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
                done: index % 2 == 1,
            })
            .collect()
    }

    fn scene(tab: Tab, clips: &[ClipRow], todos: &[TodoRow], scroll: f32) -> Scene {
        scene_of(tab, clips, todos, scroll, ClipFilter::All, TodoPage::Open)
    }

    fn scene_of(
        tab: Tab,
        clips: &[ClipRow],
        todos: &[TodoRow],
        scroll: f32,
        filter: ClipFilter,
        page: TodoPage,
    ) -> Scene {
        layout(
            window(),
            &metrics(),
            &Rows::new(tab, clips, todos, filter, page, (clips.len() as i64, 1), &flat),
            scroll,
        )
    }

    #[test]
    fn the_panel_divides_into_header_field_list_and_footer() {
        let clips = clips(3);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        assert!(scene.header.bottom <= scene.field.top);
        assert!(scene.field.bottom <= scene.list.top);
        assert!(scene.list.bottom <= scene.footer.top);
        assert_eq!(scene.footer.bottom, 480.0);
    }

    #[test]
    fn rows_stack_without_overlap() {
        let clips = clips(6);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        for pair in scene.rows.windows(2) {
            assert_eq!(pair[0].rect.bottom, pair[1].rect.top);
        }
    }

    #[test]
    fn an_image_row_gets_a_thumbnail_above_its_text() {
        let clips = clips(2);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        let image = &scene.rows[0];
        let thumb = image.thumbnail.expect("an image gets a thumbnail");
        assert!(
            thumb.bottom <= image.title.top,
            "the picture goes above the text, not beside it"
        );
        assert!(thumb.width() > thumb.height(), "800×600 is wider than tall");
        assert!(thumb.height() <= metrics().thumbnail_max() + 0.01);
        // The badge stays: it is the row's fixed landmark.
        assert!(image.badge.is_some());
        assert!(
            image.buttons.iter().any(|button| button.icon == Icon::Pin),
            "only an image can be pinned to the screen"
        );

        let text = &scene.rows[1];
        assert!(text.thumbnail.is_none());
        assert!(!text.buttons.iter().any(|button| button.icon == Icon::Pin));
    }

    #[test]
    fn a_tall_image_is_capped_and_a_wide_one_keeps_its_ratio() {
        let m = metrics();
        let mut entry = clips(1).remove(0);
        entry.image_width = 100;
        entry.image_height = 4000;
        let (width, height) = thumbnail_box(&m, 200.0, &entry).expect("a box");
        assert_eq!(width, 200.0, "the box is as wide as the text column");
        assert_eq!(height, m.thumbnail_max(), "…and no taller than the cap");

        entry.image_width = 400;
        entry.image_height = 100;
        let (_, height) = thumbnail_box(&m, 200.0, &entry).expect("a box");
        assert!((height - 50.0).abs() < 0.01, "half as tall as it is wide");
    }

    #[test]
    fn a_two_line_preview_makes_a_taller_row_than_a_one_line_one() {
        let mut short = clips(1).remove(0);
        short.kind = ClipKind::Text;
        short.image_path.clear();
        short.image_width = 0;
        short.image_height = 0;
        short.title = "短".into();
        // A row is never shorter than its buttons, so the two have to differ in
        // more than the preview for the floor not to swallow the difference.
        short.ocr = "识别出来的两行字，第一行而已，第二行在这里。".into();
        let mut long = short.clone();
        long.title = "这一段话足够长，长到需要折成两三行才放得下，于是这一行会比上面那一行高出一截。".into();

        let m = metrics();
        let height = |entry: &ClipRow| {
            layout(
                window(),
                &m,
                &Rows::new(
                    Tab::Clipboard,
                    std::slice::from_ref(entry),
                    &[],
                    ClipFilter::All,
                    TodoPage::Open,
                    (1, 0),
                    &flat,
                ),
                0.0,
            )
            .rows[0]
            .rect
            .height()
        };
        assert!(
            height(&long) > height(&short),
            "a wrapped preview has to make the row taller: {} vs {}",
            height(&long),
            height(&short)
        );
    }

    #[test]
    fn a_short_entry_still_clears_its_own_buttons() {
        // Two 26-pixel buttons and the gaps between them are 54 pixels, and they
        // are inside the row's padding — a one-line entry has less text than
        // that, and the buttons must not hang out of the bottom of the row.
        let mut entry = clips(1).remove(0);
        entry.kind = ClipKind::Text;
        entry.image_path.clear();
        entry.image_width = 0;
        entry.image_height = 0;
        entry.title = "短".into();
        let m = metrics();
        let scene = layout(
            window(),
            &m,
            &Rows::new(
                Tab::Clipboard,
                std::slice::from_ref(&entry),
                &[],
                ClipFilter::All,
                TodoPage::Open,
                (1, 0),
                &flat,
            ),
            0.0,
        );
        let row = &scene.rows[0];
        let lowest = row
            .buttons
            .iter()
            .map(|button| button.rect.bottom)
            .fold(f32::MIN, f32::max);
        assert!(
            lowest <= row.rect.bottom,
            "a button ends {lowest} past the row's {}", row.rect.bottom
        );
    }

    #[test]
    fn the_meta_line_sits_under_the_recognised_text() {
        let mut entry = clips(1).remove(0);
        entry.kind = ClipKind::Text;
        entry.image_path.clear();
        entry.ocr = "识别出来的一行字".into();
        let m = metrics();
        let scene = layout(
            window(),
            &m,
            &Rows::new(
                Tab::Clipboard,
                std::slice::from_ref(&entry),
                &[],
                ClipFilter::All,
                TodoPage::Open,
                (1, 0),
                &flat,
            ),
            0.0,
        );
        let row = &scene.rows[0];
        let ocr = row.ocr.expect("an entry with recognised text shows it");
        let meta = row.meta.expect("every entry has a meta line");
        assert!(ocr.top >= row.title.bottom);
        assert!(meta.top >= ocr.bottom);
    }

    #[test]
    fn an_image_without_recognised_text_has_no_ocr_block() {
        let clips = clips(1);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        assert!(scene.rows[0].ocr.is_none());
        // …but it still has a meta line under its picture.
        assert!(scene.rows[0].meta.is_some());
    }

    #[test]
    fn a_favourite_is_lit_and_its_neighbour_is_not() {
        let clips = clips(2);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        let lit = |row: &Row| {
            row.buttons
                .iter()
                .find(|b| b.icon == Icon::Star)
                .unwrap()
                .active
        };
        assert!(lit(&scene.rows[1]), "entry 1 is a favourite");
        assert!(!lit(&scene.rows[0]));
    }

    #[test]
    fn buttons_stay_inside_their_row_and_do_not_overlap() {
        let clips = clips(2);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        for row in &scene.rows {
            for button in &row.buttons {
                assert!(button.rect.right <= row.rect.right);
                assert!(button.rect.bottom <= row.rect.bottom);
                assert!(button.rect.right - button.rect.left > 8.0, "clickable");
            }
            for pair in row.buttons.windows(2) {
                assert!(pair[1].rect.top >= pair[0].rect.bottom, "buttons overlap");
            }
        }
    }

    #[test]
    fn the_text_column_does_not_run_under_the_buttons() {
        let clips = clips(4);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
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
        let todos = todos(2);
        let scene = scene(Tab::Todo, &[], &todos, 0.0);
        let row = scene
            .rows
            .iter()
            .find(|row| matches!(row.target, RowTarget::Todo(_)))
            .expect("a task row");
        assert!(row.checkbox.is_some());
        assert_eq!(row.buttons.len(), 1);
        assert_eq!(row.buttons[0].icon, Icon::Delete);
        assert!(row.checkbox.unwrap().left < row.title.left);
    }

    #[test]
    fn the_two_pages_show_one_half_of_the_list_each() {
        let todos = todos(4);
        let open = scene_of(Tab::Todo, &[], &todos, 0.0, ClipFilter::All, TodoPage::Open);
        assert_eq!(open.rows.len(), 2, "two of the four are unfinished");
        for row in &open.rows {
            let RowTarget::Todo(index) = row.target else {
                panic!("a page holds tasks, nothing else");
            };
            assert!(!todos[index].done, "the open page holds open tasks");
        }

        let done = scene_of(Tab::Todo, &[], &todos, 0.0, ClipFilter::All, TodoPage::Done);
        assert_eq!(done.rows.len(), 2);
        for row in &done.rows {
            let RowTarget::Todo(index) = row.target else {
                panic!("a page holds tasks, nothing else");
            };
            assert!(todos[index].done, "the done page holds finished tasks");
        }
    }

    #[test]
    fn an_empty_page_lays_out_with_no_rows() {
        let all_open: Vec<TodoRow> = (0..3)
            .map(|index| TodoRow {
                id: index,
                title: format!("task {index}"),
                done: false,
            })
            .collect();
        let done = scene_of(Tab::Todo, &[], &all_open, 0.0, ClipFilter::All, TodoPage::Done);
        assert!(done.rows.is_empty(), "nothing has been finished yet");
        assert_eq!(done.content_height, 0.0);
    }

    #[test]
    fn the_segmented_row_is_under_the_field_and_out_of_the_list() {
        let clips = clips(2);
        let chips = scene(Tab::Clipboard, &clips, &[], 0.0);
        assert_eq!(chips.segments.len(), 5, "one chip per clipboard kind");
        let (_, first) = chips.segments[0];
        assert!(first.top >= chips.field.bottom, "below the field");
        assert!(first.bottom <= chips.list.top, "above the list");
        for pair in chips.segments.windows(2) {
            assert!(pair[0].1.right <= pair[1].1.left, "chips overlap");
        }
        assert!(chips
            .segments
            .iter()
            .all(|(_, rect)| rect.right <= chips.window.right));

        let todos = todos(2);
        let todo = scene(Tab::Todo, &[], &todos, 0.0);
        assert_eq!(todo.segments.len(), 2, "one button per page");
    }

    #[test]
    fn the_footer_button_says_what_it_does() {
        let clips = clips(2);
        let some = scene(Tab::Clipboard, &clips, &[], 0.0);
        assert_eq!(
            some.footer_button.map(|(action, _)| action),
            Some(FooterAction::ClearUnpinnedClips)
        );
        // On the open page there is nothing to clear.
        let todos = todos(2);
        let open = scene(Tab::Todo, &[], &todos, 0.0);
        assert!(open.footer_button.is_none());
        // …and on the done page there is, as long as something was finished.
        let done = scene_of(Tab::Todo, &[], &todos, 0.0, ClipFilter::All, TodoPage::Done);
        assert_eq!(
            done.footer_button.map(|(action, _)| action),
            Some(FooterAction::ClearCompletedTodos)
        );
    }

    #[test]
    fn scrolling_moves_the_list_and_stops_at_the_end() {
        let few = clips(2);
        let short = scene(Tab::Clipboard, &few, &[], 0.0);
        assert_eq!(short.scroll_max, 0.0, "two rows fit");

        let many = clips(40);
        let long = scene(Tab::Clipboard, &many, &[], 0.0);
        assert!(long.scroll_max > 0.0);
        let scrolled = scene(Tab::Clipboard, &many, &[], 60.0);
        assert!(
            (long.rows[0].rect.top - scrolled.rows[0].rect.top - 60.0).abs() < 0.01,
            "scrolling by 60 moves the list by 60"
        );
        let over = scene(Tab::Clipboard, &many, &[], 100_000.0);
        assert_eq!(over.scroll, over.scroll_max, "the offset is clamped");
    }

    #[test]
    fn only_the_rows_on_screen_are_built() {
        let many = clips(400);
        let scene = scene(Tab::Clipboard, &many, &[], 0.0);
        assert!(
            scene.rows.len() < 40,
            "a 400-row list must not build 400 rows: {}",
            scene.rows.len()
        );
        assert!(scene.content_height > scene.list.height());
        assert_eq!(scene.rows[0].target, RowTarget::Clip(0));
        assert!(scene.rows[0].rect.top >= scene.list.top);
    }

    #[test]
    fn a_scrolled_list_starts_at_the_entry_it_was_scrolled_to() {
        let many = clips(40);
        let scene = scene(Tab::Clipboard, &many, &[], 160.0);
        let first = scene.rows[0].target;
        assert!(
            matches!(first, RowTarget::Clip(index) if index > 0),
            "the entries above the fold are gone, got {first:?}"
        );
        assert!(scene.rows.iter().all(|row| row.rect.bottom > scene.list.top));
        assert!(scene.rows.iter().all(|row| row.rect.top < scene.list.bottom));
    }

    #[test]
    fn hit_testing_finds_the_row_and_the_button_under_a_point() {
        let clips = clips(4);
        let scene = scene(Tab::Clipboard, &clips, &[], 0.0);
        let row = &scene.rows[1];
        let centre = ((row.rect.left + row.rect.right) * 0.5, row.rect.center_y());
        assert_eq!(
            scene.row_at(centre.0, centre.1).map(|row| row.target),
            Some(row.target)
        );

        let star = row.buttons.iter().find(|b| b.icon == Icon::Star).unwrap();
        let star_centre = ((star.rect.left + star.rect.right) * 0.5, star.rect.center_y());
        assert!(star.rect.contains(star_centre.0, star_centre.1));
        assert_eq!(scene.row(row.target).map(|row| row.rect), Some(row.rect));
        // A point over the footer is not a row.
        assert!(scene.row_at(centre.0, scene.footer.center_y()).is_none());
    }

    #[test]
    fn the_tab_buttons_are_inside_the_header_and_do_not_overlap() {
        let m = metrics();
        let todos = todos(1);
        let scene = scene(Tab::Todo, &[], &todos, 0.0);
        assert_eq!(scene.tabs.len(), 2);
        for (_, rect) in &scene.tabs {
            assert!(rect.top >= scene.header.top && rect.bottom <= scene.header.bottom);
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
    fn an_empty_clipboard_has_no_footer_button() {
        let empty = scene(Tab::Clipboard, &[], &[], 0.0);
        assert!(empty.footer_button.is_none());
    }

    #[test]
    fn an_empty_list_lays_out_with_no_rows() {
        let scene = scene(Tab::Clipboard, &[], &[], 0.0);
        assert!(scene.rows.is_empty());
        assert_eq!(scene.content_height, 0.0);
        assert!(scene.row_at(100.0, 200.0).is_none());
    }

    #[test]
    fn dpi_scales_the_rows() {
        let m = metrics();
        let clips = clips(2);
        let normal = layout(
            window(),
            &m,
            &Rows::new(Tab::Clipboard, &clips, &[], ClipFilter::All, TodoPage::Open, (2, 0), &flat),
            0.0,
        );
        let big = Metrics::new(192);
        let big_window = Rect::new(0.0, 0.0, 760.0, 960.0);
        let scaled = layout(
            big_window,
            &big,
            &Rows::new(Tab::Clipboard, &clips, &[], ClipFilter::All, TodoPage::Open, (2, 0), &flat),
            0.0,
        );
        let ratio = scaled.rows[0].rect.height() / normal.rows[0].rect.height();
        assert!((ratio - 2.0).abs() < 0.01, "got {ratio}");
    }

    #[test]
    fn a_line_clamp_marks_what_it_cut() {
        let mut lines = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        clamp_lines(&mut lines, 2);
        assert_eq!(lines, vec!["one".to_string(), "two…".to_string()]);
        // Nothing to cut, nothing marked.
        let mut short = vec!["one".to_string()];
        clamp_lines(&mut short, 2);
        assert_eq!(short, vec!["one".to_string()]);
    }
}
