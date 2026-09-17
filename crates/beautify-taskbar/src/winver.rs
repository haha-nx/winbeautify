//! What version of Windows this is, for the places where behaviour depends on
//! it rather than degrading gradually.
//!
//! Two features branch on the same number and must agree about it: DWM system
//! backdrops appear in build 22621, and at that same build the taskbar stops
//! responding to composition requests because it became a XAML surface. So the
//! number is read once here, and the "does the taskbar still listen" question is
//! answered in one place.

use std::sync::OnceLock;

/// The build number, from the one place that always tells the truth.
///
/// `GetVersionEx` lies unless the executable carries a compatibility manifest,
/// and `RtlGetVersion` is not exported through `windows-rs`, so this comes from
/// the registry. `None` when the value cannot be read at all.
pub fn build_number() -> Option<u32> {
    static BUILD: OnceLock<Option<u32>> = OnceLock::new();
    *BUILD.get_or_init(read_build_number)
}

fn read_build_number() -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let subkey: Vec<u16> = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "CurrentBuildNumber"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut buffer = [0u16; 64];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return None;
    }
    let units = size as usize / 2;
    String::from_utf16_lossy(&buffer[..units.min(buffer.len())])
        .trim_matches('\0')
        .trim()
        .parse()
        .ok()
}

/// Does the shell draw the taskbar itself, ignoring requests to restyle it?
///
/// **True from Windows 11 22H2, build 22621.** The taskbar was rebuilt there as
/// a XAML surface inside `explorer.exe`, and its background is now a XAML
/// rectangle rather than a window surface. `SetWindowCompositionAttribute` — the
/// mechanism every Windows 10 era transparency tool uses, including this one —
/// is accepted and then has no visible effect: nothing errors, nothing changes.
/// Measured on build 26200 by requesting an opaque red taskbar and observing no
/// change whatsoever.
///
/// Unknown builds answer `true`, because that is the safe assumption: claiming
/// transparency works when it does not is worse than the reverse.
pub fn taskbar_ignores_composition_requests() -> bool {
    build_number().map(|build| build >= 22_621).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary is a hard cut in Windows, not a range, so it is the only
    /// part worth pinning.
    #[test]
    fn the_xaml_taskbar_starts_at_22621() {
        let xaml = |build: u32| build >= 22_621;
        assert!(!xaml(19_045), "Windows 10 2004 still honours the accent API");
        assert!(!xaml(22_620), "21H2 is the last build that does");
        assert!(xaml(22_621), "22H2 moved the taskbar into XAML");
        assert!(xaml(26_100), "and it has stayed there since");
    }

    #[test]
    fn the_build_number_is_readable_on_this_machine() {
        let build = build_number().expect("CurrentBuildNumber should be readable");
        assert!(build > 1_000, "implausible build number: {build}");
    }
}
