//! A full-taskbar-strip window painted with red/white bands, parked at the
//! bottom of the z-order. Blur and acrylic materials sample what is behind the
//! taskbar, and a uniform wallpaper cannot show whether anything is actually
//! being blurred — sharp bands under 透明 and a blended smear under 模糊 are
//! readable at a glance and in pixel probes.
//!
//! ```text
//! cargo run --manifest-path tools/backdrop-stripes/Cargo.toml --release
//! backdrop-stripes hide   # remove it again
//! ```

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, HBRUSH, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect,
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassExW, SetWindowPos,
    ShowWindow, TranslateMessage, DestroyWindow, HWND_BOTTOM, MSG, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_SHOW, WNDCLASSEXW, WM_PAINT,
};

/// The class proc: paints the upper half red and the lower half white on every
/// paint, entirely by hand — no brushes kept alive, no class background,
/// nothing to second-guess.
unsafe extern "system" fn class_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        let half = rect.bottom / 2;
        // COLORREF is 0x00BBGGRR: red is 0xFF in the low byte.
        let red = CreateSolidBrush(COLORREF(0x0000_00FF));
        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let top = RECT { bottom: half, ..rect };
        let bottom = RECT { top: half, ..rect };
        FillRect(hdc, &top, red);
        FillRect(hdc, &bottom, white);
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hide") {
        unsafe {
            let existing = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
                w!("WBBackdropStripes"),
                windows::core::PCWSTR::null(),
            )
            .unwrap_or_default();
            if !existing.is_invalid() {
                let _ = DestroyWindow(existing);
            }
            println!("hidden (or never up)");
        }
        return;
    }

    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(class_proc),
            lpszClassName: w!("WBBackdropStripes"),
            hInstance: instance.into(),
            ..Default::default()
        };
        RegisterClassExW(&class);

        // WS_POPUP | WS_VISIBLE
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("WBBackdropStripes"),
            w!("WBBackdropStripes"),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0x9000_0000),
            0,
            2088,
            3840,
            72,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            println!("CreateWindowExW failed");
            return;
        };
        let _ = ShowWindow(hwnd, SW_SHOW);
        // Below every window; the taskbar (topmost) stays above, the wallpaper
        // is not a window at all.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            2088,
            3840,
            72,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        println!("stripes window up at (0,2088) 3840x72");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
