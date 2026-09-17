//! The settings window: one window, its message loop, and input handling.
//!
//! # How input works
//!
//! There is no widget tree and no per-control retained state. Every input event
//! runs the same three steps: ask [`crate::layout`] what row is under the
//! pointer, ask [`crate::controls`] which part of that row it is, and act on the
//! field the row came from. Adding a control to `schema.rs` therefore needs no
//! code here at all unless it is a new *kind*.
//!
//! # Why the state lives in a thread local
//!
//! The window is created on, and owned by, its own thread; `window_proc` reaches
//! it through a thread-local rather than through `GWLP_USERDATA`. The GWLP route
//! needs a raw pointer that the message handler derefs as `&mut`, and the
//! tempting way to produce one — leaking an `Arc` and rebuilding it in
//! `WM_DESTROY` — is where a use-after-free hides: the borrow is still live when
//! the reclaim happens. There is exactly one settings window per pumping thread,
//! so a thread-local is both simpler and impossible to get wrong.
//!
//! `WM_DESTROY` is deliberately answered without touching the state at all: it
//! only posts the quit message. `DestroyWindow` runs *inside* the handler for
//! `WM_CLOSE`, and `WM_DESTROY` is then delivered synchronously from within that
//! call — reaching back into the borrow we are already inside.
//!
//! # Text editing
//!
//! The text fields are edited in place rather than through child `EDIT`
//! controls: Win32 child windows do not composite into the parent's Direct2D
//! surface, and they would have to be moved and shown as the page scrolls. The
//! caret is drawn by the painter, because with no child control there is no
//! system caret.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT_NORMAL;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    IsZoomed, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SetCursor,
    SetForegroundWindow, SetWindowPos, ShowWindow, TranslateMessage, CW_USEDEFAULT,
    HTCLIENT, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, IDC_ARROW, IDC_HAND, MINMAXINFO, MSG, NCCALCSIZE_PARAMS, SWP_NOZORDER,
    SW_SHOW, SW_SHOWMINIMIZED, WM_APP, WM_CLOSE, WM_DPICHANGED, WM_DESTROY, WM_ERASEBKGND,
    WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_SIZE, WM_CHAR, WNDCLASSEXW,
    WINDOW_EX_STYLE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

use beautify_core::config::Config;

use crate::access::{self, Value};
use crate::controls;
use crate::geom::{clamp, Rect};
use crate::layout::{self, Layout, Metrics};
use crate::paint::{self, Interaction, Painter, StatusText, WindowButton};
use crate::palette::Palette;
use crate::schema::{ActionId, Kind, Section, SECTIONS};
use beautify_core::hotkey::Modifiers;

/// Posted to make the window repaint and re-read the config.
pub const WM_APP_REFRESH: u32 = WM_APP + 81;
/// Posted to close the window from another thread.
pub const WM_APP_CLOSE: u32 = WM_APP + 82;

/// The class name, so other code can find the window.
pub const CLASS_NAME: PCWSTR = w!("WinBeautify.Settings");

/// `WM_MOUSELEAVE` from `winuser.h`. Declared here because `windows-rs` files it
/// under the controls module, which this crate otherwise has no use for.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// The window's size limits, in logical pixels.
const MIN_WIDTH: i32 = 620;
const MIN_HEIGHT: i32 = 420;
const DEFAULT_WIDTH: i32 = 880;
const DEFAULT_HEIGHT: i32 = 620;

/// Everything the window needs from the application.
///
/// The window owns no state the app cares about: it asks for the config, hands
/// back changes and runs actions. That keeps this crate free of any dependency
/// on the app, so its layout and painting can be tested without one.
pub trait Host: Send + Sync {
    /// The config as it currently stands.
    fn config(&self) -> Config;
    /// Is Windows in its light app theme? Used when the config follows it.
    fn system_is_light(&self) -> bool;
    /// Store a change and return what was actually stored, after clamping.
    fn update(&self, config: Config) -> Config;
    /// Live values for the status and 关于 rows.
    fn status(&self) -> StatusText;
    /// Run an action row button.
    fn action(&self, action: ActionId);
    /// Copy text to the system clipboard, for the text fields.
    fn copy_text(&self, text: &str);
    /// Read text from the system clipboard.
    fn paste_text(&self) -> Option<String>;
}

/// Handle to a running settings window.
pub struct SettingsWindow {
    /// The window, once it exists. Zero until then, and again after it closes.
    ///
    /// Shared with the window's own thread, which is the only writer: the window
    /// is created and destroyed there, and the host thread only reads it.
    hwnd: Arc<AtomicIsize>,
    host: Arc<dyn Host>,
}

impl SettingsWindow {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self {
            hwnd: Arc::new(AtomicIsize::new(0)),
            host,
        }
    }

    /// Bring the window up, or focus it if it is already open.
    pub fn open(&self) {
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

        let host = Arc::clone(&self.host);
        let slot = Arc::clone(&self.hwnd);
        std::thread::Builder::new()
            .name("wb-settings".into())
            .spawn(move || {
                if let Err(e) = run(host, slot) {
                    tracing::error!("settings window exited: {e}");
                }
            })
            .ok();
    }

    /// Ask the window to close. Returns immediately; the window is destroyed on
    /// its own thread.
    pub fn close(&self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// Ask the window to re-read the config, the theme and the status values.
    ///
    /// For changes made outside the window — a hand-edited `config.toml`, the
    /// tray's "reload" — which the window cannot know about.
    pub fn refresh(&self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }

    /// Is the window up right now?
    pub fn is_open(&self) -> bool {
        self.hwnd.load(Ordering::Acquire) != 0
    }

    /// Minimise the window, if it is open.
    pub fn minimize(&self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWMINIMIZED);
        }
    }
}

/// Where a click landed, worked out before anything acts on it.
///
/// Carries everything the action needs, so acting never has to go back and read
/// the layout — which is what keeps a click from needing a borrow of `self` that
/// is still live when the config is written.
enum Hit {
    Window(WindowButton),
    /// A sidebar entry, by section id.
    Section(&'static str),
    /// A click on the scrollbar track, as a `0..1` fraction of it.
    Scrollbar(f32),
    DropdownChoice {
        row: usize,
        value: &'static str,
    },
    DismissDropdown,
    Row(usize),
    Nothing,
}

/// All of the window's mutable state.
struct Window {
    host: Arc<dyn Host>,
    hwnd: HWND,
    /// `None` when Direct2D is unavailable; the page then lays out but does not
    /// draw, which beats taking the window down.
    painter: Option<Painter>,
    config: Config,
    metrics: Metrics,
    palette: Palette,
    layout: Option<Layout>,
    /// Which sidebar entry is showing.
    ///
    /// Held rather than recovered from the previous layout: a layout is rebuilt
    /// for the new section before the old one is replaced, so recovering it
    /// would read the *previous* choice and refuse the switch.
    active: &'static Section,
    interaction: Interaction,
    scroll: f32,
    status: StatusText,
    /// Set while the left button is down on a slider.
    drag_row: Option<usize>,
    /// Whether a `WM_MOUSELEAVE` is pending, so it is only requested once.
    tracking_leave: bool,
}

impl Window {
    fn new(host: Arc<dyn Host>, hwnd: HWND) -> Self {
        let config = host.config();
        let system_is_light = host.system_is_light();
        Self {
            palette: Palette::resolve(&config, system_is_light),
            host,
            hwnd,
            painter: None,
            config,
            metrics: Metrics::new(unsafe { GetDpiForWindow(hwnd) }),
            layout: None,
            active: &SECTIONS[0],
            interaction: Interaction::default(),
            scroll: 0.0,
            status: StatusText::default(),
            drag_row: None,
            tracking_leave: false,
        }
    }

    /// The client area, in physical pixels.
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

    /// Re-read everything the host owns and repaint.
    fn refresh(&mut self) {
        self.config = self.host.config();
        self.palette = Palette::resolve(&self.config, self.host.system_is_light());
        self.status = self.host.status();
        self.repaint();
    }

    /// Build the layout for the current size, section and scroll offset.
    fn build_layout(&self, width: f32, height: f32) -> Layout {
        let window = Rect::new(0.0, 0.0, width, height);
        layout::layout(
            window,
            &self.metrics,
            self.active,
            &self.config,
            self.scroll,
            &|text, width| self.measure_hint(text, width),
        )
    }

    /// Measure a hint with the real font: the height it needs once wrapped.
    ///
    /// `TextEngine::measure` returns a *width* — it is named for what it
    /// measures, not for what the caller wants — so the height has to come from
    /// the wrapped line count instead. It uses the same size and the same line
    /// spacing as the painter draws with, so the box the layout reserves and the
    /// text that lands in it cannot disagree.
    fn measure_hint(&self, text: &str, width: f32) -> f32 {
        let line = self.metrics.description_size() * layout::LINE_SPACING;
        let Some(painter) = self.painter.as_ref() else {
            return line;
        };
        let Ok(format) = painter
            .text
            .format(self.metrics.description_size(), DWRITE_FONT_WEIGHT_NORMAL)
        else {
            return line;
        };
        let lines = painter.text.wrap(text, &format, width).len().max(1);
        lines as f32 * line
    }

    /// Recompute the layout and draw a frame.
    fn repaint(&mut self) {
        let (width, height) = self.client_size();
        if let Some(painter) = self.painter.as_mut() {
            if let Err(e) = painter.attach(self.hwnd, width, height) {
                tracing::warn!("settings render target: {e}");
            }
        }

        let mut layout = self.build_layout(width as f32, height as f32);
        if (layout.scroll - self.scroll).abs() > 0.01 {
            // The offset was past the end of the page and got clamped. Rebuild
            // at the clamped value, or the page is drawn at one offset while the
            // scrollbar thumb claims another.
            self.scroll = layout.scroll;
            layout = self.build_layout(width as f32, height as f32);
        }

        if let Some(painter) = self.painter.as_mut() {
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

    /// The row at `index`, in the order the page is drawn.
    fn row(&self, index: usize) -> Option<&layout::Row> {
        self.layout
            .as_ref()?
            .content
            .cards
            .iter()
            .flat_map(|card| card.rows.iter())
            .nth(index)
    }

    /// Highlight whatever is under the pointer. Returns whether anything moved.
    fn update_hover(&mut self, x: f32, y: f32, inside: bool) -> bool {
        let previous = (
            self.interaction.hover_row,
            self.interaction.hover_part,
            self.interaction.hover_nav,
            self.interaction.hover_window,
        );

        // The title-bar buttons are above the page and belong to no row, so
        // they are checked first and independently.
        let window_button = if inside {
            self.layout.as_ref().and_then(|layout| {
                paint::window_buttons(layout, &self.metrics)
                    .into_iter()
                    .find(|button| button.rect.contains(x, y))
                    .map(|button| button.kind)
            })
        } else {
            None
        };
        self.interaction.hover_window = window_button;
        let inside = inside && window_button.is_none();

        let (row, part, nav) = if inside {
            let row = self.row_at(x, y);
            // A read-only row is not a target: no hand cursor, no hover wash.
            let interactive = row.filter(|index| {
                self.row(*index)
                    .map(|row| row.field.kind.is_interactive())
                    .unwrap_or(false)
            });
            let part = interactive.and_then(|index| {
                let row = self.row(index)?;
                let parts = controls::parts(row.field, row.control, &self.metrics);
                parts.part_at(x, y)
            });
            let nav = self.layout.as_ref().and_then(|layout| {
                let item = layout.nav_at(x, y)?;
                layout
                    .nav
                    .iter()
                    .position(|candidate| candidate.section.id == item.section.id)
            });
            (interactive, part, nav)
        } else {
            (None, None, None)
        };

        self.interaction.hover_row = row;
        self.interaction.hover_part = part;
        self.interaction.hover_nav = nav;

        // Highlight whichever entry of an open dropdown is under the pointer.
        if let Some(row_index) = self.interaction.open_dropdown {
            if let Some(rect) = self.dropdown_rect(row_index) {
                if rect.contains(x, y) {
                    self.interaction.dropdown_highlight =
                        ((y - rect.top) / self.metrics.dropdown_row_height()) as usize;
                }
            }
        }

        previous
            != (
                self.interaction.hover_row,
                self.interaction.hover_part,
                self.interaction.hover_nav,
                self.interaction.hover_window,
            )
    }

    /// Where a row's dropdown list is drawn, if it is a dropdown and open.
    fn dropdown_rect(&self, row_index: usize) -> Option<Rect> {
        let row = self.row(row_index)?;
        let Kind::Select(choices) = row.field.kind else {
            return None;
        };
        paint::dropdown_rect(row, choices.len(), &self.metrics)
    }

    /// Work out what a click at `(x, y)` landed on.
    ///
    /// Split from acting on it so the borrow of the layout — and of the row a
    /// point falls in — is over before anything mutates the config or the
    /// window. Everything a hit needs to carry out is copied out here.
    fn hit_test(&self, x: f32, y: f32) -> Hit {
        let Some(layout) = self.layout.as_ref() else {
            return Hit::Nothing;
        };

        // Window buttons first: they sit above the page.
        for button in paint::window_buttons(layout, &self.metrics) {
            if button.rect.contains(x, y) {
                return Hit::Window(button.kind);
            }
        }

        // An open dropdown swallows the next click, wherever it lands.
        if let Some(row_index) = self.interaction.open_dropdown {
            let choice = self
                .dropdown_rect(row_index)
                .filter(|rect| rect.contains(x, y))
                .and_then(|rect| {
                    let offset = (y - rect.top) / self.metrics.dropdown_row_height();
                    match self.row(row_index).map(|row| row.field.kind) {
                        Some(Kind::Select(choices)) => choices.get(offset as usize),
                        _ => None,
                    }
                });
            return match choice {
                Some(choice) => Hit::DropdownChoice {
                    row: row_index,
                    value: choice.value,
                },
                None => Hit::DismissDropdown,
            };
        }

        // Sidebar.
        if let Some(item) = layout.nav_at(x, y) {
            return Hit::Section(item.section.id);
        }

        // Scrollbar: a click on the track jumps proportionally.
        if layout.scroll_max > 0.0 && layout.scrollbar.contains(x, y) {
            let fraction = clamp(
                (y - layout.viewport.top) / layout.viewport.height().max(1.0),
                0.0,
                1.0,
            );
            return Hit::Scrollbar(fraction);
        }

        match self.row_at(x, y) {
            Some(index) => Hit::Row(index),
            None => Hit::Nothing,
        }
    }

    /// Handle a left click.
    fn click(&mut self, x: f32, y: f32) {
        match self.hit_test(x, y) {
            Hit::Window(WindowButton::Minimize) => self.minimize(),
            Hit::Window(WindowButton::Close) => self.close(),
            Hit::Section(id) => {
                if let Some(target) = SECTIONS.iter().find(|section| section.id == id) {
                    self.switch_section(target);
                }
            }
            Hit::Scrollbar(fraction) => {
                let max = self.layout.as_ref().map(|layout| layout.scroll_max).unwrap_or(0.0);
                self.scroll = fraction * max;
                self.repaint();
            }
            Hit::DropdownChoice { row, value } => {
                self.interaction.open_dropdown = None;
                self.write_value(row, Value::Text(value.to_string()));
            }
            Hit::DismissDropdown => {
                self.interaction.open_dropdown = None;
                self.repaint();
            }
            Hit::Nothing => {
                // A click on empty space commits any edit in progress.
                self.stop_recording();
                self.commit_edit();
            }
            Hit::Row(row_index) => self.click_row(row_index, x, y),
        }
    }

    /// Act on a click inside a row.
    fn click_row(&mut self, row_index: usize, x: f32, y: f32) {
        // `Row` is `Copy` and the field is `'static`, so both come out of the
        // layout by value and the borrow ends here.
        let Some((field, control)) = self
            .row(row_index)
            .map(|row| (row.field, row.control))
        else {
            return;
        };
        let parts = controls::parts(field, control, &self.metrics);
        let Some(part) = parts.part_at(x, y) else {
            self.stop_recording();
            self.commit_edit();
            return;
        };

        match (field.kind, part) {
            (Kind::Switch, _) => {
                let mut config = self.config.clone();
                if access::toggle(&mut config, field.path).is_some() {
                    self.commit(config);
                }
            }
            (Kind::Slider(slider), controls::Part::SliderTrack) => {
                if let Some(fraction) = parts.slider_fraction(x) {
                    let value = slider.min + (slider.max - slider.min) * fraction as f64;
                    self.drag_row = Some(row_index);
                    self.interaction.dragging_row = Some(row_index);
                    unsafe {
                        SetCapture(self.hwnd);
                    }
                    self.write_value(row_index, Value::Float(value));
                }
            }
            (Kind::Select(choices), _) => {
                self.interaction.open_dropdown = Some(row_index);
                // Start on the value the row holds, so the list opens where the
                // user already is rather than at the top of it.
                self.interaction.dropdown_highlight = choices
                    .iter()
                    .position(|choice| {
                        Some(choice.value)
                            == crate::access::read(&self.config, field.path)
                                .as_ref()
                                .and_then(|value| value.as_text())
                    })
                    .unwrap_or(0);
                self.repaint();
            }
            (Kind::Hotkey, _) => self.begin_recording(row_index),
            // Every editable box puts the caret in the text, so the value can be
            // corrected by hand as well as dragged.
            (Kind::Color, _) | (Kind::Number { .. }, _) | (Kind::Text { .. }, _) => {
                self.begin_edit(row_index)
            }
            (Kind::Action(buttons), controls::Part::Button(index)) => {
                if let Some(button) = buttons.get(index) {
                    let action = button.action;
                    self.run_action(action);
                }
            }
            _ => {}
        }
    }

    /// Drag a slider to `x`.
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
    fn switch_section(&mut self, section: &'static Section) {
        if self.active.id == section.id {
            return;
        }
        self.stop_recording();
        self.commit_edit();
        self.active = section;
        self.scroll = 0.0;
        self.interaction.open_dropdown = None;
        self.interaction.hover_row = None;
        self.interaction.hover_part = None;
        self.repaint();
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
        self.repaint();
    }

    /// Put the caret in a row's text box, with its current value selected.
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
        self.repaint();
    }

    /// Arm a hotkey field: the next combination pressed goes into it.
    fn begin_recording(&mut self, row_index: usize) {
        self.commit_edit();
        self.interaction.recording = Some(row_index);
        self.interaction.recording_text = "请按下组合键…".to_string();
        self.repaint();
    }

    /// Stop recording, leaving the stored binding as it was.
    fn stop_recording(&mut self) {
        if self.interaction.recording.take().is_some() {
            self.interaction.recording_text.clear();
        }
    }

    /// Handle a key press while a hotkey field is armed.
    ///
    /// The modifiers come from the keyboard state, not from the message:
    /// `WM_KEYDOWN` reports the key that changed, not the ones already held, and
    /// a combination is built from both.
    fn record_key(&mut self, key: u32) {
        use beautify_core::hotkey::{is_modifier_key, Binding, Modifier};

        let held = |virtual_key: i32| unsafe { GetKeyState(virtual_key) } < 0;
        let modifiers = Modifiers::NONE
            .with(Modifier::Control, held(VK_CONTROL.0 as i32))
            .with(Modifier::Alt, held(VK_MENU.0 as i32))
            .with(Modifier::Shift, held(VK_SHIFT.0 as i32))
            .with(Modifier::Win, held(VK_LWIN.0 as i32) || held(VK_RWIN.0 as i32));

        let Some(row_index) = self.interaction.recording else {
            return;
        };

        // A modifier on its own begins a combination; it is not one.
        if is_modifier_key(key) {
            self.interaction.recording_text = modifiers.prefix();
            self.repaint();
            return;
        }
        if key == VK_ESCAPE.0 as u32 {
            self.stop_recording();
            self.repaint();
            return;
        }
        if key == VK_BACK.0 as u32 || key == VK_DELETE.0 as u32 {
            // Clearing is how a hotkey is unregistered, so it must be reachable
            // without editing text.
            self.stop_recording();
            self.write_value(row_index, Value::Text(String::new()));
            return;
        }

        let binding = Binding::new(modifiers, key);
        if !binding.is_usable() {
            // A bare letter or digit would swallow that key everywhere.
            self.interaction.recording_text = "需要 Ctrl / Alt / Shift / Win".to_string();
            self.repaint();
            return;
        }
        self.stop_recording();
        self.write_value(row_index, Value::Text(binding.to_string()));
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
                    self.repaint();
                    return;
                }
                Value::Text(trimmed)
            }
            Kind::Number { min, max, .. } => match text.trim().parse::<f64>() {
                // The config's own range, not the float `clamp` in `geom`.
                Ok(number) => {
                    Value::Integer(number.clamp(min as f64, max as f64).round() as i64)
                }
                Err(_) => {
                    self.repaint();
                    return;
                }
            },
            _ => Value::Text(text.trim().to_string()),
        };
        self.write_value(row_index, value);
    }

    /// Run an action row button.
    fn run_action(&mut self, action: ActionId) {
        // Any edit closes first, so the config the action sees is the one on
        // screen.
        self.commit_edit();
        if action == ActionId::Quit {
            self.close();
        }
        self.host.action(action);
        // An action can change the config or the values the 关于 page reports
        // (testing a lyric provider re-saves it and leaves a note), so
        // everything the host owns is re-read.
        self.refresh();
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
        let max = layout.scroll_max;
        self.scroll = clamp(self.scroll - notches * step, 0.0, max);
        self.repaint();
    }

    /// A key press.
    fn key(&mut self, key: u32, shift: bool, control: bool) {
        // A recorder consumes everything while it is armed: Escape there means
        // "stop recording", not "close the window".
        if self.interaction.recording.is_some() {
            self.record_key(key);
            return;
        }
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
                    self.repaint();
                }
                0x28 => {
                    self.interaction.dropdown_highlight =
                        (self.interaction.dropdown_highlight + 1).min(count.saturating_sub(1));
                    self.repaint();
                }
                0x0D => {
                    let value = match self.row(row_index).map(|row| row.field.kind) {
                        Some(Kind::Select(choices)) => choices
                            .get(self.interaction.dropdown_highlight)
                            .map(|choice| choice.value.to_string()),
                        _ => None,
                    };
                    self.interaction.open_dropdown = None;
                    match value {
                        Some(value) => self.write_value(row_index, Value::Text(value)),
                        None => self.repaint(),
                    }
                }
                0x1B => {
                    self.interaction.open_dropdown = None;
                    self.repaint();
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
            // Backspace.
            0x08 => {
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
            // Ctrl+V. Only the first line is taken: these fields hold a single
            // accelerator or path, and pasting a paragraph into one is never
            // what was meant.
            0x56 if control => {
                if let Some(text) = self.host.paste_text() {
                    self.interaction
                        .editing
                        .push_str(text.trim().lines().next().unwrap_or(""));
                }
            }
            // Ctrl+C copies the field's contents, so a path can be taken away.
            0x43 if control => {
                let text = self.interaction.editing.clone();
                self.host.copy_text(&text);
                return;
            }
            _ => return,
        }
        self.repaint();
    }

    /// A typed character, for the focused text field.
    fn character(&mut self, ch: char) {
        if self.interaction.focused_row.is_none() {
            return;
        }
        // Control characters are not text; Enter and Escape arrive as keys.
        if ch.is_control() {
            return;
        }
        self.interaction.editing.push(ch);
        self.repaint();
    }

    fn minimize(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWMINIMIZED);
        }
    }

    fn close(&mut self) {
        // Drop the device-bound resources while the window still exists: a
        // Direct2D render target refers to its HWND.
        self.painter = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    /// Answer one message. `None` means "not handled", and the caller passes it
    /// to `DefWindowProc`.
    fn handle(&mut self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match message {
            WM_GETMINMAXINFO => {
                let info = lparam.0 as *mut MINMAXINFO;
                let scale = self.metrics.scale;
                if !info.is_null() {
                    unsafe {
                        (*info).ptMinTrackSize.x = (MIN_WIDTH as f32 * scale) as i32;
                        (*info).ptMinTrackSize.y = (MIN_HEIGHT as f32 * scale) as i32;
                    }
                }
                Some(LRESULT(0))
            }
            WM_NCHITTEST => {
                // Edges resize, the title bar strip drags, everything else is
                // client area.
                let mut point = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut point);
                }
                let (width, height) = self.client_size();
                let (width, height) = (width as i32, height as i32);
                let edge = self.metrics.px(6.0) as i32;
                let left = point.x < edge;
                let right = point.x >= width - edge;
                let top = point.y < edge;
                let bottom = point.y >= height - edge;
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
                        let titlebar_bottom = self
                            .layout
                            .as_ref()
                            .map(|layout| layout.titlebar.bottom)
                            .unwrap_or(0.0);
                        let on_button = self
                            .layout
                            .as_ref()
                            .map(|layout| {
                                paint::window_buttons(layout, &self.metrics).iter().any(|button| {
                                    button.rect.contains(point.x as f32, point.y as f32)
                                })
                            })
                            .unwrap_or(false);
                        if (point.y as f32) < titlebar_bottom && !on_button {
                            HTCAPTION
                        } else {
                            HTCLIENT
                        }
                    }
                };
                Some(LRESULT(hit as isize))
            }
            WM_SETCURSOR => {
                // Only the client area is ours; the edges keep the resize
                // arrows Windows draws for them.
                if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                    let clickable = self.interaction.hover_part.is_some()
                        || self.interaction.hover_nav.is_some()
                        || self.interaction.hover_window.is_some();
                    set_cursor(clickable);
                    return Some(LRESULT(1));
                }
                None
            }
            WM_SIZE => {
                // The render target is resized and the page re-laid out; the
                // paint that follows draws it.
                self.repaint();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.metrics = Metrics::new(unsafe { GetDpiForWindow(self.hwnd) });
                // A Direct2D target belongs to the DPI it was created for.
                self.painter = Painter::new().ok();
                let suggested = lparam.0 as *const RECT;
                if !suggested.is_null() {
                    let rect = unsafe { *suggested };
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            None,
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            SWP_NOZORDER,
                        );
                    }
                }
                self.repaint();
                Some(LRESULT(0))
            }
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
            // Everything the target draws is opaque, so erasing first would only
            // cause a flicker.
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_MOUSEMOVE => {
                let (x, y) = point_of(lparam);
                if self.drag_row.is_some() {
                    self.drag_to(x);
                } else if self.update_hover(x, y, true) {
                    self.repaint();
                }
                if !self.tracking_leave {
                    // Asked for once per entry: without it the window never
                    // learns that the pointer left, and the last hovered control
                    // stays lit.
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
                if self.drag_row.take().is_some() {
                    self.interaction.dragging_row = None;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let notches = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                self.scroll_by(notches);
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                self.key(wparam.0 as u32, shift, control);
                Some(LRESULT(0))
            }
            WM_CHAR => {
                if let Some(ch) = char::from_u32(wparam.0 as u32) {
                    self.character(ch);
                }
                Some(LRESULT(0))
            }
            WM_APP_REFRESH => {
                self.refresh();
                Some(LRESULT(0))
            }
            WM_APP_CLOSE | WM_CLOSE => {
                self.close();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// Create the window and pump its messages until it closes. Blocks the caller.
fn run(host: Arc<dyn Host>, slot: Arc<AtomicIsize>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        // Per-monitor v2 so the metrics below are the real ones. The widget bar
        // usually sets this process-wide already, and the "already set" failure
        // is not interesting.
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

    // The design constants are logical pixels; the window is created in physical
    // ones. Without this the page opens at 80% of its intended size at 125% DPI
    // and the two columns look cramped for no visible reason.
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let width = (DEFAULT_WIDTH as f32 * scale).round() as i32;
    let height = (DEFAULT_HEIGHT as f32 * scale).round() as i32;

    // Created hidden: nothing is drawn until the layout and the render target
    // are ready, and showing a window that cannot paint itself yet shows a white
    // rectangle.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("WinBeautify 设置"),
            // Deliberately no `WS_CAPTION`: a caption is another title bar on
            // top of the one painted here, and Windows computes its height while
            // creating the window, before anything could correct it.
            // `WS_THICKFRAME` is what keeps the resize borders and the drop
            // shadow, and `WS_MAXIMIZEBOX` what makes the painted title bar's
            // double-click work.
            WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;

    let mut window = Window::new(host, hwnd);
    window.painter = Painter::new().ok();
    if window.painter.is_none() {
        tracing::error!("Direct2D is unavailable; the settings page cannot be drawn");
    }
    window.refresh();
    PUMP.with(|pump| *pump.borrow_mut() = Some(window));
    slot.store(hwnd.0 as isize, Ordering::Release);

    unsafe {
        // Rounded corners are the shell's job; asking DWM keeps them right at
        // every DPI and across theme changes.
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
            let _ = DispatchMessageW(&message);
        }
    }

    PUMP.with(|pump| *pump.borrow_mut() = None);
    slot.store(0, Ordering::Release);
    tracing::debug!("settings window closed");
    Ok(())
}

/// The work area of the monitor a maximised window is on.
///
/// "No non-client area" means the client is the whole window, and Windows sizes
/// a maximised window to the *monitor* — so without this it would cover the
/// taskbar. `None` when the window is not maximised, which is the signal to
/// leave the proposed rectangle alone.
fn maximised_work_area(hwnd: HWND) -> Option<RECT> {
    if !unsafe { IsZoomed(hwnd) }.as_bool() {
        return None;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then_some(info.rcWork)
}

/// The `NCCALCSIZE_PARAMS` form of the same adjustment.
fn clamp_maximised_to_work_area(hwnd: HWND, lparam: LPARAM) {
    let Some(work) = maximised_work_area(hwnd) else {
        return;
    };
    let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
    if !params.is_null() {
        unsafe { (*params).rgrc[0] = work };
    }
}

thread_local! {
    static PUMP: std::cell::RefCell<Option<Window>> = const { std::cell::RefCell::new(None) };
}

/// Borrow the thread-local window, if it can be borrowed.
///
/// # Why this cannot simply `borrow_mut`
///
/// Windows sends several messages *synchronously from inside a handler*:
/// `DestroyWindow` delivers `WM_NCDESTROY` before it returns, `ShowWindow`
/// delivers `WM_SIZE` and `WM_WINDOWPOSCHANGED`, and `SetWindowPos` does the
/// same. Each of those re-enters `window_proc` while the handler that caused it
/// still holds this borrow, and a second `borrow_mut` panics — inside an
/// `extern "system"` function, where a panic cannot unwind and therefore aborts
/// the whole process. Closing the window is enough to trigger it, so it is not a
/// theoretical hazard.
///
/// A re-entrant message is answered by `DefWindowProc` instead. That is the right
/// answer, not just a safe one: the outer handler is mid-way through changing the
/// state, and the nested message is a notification of that change.
///
/// The panic guard around the handler is the same idea one level up: a window
/// procedure is the one place where a bug in the drawing code would take the
/// whole daemon down, and this window is a convenience, not the product.
fn with_window<R>(f: impl FnOnce(&mut Window) -> R) -> Option<R> {
    PUMP.with(|pump| {
        let mut slot = pump.try_borrow_mut().ok()?;
        let window = slot.as_mut()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(window))) {
            Ok(result) => Some(result),
            Err(_) => {
                tracing::error!(
                    "the settings window hit a bug while handling a message;                      that message was skipped"
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

/// Set the mouse cursor for the whole window.
fn set_cursor(clickable: bool) {
    unsafe {
        let id = if clickable { IDC_HAND } else { IDC_ARROW };
        if let Ok(cursor) = LoadCursorW(None, id) {
            SetCursor(Some(cursor));
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Answered without the window state on purpose: `DestroyWindow` runs inside
    // the handler for `WM_CLOSE`, so this message arrives while a `&mut Window`
    // borrow is still live.
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }

    // Removing the non-client area is answered without the window state, because
    // it arrives while the window is being created — before the state exists.
    //
    // Both forms have to be answered, and they are not interchangeable: wParam is
    // 0 while the window is being created (lParam is the proposed client rect)
    // and non-zero when it is resized (lParam is `NCCALCSIZE_PARAMS`). Answering
    // only the second is what leaves a system title bar sitting above the one
    // painted here, with the client area pushed down by its height.
    if message == WM_NCCALCSIZE {
        if wparam.0 != 0 {
            clamp_maximised_to_work_area(hwnd, lparam);
        } else if let Some(work) = maximised_work_area(hwnd) {
            let proposed = lparam.0 as *mut RECT;
            if !proposed.is_null() {
                unsafe { *proposed = work };
            }
        }
        return LRESULT(0);
    }

    match with_window(|window| window.handle(message, wparam, lparam)) {
        Some(Some(result)) => result,
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that answers with the defaults and does nothing.
    struct Stub;

    impl Host for Stub {
        fn config(&self) -> Config {
            Config::default()
        }
        fn system_is_light(&self) -> bool {
            false
        }
        fn update(&self, config: Config) -> Config {
            config
        }
        fn status(&self) -> StatusText {
            StatusText::default()
        }
        fn action(&self, _action: ActionId) {}
        fn copy_text(&self, _text: &str) {}
        fn paste_text(&self) -> Option<String> {
            None
        }
    }

    /// A stub host, do nothing, and answer with nothing.
    fn wait_for_open(window: &SettingsWindow, open: bool) -> bool {
        for _ in 0..40 {
            if window.is_open() == open {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// Open, close, open again — on a real desktop.
    ///
    /// This is the test that would have caught the crash that made closing the
    /// window take the whole process down: `DestroyWindow` delivers
    /// `WM_NCDESTROY` from inside the handler that called it, and the second
    /// entry into the message procedure hit an already-borrowed `RefCell`.
    /// Reopening is in here too, because the class is registered once per
    /// process and the second window has to cope with that failing.
    #[test]
    #[ignore = "opens real windows; run with --ignored on a desktop session"]
    fn the_window_opens_closes_and_reopens() {
        let window = SettingsWindow::new(Arc::new(Stub));
        window.open();
        assert!(wait_for_open(&window, true), "the window never appeared");

        window.close();
        assert!(
            wait_for_open(&window, false),
            "the window did not go away when asked"
        );

        window.open();
        assert!(
            wait_for_open(&window, true),
            "the window could not be opened a second time"
        );
        window.close();
        assert!(wait_for_open(&window, false));
    }

    #[test]
    fn the_window_has_sane_limits() {
        assert!(MIN_WIDTH > 400, "the page cannot lay out narrower than this");
        assert!(MIN_HEIGHT > 300);
        assert!(DEFAULT_WIDTH > MIN_WIDTH && DEFAULT_HEIGHT > MIN_HEIGHT);
    }

    #[test]
    fn the_settings_class_name_is_namespaced() {
        // Other code finds the window by class name, and a bare "Settings"
        // would collide with any other window on the machine.
        let expected: Vec<u16> = "WinBeautify.Settings\0".encode_utf16().collect();
        let actual = unsafe { std::slice::from_raw_parts(CLASS_NAME.as_ptr(), expected.len()) };
        assert_eq!(actual, expected.as_slice());
    }
}
