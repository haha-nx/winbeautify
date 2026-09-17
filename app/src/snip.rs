//! The screenshot feature's link to the application.
//!
//! The capture itself lives in [`beautify_snip`], which owns its own threads,
//! windows and message loops. What is here is the glue: read the options out of
//! the config, start a capture, and say what happened afterwards.

use std::sync::Arc;

use beautify_snip::{Host, Options, Outcome};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Start a region capture.
///
/// Returns `Err` with something to show the user when the request cannot be
/// honoured at all — the feature is off, or another capture is already running.
pub fn start(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<Arc<AppState>>();
    let config = state.config.get();
    if !config.snip.enabled {
        return Err("截图功能已在设置中关闭".into());
    }
    let options = Options {
        dim: config.snip.dim,
        accent: config.ui.accent,
        copy_to_clipboard: config.snip.copy_to_clipboard,
        auto_pin: config.snip.auto_pin,
    };
    if beautify_snip::capture(Arc::new(SnipHost { app: app.clone() }), options) {
        Ok(())
    } else {
        Err("无法启动截图线程".into())
    }
}

/// Dismiss every image that is currently pinned to the desktop.
pub fn close_pins() {
    beautify_snip::close_all_pins();
}

/// Called from the capture thread once a capture has finished.
struct SnipHost {
    app: AppHandle,
}

impl Host for SnipHost {
    fn completed(&self, outcome: &Outcome) {
        let message = match outcome {
            Outcome::Cancelled => "截图已取消".to_string(),
            Outcome::Captured {
                width,
                height,
                copied,
                pinned,
            } => {
                let mut what = Vec::new();
                if *copied {
                    what.push("已复制到剪贴板".to_string());
                }
                if *pinned {
                    what.push("已贴到屏幕".to_string());
                }
                if what.is_empty() {
                    // Neither delivery step was configured or succeeded; the
                    // capture would otherwise have vanished without a trace.
                    what.push("未做任何处理（复制与贴图都已关闭）".to_string());
                }
                format!("截图 {width}×{height} · {}", what.join("，"))
            }
        };
        tracing::info!("{message}");

        // The 关于 page shows the last action, and the window repaints from the
        // host, so recording the note is all that is needed to surface it.
        let state = self.app.state::<Arc<AppState>>();
        *state.last_action.write() = message;
    }
}
