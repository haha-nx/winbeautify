//! Restarting the Windows shell.
//!
//! The TAP DLL stays mapped in explorer for the shell's lifetime (see
//! [`crate::inject`]'s module notes on why it is never unmapped), which means
//! the file on disk can only be replaced once explorer is gone. An update
//! therefore needs a shell restart, and it needs one that is *safe to run
//! unattended*: a `taskkill /f` skips the shell's own shutdown and can race
//! the `AutoRestartShell` setting — winlogon may spawn a new explorer while we
//! spawn a second one, and the loser of that race opens a File Explorer
//! folder window instead of becoming the shell.
//!
//! So [`restart_explorer`] does it in escalating, verified steps:
//!
//! 1. `WM_QUIT` posted to the thread that owns `Shell_TrayWnd` — the shell
//!    exits through its own shutdown path (saving state), and once the
//!    process is gone the TAP DLL's file lock is released with it.
//! 2. If it will not die within the grace period, `TerminateProcess`.
//! 3. Wait for the shell to come back on its own — `AutoRestartShell` is on
//!    by default and usually reacts within a couple of seconds.
//! 4. Only if it does not, start `explorer.exe` explicitly, and only then:
//!    a launch while no shell exists becomes the shell, a launch beside one
//!    becomes a folder window.
//!
//! Every step polls a real window (`Shell_TrayWnd`) rather than a process
//! count, because a taskbar module cares about the window existing, not about
//! how explorer processes happen to be alive.
//!
//! The call blocks for up to half a minute in the worst case. It is meant for
//! the update helper (no UI to stall) and for a settings "restart explorer"
//! action running on its own thread — never inline on a pump thread.

use std::time::{Duration, Instant};

use tracing::info;

use crate::shell::primary_taskbar;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, PostThreadMessageW, WM_QUIT};

/// How long the shell gets to exit on its own after `WM_QUIT` before it is
/// considered hung. A graceful exit normally takes well under a second.
const GRACEFUL_EXIT: Duration = Duration::from_secs(6);

/// How long `TerminateProcess` gets to finish the job. Process termination is
/// asynchronous; the wait is what proves the locks are gone.
const KILL_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait for winlogon's `AutoRestartShell` to bring the shell back
/// before starting one ourselves. Explorer restarts in well under this on a
/// healthy machine; the budget has to be long enough that we rarely race it.
const AUTO_RESTART_BUDGET: Duration = Duration::from_secs(6);

/// How long an explicitly started explorer gets to produce a taskbar window.
const MANUAL_START_BUDGET: Duration = Duration::from_secs(15);

/// How often the shell-return polls run. Also the pacing of the earliest poll
/// after the old process is confirmed dead: an instant first poll would find
/// nothing and is pointless work, but there is no need to be shy either.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Restart the shell, and return once a taskbar window exists again.
///
/// The old explorer process is always fully waited out before this returns:
/// its death is what releases the file locks the update needs, so "the shell
/// is back" is never reported while the old process could still be holding a
/// mapped DLL. An already-dying shell (no `Shell_TrayWnd` on entry) skips
/// straight to the waiting steps.
pub fn restart_explorer() -> Result<(), String> {
    let Some(tray) = primary_taskbar() else {
        info!("no Shell_TrayWnd on entry; the shell is already down");
        return finish_without_kill();
    };

    let mut pid = 0u32;
    let tid = unsafe { GetWindowThreadProcessId(tray, Some(&mut pid)) };
    if tid == 0 {
        // The window vanished between the find and this call — the shell is
        // going down on its own; treat it like the already-down case.
        return finish_without_kill();
    }

    // Graceful first: WM_QUIT to the thread that owns the taskbar window lets
    // explorer run its own shutdown (which is what a logged-in user would get
    // from Task Manager's restart).
    let graceful = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok();
    if graceful && wait_process_exit(pid, GRACEFUL_EXIT).is_ok() {
        info!(pid, "explorer exited gracefully");
    } else {
        if !graceful {
            info!(pid, "could not post WM_QUIT; escalating to TerminateProcess");
        } else {
            info!(pid, "explorer ignored WM_QUIT; escalating to TerminateProcess");
        }
        terminate_process(pid)?;
        wait_process_exit(pid, KILL_TIMEOUT)?;
        info!(pid, "explorer terminated");
    }

    finish_without_kill()
}

/// The second half of every path: wait for the shell to return, starting one
/// if the system does not do it alone. Called only after the old process is
/// confirmed dead, so the caller can rely on its file locks being released.
fn finish_without_kill() -> Result<(), String> {
    if wait_for_shell_return(AUTO_RESTART_BUDGET) {
        info!("the shell came back on its own");
        return Ok(());
    }
    info!("AutoRestartShell did not fire; starting explorer.exe explicitly");
    start_explorer()?;
    if wait_for_shell_return(MANUAL_START_BUDGET) {
        Ok(())
    } else {
        Err("explorer 已启动但任务栏没有在期限内出现".to_string())
    }
}

/// Wait until a `Shell_TrayWnd` exists again. Returns as soon as it does.
fn wait_for_shell_return(budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if primary_taskbar().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Block until `pid` is no longer a process — or rather, until the *handle*
/// taken for that pid is signalled. A dead pid's `OpenProcess` fails, which
/// counts as exited. (The theoretical wrap-around where Windows hands the pid
/// to a fresh process inside this window is not defended against; the window
/// is seconds wide and pid reuse on a busy desktop is measured in days.)
fn wait_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => handle,
        // Cannot open a thing that is not there.
        Err(_) => return Ok(()),
    };
    let outcome = unsafe { WaitForSingleObject(handle, timeout.as_millis() as u32) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    match outcome {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(format!("explorer (pid {pid}) 没有在期限内退出")),
        code => Err(format!("等待 explorer 退出失败: {code:?}")),
    }
}

/// Ask the kernel to kill the shell. `WaitForSingleObject` afterwards is the
/// caller's problem — termination is async, and only the wait proves it.
fn terminate_process(pid: u32) -> Result<(), String> {
    let handle: HANDLE =
        unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(|e| format!("打开 explorer 进程失败: {e}"))?;
    let result = unsafe { TerminateProcess(handle, 1) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    result.map_err(|e| format!("终止 explorer 失败: {e}"))
}

/// Launch a new shell. `explorer.exe` from the Windows directory, no
/// arguments: with no shell present this instance becomes the shell (desktop,
/// taskbar, tray). Spawned detached — nobody waits on it; the caller polls
/// for the taskbar window instead.
fn start_explorer() -> Result<(), String> {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let explorer = std::path::Path::new(&windir).join("explorer.exe");
    std::process::Command::new(&explorer)
        .spawn()
        .map_err(|e| format!("启动 explorer 失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell queries this module leans on must not panic; what they
    /// return depends on the machine, but a wedged `FindWindowW` here would
    /// wedge every caller.
    #[test]
    fn shell_queries_do_not_panic() {
        let _ = primary_taskbar();
    }

    /// A pid that does not exist counts as exited — the update helper relies
    /// on this when the parent is already gone by the time it opens the
    /// handle. `pid 0` is the system idle process and can never be opened
    /// with `PROCESS_SYNCHRONIZE` by a user token, which makes it a stable
    /// stand-in for "gone" without racing a real pid.
    #[test]
    fn a_pid_that_cannot_be_opened_counts_as_exited() {
        wait_process_exit(0, Duration::from_millis(100))
            .expect("a pid that cannot be opened must report exit");
    }

    /// The real restart, against the real shell. Run explicitly:
    /// `cargo test -p beautify-taskbar -- --ignored --nocapture`
    /// — it takes the user's taskbar down for a couple of seconds and closes
    /// open File Explorer windows, which is exactly the feature, but not
    /// something a test run should do uninvited.
    #[test]
    #[ignore = "restarts the real shell; run on purpose"]
    fn the_real_shell_comes_back() {
        restart_explorer().expect("the shell should come back");
        assert!(primary_taskbar().is_some(), "no taskbar after restart");
    }
}
