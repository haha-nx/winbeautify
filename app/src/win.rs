//! Window creation and placement.
//!
//! Two windows off the webview renderer, both created from Rust rather than
//! `tauri.conf.json` so their styles can depend on the live configuration:
//!
//! * `widget` — the launcher + adaptive audio component, floating over the
//!   taskbar. Created at startup, lives for the whole session.
//! Only the widget bar's *webview fallback* is a window here: the flyout panel
//! and the settings centre are drawn natively, by [`beautify_flyout`] and
//! [`beautify_settings`] respectively, and each owns its own window.
//!
//! The settings window is *not* here: it is drawn natively by
//! [`beautify_settings`], which owns its own window, and is reached through
//! [`crate::settings`]. The webview settings page it replaced is gone.

use beautify_core::config::{Config, WidgetAnchor, WidgetRenderer};
use beautify_core::geometry::Rect;
use beautify_taskbar::shell;
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::state::AppState;

pub const WIDGET: &str = "widget";

/// Vertical inset of the widget bar inside the taskbar rect, in physical px.
const WIDGET_VERTICAL_INSET: i32 = 3;
/// Gap between the widget bar and the flyout.
const FLYOUT_GAP: i32 = 6;
/// Height used when the bar is anchored to the work area rather than the taskbar.
const FREESTANDING_HEIGHT: i32 = 40;
/// Smallest sensible bar height, so a tiny taskbar cannot produce a 0-px window.
const MIN_WIDGET_HEIGHT: i32 = 26;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Where the widget bar should sit for the current configuration.
///
/// `width` comes from the webview, which is the only side that can measure the
/// lyric text. Everything else is resolved here.
pub fn widget_geometry(config: &Config, width: i32) -> Geometry {
    let width = width.max(80);
    let offset_x = config.widget.offset_x;
    let offset_y = config.widget.offset_y;
    let margin = config.widget.margin.max(0);

    if config.widget.anchor.is_taskbar() {
        if let Some(bar) = shell::primary_taskbar().and_then(shell::window_rect) {
            let height = (bar.height() - 2 * WIDGET_VERTICAL_INSET).max(MIN_WIDGET_HEIGHT);
            let y = bar.top + WIDGET_VERTICAL_INSET + offset_y;

            // The right edge of the usable area is the notification area, not
            // the taskbar's own right edge — otherwise the bar sits underneath
            // the clock.
            let usable_left = bar.left;
            let usable_right = shell::tray_rect()
                .map(|t| t.left)
                .filter(|left| *left > bar.left && *left <= bar.right)
                .unwrap_or(bar.right);

            let x = match config.widget.anchor {
                WidgetAnchor::TaskbarLeft => usable_left + margin,
                WidgetAnchor::TaskbarCenter => {
                    usable_left + ((usable_right - usable_left) - width) / 2
                }
                // Grow leftwards away from the tray.
                _ => usable_right - margin - width,
            };
            return Geometry {
                x: x + offset_x,
                y,
                width,
                height,
            };
        }
        // No taskbar (Explorer restarting): fall through to the work area.
    }

    let host = shell::primary_taskbar();
    let work = host
        .and_then(shell::work_area_of)
        .or_else(|| host.and_then(shell::monitor_rect_of))
        .unwrap_or(Rect::new(0, 0, 1920, 1080));
    let height = FREESTANDING_HEIGHT;
    let y = work.bottom - margin - height + offset_y;
    let x = match config.widget.anchor {
        WidgetAnchor::BottomLeft => work.left + margin,
        WidgetAnchor::BottomCenter => work.left + (work.width() - width) / 2,
        _ => work.right - margin - width,
    };
    Geometry {
        x: x + offset_x,
        y,
        width,
        height,
    }
}

/// Place the flyout next to the widget bar, flipping above it when there is not
/// enough room below.
pub fn flyout_position(config: &Config, bar: Rect, flyout: (i32, i32)) -> (i32, i32) {
    let (width, height) = flyout;
    let widget = Geometry {
        x: bar.left,
        y: bar.top,
        width: bar.width(),
        height: bar.height(),
    };
    // Match the bar's growth direction: a bar hugging the right edge gets a
    // flyout aligned to its right edge, and vice versa.
    let screen_center = widget.x + widget.width / 2;
    let monitor_width = shell::primary_taskbar()
        .and_then(shell::monitor_rect_of)
        .map(|m| m.width())
        .unwrap_or(1920);
    let right_aligned = screen_center > monitor_width / 2;

    let mut x = if right_aligned {
        widget.x + widget.width - width
    } else {
        widget.x
    };

    let monitor = shell::primary_taskbar()
        .and_then(shell::monitor_rect_of)
        .unwrap_or(Rect::new(0, 0, monitor_width, 1080));
    x = x.clamp(monitor.left + 4, (monitor.right - width - 4).max(monitor.left + 4));

    let below = widget.y + widget.height + FLYOUT_GAP;
    let above = widget.y - FLYOUT_GAP - height;
    let y = if config.widget.flyout_flip && below + height > monitor.bottom {
        above.max(monitor.top + 4)
    } else {
        below
    };

    (x, y)
}

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

/// Does this build support DWM system backdrops (`DWMSBT_ACRYLIC` &c.)?
///
/// Windows 11 22H2 (build 22621) and later. Anything older needs the legacy
/// blur-behind path, because a translucent webview over a plain window would
/// show straight through to the desktop instead of to a blurred backdrop.
fn supports_dwm_backdrop() -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        beautify_taskbar::winver::build_number()
            .map(|build| build >= 22_621)
            .unwrap_or(false)
    })
}

/// Options that turn a borderless window into a floating, translucent panel.
///
/// # Why not simply `transparent(true)`
///
/// Tauri implements `transparent(true)` on Windows through tao, which calls
/// `DwmEnableBlurBehindWindow` with an empty blur region — the Vista-era
/// "glass" trick. That puts the window into a composition mode where the
/// WebView2 surface's alpha is not handled consistently: on machines without a
/// full D3D stack the content either vanishes entirely or composites at a
/// fraction of its intended opacity.
///
/// Asking DWM for a *system backdrop* instead, and telling the webview its own
/// background is transparent, produces the same visual result through the
/// supported path — and gets the corner rounding and backdrop blur for free.
fn apply_panel_style(builder: WebviewWindowBuilder<'_, tauri::Wry, AppHandle>, radius: f64) -> WebviewWindowBuilder<'_, tauri::Wry, AppHandle> {
    let builder = builder.decorations(false).shadow(false);
    if supports_dwm_backdrop() {
        builder
            .transparent(false)
            .background_color(tauri::webview::Color(0, 0, 0, 0))
            .effects(
                tauri::window::EffectsBuilder::new()
                    .effect(tauri::window::Effect::Acrylic)
                    .radius(radius)
                    .build(),
            )
    } else {
        builder.transparent(true)
    }
}

/// Create the widget bar if it does not exist yet.
pub fn ensure_widget(app: &AppHandle) -> tauri::Result<Option<WebviewWindow>> {
    if let Some(existing) = app.get_webview_window(WIDGET) {
        return Ok(Some(existing));
    }
    let state = app.state::<std::sync::Arc<AppState>>();
    let config = state.config.get();
    // The native renderer owns the bar itself; there is no webview to create.
    if config.widget.renderer == WidgetRenderer::Native {
        return Ok(None);
    }
    if !config.widget.enabled || !config.any_widget_source() {
        return Ok(None);
    }

    let geometry = widget_geometry(&config, 220);
    let builder = WebviewWindowBuilder::new(app, WIDGET, WebviewUrl::App("widget.html".into()))
        .title("WinBeautify 小组件")
        .inner_size(geometry.width as f64, geometry.height as f64)
        .position(geometry.x as f64, geometry.y as f64)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        // Never take focus: the widget bar sits on the taskbar and stealing the
        // foreground would dismiss whatever menu the user just opened.
        .focused(false)
        // Built visible on purpose. The geometry is already final at this
        // point, so there is nothing to avoid showing, and a WebView2 created
        // into a hidden window can miss its first composition frame — the
        // window then exists, is visible and hit-testable, but never paints.
        .visible(true);

    let window = apply_panel_style(builder, config.widget.corner_radius as f64).build()?;

    // Belt and braces on top of `focused(false)`: WS_EX_NOACTIVATE means a
    // click on the bar cannot activate it either.
    let _ = window.set_focusable(false);
    adopt_by_taskbar(&window);
    state.widget_width.store(geometry.width, std::sync::atomic::Ordering::Release);
    window.show()?;
    Ok(Some(window))
}

/// Make the widget bar an *owned* window of the taskbar.
///
/// Two topmost windows are ordered by whoever raised last, and the shell
/// re-raises `Shell_TrayWnd` whenever it repaints — so without this the bar
/// disappears behind the taskbar the first time the clock ticks. An owned
/// window is structurally kept above its owner, which is exactly the
/// relationship a taskbar overlay needs, and it costs nothing to maintain.
///
/// Re-applied on every reposition, because Explorer restarts replace the
/// taskbar window and leave the old owner handle dangling.
fn adopt_by_taskbar(window: &WebviewWindow) {
    let Ok(handle) = window.hwnd() else {
        return;
    };
    let Some(taskbar) = shell::primary_taskbar() else {
        return;
    };
    // Tauri links its own copy of the `windows` crate, so the handle is
    // translated through the raw pointer instead of relying on the two HWND
    // types unifying.
    let hwnd = windows::Win32::Foundation::HWND(handle.0);

    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowLongPtrW, SetWindowPos, GWLP_HWNDPARENT, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE,
    };
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, taskbar.0 as isize);
        // Re-stack so the new owner relationship takes effect immediately.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Where the visible bar is, whichever renderer is drawing it.
///
/// The native bar reports its own geometry; the webview bar is positioned by
/// [`widget_geometry`], so the rect is derived from the same numbers.
pub fn bar_rect(app: &AppHandle, config: &Config) -> Rect {
    let state = app.state::<std::sync::Arc<AppState>>();
    if config.widget.renderer == WidgetRenderer::Native {
        if let Some(rect) = state.widget.rect() {
            return rect;
        }
    }
    let geometry = widget_geometry(config, current_widget_width(app));
    Rect::new(
        geometry.x,
        geometry.y,
        geometry.x + geometry.width,
        geometry.y + geometry.height,
    )
}

/// Show the panel on `tab`.
///
/// The panel is a native window of this process now; its placement is still
/// resolved here, because that is the one place that knows the taskbar geometry
/// and the configured anchor.
pub fn show_flyout(app: &AppHandle, tab: beautify_core::model::FlyoutTab) -> tauri::Result<()> {
    crate::flyout::show(app, tab);
    Ok(())
}

pub fn hide_flyout(app: &AppHandle) {
    crate::flyout::hide(app);
}

pub fn current_widget_width(app: &AppHandle) -> i32 {
    app.state::<std::sync::Arc<AppState>>()
        .widget_width
        .load(std::sync::atomic::Ordering::Acquire)
}

/// Move and resize the widget bar to match the configuration and `width`.
pub fn reposition_widget(app: &AppHandle, width: i32) {
    let state = app.state::<std::sync::Arc<AppState>>();
    let config = state.config.get();
    if state.is_shutting_down() {
        return;
    }
    // The native renderer places its own window; there is nothing to move here.
    if config.widget.renderer == WidgetRenderer::Native {
        return;
    }
    let Some(window) = app.get_webview_window(WIDGET) else {
        return;
    };
    if !config.widget.enabled || !config.any_widget_source() {
        let _ = window.hide();
        return;
    }

    // Respect auto-hide: a retracted taskbar has no room for the bar.
    if config.widget.hide_with_autohide && shell::is_autohide_enabled() {
        let retracted = shell::primary_taskbar()
            .and_then(|bar| {
                let rect = shell::window_rect(bar)?;
                let monitor = shell::monitor_rect_of(bar)?;
                Some(rect.top >= monitor.bottom)
            })
            .unwrap_or(false);
        if retracted {
            let _ = window.hide();
            return;
        }
    }

    adopt_by_taskbar(&window);

    let geometry = widget_geometry(&config, width);
    state
        .widget_width
        .store(geometry.width, std::sync::atomic::Ordering::Release);
    let _ = window.set_size(PhysicalSize::new(geometry.width as u32, geometry.height as u32));
    let _ = window.set_position(PhysicalPosition::new(geometry.x, geometry.y));
    if !window.is_visible().unwrap_or(false) {
        let _ = window.show();
    }
}
