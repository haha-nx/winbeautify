//! Shell geometry: finding the taskbars, their tray, monitors and the
//! foreground window.
//!
//! Everything here is a thin, safe wrapper over Win32. The functions are
//! deliberately cheap enough to call from an event hook.

use beautify_core::geometry::Rect;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetForegroundWindow, GetWindowLongW,
    GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    IsZoomed, RegisterWindowMessageW, GWL_EXSTYLE, GWL_STYLE, WINDOW_LONG_PTR_INDEX,
    WS_EX_TOOLWINDOW, WS_MAXIMIZE,
};

/// `TaskbarCreated` is broadcast to every top-level window when Explorer
/// restarts. The widget host registers this message so it can rebuild itself.
pub fn taskbar_created_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// The primary taskbar, `Shell_TrayWnd`. `None` while Explorer is restarting.
pub fn primary_taskbar() -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }.ok()?;
    (!hwnd.is_invalid()).then_some(hwnd)
}

/// Secondary taskbars (`Shell_SecondaryTrayWnd`), one per extra monitor that
/// shows a taskbar. The primary taskbar is always first.
pub fn all_taskbars() -> Vec<HWND> {
    let mut bars = Vec::with_capacity(4);
    if let Some(primary) = primary_taskbar() {
        bars.push(primary);
    }
    let mut prev = HWND::default();
    loop {
        let Ok(next) = (unsafe { FindWindowExW(None, Some(prev), w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) })
        else {
            break;
        };
        if next.is_invalid() {
            break;
        }
        bars.push(next);
        prev = next;
    }
    bars
}

/// Rectangle of a window in physical screen pixels.
pub fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    Some(Rect::new(r.left, r.top, r.right, r.bottom))
}

/// True when the shell has auto-hide enabled.
pub fn is_autohide_enabled() -> bool {
    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) } as u32;
    state & ABS_AUTOHIDE != 0
}

/// Rectangle of the notification area (clock/status icons) inside the primary
/// taskbar. Used to keep the widget bar from covering it.
pub fn tray_rect() -> Option<Rect> {
    let parent = primary_taskbar()?;
    let hwnd = unsafe { FindWindowExW(Some(parent), None, w!("TrayNotifyWnd"), PCWSTR::null()) }.ok()?;
    if hwnd.is_invalid() {
        return None;
    }
    window_rect(hwnd)
}

/// The window the user is currently interacting with.
pub fn foreground_window() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// Class name of a window (e.g. `"Shell_TrayWnd"`).
pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// Process that owns a window.
pub fn process_id(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn has_style(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, mask: i32) -> bool {
    let style = unsafe { GetWindowLongW(hwnd, index) };
    style & mask != 0
}

/// True when DWM has cloaked the window — i.e. it exists but is not on screen.
/// UWP apps leave a lot of these lying around, so every enumeration must skip
/// them.
pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && cloaked != 0
}

/// Visible, non-cloaked, non-tool window with a title — i.e. something the user
/// would call "an app window".
pub fn is_candidate_window(hwnd: HWND) -> bool {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return false;
    }
    if unsafe { GetWindowTextLengthW(hwnd) } == 0 {
        return false;
    }
    if has_style(hwnd, GWL_EXSTYLE, WS_EX_TOOLWINDOW.0 as i32) {
        return false;
    }
    !is_cloaked(hwnd)
}

/// DWM's idea of the visible frame, which excludes the invisible resize border
/// that `GetWindowRect` includes on Windows 10+.
pub fn extended_frame_bounds(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if ok.is_err() {
        return window_rect(hwnd);
    }
    Some(Rect::new(r.left, r.top, r.right, r.bottom))
}

/// Is any window maximised on `monitor`?
///
/// This is the test behind TranslucentTB's "dynamic mode". The area ratio
/// filters out windows that carry the maximised style while actually restored.
pub fn any_maximized_on(monitor: Rect) -> bool {
    struct Ctx {
        monitor: Rect,
        found: bool,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: `lparam` is the `&mut Ctx` we passed to `EnumWindows` below,
        // and the callback never outlives that call.
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        if ctx.found {
            return windows::core::BOOL(0);
        }
        if !is_candidate_window(hwnd) {
            return windows::core::BOOL(1);
        }
        let maximized = unsafe { IsZoomed(hwnd) }.as_bool()
            || has_style(hwnd, GWL_STYLE, WS_MAXIMIZE.0 as i32);
        if !maximized {
            return windows::core::BOOL(1);
        }
        let Some(rect) = extended_frame_bounds(hwnd) else {
            return windows::core::BOOL(1);
        };
        let area = rect.intersection_area(&ctx.monitor) as f64;
        let monitor_area = (ctx.monitor.width() as f64) * (ctx.monitor.height() as f64);
        if monitor_area > 0.0 && area / monitor_area >= 0.5 {
            ctx.found = true;
            return windows::core::BOOL(0);
        }
        windows::core::BOOL(1)
    }

    let mut ctx = Ctx {
        monitor,
        found: false,
    };
    unsafe {
        let _ = EnumWindows(
            Some(callback),
            LPARAM(&mut ctx as *mut Ctx as isize),
        );
    }
    ctx.found
}

/// Does the foreground window cover the whole monitor, and is it a real app
/// rather than the shell?
///
/// Used to suppress the taskbar accent while a game or video is fullscreen.
pub fn is_fullscreen_foreground(monitor: Rect) -> bool {
    let hwnd = foreground_window();
    if hwnd.is_invalid() {
        return false;
    }
    let class = class_name(hwnd).to_ascii_lowercase();
    if matches!(
        class.as_str(),
        "shell_traywnd"
            | "shell_secondarytraywnd"
            | "progman"
            | "workerw"
            | "windows.ui.core.corewindow"
            | "applicationframewindow"
    ) {
        return false;
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return false;
    }
    let Some(rect) = extended_frame_bounds(hwnd) else {
        return false;
    };
    rect.left <= monitor.left
        && rect.top <= monitor.top
        && rect.right >= monitor.right
        && rect.bottom >= monitor.bottom
}

/// Full monitor rectangle (not the work area) that hosts `window`.
pub fn monitor_rect_of(window: HWND) -> Option<Rect> {
    monitor_info(window).map(|m| m.0)
}

/// Work area of the monitor that hosts `window` — what is left after the
/// taskbar, in physical pixels.
pub fn work_area_of(window: HWND) -> Option<Rect> {
    monitor_info(window).map(|m| m.1)
}

/// `(monitor, work_area)` for the monitor nearest `window`.
fn monitor_info(window: HWND) -> Option<(Rect, Rect)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let full = info.rcMonitor;
    let work = info.rcWork;
    Some((
        Rect::new(full.left, full.top, full.right, full.bottom),
        Rect::new(work.left, work.top, work.right, work.bottom),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_name_of_a_null_window_is_empty() {
        assert_eq!(class_name(HWND::default()), "");
    }

    #[test]
    fn shell_queries_do_not_panic() {
        let _ = is_autohide_enabled();
        let _ = primary_taskbar();
        let _ = all_taskbars();
        let _ = tray_rect();
    }
}
