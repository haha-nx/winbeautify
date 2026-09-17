//! Bindings for the undocumented `user32!SetWindowCompositionAttribute`.
//!
//! This is the API TranslucentTB, TaskbarX and friends use, and it remains the
//! only way to put an acrylic/blur backdrop on the taskbar on Windows 11. It is
//! exported from `user32.dll` but absent from every public header, so the
//! declaration lives here rather than in the `windows` crate.

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

/// `WINDOWCOMPOSITIONATTRIB::WCA_ACCENT_POLICY`.
pub const WCA_ACCENT_POLICY: u32 = 19;
/// `WINDOWCOMPOSITIONATTRIB::WCA_USEDARKMODECOLORS`.
pub const WCA_USEDARKMODECOLORS: u32 = 26;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccentPolicy {
    /// See [`AccentState`].
    pub accent_state: u32,
    /// Bit flags. `2` is what the shell itself passes for a tinted backdrop.
    pub accent_flags: u32,
    /// Tint colour packed as `0xAABBGGRR` — note the byte order.
    pub gradient_color: u32,
    pub animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: u32,
    data: *mut core::ffi::c_void,
    size_of_data: usize,
}

/// `ACCENT_STATE` values we care about.
pub mod accent_state {
    pub const DISABLED: u32 = 0;
    pub const ENABLE_GRADIENT: u32 = 1;
    pub const ENABLE_TRANSPARENTGRADIENT: u32 = 2;
    pub const ENABLE_BLURBEHIND: u32 = 3;
    pub const ENABLE_ACRYLICBLURBEHIND: u32 = 4;
    pub const ENABLE_HOSTBACKDROP: u32 = 5;
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> BOOL;

/// Resolved once on first use; `None` on the (theoretical) builds that do not
/// export the function.
fn entry_point() -> Option<SetWindowCompositionAttributeFn> {
    use std::sync::OnceLock;
    static FN: OnceLock<Option<SetWindowCompositionAttributeFn>> = OnceLock::new();
    *FN.get_or_init(|| unsafe {
        let module = GetModuleHandleW(PCWSTR(windows::core::w!("user32.dll").as_ptr())).ok()?;
        let proc = GetProcAddress(module, windows::core::s!("SetWindowCompositionAttribute"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            SetWindowCompositionAttributeFn,
        >(proc))
    })
}

/// True when the private API is available on this machine.
pub fn available() -> bool {
    entry_point().is_some()
}

/// Push an accent policy onto `hwnd`.
pub fn set_accent(hwnd: HWND, mut policy: AccentPolicy) -> bool {
    let Some(f) = entry_point() else {
        return false;
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut core::ffi::c_void,
        size_of_data: std::mem::size_of::<AccentPolicy>(),
    };
    unsafe { f(hwnd, &mut data).as_bool() }
}

/// Toggle the shell's dark-mode colours for a window's non-client area.
pub fn set_dark_mode(hwnd: HWND, dark: bool) -> bool {
    let Some(f) = entry_point() else {
        return false;
    };
    let mut value: BOOL = dark.into();
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_USEDARKMODECOLORS,
        data: &mut value as *mut BOOL as *mut core::ffi::c_void,
        size_of_data: std::mem::size_of::<BOOL>(),
    };
    unsafe { f(hwnd, &mut data).as_bool() }
}

/// Pack a tint into the `0xAABBGGRR` layout `AccentPolicy` expects.
pub const fn pack_gradient(alpha: u8, r: u8, g: u8, b: u8) -> u32 {
    (alpha as u32) << 24 | (b as u32) << 16 | (g as u32) << 8 | r as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_packing_is_abgr() {
        assert_eq!(pack_gradient(0x80, 0x11, 0x22, 0x33), 0x8033_2211);
        assert_eq!(pack_gradient(0xFF, 0, 0, 0), 0xFF00_0000);
        assert_eq!(pack_gradient(0x00, 0xFF, 0xFF, 0xFF), 0x00FF_FFFF);
    }
}
