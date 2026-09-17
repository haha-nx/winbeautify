//! Run-at-login support.
//!
//! A single `REG_SZ` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! is all this needs: it is per-user, needs no elevation, and is exactly what
//! the Task Manager "Startup apps" list reads — so the user can always turn it
//! off from Windows itself without fighting us.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
    REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "WinBeautify";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run_key(sam: REG_SAM_FLAGS) -> Option<HKEY> {
    let path = wide(RUN_KEY);
    let mut key = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            sam,
            &mut key,
        )
    };
    (result == ERROR_SUCCESS).then_some(key)
}

/// The command line currently registered, if any.
pub fn current() -> Option<String> {
    let key = open_run_key(KEY_QUERY_VALUE)?;
    let name = wide(VALUE_NAME);
    let mut size: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, None, None, Some(&mut size))
    };
    if status != ERROR_SUCCESS || size == 0 {
        unsafe {
            let _ = RegCloseKey(key);
        }
        return None;
    }

    // The stored value is UTF-16 including the terminator, so a byte size that
    // is not a multiple of two means the registry is corrupt.
    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        return None;
    }

    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Register (or remove) the login entry.
///
/// The executable path is quoted, which matters the moment anyone installs
/// WinBeautify somewhere with a space in the path.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let name = wide(VALUE_NAME);

    if !enabled {
        let Some(key) = open_run_key(KEY_SET_VALUE) else {
            // Nothing to remove; treat as success.
            return Ok(());
        };
        let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        // Deleting a value that is not there is fine.
        return if status == ERROR_SUCCESS || status == WIN32_ERROR(2) {
            Ok(())
        } else {
            Err(format!("RegDeleteValueW failed with {status:?}"))
        };
    }

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let command = format!("\"{}\"", exe.display());
    let mut value = wide(&command);
    let bytes = value.len() * 2;

    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(RUN_KEY).as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("RegCreateKeyExW failed with {status:?}"));
    }

    let status = unsafe {
        RegSetValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                value.as_mut_ptr() as *const u8,
                bytes,
            )),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status == ERROR_SUCCESS {
        tracing::info!(command, "registered for run at login");
        Ok(())
    } else {
        Err(format!("RegSetValueExW failed with {status:?}"))
    }
}

/// Bring the registry in line with the configured preference.
///
/// Called on every config change, so a user who deletes the entry from Task
/// Manager gets it back only if they re-enable the switch.
pub fn sync(enabled: bool) {
    match (enabled, current()) {
        (true, None) => {
            if let Err(e) = set_enabled(true) {
                tracing::warn!("could not enable run at login: {e}");
            }
        }
        (false, Some(_)) => {
            if let Err(e) = set_enabled(false) {
                tracing::warn!("could not disable run at login: {e}");
            }
        }
        _ => {}
    }
}
