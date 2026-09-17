//! Translating a [`TaskbarMode`] into an actual composition state.
//!
//! Two mechanisms are involved and they are not interchangeable:
//!
//! * `SetWindowCompositionAttribute` (undocumented, see [`crate::ffi`]) puts a
//!   blur/acrylic backdrop *behind* the window. Available since Windows 10 1803
//!   and still functional on Windows 11.
//! * `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE)` asks DWM for one of the
//!   Windows 11 materials (Mica, Mica Alt, Acrylic). Only honoured on 22H2+.
//!
//! [`Mode::Mica`] prefers the DWM path and falls back to acrylic when DWM
//! rejects it, so the user always ends up with *something*.

use crate::ffi::{accent_state, pack_gradient, set_accent, AccentPolicy};
use beautify_core::config::TaskbarMode;
use beautify_core::geometry::Color;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMSBT_MAINWINDOW, DWMSBT_NONE,
};

/// `AccentFlags` value the shell itself uses when it wants the tint honoured.
const ACCENT_FLAG_TINTED: u32 = 2;

/// A fully-resolved backdrop request, cheap to compare so the module can skip
/// redundant DWM round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backdrop {
    pub mode: TaskbarMode,
    pub color: Color,
    /// 0..=255
    pub alpha: u8,
}

impl Backdrop {
    pub fn new(mode: TaskbarMode, color: Color, opacity: f32) -> Self {
        Self {
            mode,
            color,
            alpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }

    /// Turn the requested mode into an `ACCENT_POLICY`, or `None` when the mode
    /// means "hand the window back to Windows".
    pub fn to_accent_policy(&self, mica_supported: bool) -> Option<AccentPolicy> {
        let (r, g, b) = (self.color.r, self.color.g, self.color.b);

        match self.mode {
            TaskbarMode::Normal => None,
            TaskbarMode::Opaque => Some(AccentPolicy {
                accent_state: accent_state::ENABLE_GRADIENT,
                accent_flags: ACCENT_FLAG_TINTED,
                gradient_color: pack_gradient(0xFF, r, g, b),
                animation_id: 0,
            }),
            TaskbarMode::Clear => Some(AccentPolicy {
                accent_state: accent_state::ENABLE_TRANSPARENTGRADIENT,
                accent_flags: ACCENT_FLAG_TINTED,
                gradient_color: pack_gradient(0x00, r, g, b),
                animation_id: 0,
            }),
            TaskbarMode::Blur => Some(AccentPolicy {
                accent_state: accent_state::ENABLE_BLURBEHIND,
                accent_flags: ACCENT_FLAG_TINTED,
                gradient_color: pack_gradient(self.alpha, r, g, b),
                animation_id: 0,
            }),
            TaskbarMode::Acrylic => Some(AccentPolicy {
                accent_state: accent_state::ENABLE_ACRYLICBLURBEHIND,
                accent_flags: ACCENT_FLAG_TINTED,
                gradient_color: pack_gradient(self.alpha, r, g, b),
                animation_id: 0,
            }),
            TaskbarMode::Mica => {
                if mica_supported {
                    // DWM owns the material; the accent only keeps the window
                    // from painting its own opaque background over it.
                    Some(AccentPolicy {
                        accent_state: accent_state::ENABLE_HOSTBACKDROP,
                        accent_flags: ACCENT_FLAG_TINTED,
                        gradient_color: pack_gradient(self.alpha, r, g, b),
                        animation_id: 0,
                    })
                } else {
                    Some(AccentPolicy {
                        accent_state: accent_state::ENABLE_ACRYLICBLURBEHIND,
                        accent_flags: ACCENT_FLAG_TINTED,
                        gradient_color: pack_gradient(self.alpha, r, g, b),
                        animation_id: 0,
                    })
                }
            }
        }
    }
}

/// Applies backdrops to taskbar windows and records what was applied so it can
/// be undone.
#[derive(Debug, Default)]
pub struct AccentApplicator {
    /// Windows we have touched, so context-menu/explorer rebuilds can be
    /// restored even if the module dies unexpectedly.
    touched: Vec<HWND>,
}

impl AccentApplicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the Windows 11 system-backdrop attribute honoured on this build?
    ///
    /// Read back rather than trusting the setter's return value. On Windows 11
    /// 22H2+ `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE)` returns `S_OK`
    /// for the taskbar even though the shell paints an opaque XAML background
    /// over the result — a setter that succeeds proves only that the attribute
    /// exists, not that it is visible. A round-trip that does not come back is
    /// the honest signal that the material is not in play.
    pub fn probe_mica(hwnd: HWND) -> bool {
        use windows::Win32::Graphics::Dwm::DwmGetWindowAttribute;

        let requested: i32 = DWMSBT_MAINWINDOW.0;
        if unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &requested as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        }
        .is_err()
        {
            return false;
        }

        let mut observed: i32 = DWMSBT_NONE.0;
        let read = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &mut observed as *mut i32 as *mut core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        read.is_ok() && observed == requested
    }

    /// Apply `backdrop` to `hwnd`. Returns true when something changed on the
    /// window.
    pub fn apply(&mut self, hwnd: HWND, backdrop: &Backdrop, dark: bool, mica_supported: bool) -> bool {
        if hwnd.is_invalid() {
            return false;
        }
        if !self.touched.contains(&hwnd) {
            self.touched.push(hwnd);
        }

        // Keep the taskbar's own glyphs readable against the new backdrop.
        crate::ffi::set_dark_mode(hwnd, dark);
        unsafe {
            let flag: i32 = dark as i32;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &flag as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }

        if backdrop.mode == TaskbarMode::Mica && mica_supported {
            let value: i32 = DWMSBT_MAINWINDOW.0;
            let ok = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &value as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                )
            }
            .is_ok();
            if ok {
                return self
                    .to_accent_policy(backdrop, true)
                    .map(|p| set_accent(hwnd, p))
                    .unwrap_or(false);
            }
        } else if mica_supported {
            // Leaving Mica for another mode: clear the DWM material first, or
            // it keeps compositing underneath the accent.
            let none: i32 = DWMSBT_NONE.0;
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &none as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }

        self.to_accent_policy(backdrop, mica_supported)
            .map(|policy| set_accent(hwnd, policy))
            .unwrap_or_else(|| self.reset(hwnd))
    }

    fn to_accent_policy(&self, backdrop: &Backdrop, mica_supported: bool) -> Option<AccentPolicy> {
        backdrop.to_accent_policy(mica_supported)
    }

    /// Hand a window back to Windows.
    pub fn reset(&mut self, hwnd: HWND) -> bool {
        if hwnd.is_invalid() {
            return false;
        }
        let none: i32 = DWMSBT_NONE.0;
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &none as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
        set_accent(
            hwnd,
            AccentPolicy {
                accent_state: accent_state::DISABLED,
                ..Default::default()
            },
        )
    }

    /// Undo every window we touched. Called on module stop and on process exit
    /// so the taskbar is never left in a broken state.
    pub fn reset_all(&mut self) {
        for hwnd in std::mem::take(&mut self.touched) {
            self.reset(hwnd);
        }
    }

    /// Drop windows that no longer exist (Explorer restart).
    pub fn prune(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        self.touched
            .retain(|h| unsafe { IsWindow(Some(*h)) }.as_bool());
    }

    pub fn touched_count(&self) -> usize {
        self.touched.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdrop(mode: TaskbarMode, opacity: f32) -> Backdrop {
        Backdrop::new(mode, Color::rgb(0x10, 0x20, 0x30), opacity)
    }

    #[test]
    fn normal_mode_requests_no_accent() {
        assert!(backdrop(TaskbarMode::Normal, 1.0)
            .to_accent_policy(true)
            .is_none());
    }

    #[test]
    fn opaque_mode_forces_full_alpha() {
        let p = backdrop(TaskbarMode::Opaque, 0.1).to_accent_policy(true).unwrap();
        assert_eq!(p.accent_state, accent_state::ENABLE_GRADIENT);
        assert_eq!(p.gradient_color >> 24, 0xFF);
    }

    #[test]
    fn opacity_becomes_the_gradient_alpha() {
        let p = backdrop(TaskbarMode::Acrylic, 0.5).to_accent_policy(true).unwrap();
        assert_eq!(p.accent_state, accent_state::ENABLE_ACRYLICBLURBEHIND);
        assert_eq!(p.gradient_color >> 24, 0x80);
        // BGR ordering of the tint
        assert_eq!(p.gradient_color & 0x00FF_FFFF, 0x0030_2010);
    }

    #[test]
    fn clear_mode_is_fully_transparent() {
        let p = backdrop(TaskbarMode::Clear, 0.9).to_accent_policy(true).unwrap();
        assert_eq!(p.accent_state, accent_state::ENABLE_TRANSPARENTGRADIENT);
        assert_eq!(p.gradient_color >> 24, 0x00);
    }

    #[test]
    fn mica_falls_back_to_acrylic_without_dwm_support() {
        let p = backdrop(TaskbarMode::Mica, 0.4).to_accent_policy(false).unwrap();
        assert_eq!(p.accent_state, accent_state::ENABLE_ACRYLICBLURBEHIND);
        let p = backdrop(TaskbarMode::Mica, 0.4).to_accent_policy(true).unwrap();
        assert_eq!(p.accent_state, accent_state::ENABLE_HOSTBACKDROP);
    }

    #[test]
    fn backdrop_equality_ignores_nothing() {
        assert_eq!(backdrop(TaskbarMode::Blur, 0.5), backdrop(TaskbarMode::Blur, 0.5));
        assert_ne!(backdrop(TaskbarMode::Blur, 0.5), backdrop(TaskbarMode::Blur, 0.6));
    }
}
