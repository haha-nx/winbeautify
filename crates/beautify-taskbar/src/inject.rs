//! Loading the TAP into explorer.exe and talking to it.
//!
//! On Windows 11 22H2+ the taskbar is a XAML island inside explorer and every
//! composition request from outside the process succeeds while doing nothing,
//! so the repaint has to happen inside the shell. `beautify-taskbar-tap` is
//! the module that does it; this file is the loader vehicle and the control
//! channel, mirroring how TranslucentTB drives its `ExplorerTAP`:
//!
//! 1. `SetWindowsHookExW(WH_CALLWNDPROC, tap_hook_proc, tap_dll, taskbar_tid)`
//!    — the first sent message to the taskbar thread makes Windows map our
//!    DLL into explorer. We trigger that deterministically by sending
//!    `WM_NULL` ourselves.
//! 2. Inside explorer the DLL connects to the XAML diagnostics framework and
//!    signals a named manual-reset event (see `protocol::READY_EVENT_NAME`).
//! 3. The host unhooks and talks to the message-only window the TAP created
//!    on the XAML UI thread, one `WM_COPYDATA` command at a time.

use beautify_taskbar_tap::protocol::{
    COPYDATA_MAGIC, DLL_FILE_NAME, READY_EVENT_NAME, TapCommand, TAP_WINDOW_CLASS,
};

use std::os::windows::ffi::OsStrExt;
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, IsWindow, SendMessageTimeoutW, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, HOOKPROC, HWND_MESSAGE,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT, WH_CALLWNDPROC, WM_COPYDATA, WM_NULL,
};

/// How long the host waits for the injected DLL to finish connecting before
/// giving up and leaving the hook. The TAP signals both success and failure.
const READY_TIMEOUT_MS: u32 = 300_000;

/// Command round-trip budget. The XAML UI thread is responsive; if it is not,
/// a lost repaint is better than hanging the pump.
const COMMAND_TIMEOUT_MS: u32 = 2_000;

/// Is the channel window still a live window? (Explorer restarts invalidate
/// the old HWND even though the class name would resolve again once the new
/// TAP has registered.)
pub fn channel_alive(channel: HWND) -> bool {
    unsafe { IsWindow(Some(channel)).as_bool() }
}

/// Look for a TAP command window from a previous run of this module or of the
/// app: the DLL stays pinned in explorer until the shell restarts, so the
/// channel often outlives the host process.
pub fn find_channel() -> Option<HWND> {
    unsafe {
        let class = windows::core::HSTRING::from(TAP_WINDOW_CLASS);
        let found = FindWindowExW(
            Some(HWND_MESSAGE),
            None,
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
        )
        .ok()?;
        (!found.is_invalid()).then_some(found)
    }
}

/// Force the TAP DLL into the taskbar's process and wait for it to be ready.
///
/// Blocks for up to `READY_TIMEOUT_MS`; run it on a helper thread so the pump
/// keeps ticking. The hook is removed once the event fires either way — on
/// success the framework pins the DLL forever, on failure it unloads.
pub fn inject(taskbar: HWND) -> Result<(), String> {
    let dll_path = dll_path().ok_or_else(|| "cannot resolve executable directory".to_string())?;
    if !dll_path.exists() {
        return Err(format!("TAP DLL not found at {}", dll_path.display()));
    }

    let module = unsafe {
        let wide: Vec<u16> = dll_path.as_os_str().encode_wide().chain([0]).collect();
        LoadLibraryW(PCWSTR(wide.as_ptr())).map_err(|e| format!("loading TAP DLL failed: {e}"))?
    };
    let proc = unsafe {
        GetProcAddress(module, PCSTR(c"tap_hook_proc".as_ptr().cast()))
            .ok_or_else(|| "TAP DLL has no tap_hook_proc export".to_string())?
    };
    let hook_proc: HOOKPROC = Some(unsafe {
        core::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT>(proc)
    });

    let mut pid = 0u32;
    let tid = unsafe { GetWindowThreadProcessId(taskbar, Some(&mut pid)) };
    if tid == 0 {
        return Err("taskbar window vanished before injection".to_string());
    }

    let event_name: Vec<u16> = READY_EVENT_NAME.encode_utf16().chain([0]).collect();
    let ready = unsafe {
        CreateEventW(None, true, false, PCWSTR(event_name.as_ptr()))
            .map_err(|e| format!("creating ready event failed: {e}"))?
    };

    let hook = unsafe {
        SetWindowsHookExW(WH_CALLWNDPROC, hook_proc, Some(module.into()), tid)
            .map_err(|e| format!("SetWindowsHookEx failed: {e}"))
    };
    let hook: HHOOK = hook?;

    // Any sent message to the hooked thread loads the DLL into explorer;
    // WM_NULL is the polite no-op.
    let mut reply = 0usize;
    unsafe {
        SendMessageTimeoutW(
            taskbar,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_BLOCK | SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            2_000,
            Some(&mut reply),
        );
        let outcome = WaitForSingleObject(ready, READY_TIMEOUT_MS);
        let _ = UnhookWindowsHookEx(hook);
        let _ = CloseHandle(ready);
        match outcome {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err("TAP did not signal readiness in time".to_string()),
            code => Err(format!("waiting for TAP readiness failed: {code:?}")),
        }
    }
}

/// Send one command and report whether the TAP applied it.
pub fn send(channel: HWND, cmd: &TapCommand) -> bool {
    let mut data = COPYDATASTRUCT {
        dwData: COPYDATA_MAGIC as usize,
        cbData: std::mem::size_of::<TapCommand>() as u32,
        lpData: cmd as *const TapCommand as *mut core::ffi::c_void,
    };
    let mut result = 0usize;
    let delivered = unsafe {
        SendMessageTimeoutW(
            channel,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&mut data as *mut COPYDATASTRUCT as isize),
            SMTO_BLOCK | SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            COMMAND_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    // Nonzero result means the TAP returned TRUE: the command was applied.
    delivered.0 != 0 && result != 0
}

/// Locate the TAP DLL next to our own executable.
fn dll_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(DLL_FILE_NAME))
}
