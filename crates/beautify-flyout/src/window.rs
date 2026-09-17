//! The panel's window, message loop and input handling.
//!
//! The same shape as the settings window: the state lives in a thread local
//! owned by the pumping thread, the window handle is published back through an
//! atomic, and every input event runs the same three steps — ask
//! [`crate::layout`] what is under the pointer, ask nothing else, and act on the
//! row it came from. The difference is that the rows are re-read from the host
//! every frame, so there is no per-row state to keep in step with a database
//! that other modules also write to.
//!
//! # Why the panel is a popup
//!
//! It floats next to the taskbar, must not take the foreground from whatever the
//! user was doing, and must go away when they click elsewhere. That is a
//! `WS_EX_TOOLWINDOW | WS_EX_TOPMOST` popup, and "click elsewhere" falls out of
//! `WM_ACTIVATE`: losing the foreground *is* the click elsewhere.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMSBT_TRANSIENTWINDOW, DWMWCP_ROUND,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, HWND_TOPMOST,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SetCursor, SetWindowPos,
    ShowWindow, TranslateMessage, HTCLIENT, IDC_ARROW, IDC_HAND, IDC_IBEAM, MSG, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_HIDE, SW_SHOW, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_PAINT,
    WM_SETCURSOR, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::layout::{self, Hit, Metrics, RowTarget, Scene};
use crate::paint::{Icon, Interaction, Painter, Palette};
use crate::{ClipRow, Host, Tab, TodoRow};

/// Posted to make the panel redraw.
pub const WM_APP_REFRESH: u32 = WM_APP + 91;
/// Posted to take the panel down from another thread.
pub const WM_APP_HIDE: u32 = WM_APP + 92;

/// `WM_MOUSELEAVE` from `winuser.h`.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// How many clipboard rows the panel will hold. Far more than fits on screen;
/// the rest are reached by searching.
const ROW_LIMIT: u32 = 400;

const CLASS_NAME: PCWSTR = w!("WinBeautify.Flyout");

/// The panel's size on screen for a logical size, at the DPI of the monitor the
/// point is on.
///
/// The config stores logical pixels and the window is created in physical ones.
/// Without this the panel opens at four fifths of its intended size on a 125%
/// display, and the columns look cramped for no visible reason.
pub fn scaled_size(x: i32, y: i32, logical: (i32, i32)) -> (i32, i32) {
    let scale = unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        dpi_x as f32 / 96.0
    };
    (
        (logical.0 as f32 * scale).round() as i32,
        (logical.1 as f32 * scale).round() as i32,
    )
}

/// Handle to the panel's window.
pub struct FlyoutWindow {
    /// The window, once it exists. Zero until it does.
    hwnd: Arc<AtomicIsize>,
    host: Arc<dyn Host>,
}

impl FlyoutWindow {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            hwnd: Arc::new(AtomicIsize::new(0)),
            host,
        }
    }

    /// Show the panel at `(x, y)` — screen coordinates, physical pixels.
    ///
    /// The window is created on first use and then reused: opening the panel is
    /// a click, and a click has to feel immediate.
    pub fn show(&self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(hwnd) = self.handle() {
            unsafe {
                let _ = SetWindowPos(hwnd, None, x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
            }
            return;
        }

        let host = Arc::clone(&self.host);
        let slot = Arc::clone(&self.hwnd);
        // The failure of a thread spawn used to be swallowed by `.ok()`, which
        // is how a panel that never appeared produced no diagnostic at all.
        match std::thread::Builder::new()
            .name("wb-flyout".into())
            .spawn(move || {
                if let Err(e) = run(host, slot, (x, y, width, height)) {
                    tracing::error!("flyout panel exited: {e}");
                }
            }) {
            Ok(_) => {}
            Err(e) => tracing::error!("could not start the flyout panel thread: {e}"),
        }
    }

    /// Take the panel down. The window is reused next time it is opened.
    pub fn hide(&self) {
        if let Some(hwnd) = self.handle() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    /// Has the window been created? Not "is it visible" — the panel hides rather
    /// than closing, so the window outlives each opening.
    pub fn is_open(&self) -> bool {
        self.hwnd.load(Ordering::Acquire) != 0
    }

    /// Tell the panel its data changed.
    pub fn refresh(&self) {
        if let Some(hwnd) = self.handle() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn handle(&self) -> Option<HWND> {
        let raw = self.hwnd.load(Ordering::Acquire);
        (raw != 0).then_some(HWND(raw as *mut core::ffi::c_void))
    }
}

/// Everything the panel owns.
struct Panel {
    host: Arc<dyn Host>,
    hwnd: HWND,
    painter: Option<Painter>,
    metrics: Metrics,
    palette: Palette,
    tab: Tab,
    /// The search text on the clipboard tab, or the task being typed on the task
    /// tab. One field, two jobs, matching the layout.
    field_text: String,
    field_focused: bool,
    /// The task whose title is being edited in place, with the text so far.
    editing_todo: Option<(i64, String)>,
    scroll: f32,
    /// The last frame's rectangles, for hit testing.
    scene: Option<Scene>,
    interaction: Interaction,
    tracking_leave: bool,
    /// The rows the last frame was built from, so a click can be turned back
    /// into the entry it belongs to. Kept alongside the scene because the scene
    /// is rectangles only, while the identifiers live here.
    clips: Vec<ClipRow>,
    todos: Vec<TodoRow>,
    stats: (i64, i64),
}

impl Panel {
    fn new(host: Arc<dyn Host>, hwnd: HWND, tab: Tab) -> Self {
        Self {
            host,
            hwnd,
            painter: None,
            metrics: Metrics::new(unsafe { GetDpiForWindow(hwnd) }),
            // Replaced from the host on the first frame; this is only what the
            // window shows for the few milliseconds before that.
            palette: Palette::resolve(beautify_core::geometry::Color::rgb(0x6C, 0x8C, 0xFF), false),
            tab,
            field_text: String::new(),
            field_focused: false,
            editing_todo: None,
            scroll: 0.0,
            scene: None,
            interaction: Interaction::default(),
            tracking_leave: false,
            clips: Vec::new(),
            todos: Vec::new(),
            stats: (0, 0),
        }
    }

    fn client_size(&self) -> (u32, u32) {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
        }
        (
            (client.right - client.left).max(1) as u32,
            (client.bottom - client.top).max(1) as u32,
        )
    }

    /// Re-read the rows and draw a frame.
    fn refresh(&mut self) {
        // The colours are read every frame, so changing the theme or the accent
        // shows up the moment the panel is redrawn — which the app asks for when
        // the configuration changes.
        self.palette = self.host.palette();
        let (width, height) = self.client_size();
        let query = match self.tab {
            Tab::Clipboard => self.field_text.trim().to_string(),
            Tab::Todo => String::new(),
        };
        self.clips = match self.tab {
            Tab::Clipboard => self.host.clipboard_rows(&query, ROW_LIMIT),
            Tab::Todo => Vec::new(),
        };
        self.todos = match self.tab {
            Tab::Todo => self.host.todo_rows(),
            Tab::Clipboard => Vec::new(),
        };
        self.stats = self.host.clipboard_stats();

        let scene = layout::layout(
            beautify_widget::layout::Rect::new(0.0, 0.0, width as f32, height as f32),
            &self.metrics,
            &layout::Rows::new(
                self.tab,
                &self.clips,
                &self.todos,
                self.stats,
                &|text, width, px, max_lines| self.text_height(text, width, px, max_lines),
            ),
            self.scroll,
        );
        // The clamp inside the layout is authoritative, so the offset cannot
        // drift past the end of the list.
        self.scroll = scene.scroll;
        self.draw(&scene);
        self.scene = Some(scene);
    }

    /// The height of a wrapped text block, measured with the font it is drawn in.
    ///
    /// Only two answers are ever needed, because every block is clamped: either
    /// it fits on one line, or it takes the maximum the row allows. That makes
    /// this one measurement per block instead of a line-by-line wrap, which
    /// matters because the whole list is measured to size the scrollbar.
    fn text_height(&self, text: &str, width: f32, px: f32, max_lines: usize) -> f32 {
        let line = px * layout::LINE_SPACING;
        let Some(painter) = self.painter.as_ref() else {
            return line;
        };
        painter.text_height(text, width, px, max_lines).max(line)
    }

    fn draw(&mut self, scene: &Scene) {
        let (width, height) = self.client_size();
        let (placeholder, empty) = match scene.tab {
            Tab::Clipboard => ("搜索剪贴板…", "还没有记录"),
            Tab::Todo => ("添加任务，回车确认", "今天没有任务"),
        };
        let Some(painter) = self.painter.as_mut() else {
            return;
        };
        if let Err(e) = painter.attach(self.hwnd, width, height) {
            tracing::warn!("flyout render target: {e}");
        }
        if let Err(e) = painter.render(
            scene,
            &self.clips,
            &self.todos,
            &self.metrics,
            &self.palette,
            &self.interaction,
            placeholder,
            self.stats,
            empty,
        ) {
            tracing::warn!("flyout frame failed: {e}");
        }
    }

    /// Redraw the frame that is already laid out.
    fn repaint(&mut self) {
        match self.scene.clone() {
            Some(scene) => self.draw(&scene),
            None => self.refresh(),
        }
    }

    /// Work out what a click landed on.
    fn hit_test(&self, x: f32, y: f32) -> Hit {
        let Some(scene) = self.scene.as_ref() else {
            return Hit::Nothing;
        };
        if scene.close.contains(x, y) {
            return Hit::Close;
        }
        if let Some(tab) = scene.tab_at(x, y) {
            return Hit::Tab(tab);
        }
        if scene.field.contains(x, y) {
            return Hit::Field;
        }
        if scene.footer_button.is_some_and(|rect| rect.contains(x, y)) {
            return Hit::FooterButton;
        }
        if scene.scroll_max > 0.0 && scene.scrollbar.contains(x, y) {
            let fraction = ((y - scene.list.top) / scene.list.height().max(1.0)).clamp(0.0, 1.0);
            return Hit::Scrollbar(fraction);
        }
        match scene.row_at(x, y) {
            Some(row) => {
                let target = row.target;
                if let Some(checkbox) = row.checkbox {
                    if checkbox.contains(x, y) {
                        if let RowTarget::Todo(index) = target {
                            return Hit::Checkbox(index);
                        }
                    }
                }
                for button in &row.buttons {
                    if button.rect.contains(x, y) {
                        return Hit::RowButton {
                            target,
                            icon: button.icon,
                        };
                    }
                }
                // A section title is a label: clicking it does nothing.
                if matches!(target, RowTarget::Heading(_)) {
                    return Hit::Nothing;
                }
                Hit::Row(target)
            }
            None => Hit::Nothing,
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        match self.hit_test(x, y) {
            Hit::Close => self.hide(),
            Hit::Tab(tab) => self.switch_tab(tab),
            Hit::Field => {
                self.commit_edit();
                self.field_focused = true;
                self.interaction.editing = true;
                self.interaction.editing_text = self.field_text.clone();
                self.repaint();
            }
            Hit::Scrollbar(fraction) => {
                if let Some(scene) = self.scene.as_ref() {
                    self.scroll = fraction * scene.scroll_max;
                }
                self.refresh();
            }
            Hit::FooterButton => {
                let removed = self.host.clear_unpinned_clips();
                tracing::debug!(removed, "cleared unpinned clipboard entries");
                self.refresh();
            }
            Hit::Checkbox(index) => {
                if let Some(task) = self.todos.get(index) {
                    self.host.set_todo_done(task.id, !task.done);
                }
                self.refresh();
            }
            Hit::RowButton { target, icon } => self.click_button(target, icon),
            Hit::Row(target) => self.click_row(target),
            Hit::Nothing => {
                self.commit_edit();
                self.repaint();
            }
        }
    }

    fn click_button(&mut self, target: RowTarget, icon: Icon) {
        match (target, icon) {
            (RowTarget::Clip(index), _) => {
                let Some(entry) = self.clips.get(index).cloned() else {
                    return;
                };
                match icon {
                    Icon::Star => self.host.set_favourite(entry.id, !entry.favourite),
                    Icon::Pin => {
                        let _ = self
                            .host
                            .toggle_pinned_to_screen(entry.id, &entry.image_path);
                    }
                    Icon::Delete => self.host.delete_clip(entry.id),
                    Icon::Kind(_) => return,
                }
            }
            (RowTarget::Todo(index), Icon::Delete) => {
                if let Some(task) = self.todos.get(index) {
                    self.host.delete_todo(task.id);
                }
            }
            _ => return,
        }
        self.refresh();
    }

    fn click_row(&mut self, target: RowTarget) {
        match target {
            RowTarget::Clip(index) => {
                let Some(entry) = self.clips.get(index).cloned() else {
                    return;
                };
                // Copying is the point of the panel; staying open would make the
                // user close it every single time.
                self.host.copy_clip(entry.id);
                self.commit_edit();
                self.hide();
            }
            // Clicking a task's title puts the caret in it. Done/not-done is the
            // check box, which is a deliberate target rather than the whole row.
            RowTarget::Todo(index) => {
                let Some(task) = self.todos.get(index).cloned() else {
                    return;
                };
                self.commit_edit();
                self.field_focused = false;
                self.interaction.editing = false;
                self.interaction.editing_row = Some(target);
                self.interaction.editing_text = task.title.clone();
                self.editing_todo = Some((task.id, task.title));
                self.repaint();
            }
            RowTarget::Heading(_) => {}
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.commit_edit();
        self.tab = tab;
        self.host.remember_tab(tab);
        // The field means something different on each tab, so it starts empty.
        self.field_text.clear();
        self.interaction.editing_text.clear();
        self.field_focused = false;
        self.interaction.editing = false;
        self.scroll = 0.0;
        self.refresh();
    }

    /// Write whatever is being edited back to the host.
    fn commit_edit(&mut self) {
        if let Some((id, title)) = self.editing_todo.take() {
            self.interaction.editing_row = None;
            let trimmed = title.trim().to_string();
            self.interaction.editing_text.clear();
            if !trimmed.is_empty() {
                self.host.set_todo_title(id, &trimmed);
            }
            self.refresh();
            return;
        }
        if self.field_focused {
            self.field_focused = false;
            self.interaction.editing = false;
            self.field_text = self.interaction.editing_text.trim().to_string();
            self.interaction.editing_text.clear();
            // The field filters the clipboard list; on the task tab it adds an
            // entry.
            if self.tab == Tab::Todo && !self.field_text.is_empty() {
                self.host.add_todo(&self.field_text);
                self.field_text.clear();
            }
            self.refresh();
        }
    }

    fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.host.dismissed();
    }

    /// Highlight whatever is under the pointer. Returns whether anything moved.
    fn update_hover(&mut self, x: f32, y: f32, inside: bool) -> bool {
        let previous = (
            self.interaction.hover_row,
            self.interaction.hover_button,
            self.interaction.hover_tab,
            self.interaction.hover_close,
            self.interaction.hover_footer,
            self.interaction.hover_checkbox,
        );
        let hit = if inside {
            self.hit_test(x, y)
        } else {
            Hit::Nothing
        };
        self.interaction.hover_row = match hit {
            Hit::Row(target) | Hit::RowButton { target, .. } => Some(target),
            Hit::Checkbox(index) => Some(RowTarget::Todo(index)),
            _ => None,
        };
        self.interaction.hover_button = match hit {
            Hit::RowButton { target, icon } => Some((target, icon)),
            _ => None,
        };
        self.interaction.hover_checkbox = match hit {
            Hit::Checkbox(index) => Some(index),
            _ => None,
        };
        self.interaction.hover_tab = match hit {
            Hit::Tab(tab) => Some(tab),
            _ => None,
        };
        self.interaction.hover_close = hit == Hit::Close;
        self.interaction.hover_footer = hit == Hit::FooterButton;

        previous
            != (
                self.interaction.hover_row,
                self.interaction.hover_button,
                self.interaction.hover_tab,
                self.interaction.hover_close,
                self.interaction.hover_footer,
                self.interaction.hover_checkbox,
            )
    }

    /// Scroll by a number of notches.
    fn scroll_by(&mut self, notches: f32) {
        let Some(scene) = self.scene.as_ref() else {
            return;
        };
        if scene.scroll_max <= 0.0 {
            return;
        }
        let max = scene.scroll_max;
        // Rows are as tall as their content, so the step comes from a row that
        // is on screen rather than from a constant.
        let step = scene
            .rows
            .first()
            .map(|row| row.rect.height())
            .unwrap_or_else(|| self.metrics.nominal_row());
        // Two rows a notch: one is too slow to be worth a wheel.
        self.scroll = (self.scroll - notches * step * 2.0).clamp(0.0, max);
        self.refresh();
    }

    /// A key press that is not text.
    fn key(&mut self, key: u32) {
        // Escape closes the panel, except while something is being edited, where
        // it abandons the edit — the same rule as the settings window, and what
        // a form does.
        if self.editing_todo.is_some() {
            match key {
                0x1B => {
                    self.editing_todo = None;
                    self.interaction.editing_row = None;
                    self.interaction.editing_text.clear();
                    self.refresh();
                }
                0x0D => self.commit_edit(),
                0x08 => self.backspace(),
                _ => {}
            }
            return;
        }
        if self.field_focused {
            match key {
                0x1B => {
                    self.field_focused = false;
                    self.interaction.editing = false;
                    self.interaction.editing_text.clear();
                    self.repaint();
                }
                0x0D => self.commit_edit(),
                0x08 => self.backspace(),
                _ => {}
            }
            return;
        }

        match key {
            k if k == VK_ESCAPE.0 as u32 => self.hide(),
            // The arrow keys walk the list, which is what a keyboard user
            // expects from a panel anchored to a bar.
            k if k == VK_DOWN.0 as u32 => self.scroll_by(-1.0),
            k if k == VK_UP.0 as u32 => self.scroll_by(1.0),
            k if k == VK_RETURN.0 as u32 => {
                // Enter copies the entry under the pointer, so the panel is
                // usable without reaching for the mouse.
                if let Some(row) = self.interaction.hover_row {
                    self.click_row(row);
                }
            }
            _ => {}
        }
    }

    fn backspace(&mut self) {
        self.interaction.editing_text.pop();
        if let Some((_, text)) = self.editing_todo.as_mut() {
            *text = self.interaction.editing_text.clone();
            self.repaint();
        } else {
            self.field_text = self.interaction.editing_text.clone();
            // The clipboard field is a filter, so the list follows the typing.
            if self.tab == Tab::Clipboard {
                self.refresh();
            } else {
                self.repaint();
            }
        }
    }

    /// A typed character.
    fn character(&mut self, ch: char) {
        if ch.is_control() || (self.editing_todo.is_none() && !self.field_focused) {
            return;
        }
        self.interaction.editing_text.push(ch);
        if let Some((_, text)) = self.editing_todo.as_mut() {
            *text = self.interaction.editing_text.clone();
            self.repaint();
        } else {
            self.field_text = self.interaction.editing_text.clone();
            if self.tab == Tab::Clipboard {
                self.refresh();
            } else {
                self.repaint();
            }
        }
    }

    /// Answer one message. `None` means "not handled".
    fn handle(&mut self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match message {
            WM_PAINT => {
                // The render target draws the whole window, so this only has to
                // mark the region valid after the frame is pushed.
                let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::BeginPaint(self.hwnd, &mut paint);
                }
                self.repaint();
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::EndPaint(self.hwnd, &paint);
                }
                Some(LRESULT(0))
            }
            windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => Some(LRESULT(1)),
            WM_MOUSEMOVE => {
                let (x, y) = point_of(lparam);
                if self.update_hover(x, y, true) {
                    self.repaint();
                }
                if !self.tracking_leave {
                    // Asked for once per entry: without it the panel never learns
                    // that the pointer left, and the last row stays lit.
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut track);
                    }
                    self.tracking_leave = true;
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                if self.update_hover(0.0, 0.0, false) {
                    self.repaint();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point_of(lparam);
                self.click(x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let _ = unsafe { ReleaseCapture() };
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let notches = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                self.scroll_by(notches);
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                self.key(wparam.0 as u32);
                Some(LRESULT(0))
            }
            WM_CHAR => {
                if let Some(ch) = char::from_u32(wparam.0 as u32) {
                    self.character(ch);
                }
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                // Only the client area is ours; the frame is not drawn at all, so
                // there is nothing else to leave to Windows.
                if (lparam.0 & 0xFFFF) as u32 != HTCLIENT {
                    return None;
                }
                let id = if self.field_focused {
                    IDC_IBEAM
                } else if self.interaction.hover_button.is_some()
                    || self.interaction.hover_tab.is_some()
                    || self.interaction.hover_close
                    || self.interaction.hover_footer
                    || self.interaction.hover_checkbox.is_some()
                {
                    IDC_HAND
                } else {
                    IDC_ARROW
                };
                unsafe {
                    if let Ok(cursor) = LoadCursorW(None, id) {
                        SetCursor(Some(cursor));
                    }
                }
                Some(LRESULT(1))
            }
            // Losing the foreground is the "clicked elsewhere" that dismisses a
            // popup: `WM_ACTIVATE` with `WA_INACTIVE` in the low word.
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) == 0 {
                    self.hide();
                }
                Some(LRESULT(0))
            }
            WM_APP_REFRESH => {
                self.refresh();
                Some(LRESULT(0))
            }
            WM_APP_HIDE | WM_CLOSE => {
                self.hide();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// Create the window and pump its messages. Blocks the calling thread.
fn run(
    host: Arc<dyn Host>,
    slot: Arc<AtomicIsize>,
    geometry: (i32, i32, i32, i32),
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // A second registration in the same process fails benignly.
    unsafe { RegisterClassExW(&class) };

    let (x, y, width, height) = geometry;
    let tab = host.remembered_tab();
    let hwnd = unsafe {
        CreateWindowExW(
            // No activation flag: the panel must not take the foreground from
            // whatever the user was doing.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("WinBeautify"),
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;

    let mut panel = Panel::new(host, hwnd, tab);
    panel.painter = Painter::new().ok();
    if panel.painter.is_none() {
        tracing::error!("Direct2D is unavailable; the panel cannot be drawn");
    }
    panel.refresh();
    PUMP.with(|pump| *pump.borrow_mut() = Some(panel));
    slot.store(hwnd.0 as isize, Ordering::Release);

    unsafe {
        let preference: i32 = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        // A translucent panel with the desktop blurred behind it, which is what
        // the webview version had: a system backdrop plus the client area
        // extended into the frame, so what the panel draws with an alpha
        // composites over the blur instead of over black.
        let backdrop: i32 = DWMSBT_TRANSIENTWINDOW.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_SHOWWINDOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    let mut message = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }
    }

    PUMP.with(|pump| *pump.borrow_mut() = None);
    slot.store(0, Ordering::Release);
    Ok(())
}

thread_local! {
    static PUMP: std::cell::RefCell<Option<Panel>> = const { std::cell::RefCell::new(None) };
}

/// Borrow the thread-local panel, if it can be borrowed.
///
/// A re-entrant message — Windows sends several synchronously from inside a
/// handler like `ShowWindow` — falls through to `DefWindowProc` instead of
/// panicking: a panic inside a window procedure cannot unwind and would abort the
/// whole process.
fn with_panel<R>(f: impl FnOnce(&mut Panel) -> R) -> Option<R> {
    PUMP.with(|pump| {
        let mut slot = pump.try_borrow_mut().ok()?;
        let panel = slot.as_mut()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(panel))) {
            Ok(result) => Some(result),
            Err(_) => {
                tracing::error!(
                    "the panel hit a bug while handling a message; that message was skipped"
                );
                None
            }
        }
    })
}

/// The window coordinates in a mouse message.
fn point_of(lparam: LPARAM) -> (f32, f32) {
    (
        (lparam.0 & 0xFFFF) as i16 as f32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as f32,
    )
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Answered without the panel state: it arrives while the window is being
    // created, before the state exists. Returning 0 makes the client area the
    // whole window, which is what leaves room for the header drawn here.
    if message == WM_NCCALCSIZE {
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    match with_panel(|panel| panel.handle(message, wparam, lparam)) {
        Some(Some(result)) => result,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_class_name_is_namespaced() {
        let expected: Vec<u16> = "WinBeautify.Flyout\0".encode_utf16().collect();
        let actual = unsafe { std::slice::from_raw_parts(CLASS_NAME.as_ptr(), expected.len()) };
        assert_eq!(actual, expected.as_slice());
    }
}
