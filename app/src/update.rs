//! Self-update: the tray entry point and the helper-process entry point.
//!
//! Two very different things live here, glued by one argument:
//!
//! * [`check_for_update`] — what the tray menu runs. It resolves the latest
//!   release, stages a verified payload under `%LOCALAPPDATA%\WinBeautify\update\`,
//!   spawns a *helper* and shuts the app down. Toasts are the whole UI: an
//!   update either works (one shell flash) or explains itself in a panel.
//! * [`helper_main`] — what the helper process runs. It re-enters this exe
//!   with `--update-finish <payload>` and must be intercepted in `main`
//!   **before the single-instance claim**: at helper startup the old copy
//!   still holds the mutex (it exits only after the helper is running), so a
//!   helper that ran the normal startup path would immediately conclude
//!   "another copy is running", open the settings window, and exit.
//!
//! The helper's sequence is the one the DLL-lock situation dictates:
//!
//! 1. Wait for the old app to fully exit — it holds its own exe open and
//!    keeps the TAP DLL loaded via `inject`'s `LoadLibraryW`, so nothing in
//!    the install directory can be replaced until it is gone.
//! 2. Restart the shell (`beautify_taskbar::restart_explorer`) — the TAP DLL
//!    stays mapped in explorer for the shell's lifetime, so the shell has to
//!    go for that lock to be released. Best effort: a shell that refuses to
//!    restart is covered by the apply's rename fallback instead.
//! 3. Copy the payload over the install directory.
//! 4. Launch the new exe, which re-injects the new DLL into the fresh shell.
//!
//! The helper logs to `logs/update-helper.log` — it runs before `logging::init`
//! (and before tauri), so it writes its own trail for the failures nobody
//! is watching.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use crate::state::AppState;

/// One update flow at a time: two concurrent checks would stage two payloads
/// and race each other's helper into the install directory.
static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// How long the helper waits for the old app to exit before giving up. The
/// shutdown path tears down every module (which is what restores the taskbar)
/// and normally takes well under two seconds; the budget is generous so a
/// slow disk never turns an update into an abort.
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// `CREATE_NO_WINDOW` for the helper spawn. Release builds are a GUI-subsystem
/// image and open nothing anyway, but debug builds (used for testing this
/// whole flow) would otherwise put a console window on screen.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Tray menu entry: check for a newer release, and offer to install it.
///
/// The check, download and staging all happen on a plain thread — the tray
/// handler must return, and nothing here is fast enough to do inline. Guarded
/// against re-entry; a second click during a run gets a toast instead of a
/// second staging directory.
pub fn check_for_update(app: &AppHandle) {
    if UPDATE_IN_PROGRESS
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        app.state::<Arc<AppState>>()
            .toast
            .show("更新进行中", "请等当前更新流程结束");
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("wb-update".into())
        .spawn(move || run_update(app));
    if spawned.is_err() {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
        tracing::warn!("could not spawn the update thread");
    }
}

/// The update thread's body: check → stage → helper → graceful exit.
fn run_update(app: AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let current = env!("CARGO_PKG_VERSION");
    state.toast.show("检查更新", "正在连接发布页…");

    match beautify_update::check(current) {
        Err(e) => state.toast.show("检查更新失败", &e),
        Ok(None) => {
            state
                .toast
                .show("已是最新版本", &format!("当前版本 v{current}"));
        }
        Ok(Some(release)) => {
            state.toast.show(
                &format!("发现新版本 v{}", release.version),
                "正在下载并校验…",
            );
            let staging = beautify_core::paths::data_dir().join("update");
            match beautify_update::stage(&release, &staging) {
                Err(e) => state.toast.show("下载更新失败", &e),
                Ok(payload) => {
                    state.toast.show(
                        &format!("准备安装 v{}", release.version),
                        "资源管理器将自动重启，桌面会短暂闪动",
                    );
                    match launch_helper(&payload) {
                        Ok(()) => {
                            // The helper is waiting for this process to exit;
                            // shutting down is the last thing this process
                            // does. `shutdown` restores the taskbar and stops
                            // the modules, and it belongs on the main thread
                            // like every other window-owning call.
                            let handle = app.clone();
                            let _ = app.run_on_main_thread(move || crate::shutdown(&handle));
                        }
                        Err(e) => state.toast.show("启动更新进程失败", &e),
                    }
                }
            }
        }
    }

    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
}

/// Spawn a detached copy of this exe in helper mode. The parent pid travels
/// with it: the helper's first job is waiting for *this* process to die.
fn launch_helper(payload: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序: {e}"))?;
    let payload = payload
        .canonicalize()
        .map_err(|e| format!("无法定位更新暂存目录: {e}"))?;
    std::process::Command::new(&exe)
        .arg("--update-finish")
        .arg(&payload)
        .arg("--parent")
        .arg(std::process::id().to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("无法启动更新进程: {e}"))?;
    Ok(())
}

/// The early-main interception: `Some(exit code)` when this launch *is* the
/// update helper (and has therefore already done all of its work), `None`
/// when this is a normal app launch. Called before the single-instance claim
/// — see the module docs for why.
pub fn helper_main<I: IntoIterator<Item = String>>(args: I) -> Option<i32> {
    let (payload, parent) = parse_helper_args(args)?;
    Some(run_helper(&payload, parent))
}

/// `--update-finish <payload> [--parent <pid>]`. Anything else is ignored:
/// the helper shares its argv with every future app flag by accident of
/// being the same binary.
fn parse_helper_args<I: IntoIterator<Item = String>>(args: I) -> Option<(PathBuf, u32)> {
    let args: Vec<String> = args.into_iter().collect();
    let mut payload = None;
    let mut parent = 0u32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--update-finish" => {
                payload = args.get(i + 1).cloned();
                i += 2;
            }
            "--parent" => {
                parent = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    payload.map(|p| (PathBuf::from(p), parent))
}

/// The helper's whole job. Exit code 0 only when the payload is applied and
/// the new version is running.
fn run_helper(payload: &Path, parent: u32) -> i32 {
    helper_log(&format!(
        "helper started: payload={} parent={parent}",
        payload.display()
    ));

    if parent != 0 {
        match wait_parent_exit(parent) {
            Ok(()) => helper_log("old copy exited; its exe and TAP-DLL locks are gone"),
            Err(e) => {
                helper_log(&format!("waiting for the old copy failed: {e}"));
                return 1;
            }
        }
    } else {
        helper_log("no parent pid; not waiting (manual run?)");
    }

    match beautify_taskbar::restart_explorer() {
        Ok(()) => helper_log("shell restarted; the TAP DLL's lock is released"),
        // Not fatal: the shell coming down is what releases the DLL, but the
        // apply's rename fallback can free the path even with the shell up.
        Err(e) => helper_log(&format!(
            "shell restart failed, continuing on the rename fallback: {e}"
        )),
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            helper_log(&format!("cannot resolve the install directory: {e}"));
            return 1;
        }
    };
    let Some(install_dir) = exe.parent() else {
        helper_log("cannot resolve the install directory");
        return 1;
    };

    if let Err(e) = beautify_update::apply_payload(payload, install_dir) {
        // Deliberately no relaunch on failure: with the exe copied last, a
        // failed apply leaves the old exe but possibly a new DLL, and
        // starting that pair is the one outcome worse than no app.
        helper_log(&format!("apply failed: {e}"));
        return 1;
    }
    helper_log("payload applied");

    match std::process::Command::new(&exe).spawn() {
        Ok(_) => {
            helper_log("new version launched");
            0
        }
        Err(e) => {
            helper_log(&format!("relaunch failed: {e}"));
            1
        }
    }
}

/// Block until the old app process is gone (or was already). Its death is
/// what releases the two locks the apply needs: the running exe and the
/// `LoadLibraryW` handle on the TAP DLL.
fn wait_parent_exit(pid: u32) -> Result<(), String> {
    match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => {
            let outcome = unsafe { WaitForSingleObject(handle, PARENT_EXIT_TIMEOUT.as_millis() as u32) };
            unsafe {
                let _ = CloseHandle(handle);
            }
            match outcome {
                WAIT_OBJECT_0 => Ok(()),
                WAIT_TIMEOUT => Err(format!("旧进程 (pid {pid}) 没有在期限内退出")),
                code => Err(format!("等待旧进程退出失败: {code:?}")),
            }
        }
        // Cannot open a thing that is not there: already exited, which is
        // exactly what was being waited for.
        Err(_) => Ok(()),
    }
}

/// Sweep the `.old` leftovers a previous update may have left beside the exe.
/// Called once per startup, after logging is up.
pub fn cleanup_leftovers() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(install_dir) = exe.parent() else {
        return;
    };
    beautify_update::cleanup_old_files(install_dir);
}

/// The helper's own log, written before logging exists. Unix seconds keep the
/// format trivial; ordering is what matters in a post-mortem, not beauty.
fn helper_log(line: &str) {
    let dir = beautify_core::paths::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("update-helper.log"))
    {
        let _ = writeln!(file, "[{ts}] {line}");
    }
    // Debug builds keep a console, so local test runs see the trail live.
    if cfg!(debug_assertions) {
        println!("update-helper: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact argv `launch_helper` produces must parse back into the same
    /// payload and parent — this pair of functions is the entire handoff
    /// between the two processes.
    #[test]
    fn the_helper_argv_round_trips() {
        let argv = vec![
            "--update-finish".to_string(),
            r"C:\staging\0.2.1\payload".to_string(),
            "--parent".to_string(),
            "1234".to_string(),
        ];
        let (payload, parent) = parse_helper_args(argv).expect("the flags must be recognized");
        assert_eq!(payload, PathBuf::from(r"C:\staging\0.2.1\payload"));
        assert_eq!(parent, 1234);
    }

    /// No `--update-finish` means "not the helper" — the normal startup path
    /// must run. A stray `--parent` alone must not flip the decision either.
    #[test]
    fn a_normal_launch_is_not_the_helper() {
        assert!(parse_helper_args(Vec::<String>::new()).is_none());
        assert!(parse_helper_args(["--parent".to_string(), "1".to_string()]).is_none());
        assert!(parse_helper_args(["--verbose".to_string()]).is_none());
    }

    /// A missing or malformed value must degrade, not crash or misparse:
    /// the helper is the last code standing between the user and their old
    /// install.
    #[test]
    fn malformed_flags_degrade_safely() {
        // Flag without a value: ignored entirely.
        assert!(parse_helper_args(["--update-finish".to_string()]).is_none());
        // Malformed pid falls back to "no parent", which skips the wait.
        let argv = vec![
            "--update-finish".to_string(),
            "p".to_string(),
            "--parent".to_string(),
            "not-a-pid".to_string(),
        ];
        let (payload, parent) = parse_helper_args(argv).unwrap();
        assert_eq!(payload, PathBuf::from("p"));
        assert_eq!(parent, 0);
    }
}
