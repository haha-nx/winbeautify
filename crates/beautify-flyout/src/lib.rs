//! The native flyout: the task list and the clipboard history.
//!
//! Replaces the WebView2 panel. The layout is the settings window's: a page
//! described as data, one module of pure arithmetic that turns it into
//! rectangles, one that draws, and one that owns the window and input. The
//! difference is that the rows here come from a database rather than from a
//! compiled table, so the page is built from [`Host`] calls on every frame.
//!
//! # Why it is not a webview
//!
//! The panel is a list of rows with a handful of buttons; a Chromium process
//! tree to render that costs more than the entire rest of the daemon (see the
//! README's measurements). It is also the third such panel — after the widget bar
//! and the settings window — and the two already share their drawing code.
//!
//! # Rows are not retained
//!
//! There is no widget tree. Every frame rebuilds the rows from the host, which
//! means the panel cannot show a stale note or a deleted task: the list *is* the
//! database as of this frame. The cost is a query per frame, which for a list
//! capped at a few hundred rows is far cheaper than keeping a mirror in step.

pub mod layout;
pub mod paint;
pub mod window;

use std::sync::Arc;

pub use layout::{Heading, Hit, Metrics, Row, RowTarget, Rows, Scene, DEFAULT_SIZE};
pub use paint::{Interaction, Palette};
pub use window::{scaled_size, FlyoutWindow};

/// Which page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Todo,
    Clipboard,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Todo, Tab::Clipboard];

    pub const fn label(self) -> &'static str {
        match self {
            Tab::Todo => "任务清单",
            Tab::Clipboard => "剪贴板",
        }
    }
}

/// What an entry in the clipboard list is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Link,
    Files,
    Image,
}

impl ClipKind {
    pub const fn is_image(self) -> bool {
        matches!(self, ClipKind::Image)
    }
}

/// One row of the clipboard list, as the host presents it.
///
/// Three pieces of text rather than one, because a row shows them differently:
/// the preview is the entry, the recognised text is a hint about it, and the
/// meta line is what the reader scans when looking for "the one from ten
/// minutes ago". Folding them into one string is how the time came to be the
/// only thing under a forty-character sentence.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipRow {
    pub id: i64,
    pub kind: ClipKind,
    /// The line the row leads with: the entry's first line, or an image's
    /// pixel size.
    pub title: String,
    /// Text recognised inside an image, shown dimmer under the preview. Empty
    /// when there is none.
    pub ocr: String,
    /// Time and the per-kind detail: "3 分钟前 · 2.1 MB", "12 分钟前 · 40 字符".
    pub meta: String,
    /// Absolute path to the stored `.bmp`, for images.
    pub image_path: String,
    /// The image's own size, for working out how tall its thumbnail is. Zero
    /// when the entry is not an image or the size was never recorded.
    pub image_width: i32,
    pub image_height: i32,
    /// Favourite, which also protects it from being cleared.
    pub favourite: bool,
    /// On screen as a pinned image right now.
    pub pinned_to_screen: bool,
}

/// One row of the task list.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoRow {
    pub id: i64,
    pub title: String,
    pub done: bool,
}

/// Everything the panel needs from the application.
///
/// The panel owns no data of its own: it asks for the rows it is about to draw
/// and sends back the clicks. That keeps this crate free of any dependency on the
/// binary that hosts it, and means the lists cannot drift from the database.
pub trait Host: Send + Sync {
    fn clipboard_rows(&self, query: &str, limit: u32) -> Vec<ClipRow>;
    fn clipboard_stats(&self) -> (i64, i64);

    fn copy_clip(&self, id: i64);
    fn delete_clip(&self, id: i64);
    fn set_favourite(&self, id: i64, favourite: bool);
    /// Is this entry's image currently on screen as a pinned image?
    fn is_pinned_to_screen(&self, id: i64, image_path: &str) -> bool;
    /// Put the entry's image on screen, or take it away. Returns the new state.
    fn toggle_pinned_to_screen(&self, id: i64, image_path: &str) -> bool;
    /// Delete every entry that is not a favourite. Returns how many went.
    fn clear_unpinned_clips(&self) -> u32;

    fn todo_rows(&self) -> Vec<TodoRow>;
    fn add_todo(&self, title: &str);
    fn set_todo_done(&self, id: i64, done: bool);
    fn set_todo_title(&self, id: i64, title: &str);
    fn delete_todo(&self, id: i64);
    fn clear_completed_todos(&self) -> u32;

    /// The tab the panel should open on, remembered between openings.
    fn remembered_tab(&self) -> Tab;
    fn remember_tab(&self, tab: Tab);

    /// The colours to draw with: the theme, the accent and the widget
    /// background, taken from the configuration. The panel is part of the
    /// desktop furniture, so it follows the same settings the widget bar does.
    fn palette(&self) -> Palette;

    /// The panel lost the foreground, which is how "click outside closes it"
    /// works for a popup. The host decides what to do about it.
    fn dismissed(&self);
}

/// A running panel, or the handle to one.
pub struct Panel {
    window: FlyoutWindow,
}

impl Panel {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            window: FlyoutWindow::new(host),
        }
    }

    /// Show the panel at `(x, y)` — screen coordinates, physical pixels.
    pub fn show(&self, x: i32, y: i32, width: i32, height: i32) {
        self.window.show(x, y, width, height);
    }

    /// Hide it, if it is up.
    pub fn hide(&self) {
        self.window.hide();
    }

    /// Tell the panel its data changed. Cheap when it is not up: the window is
    /// told to redraw, and a hidden window simply does not.
    pub fn refresh(&self) {
        self.window.refresh();
    }

    pub fn is_open(&self) -> bool {
        self.window.is_open()
    }
}
