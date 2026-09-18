//! The Windows "app theme" flag that `主题 = 跟随系统` resolves against.
//!
//! It lives here rather than in the application because every native surface
//! needs it and this is the lowest crate they all already depend on: the
//! settings window resolves its palette against it, and the widget bar asks for
//! it whenever its foreground follows the theme.
//!
//! Reading it is a registry round-trip and the widget bar draws at 30 fps, so
//! the answer is memoised for a second. The setting changes when the user
//! changes it, not thirty times a second, and a one-second lag on a theme
//! switch is not something anyone can see.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a memoised answer is reused.
const CACHE_TTL: Duration = Duration::from_secs(1);

static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Is Windows using its light app theme?
///
/// Anything but an explicit `1` counts as dark, which is also the app's own
/// default and what a missing registry value should look like.
pub fn apps_use_light_theme() -> bool {
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, value)) = *guard {
            if at.elapsed() < CACHE_TTL {
                return value;
            }
        }
    }
    let value = read_apps_use_light_theme();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((Instant::now(), value));
    }
    value
}

fn read_apps_use_light_theme() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    read.is_ok() && value == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the machine is set to, the answer has to be stable across the
    /// calls the widget bar makes in a second — that is the entire point of the
    /// cache, and a registry read that returned a different value per call would
    /// make the bar flicker between two foreground colours.
    #[test]
    fn the_answer_is_memoised() {
        let first = apps_use_light_theme();
        for _ in 0..100 {
            assert_eq!(apps_use_light_theme(), first);
        }
    }
}
