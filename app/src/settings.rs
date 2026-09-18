//! The native settings window's link to the application.
//!
//! [`beautify_settings`] deliberately knows nothing about WinBeautify: it draws
//! a page and asks a [`Host`] for values and actions. This module is that host,
//! and it is the whole of the coupling — everything the window can do to the
//! application is a method below.
//!
//! The window runs on its own thread, so every method here may be called from
//! there. The config manager and the module handles are all `Send + Sync`, and
//! the Tauri calls that are not (`emit`) are documented as thread-safe.

use std::sync::Arc;

use beautify_core::config::Config;
use beautify_core::model::TaskbarVisualState;
use beautify_settings::paint::StatusText;
use beautify_settings::schema::{ActionId, InfoKey};
use beautify_settings::{Host, SettingsWindow};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;
use crate::{commands, snip};

/// Build (once) and show the settings window.
pub fn open(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let mut window = state.settings_window.lock();
    let window = window.get_or_insert_with(|| {
        SettingsWindow::new(Arc::new(SettingsHost { app: app.clone() }))
    });
    window.open();
}

/// Ask the settings window to close itself.
pub fn close(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let guard = state.settings_window.lock();
    if let Some(window) = guard.as_ref() {
        window.close();
    }
}

/// Minimise the settings window.
pub fn minimize(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let guard = state.settings_window.lock();
    if let Some(window) = guard.as_ref() {
        window.minimize();
    }
}

/// Tell the settings window to re-read the config, if it is up.
///
/// For changes made while it is open from somewhere else — the tray's
/// "重新载入配置", or a hand-edited `config.toml`. Changes made *in* the window
/// already refresh it, so this would only cause a second repaint there.
pub fn refresh(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let guard = state.settings_window.lock();
    if let Some(window) = guard.as_ref() {
        window.refresh();
    }
}

struct SettingsHost {
    app: AppHandle,
}

impl SettingsHost {
    fn state(&self) -> tauri::State<'_, Arc<AppState>> {
        self.app.state::<Arc<AppState>>()
    }

    /// Record what an action did, for the 关于 page.
    fn note(&self, text: impl Into<String>) {
        *self.state().last_action.write() = text.into();
    }
}

impl Host for SettingsHost {
    fn config(&self) -> Config {
        self.state().config.get()
    }

    /// The Windows apps-light-theme setting, so `主题 = 跟随系统` tracks it.
    fn system_is_light(&self) -> bool {
        system_is_light()
    }

    fn update(&self, config: Config) -> Config {
        let state = self.state();
        let applied = match state.config.update(|current| *current = config) {
            Ok(applied) => applied,
            Err(e) => {
                // The save failed, so the running config is what the modules
                // still have. Hand that back rather than the rejected value, or
                // the page would show a setting that is not in force.
                tracing::error!("saving configuration: {e}");
                state.config.get()
            }
        };
        state.apply_config(&applied);
        crate::sync_side_effects(&self.app, &applied);
        let _ = self.app.emit("config-changed", &applied);
        applied
    }

    fn status(&self) -> StatusText {
        let state = self.state();
        let config = state.config.get();
        let mut status = StatusText::default();

        // Taskbar: whether the module is running, and — the case worth
        // reporting — whether this Windows build ignores the request entirely.
        let taskbar = state.taskbar_state.read().clone();
        let (text, tone) = taskbar_status(&config, &taskbar);
        status.taskbar = text;
        status.taskbar_tone = tone;

        if !config.media.enabled {
            status.spectrum = "媒体模块已关闭".into();
            status.spectrum_tone = beautify_settings::Tone::Off;
        } else if state.media.spectrum_running() {
            status.spectrum = "正在从默认播放设备采集".into();
            status.spectrum_tone = beautify_settings::Tone::On;
        } else {
            status.spectrum = "未在采集（没有音频会话）".into();
            status.spectrum_tone = beautify_settings::Tone::Off;
        }

        if !config.clipboard.enabled {
            status.clipboard = "剪贴板历史已关闭".into();
            status.clipboard_tone = beautify_settings::Tone::Off;
        } else if let Some(store) = state.clipboard.store() {
            match store.stats() {
                Ok((total, pinned, bytes)) => {
                    status.clipboard =
                        format!("{total} 条（收藏 {pinned}）· {}", human_bytes(bytes));
                    status.clipboard_tone = beautify_settings::Tone::On;
                }
                Err(e) => {
                    status.clipboard = format!("读取失败：{e}");
                    status.clipboard_tone = beautify_settings::Tone::Warn;
                }
            }
        } else {
            status.clipboard = "数据库不可用".into();
            status.clipboard_tone = beautify_settings::Tone::Warn;
        }

        status.set_info(InfoKey::Version, env!("CARGO_PKG_VERSION"));
        status.set_info(InfoKey::Renderer, "原生 Direct2D");
        status.set_info(
            InfoKey::Build,
            beautify_taskbar::winver::build_number()
                .map(|build| format!("Windows build {build}"))
                .unwrap_or_else(|| "无法读取".into()),
        );
        status.set_info(InfoKey::ConfigPath, state.config.path().display().to_string());
        status.set_info(InfoKey::DataDir, beautify_core::paths::data_dir().display().to_string());
        status.set_info(InfoKey::LogsDir, beautify_core::paths::logs_dir().display().to_string());

        let last = state.last_action.read().clone();
        status.set_info(
            InfoKey::LastAction,
            if last.is_empty() { "还没有执行过操作".to_string() } else { last },
        );
        status
    }

    fn action(&self, action: ActionId) {
        let state = self.state();
        match action {
            ActionId::TestLyricProvider => {
                // The same code the IPC command runs, so a success in the
                // window is a success for the flyout too.
                match commands::test_lyric_provider_impl(state.inner()) {
                    Ok(message) | Err(message) => self.note(message),
                }
            }
            ActionId::OpenLyricsDir => self.note(reveal(&beautify_core::paths::lyrics_dir())),
            ActionId::OpenDataDir => self.note(reveal(&beautify_core::paths::data_dir())),
            ActionId::OpenLogsDir => self.note(reveal(&beautify_core::paths::logs_dir())),
            ActionId::ClearClipboardUnpinned | ActionId::ClearClipboardAll => {
                let include_pinned = action == ActionId::ClearClipboardAll;
                match state.clipboard.store() {
                    Some(store) => match store.clear(include_pinned) {
                        Ok(removed) => {
                            state.bus.publish(&beautify_core::Event::ClipboardChanged);
                            self.note(format!("已删除 {removed} 条剪贴板记录"));
                        }
                        Err(e) => self.note(format!("清理失败：{e}")),
                    },
                    None => self.note("剪贴板数据库不可用"),
                }
            }
            ActionId::ExportTodosMarkdown | ActionId::ExportTodosJson => {
                let markdown = action == ActionId::ExportTodosMarkdown;
                let exported = state.todo.store().map(|store| {
                    if markdown {
                        store.export_markdown()
                    } else {
                        store.export_json()
                    }
                });
                match exported {
                    Some(Ok(text)) => {
                        let copied = beautify_clipboard::writeback::put(
                            windows::Win32::Foundation::HWND::default(),
                            beautify_clipboard::store::ClipKind::Text,
                            &text,
                            "",
                        )
                        .is_ok();
                        self.note(if copied {
                            format!("已复制 {} 字符到剪贴板", text.chars().count())
                        } else {
                            "剪贴板被其它程序占用，导出失败".to_string()
                        });
                    }
                    Some(Err(e)) => self.note(format!("导出失败：{e}")),
                    None => self.note("任务清单数据库不可用"),
                }
            }
            ActionId::StartSnip => {
                let config = state.config.get();
                match snip::start(&self.app) {
                    Ok(()) => self.note(format!(
                        "已开始截图（遮罩 {:.0}%）",
                        config.snip.dim * 100.0
                    )),
                    Err(message) => self.note(message),
                }
            }
            ActionId::CloseAllPins => {
                beautify_snip::close_all_pins();
                self.note("已关闭全部贴图");
            }
            // The window closes itself for this one; going through `shutdown`
            // keeps the tray and the module teardown in one place.
            ActionId::Quit => crate::shutdown(&self.app),
        }
    }

    fn copy_text(&self, text: &str) {
        let copied = beautify_clipboard::writeback::put(
            windows::Win32::Foundation::HWND::default(),
            beautify_clipboard::store::ClipKind::Text,
            text,
            "",
        )
        .is_ok();
        if !copied {
            tracing::warn!("could not copy a settings field to the clipboard");
        }
    }

    fn paste_text(&self) -> Option<String> {
        beautify_clipboard::capture::clipboard_text()
    }
}

/// The Windows apps-light-theme setting.
///
/// Shared with the flyout panel, which has to resolve `主题 = 跟随系统` the same
/// way this window does.
pub fn system_is_light() -> bool {
    {
        use windows::core::w;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let ok = unsafe {
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
        // Anything but an explicit 1 means dark, which is also the app's default
        // and what a missing key should look like.
        ok.is_ok() && value == 1
    }
}

/// What the taskbar status pill should say.
fn taskbar_status(config: &Config, taskbar: &beautify_core::model::TaskbarState) -> (String, beautify_settings::Tone) {
    use beautify_settings::Tone;
    if !config.taskbar.enabled {
        return ("已关闭".into(), Tone::Off);
    }
    if taskbar.shell_managed {
        // Measured, not guessed: on these builds the composition call succeeds
        // and changes nothing, so claiming "已应用" would be a lie.
        return ("系统自带任务栏不响应（Win11 22H2 起）".into(), Tone::Warn);
    }
    match taskbar.state {
        TaskbarVisualState::Applied => (
            format!(
                "已应用 · {}",
                beautify_settings::schema::mode_label(&taskbar.mode)
            ),
            Tone::On,
        ),
        TaskbarVisualState::Dynamic => ("动态模式：有窗口最大化".into(), Tone::On),
        TaskbarVisualState::Hidden => ("任务栏已自动隐藏".into(), Tone::Off),
        TaskbarVisualState::Fullscreen => ("全屏应用在前台，已还原".into(), Tone::Off),
        TaskbarVisualState::Disabled => ("模块未运行".into(), Tone::Off),
    }
}

/// Open a directory in Explorer and describe it for the 关于 page.
fn reveal(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    match commands::reveal_path(&shown) {
        Ok(()) => format!("已打开 {shown}"),
        Err(e) => format!("无法打开 {shown}：{e}"),
    }
}

/// Bytes as something a person reads, for the clipboard status row.
fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes.max(0), UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_a_person_writes_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn a_disabled_taskbar_says_so_rather_than_claiming_success() {
        let mut config = Config::default();
        config.taskbar.enabled = false;
        let (text, tone) = taskbar_status(&config, &Default::default());
        assert_eq!(text, "已关闭");
        assert_eq!(tone, beautify_settings::Tone::Off);
    }

    #[test]
    fn a_shell_managed_taskbar_is_reported_as_such() {
        // The measured behaviour on Win11 22H2+: the call succeeds and nothing
        // changes, so "已应用" would be a lie.
        let config = Config::default();
        let state = beautify_core::model::TaskbarState {
            shell_managed: true,
            state: TaskbarVisualState::Applied,
            ..Default::default()
        };
        let (text, tone) = taskbar_status(&config, &state);
        assert!(text.contains("不响应"), "got {text}");
        assert_eq!(tone, beautify_settings::Tone::Warn);
    }
}
