//! The widget bar's window and message pump.
//!
//! Runs on its own thread, like the taskbar and clipboard modules, because
//! mouse handling and window ownership need a thread that pumps messages. The
//! host never has to care.
//!
//! The window is created once at its widest possible size and never resized:
//! only the *pill inside it* animates. That is possible because a layered
//! window hit-tests by alpha, so the transparent margin around the pill passes
//! clicks straight through to the taskbar. Parking a wider window over the
//! taskbar therefore costs nothing and removes every resize/animation race.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use beautify_core::config::AnchorAlign;
use beautify_core::geometry::Rect;
use beautify_taskbar::shell;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    KillTimer, PostQuitMessage, RegisterClassExW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow,
    TranslateMessage, GWLP_HWNDPARENT, HWND_TOPMOST, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_SHOWNOACTIVATE, WM_APP, WM_CLOSE, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_TIMER,
    WNDCLASSEXW, WINDOW_EX_STYLE, WS_POPUP, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::MA_NOACTIVATE;

use crate::layout::{self, Align, Hit, Metrics, Transport, WIDTH_EPSILON};
use crate::paint::Painter;
use crate::state::WidgetState;
use crate::surface::LayeredSurface;
use crate::{Shared, TransportAction, WidgetAction};

/// Posted by the host when the displayed state changed.
pub const WM_APP_DIRTY: u32 = WM_APP + 41;
/// Posted by the host to tear the pump down.
pub const WM_APP_SHUTDOWN: u32 = WM_APP + 42;

/// `WM_MOUSELEAVE` from `winuser.h`. Declared here because `windows-rs` files it
/// under the controls module, which this crate otherwise has no use for.
const WM_MOUSELEAVE: u32 = 0x02A3;

const WINDOW_CLASS: PCWSTR = w!("WinBeautify.Widget");
/// Animation tick while the bar's width is settling.
const TIMER_ANIMATE: usize = 1;
/// Safety net for taskbar moves we receive no event for.
const TIMER_SAFETY: usize = 2;
const ANIMATE_MS: u32 = 16;
const SAFETY_MS: u32 = 1500;
/// How much of the width transition is still left after `animation_ms`.
/// Used to derive the per-tick easing from the configured duration.
const ANIMATE_REMAINDER: f32 = 0.05;
const ANIMATE_FALLBACK_EASING: f32 = 0.28;
/// Bar height used when the widget is anchored to the work area, in 96-DPI px.
const FREESTANDING_HEIGHT: f32 = 40.0;

/// Where the window should be for the current taskbar and configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    align: Align,
    dpi: u32,
}

impl Geometry {
    /// The window rectangle in **window-local** coordinates.
    ///
    /// `layout()` works in the same space the renderer draws in — origin at the
    /// window's top-left — while `x`/`y` are screen coordinates used only for
    /// `UpdateLayeredWindow`. Feeding screen coordinates to `layout()` makes
    /// every hit test compare a local point against a screen-space pill, which
    /// silently breaks hover, clicks and the flyout anchor all at once.
    fn window(&self) -> layout::Rect {
        layout::Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

}

fn alignment(axis: AnchorAlign) -> Align {
    match axis {
        AnchorAlign::Start => Align::Start,
        AnchorAlign::Center => Align::Center,
        AnchorAlign::End => Align::End,
    }
}

/// Resolve the window rectangle from the configuration and the live taskbar.
fn compute_geometry(state: &WidgetState) -> Option<Geometry> {
    let cfg = &state.config;
    let taskbar = shell::primary_taskbar()?;
    let bar = shell::window_rect(taskbar)?;
    let dpi = unsafe { GetDpiForWindow(taskbar) }.max(96);
    let metrics = Metrics::new(dpi);
    let inset = metrics.inset();
    let margin = cfg.widget.margin;
    let offset_x = cfg.widget.offset_x;
    let offset_y = cfg.widget.offset_y;

    let content = state.content();
    let limits = state.limits();
    let widest = layout::max_bar_width(&content, &limits, &metrics);
    let width = (widest + 2.0 * inset).round().max(1.0) as u32;
    let (horizontal, _) = cfg.widget.anchor.alignment();
    let align = alignment(horizontal);

    if cfg.widget.anchor.is_taskbar() {
        let usable_left = bar.left + margin;
        // The notification area is the real right-hand boundary, not the
        // taskbar's own edge: the bar must not sit under the clock.
        let usable_right = shell::tray_rect()
            .map(|t| t.left - margin)
            .filter(|right| *right > bar.left)
            .unwrap_or(bar.right - margin);

        let anchor_x = match align {
            Align::Start => usable_left,
            Align::Center => usable_left + (usable_right - usable_left) / 2,
            Align::End => usable_right,
        };

        return Some(Geometry {
            x: window_origin(anchor_x, width as i32, inset, align) + offset_x,
            y: bar.top + offset_y,
            width,
            height: bar.height().max(1) as u32,
            align,
            dpi,
        });
    }

    // Anchored to the work area rather than to the taskbar.
    let work = shell::work_area_of(taskbar).or_else(|| shell::monitor_rect_of(taskbar))?;
    let height = ((FREESTANDING_HEIGHT * metrics.scale) + 2.0 * inset).round().max(1.0) as u32;
    let anchor_x = match align {
        Align::Start => work.left + margin,
        Align::Center => work.left + work.width() / 2,
        Align::End => work.right - margin,
    };

    Some(Geometry {
        x: window_origin(anchor_x, width as i32, inset, align) + offset_x,
        y: work.bottom - margin - height as i32 + offset_y,
        width,
        height,
        align,
        dpi,
    })
}

/// Per-tick easing that covers `animation_ms` worth of travel in ~95%.
///
/// The animation runs off a 16 ms timer rather than a real clock, so the
/// configured duration has to be converted into a per-tick factor.
fn easing_for(animation_ms: u32) -> f32 {
    let duration = animation_ms.max(1) as f32;
    let ticks = (duration / ANIMATE_MS as f32).max(1.0);
    let easing = 1.0 - ANIMATE_REMAINDER.powf(1.0 / ticks);
    easing.clamp(0.02, 0.9)
}

/// Window origin that puts the pill's anchor edge at `anchor_x`.
fn window_origin(anchor_x: i32, width: i32, inset: f32, align: Align) -> i32 {
    let inset = inset.round() as i32;
    match align {
        Align::Start => anchor_x - inset,
        Align::Center => anchor_x - width / 2,
        Align::End => anchor_x + inset - width,
    }
}

/// Everything the pump thread owns.
struct Pump {
    hwnd: HWND,
    shared: Arc<Shared>,
    surface: Option<LayeredSurface>,
    geometry: Option<Geometry>,
    /// Animating bar width, in physical pixels.
    bar_width: f32,
    /// Width the bar is animating towards.
    target_width: f32,
    animating: bool,
    tracking_leave: bool,
    painter: Painter,
    /// `(line, scale) -> width`. DirectWrite layout is the expensive part of a
    /// frame, and the line only changes when the track or the lyric does.
    line_cache: Option<(String, f32)>,
    lyric_width: f32,
    /// Per-tick easing of the width transition, derived from `animation_ms`.
    easing: f32,
}

impl Pump {
    /// Recompute geometry, reallocating the surface if the window changed size.
    /// Returns true when the window moved.
    fn sync_geometry(&mut self) -> bool {
        let state = self.shared.snapshot();
        let Some(geometry) = compute_geometry(&state) else {
            // No shell yet (Explorer restarting): hide rather than draw wrong.
            self.surface = None;
            self.shared.set_rect(None);
            return false;
        };

        let size_changed = self
            .geometry
            .map(|g| (g.width, g.height) != (geometry.width, geometry.height))
            .unwrap_or(true);
        let moved = self.geometry != Some(geometry);
        self.geometry = Some(geometry);

        if size_changed || self.surface.is_none() {
            match unsafe {
                LayeredSurface::new(self.hwnd, geometry.width as i32, geometry.height as i32)
            } {
                Ok(surface) => {
                    self.surface = Some(surface);
                    // `UpdateLayeredWindow` paints but never shows: without this
                    // the window is positioned, hit-testable and invisible.
                    // `SW_SHOWNOACTIVATE` keeps it from taking the foreground.
                    let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
                }
                Err(e) => {
                    tracing::error!("could not create the widget surface: {e}");
                    self.surface = None;
                    return false;
                }
            }
        }

        // Recomputed every time: a config change can widen the bar without the
        // window itself moving.
        let metrics = Metrics::new(geometry.dpi);
        let content = state.content();
        let limits = state.limits();
        let lyric = self.measure_lyric(&state, &metrics);
        self.target_width = layout::bar_width(&content, &limits, &metrics, lyric);
        self.easing = easing_for(state.config.widget.animation_ms);
        if self.bar_width <= 0.0 {
            // First frame: appear at the right width rather than growing from 0.
            self.bar_width = self.target_width;
        }
        // Publish the *pill* rect rather than the window rect: the flyout
        // anchors to the visible bar, and the window carries a transparent
        // margin that would offset it.
        let pill = self
            .layout_for(&state)
            .map(|l| l.pill)
            .unwrap_or(layout::Rect::ZERO);
        self.shared.set_rect(Some(Rect::new(
            geometry.x + pill.left.round() as i32,
            geometry.y + pill.top.round() as i32,
            geometry.x + pill.right.round() as i32,
            geometry.y + pill.bottom.round() as i32,
        )));
        moved
    }

    /// The layout for the current frame, shared by drawing and hit-testing so
    /// they can never disagree.
    fn layout_for(&self, state: &WidgetState) -> Option<layout::Layout> {
        let geometry = self.geometry?;
        let metrics = Metrics::new(geometry.dpi);
        let content = state.content();
        Some(layout::layout(
            geometry.window(),
            self.bar_width,
            geometry.align,
            &content,
            &metrics,
        ))
    }

    fn draw(&mut self) {
        let Some(geometry) = self.geometry else {
            return;
        };
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let state = self.shared.snapshot();

        let result = self.painter.render(
            &state,
            (geometry.width, geometry.height),
            geometry.dpi,
            self.bar_width,
            geometry.align,
            state.hover,
            |pixels, stride| surface.present(pixels, stride, geometry.x, geometry.y),
        );
        if let Err(e) = result {
            tracing::warn!("widget frame failed: {e}");
        }
    }

    /// Step the width animation; returns true while it is still running.
    fn animate(&mut self) -> bool {
        let delta = self.target_width - self.bar_width;
        if delta.abs() <= WIDTH_EPSILON {
            let settled = self.bar_width != self.target_width;
            self.bar_width = self.target_width;
            return settled;
        }
        self.bar_width += delta * self.easing;
        true
    }

    /// Width of the current lyric line, re-measuring only when it changed.
    fn measure_lyric(&mut self, state: &WidgetState, metrics: &Metrics) -> f32 {
        let (line, _) = state.display_line();
        if let Some((cached, width)) = self.line_cache.as_ref() {
            if cached == &line {
                self.lyric_width = *width;
                return *width;
            }
        }
        let width = self.painter.measure_line(&line, metrics.scale);
        self.line_cache = Some((line, width));
        self.lyric_width = width;
        width
    }

    /// Update the hovered element. Returns true when it changed.
    fn set_hover(&mut self, hover: Option<Hit>) -> bool {
        let mut state = self.shared.state.write();
        if state.hover == hover {
            return false;
        }
        state.hover = hover;
        true
    }

    /// Hit-test a point in window-local coordinates.
    fn hit_test(&self, x: f32, y: f32, state: &WidgetState) -> Option<Hit> {
        let geometry = self.geometry?;
        let metrics = Metrics::new(geometry.dpi);
        self.layout_for(state).and_then(|l| l.hit(&metrics, x, y))
    }

    /// Start the animation timer if the width has somewhere to go.
    fn kick_animation(&mut self) {
        if (self.target_width - self.bar_width).abs() <= WIDTH_EPSILON {
            return;
        }
        if self.animating {
            return;
        }
        self.animating = true;
        unsafe { SetTimer(Some(self.hwnd), TIMER_ANIMATE, ANIMATE_MS, None) };
    }
}

/// Run the pump until the host asks it to stop. Blocks the calling thread.
pub fn run(shared: Arc<Shared>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&class) };

    let style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(style.0),
            WINDOW_CLASS,
            w!("WinBeautify Widget"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;
    shared.hwnd.store(hwnd.0 as isize, Ordering::Release);
    adopt_by_taskbar(hwnd);

    let painter = match Painter::new() {
        Ok(painter) => painter,
        Err(e) => {
            tracing::error!("Direct2D is unavailable; the widget bar cannot be drawn: {e}");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            shared.hwnd.store(0, Ordering::Release);
            return Ok(());
        }
    };

    // The pump lives in a thread local so `wndproc` can reach it without the
    // `GWLP_USERDATA` dance; there is exactly one per pumping thread.
    let pump = Pump {
        hwnd,
        shared: Arc::clone(&shared),
        surface: None,
        geometry: None,
        bar_width: 0.0,
        target_width: 0.0,
        animating: false,
        tracking_leave: false,
        painter,
        line_cache: None,
        lyric_width: 0.0,
        easing: ANIMATE_FALLBACK_EASING,
    };
    PUMP.with(|slot| *slot.borrow_mut() = Some(pump));

    with_pump(|pump| {
        pump.sync_geometry();
        pump.draw();
    });
    unsafe { SetTimer(Some(hwnd), TIMER_SAFETY, SAFETY_MS, None) };

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        KillTimer(Some(hwnd), TIMER_ANIMATE).ok();
        KillTimer(Some(hwnd), TIMER_SAFETY).ok();
    }
    // Release the surface before the window goes away.
    PUMP.with(|slot| {
        if let Some(pump) = slot.borrow_mut().as_mut() {
            pump.surface = None;
        }
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    shared.hwnd.store(0, Ordering::Release);
    shared.set_rect(None);
    PUMP.with(|slot| *slot.borrow_mut() = None);
    tracing::debug!("widget pump stopped");
    Ok(())
}

/// Make the widget an *owned* window of the taskbar.
///
/// Two topmost windows are ordered by whoever raised last, and the shell
/// re-raises `Shell_TrayWnd` whenever it repaints. An owned window is
/// structurally kept above its owner, which is exactly the relationship a
/// taskbar overlay needs and costs nothing to maintain.
fn adopt_by_taskbar(hwnd: HWND) {
    let Some(taskbar) = shell::primary_taskbar() else {
        return;
    };
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, taskbar.0 as isize);
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

thread_local! {
    static PUMP: std::cell::RefCell<Option<Pump>> = const { std::cell::RefCell::new(None) };
}

/// Borrow the thread-local pump.
fn with_pump<R>(f: impl FnOnce(&mut Pump) -> R) -> Option<R> {
    PUMP.with(|slot| slot.borrow_mut().as_mut().map(f))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            match wparam.0 {
                TIMER_ANIMATE => {
                    let running = with_pump(|pump| pump.animate()).unwrap_or(false);
                    if running {
                        with_pump(|pump| pump.draw());
                    } else {
                        // One last frame at the settled width.
                        unsafe { KillTimer(Some(hwnd), TIMER_ANIMATE).ok() };
                        with_pump(|pump| {
                            pump.animating = false;
                            pump.draw();
                        });
                    }
                }
                TIMER_SAFETY => {
                    // Redraw unconditionally, not only when the geometry moved.
                    //
                    // A layered window is composited from the surface this
                    // process last pushed, and if that surface is lost — a
                    // full-screen topmost overlay has been and gone over it, a
                    // fullscreen app took the screen, the driver hiccuped —
                    // nothing else would ever push it again: the geometry has not
                    // changed, so the old code drew nothing and the bar stayed
                    // invisible until the process restarted. One small
                    // `UpdateLayeredWindow` every tick is the price of a bar that
                    // repairs itself.
                    with_pump(|pump| pump.sync_geometry());
                    with_pump(|pump| pump.draw());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_DIRTY => {
            with_pump(|pump| {
                pump.sync_geometry();
                pump.kick_animation();
                if !pump.animating {
                    pump.draw();
                }
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;

            let state = with_pump(|pump| pump.shared.snapshot());
            let hit = state
                .as_ref()
                .and_then(|state| with_pump(|pump| pump.hit_test(x, y, state)).flatten());
            let changed = with_pump(|pump| pump.set_hover(hit)).unwrap_or(false);

            let tracking = with_pump(|pump| pump.tracking_leave).unwrap_or(true);
            if !tracking {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut track);
                }
                with_pump(|pump| pump.tracking_leave = true);
            }
            if changed {
                with_pump(|pump| pump.draw());
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_pump(|pump| pump.tracking_leave = false);
            if with_pump(|pump| pump.set_hover(None)).unwrap_or(false) {
                with_pump(|pump| pump.draw());
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => {
            // Non-negotiable for a non-activating overlay: the default handler
            // answers MA_ACTIVATE, Windows then finds the window cannot be
            // activated, and the click that triggered it is discarded. Answering
            // MA_NOACTIVATE keeps the click and still refuses the focus.
            LRESULT(MA_NOACTIVATE as isize)
        }
        WM_LBUTTONDOWN => {
            // A layered window passes clicks through wherever the pill is
            // transparent, so any click arriving here is on the pill.
            let action = with_pump(|pump| {
                let mut point = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut point);
                }
                let geometry = pump.geometry?;
                let state = pump.shared.snapshot();
                // `GetCursorPos` is in screen coordinates; the layout is local.
                pump.hit_test(
                    (point.x - geometry.x) as f32,
                    (point.y - geometry.y) as f32,
                    &state,
                )
            })
            .flatten();

            tracing::debug!(?action, "widget click");
            let action = match action {
                Some(Hit::Launcher) => WidgetAction::ToggleFlyout,
                Some(Hit::Transport(Transport::Previous)) => {
                    WidgetAction::Transport(TransportAction::Previous)
                }
                Some(Hit::Transport(Transport::Toggle)) => {
                    WidgetAction::Transport(TransportAction::Toggle)
                }
                Some(Hit::Transport(Transport::Next)) => {
                    WidgetAction::Transport(TransportAction::Next)
                }
                // Anywhere else on the bar dismisses an open flyout, matching
                // the "click outside closes it" contract.
                _ => WidgetAction::HideFlyout,
            };
            with_pump(|pump| pump.shared.dispatch(action));
            LRESULT(0)
        }
        WM_APP_SHUTDOWN | WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
