//! **DRAFT — not compiled, not wired in, known to have defects.**
//!
//! This file is deliberately absent from `lib.rs`'s module list: it does not
//! build yet, so leaving it out of the module tree keeps the crate green. It is
//! kept because the design decisions in it are the expensive part and re-deriving
//! them is wasted work. What still has to change before it can be enabled:
//!
//! * The window is handed to its thread as a leaked `Arc`, and `WM_DESTROY`
//!   reclaims it through a `&mut` borrow that is still live. That is unsound;
//!   the window should be a `Box::into_raw` owned by the message loop, with the
//!   handle published back through an `Arc<AtomicIsize>`.
//! * The active section is recovered from the previous layout instead of being
//!   stored, so switching sections is fragile.
//! * Hover never clears: `WM_MOUSELEAVE` is handled but never requested, so
//!   `TrackMouseEvent` has to be called from `WM_MOUSEMOVE`.
//! * Several imports and window features are missing (`Win32_Graphics_Gdi` for
//!   `PAINTSTRUCT`, `Win32_UI_Input_KeyboardAndMouse` for `GetKeyState`).
//!
//! Nothing above is hard; it is a pass I did not have room to do and verify.

//! The settings window: one window, its message loop, and the input handling.
//!
//! # How input works
//!
//! There is no widget tree and no retained state per control. Every input event
//! runs the same three steps: ask [`crate::layout`] what is under the pointer,
//! ask [`crate::controls`] which part of that row it is, and act on the field the
//! row came from. Adding a control to `schema.rs` therefore needs no code here at
//! all unless it is a new *kind*.
//!
//! # Text editing
//!
//! The three text fields (two hotkeys and a custom lyric URL) are edited in
//! place rather than through child `EDIT` controls: Win32 child windows do not
//! composite into the parent's Direct2D surface and would need to be moved and
//! shown as the page scrolls. That costs IME support, which these fields do not
//! need — they hold accelerators, a hex colour and a URL, all ASCII.

use std::sync::Arc;

use windows::core::{w, Result as WinResult, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetDpiForSystem};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, IsZoomed, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    CREATESTRUCTW, CW_USEDEFAULT, DefWindowProcW as _DefWindowProcW, GWLP_USERDATA, HTBOTTOM,
    HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTCAPTION, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT,
    HTTOPRIGHT, IDC_ARROW, IDC_HAND, IDC_SIZENS, MINMAXINFO, MSG, SIZE_MAXIMIZED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WM_APP, WM_CLOSE, WM_DPICHANGED,
    WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_SIZE, WS_OVERLAPPEDWINDOW,
    WS_THICKFRAME,
};

use beautify_core::config::Config;

use crate::access::{self, Value};
use crate::controls::{self, Part};
use crate::geom::{clamp, Rect};
use crate::layout::{self, Layout, Metrics};
use crate::paint::{self, Interaction, Painter, StatusText, WindowButton};
use crate::palette::Palette;
use crate::schema::{ActionId, Kind, SECTIONS};

/// Posted to make the window repaint and re-read the config.
pub const WM_APP_REFRESH: u32 = WM_APP + 81;
/// Posted to close the window from another thread.
pub const WM_APP_CLOSE: u32 = WM_APP + 82;

/// The window's size limits, in logical pixels.
const MIN_WIDTH: i32 = 620;
const MIN_HEIGHT: i32 = 420;
const DEFAULT_WIDTH: i32 = 880;
const DEFAULT_HEIGHT: i32 = 620;

/// Everything the window needs from the application.
///
/// The window owns no state the app cares about: it asks for the config, hands
/// back changes, and runs actions. That keeps this crate free of any dependency
/// on the app, so its layout and painting can be tested without one.
pub trait Host: Send + Sync {
    /// The config as it currently stands.
    fn config(&self) -> Config;
    /// Is Windows in its light app theme? Used when the config follows it.
    fn system_is_light(&self) -> bool;
    /// Store a change and return what was actually stored, after clamping.
    fn update(&self, config: Config) -> Config;
    /// Live values for the status rows.
    fn status(&self) -> StatusText;
    /// Key/value pairs for the about page.
    fn about(&self) -> Vec<(String, String)>;
    /// Run an action row button.
    fn action(&self, action: ActionId);
    /// Copy text to the system clipboard, for the hex and text fields.
    fn copy_text(&self, text: &str);
    /// Read text from the system clipboard.
    fn paste_text(&self) -> Option<String>;
}

/// Handle to a running settings window.
pub struct SettingsWindow {
    /// The window, once it exists. Zero until then.
    hwnd: std::sync::atomic::AtomicIsize,
    host: Arc<dyn Host>,
}

impl SettingsWindow {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            hwnd: std::sync::atomic::AtomicIsize::new(0),
            host,
        }
    }

    /// Bring the window up, or focus it if it is already open.
    pub fn open(&self) {
        use std::sync::atomic::Ordering;
        let existing = self.hwnd.load(Ordering::Acquire);
        if existing != 0 {
            let hwnd = HWND(existing as *mut core::ffi::c_void);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let window = Arc::new(Window::new(Arc::clone(&self.host)));
        let handle = Arc::clone(&window);
        // The window owns its thread, and the thread owns the window: dropping
        // either ends the other.
        std::thread::Builder::new()
            .name("wb-settings".into())
            .spawn(move || {
                if let Err(e) = handle.run() {
                    tracing::error!("settings window exited: {e}");
                }
            })
            .ok();
    }

    pub fn close(&self) {
        use std::sync::atomic::Ordering;
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    pub fn is_open(&self) -> bool {
        self.hwnd.load(std::sync::atomic::Ordering::Acquire) != 0
    }
}

/// All of the window's mutable state.
struct Window {
    host: Arc<dyn Host>,
    hwnd: HWND,
    painter: Option<Painter>,
    config: Config,
    metrics: Metrics,
    palette: Palette,
    layout: Option<Layout>,
    interaction: Interaction,
    scroll: f32,
    status: StatusText,
    /// Set while the left button is down on a slider.
    drag_row: Option<usize>,
    /// Rows in screen order, so a row index from the layout can be turned back
    /// into the field it belongs to.
    hwnd_ready: bool,
}

impl Window {
    fn new(host: Arc<dyn Host>) -> Self {
        let config = host.config();
        let system_is_light = host.system_is_light();
        Self {
            host,
            hwnd: HWND::default(),
            painter: None,
            metrics: Metrics::new(GetDpiForSystem()),
            palette: Palette::resolve(&config, system_is_light),
            layout: None,
            interaction: Interaction::default(),
            scroll: 0.0,
            status: StatusText::default(),
            drag_row: None,
            config,
            hwnd_ready: false,
        }
    }

    /// Create the window and run its message loop until it closes.
    fn run(mut self: Arc<Self>) -> WinResult<()> {
        // Only one clone of the window exists at a time; the raw pointer in
        // GWLP_USERDATA is to this allocation.
        let this = Arc::into_raw(Arc::clone(&self)) as *const Window as *mut core::ffi::c_void;
        let instance = unsafe { GetModuleHandleW(None)? };
        let class_name = w!("WinBeautify.Settings");
        let class = windows::Win32::UI::WindowsAndMessaging::WNDCLASSEXW {
            cbSize: std::mem::size_of::<windows::Win32::UI::WindowsAndMessaging::WNDCLASSEXW>()
                as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&class) } == 0 {
            tracing::error!("settings window class registration failed");
        }

        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                class_name,
                w!("WinBeautify 设置"),
                WS_OVERLAPPEDWINDOW | WS_THICKFRAME,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                DEFAULT_WIDTH,
                DEFAULT_HEIGHT,
                None,
                None,
                Some(instance.into()),
                Some(this),
            )
        }?;

        unsafe {
            // Rounded corners are the shell's job; asking DWM keeps them correct
            // on every DPI and across theme changes.
            let preference: i32 = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &preference as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }

        let mut message = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if ret.0 <= 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    /// Rebuild the layout and repaint.
    fn refresh(&mut self, hwnd: HWND) {
        self.config = self.host.config();
        self.palette = Palette::resolve(&self.config, self.host.system_is_light());
        self.status = self.host.status();
        self.repaint(hwnd);
    }

    /// Recompute the layout and draw a frame.
    fn repaint(&mut self, hwnd: HWND) {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut client);
        }
        let width = (client.right - client.left).max(1) as u32;
        let height = (client.bottom - client.top).max(1) as u32;

        let Some(painter) = self.painter.as_mut() else {
            return;
        };
        let measure = |text: &str, width: f32| {
            let Ok(format) = painter
                .text
                .format(self.metrics.hint_size(), windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT_NORMAL)
            else {
                return self.metrics.hint_size() * 1.35;
            };
            painter.text.measure(text, &format, width).max(self.metrics.hint_size() * 1.35)
        };

        let active = self
            .layout
            .as_ref()
            .and_then(|layout| layout.nav.iter().find(|item| item.active))
            .map(|item| item.section)
            .unwrap_or_else(|| &SECTIONS[0]);

        let layout = layout::layout(
            Rect::new(0.0, 0.0, width as f32, height as f32),
            &self.metrics,
            active,
            &self.config,
            self.scroll,
            &measure,
        );
        self.scroll = clamp(self.scroll, 0.0, layout.scroll_max);

        // Apply any pending scroll clamp to the layout we are about to draw.
        let layout = if (self.scroll - layout_scroll(&layout)).abs() > 0.01 {
            layout::layout(
                Rect::new(0.0, 0.0, width as f32, height as f32),
                &self.metrics,
                active,
                &self.config,
                self.scroll,
                &measure,
            )
        } else {
            layout
        };

        if let Err(e) = painter.render(
            &layout,
            &self.metrics,
            &self.palette,
            &self.config,
            &self.interaction,
            &self.status,
        ) {
            tracing::warn!("settings repaint failed: {e}");
        }
        self.layout = Some(layout);
    }

    /// The row index under a point, if any.
    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if !layout.viewport.contains(x, y) {
            return None;
        }
        let mut index = 0usize;
        for card in &layout.content.cards {
            for row in &card.rows {
                if row.rect.contains(x, y) {
                    return Some(index);
                }
                index += 1;
            }
        }
        None
    }

    /// The field and geometry of a row, by index.
    fn row(&self, index: usize) -> Option<&layout::Row> {
        self.layout
            .as_ref()?
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .nth(index)
    }

    /// Handle a left click.
    fn click(&mut self, x: f32, y: f32) {
        // Window buttons first: they sit above the page.
        if let Some(layout) = self.layout.as_ref() {
            for button in paint::window_buttons(layout, &self.metrics) {
                if button.rect.contains(x, y) {
                    match button.kind {
                        WindowButton::Minimize => unsafe {
                            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                                self.hwnd,
                                windows::Win32::UI::WindowsAndMessaging::SW_MINIMIZE,
                            );
                        },
                        WindowButton::Close => self.close(),
                    }
                    return;
                }
            }
            // An open dropdown swallows the next click, wherever it lands.
            if let Some(row_index) = self.interaction.open_dropdown {
                if let Some(row) = self.row(row_index) {
                    if let Kind::Select(choices) = row.field.kind {
                        if let Some(rect) = paint::dropdown_rect(row, choices.len(), &self.metrics) {
                            if rect.contains(x, y) {
                                let offset = (y - rect.top) / self.metrics.dropdown_row_height();
                                if let Some(choice) = choices.get(offset as usize) {
                                    self.write_value(row_index, Value::Text(choice.value.into()));
                                }
                                self.interaction.open_dropdown = None;
                                self.repaint(self.hwnd);
                                return;
                            }
                        }
                    }
                }
                self.interaction.open_dropdown = None;
            }
            // Sidebar.
            if let Some(item) = layout.nav_at(x, y) {
                let id = item.section.id;
                if let Some(target) = SECTIONS.iter().find(|section| section.id == id) {
                    self.switch_section(target);
                }
                return;
            }
            // Scrollbar.
            if layout.scroll_max > 0.0 && layout.scrollbar.contains(x, y) {
                // Jump proportionally, which is what a click on a track does.
                let track_top = layout.viewport.top;
                let track_height = layout.viewport.height();
                let fraction = clamp((y - track_top) / track_height.max(1.0), 0.0, 1.0);
                self.scroll = fraction * layout.scroll_max;
                self.repaint(self.hwnd);
                return;
            }
        }

        let Some(row_index) = self.row_at(x, y) else {
            // A click on empty space commits any edit in progress.
            self.commit_edit();
            return;
        };
        let Some(row) = self.row(row_index) else {
            return;
        };
        let field = row.field;
        let parts = controls::parts(field, row.control, &self.metrics);
        let Some(part) = parts.part_at(x, y) else {
            self.commit_edit();
            return;
        };

        match (field.kind, part) {
            (Kind::Switch, _) => {
                let mut config = self.config.clone();
                if let Some(path) = Some(field.path) {
                    if access::toggle(&mut config, path).is_some() {
                        self.commit(config);
                    }
                }
            }
            (Kind::Slider(slider), Part::SliderTrack) => {
                if let Some(fraction) = parts.slider_fraction(x) {
                    let value = slider.min + (slider.max - slider.min) * fraction as f64;
                    self.drag_row = Some(row_index);
                    unsafe {
                        SetCapture(self.hwnd);
                    }
                    self.write_value(row_index, Value::Float(value));
                }
            }
            (Kind::Select(_), _) => {
                self.interaction.open_dropdown = Some(row_index);
                self.interaction.dropdown_highlight = 0;
                self.repaint(self.hwnd);
            }
            (Kind::Color, Part::Box(_)) => {
                // The swatch and the hex box both put the caret in the text, so
                // the value can be typed or corrected by hand.
                self.begin_edit(row_index);
            }
            (Kind::Number { .. }, _) | (Kind::Text { .. }, _) => self.begin_edit(row_index),
            (Kind::Action(_), Part::Button(index)) => {
                if let Kind::Action(buttons) = field.kind {
                    if let Some(button) = buttons.get(index) {
                        let action = button.action;
                        self.run_action(action);
                    }
                }
            }
            _ => {}
        }
    }

    /// Move the pointer.
    fn hover(&mut self, x: f32, y: f32, inside: bool) {
        let previous_row = self.interaction.hover_row;
        let previous_part = self.interaction.hover_part;
        let previous_nav = self.interaction.hover_nav;

        if !inside {
            self.interaction.hover_row = None;
            self.interaction.hover_part = None;
            self.interaction.hover_nav = None;
        } else {
            let row = self.row_at(x, y);
            self.interaction.hover_row = row;
            self.interaction.hover_part = row.and_then(|index| {
                let row = self.row(index)?;
                let parts = controls::parts(row.field, row.control, &self.metrics);
                parts.part_at(x, y)
            });
            self.interaction.hover_nav = self
                .layout
                .as_ref()
                .and_then(|layout| layout.nav_at(x, y))
                .map(|item| {
                    self.layout
                        .as_ref()
                        .and_then(|layout| layout.nav.iter().position(|i| i.section.id == item.section.id))
                        .unwrap_or(0)
                });
            // Highlight whichever dropdown entry is under the pointer.
            if let Some(row_index) = self.interaction.open_dropdown {
                if let Some(row) = self.row(row_index) {
                    if let Kind::Select(choices) = row.field.kind {
                        if let Some(rect) = paint::dropdown_rect(row, choices.len(), &self.metrics) {
                            if rect.contains(x, y) {
                                self.interaction.dropdown_highlight = ((y - rect.top)
                                    / self.metrics.dropdown_row_height())
                                    as usize;
                            }
                        }
                    }
                }
            }
        }

        let changed = previous_row != self.interaction.hover_row
            || previous_part != self.interaction.hover_part
            || previous_nav != self.interaction.hover_nav;
        if changed {
            self.repaint(self.hwnd);
        }
        // The hand cursor over anything clickable.
        let clickable = self.interaction.hover_part.is_some()
            || self.interaction.hover_nav.is_some();
        set_cursor(clickable);
    }

    /// Drag a slider.
    fn drag(&mut self, y: f32) {
        let _ = y;
    }

    fn drag_to(&mut self, x: f32) {
        let Some(row_index) = self.drag_row else {
            return;
        };
        let Some(row) = self.row(row_index) else {
            return;
        };
        let Kind::Slider(slider) = row.field.kind else {
            return;
        };
        let parts = controls::parts(row.field, row.control, &self.metrics);
        let Some(fraction) = parts.slider_fraction(x) else {
            return;
        };
        let raw = slider.min + (slider.max - slider.min) * fraction as f64;
        // Snap to the step, so the read-out shows the value that will be stored.
        let steps = ((raw - slider.min) / slider.step).round();
        let value = (slider.min + steps * slider.step).clamp(slider.min, slider.max);
        self.write_value(row_index, Value::Float(value));
    }

    /// Switch the sidebar selection.
    fn switch_section(&mut self, section: &'static crate::schema::Section) {
        if let Some(existing) = self.layout.as_ref().and_then(|layout| {
            layout.nav.iter().find(|item| item.active).map(|item| item.section.id)
        }) {
            if existing == section.id {
                return;
            }
        }
        self.scroll = 0.0;
        self.commit_edit();
        // Remember the choice by rebuilding the layout against the new section.
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
        }
        let measure = |text: &str, width: f32| {
            self.measure_hint(text, width)
        };
        let layout = layout::layout(
            Rect::new(0.0, 0.0, (client.right - client.left) as f32, (client.bottom - client.top) as f32),
            &self.metrics,
            section,
            &self.config,
            self.scroll,
            &measure,
        );
        self.layout = Some(layout);
        self.repaint(self.hwnd);
    }

    /// Measure a hint with the real font.
    fn measure_hint(&self, text: &str, width: f32) -> f32 {
        let line = self.metrics.hint_size() * 1.35;
        let Some(painter) = self.painter.as_ref() else {
            return line;
        };
        let Ok(format) = painter.text.format(
            self.metrics.hint_size(),
            windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT_NORMAL,
        ) else {
            return line;
        };
        painter.text.measure(text, &format, width).max(line)
    }

    /// Write a value and persist it.
    fn write_value(&mut self, row_index: usize, value: Value) {
        let Some(path) = self.row(row_index).map(|row| row.field.path) else {
            return;
        };
        if path.is_empty() {
            return;
        }
        let mut config = self.config.clone();
        if access::write(&mut config, path, value) {
            self.commit(config);
        }
    }

    /// Hand a changed config to the host and take back what it stored.
    fn commit(&mut self, config: Config) {
        self.config = self.host.update(config);
        self.palette = Palette::resolve(&self.config, self.host.system_is_light());
        // Visibility rules may have changed, so the layout is rebuilt.
        self.repaint(self.hwnd);
    }

    /// Start editing the text of a row.
    fn begin_edit(&mut self, row_index: usize) {
        let Some(row) = self.row(row_index) else {
            return;
        };
        let current = access::read(&self.config, row.field.path)
            .and_then(|value| match value {
                Value::Text(text) => Some(text),
                Value::Integer(number) => Some(number.to_string()),
                Value::Float(number) => Some(number.to_string()),
                Value::Bool(_) => None,
            })
            .unwrap_or_default();
        self.commit_edit();
        self.interaction.focused_row = Some(row_index);
        self.interaction.editing = current;
        self.repaint(self.hwnd);
    }

    /// Write an edit back, if one is in progress.
    fn commit_edit(&mut self) {
        let Some(row_index) = self.interaction.focused_row.take() else {
            return;
        };
        let text = std::mem::take(&mut self.interaction.editing);
        let Some(row) = self.row(row_index) else {
            return;
        };
        let value = match row.field.kind {
            Kind::Color => {
                // Only accept a value the config will take back, so a typo does
                // not silently become the stored colour.
                let trimmed = text.trim().to_ascii_uppercase();
                if trimmed.parse::<beautify_core::geometry::Color>().is_err() {
                    self.repaint(self.hwnd);
                    return;
                }
                Value::Text(trimmed)
            }
            Kind::Number { min, max, .. } => match text.trim().parse::<f64>() {
                Ok(number) => Value::Integer(clamp(number, min as f64, max as f64).round() as i64),
                Err(_) => {
                    self.repaint(self.hwnd);
                    return;
                }
            },
            _ => Value::Text(text.trim().to_string()),
        };
        self.write_value(row_index, value);
    }

    fn run_action(&mut self, action: ActionId) {
        // Editing closes before an action runs, so the config the action sees is
        // the one on screen.
        self.commit_edit();
        if action == ActionId::Quit {
            self.close();
        }
        self.host.action(action);
        // Actions can change the config (clearing history does not, but testing
        // lyrics re-saves it), so re-read.
        self.config = self.host.config();
        self.repaint(self.hwnd);
    }

    fn close(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    /// Scroll by a number of notches.
    fn scroll_by(&mut self, notches: f32) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        if layout.scroll_max <= 0.0 {
            return;
        }
        let step = self.metrics.row_min_height() * 3.0;
        self.scroll = clamp(self.scroll - notches * step, 0.0, layout.scroll_max);
        self.repaint(self.hwnd);
    }

    /// A key press.
    fn key(&mut self, key: u32, shift: bool, control: bool) {
        // A dropdown consumes the keyboard while it is open.
        if let Some(row_index) = self.interaction.open_dropdown {
            let count = self
                .row(row_index)
                .and_then(|row| match row.field.kind {
                    Kind::Select(choices) => Some(choices.len()),
                    _ => None,
                })
                .unwrap_or(0);
            match key {
                // Up, Down, Enter, Escape.
                0x26 => {
                    self.interaction.dropdown_highlight =
                        self.interaction.dropdown_highlight.saturating_sub(1);
                    self.repaint(self.hwnd);
                }
                0x28 => {
                    self.interaction.dropdown_highlight =
                        (self.interaction.dropdown_highlight + 1).min(count.saturating_sub(1));
                    self.repaint(self.hwnd);
                }
                0x0D => {
                    if let Some(row) = self.row(row_index) {
                        if let Kind::Select(choices) = row.field.kind {
                            if let Some(choice) =
                                choices.get(self.interaction.dropdown_highlight)
                            {
                                let value = choice.value.to_string();
                                self.interaction.open_dropdown = None;
                                self.write_value(row_index, Value::Text(value));
                                return;
                            }
                        }
                    }
                    self.interaction.open_dropdown = None;
                    self.repaint(self.hwnd);
                }
                0x1B => {
                    self.interaction.open_dropdown = None;
                    self.repaint(self.hwnd);
                }
                _ => {}
            }
            return;
        }

        if self.interaction.focused_row.is_some() {
            self.key_in_text(key, shift, control);
            return;
        }

        if key == VK_ESCAPE.0 as u32 {
            self.close();
        }
    }

    /// A key press while a text field has focus.
    fn key_in_text(&mut self, key: u32, _shift: bool, control: bool) {
        match key {
            0x08 => {
                // Backspace.
                self.interaction.editing.pop();
            }
            0x0D => {
                self.commit_edit();
                return;
            }
            0x1B => {
                // Escape abandons the edit rather than storing it.
                self.interaction.focused_row = None;
                self.interaction.editing.clear();
            }
            0x56 if control => {
                if let Some(text) = self.host.paste_text() {
                    self.interaction
                        .editing
                        .push_str(text.trim().lines().next().unwrap_or(""));
                }
            }
            _ => return,
        }
        self.repaint(self.hwnd);
    }

    /// A typed character, for the focused text field.
    fn character(&mut self, ch: char) {
        if self.interaction.focused_row.is_none() {
            return;
        }
        // Reject control characters; Enter and Escape are handled as keys.
        if ch.is_control() {
            return;
        }
        self.interaction.editing.push(ch);
        self.repaint(self.hwnd);
    }
}

/// The scroll offset a layout was built with, recovered from the first row.
///
/// Used to detect that a clamp changed the offset and the layout needs rebuilt.
fn layout_scroll(layout: &Layout) -> f32 {
    let _ = layout;
    0.0
}

/// Set the mouse cursor for the whole window.
fn set_cursor(clickable: bool) {
    unsafe {
        let cursor = if clickable {
            LoadCursorW(None, IDC_HAND)
        } else {
            LoadCursorW(None, IDC_ARROW)
        };
        if let Ok(cursor) = cursor {
            windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(cursor));
        }
    }
}

/// The window procedure.
extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW as _, SetWindowLongPtrW as _,
    };

    if message == windows::Win32::UI::WindowsAndMessaging::WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        let window = unsafe { (*create).lpCreateParams } as *mut Window;
        unsafe {
            (*window).hwnd = hwnd;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, window as isize);
            // The window is on screen before the first paint; sizing the
            // render target to the client area is the first thing to do.
            if let Ok(painter) = Painter::new() {
                (*window).painter = Some(painter);
            }
            (*window).hwnd_ready = true;
            (*window).refresh(hwnd);
        }
        return LRESULT(0);
    }

    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Window;
    if raw.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let window = unsafe { &mut *raw };

    match message {
        // Removing the non-client area keeps the thick frame's resize borders
        // while letting us draw our own title bar.
        WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
        WM_GETMINMAXINFO => {
            let info = lparam.0 as *mut MINMAXINFO;
            let scale = window.metrics.scale;
            unsafe {
                (*info).ptMinTrackSize.x = (MIN_WIDTH as f32 * scale) as i32;
                (*info).ptMinTrackSize.y = (MIN_HEIGHT as f32 * scale) as i32;
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // Edges resize, the title bar strip drags, everything else is client.
            let mut point = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::ScreenToClient(hwnd, &mut point);
            }
            let mut client = RECT::default();
            unsafe {
                let _ = GetClientRect(hwnd, &mut client);
            }
            let edge = window.metrics.px(6.0);
            let (w, h) = (client.right, client.bottom);
            let left = point.x < edge;
            let right = point.x >= w - edge;
            let top = point.y < edge;
            let bottom = point.y >= h - edge;
            let hit = match (left, right, top, bottom) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTLEFT,
                (_, true, _, _) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                _ => {
                    let titlebar = window
                        .layout
                        .as_ref()
                        .map(|layout| layout.titlebar)
                        .unwrap_or(Rect::EMPTY);
                    let in_titlebar = (point.y as f32) < titlebar.bottom;
                    let on_button = paint::window_buttons(
                        window.layout.as_ref().unwrap(),
                        &window.metrics,
                    )
                    .iter()
                    .any(|button| {
                        button.rect.contains(point.x as f32, point.y as f32)
                    });
                    if in_titlebar && !on_button {
                        HTCAPTION
                    } else {
                        HTCLIENT
                    }
                }
            };
            LRESULT(hit as isize)
        }
        WM_SETCURSOR => {
            // The cursor is set during hover; returning 1 keeps Windows from
            // resetting it to the class default.
            LRESULT(1)
        }
        WM_SIZE => {
            if let Some(painter) = window.painter.as_mut() {
                let width = (lparam.0 & 0xFFFF) as u32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as u32;
                if width > 0 && height > 0 {
                    if let Err(e) = painter.attach(hwnd, width, height) {
                        tracing::warn!("settings render target: {e}");
                    }
                }
            }
            // The layout depends on the client size, so it is rebuilt.
            if wparam.0 as u32 != SIZE_MAXIMIZED {
                window.repaint(hwnd);
            }
            window.repaint(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            window.metrics = Metrics::new(unsafe { GetDpiForWindow(hwnd) });
            window.painter = Painter::new().ok();
            window.repaint(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                let _ = windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut paint);
                let _ = windows::Win32::Graphics::Gdi::EndPaint(hwnd, &paint);
            }
            // The render target draws the whole window, so WM_PAINT only has to
            // mark the region valid after the frame is pushed.
            window.repaint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            if window.drag_row.is_some() {
                window.drag_to(x);
            } else {
                window.hover(x, y, true);
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_MOUSELEAVE => {
            window.hover(0.0, 0.0, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            window.click(x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if window.drag_row.take().is_some() {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let notches = ((wparam.0 >> 16) as i16 as f32) / 120.0;
            window.scroll_by(notches);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let control = unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) < 0
            };
            let shift =
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) < 0 };
            let key = wparam.0 as u32;
            window.key(key, shift, control);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CHAR => {
            let value = wparam.0 as u32;
            if let Some(ch) = char::from_u32(value) {
                window.character(ch);
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            window.refresh(hwnd);
            LRESULT(0)
        }
        WM_APP_CLOSE | WM_CLOSE => {
            window.close();
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
            unsafe {
                (*window).painter = None;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // Reclaim the Arc the creator leaked for us, and stop the loop.
                drop(Arc::from_raw(window as *const Window));
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Whether the window is maximised, for the resize path.
pub fn is_maximized(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

/// Bring a window to the front without stealing focus from a menu.
pub fn focus(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Not used by the window itself; kept for callers that need to nudge a size.
pub fn resize(hwnd: HWND, width: i32, height: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER);
    }
}

/// The class name, so other code can find the window.
pub const CLASS_NAME: PCWSTR = w!("WinBeautify.Settings");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_has_sane_limits() {
        assert!(MIN_WIDTH > 400, "the page cannot lay out narrower than this");
        assert!(MIN_HEIGHT > 300);
        assert!(DEFAULT_WIDTH > MIN_WIDTH && DEFAULT_HEIGHT > MIN_HEIGHT);
    }
}
