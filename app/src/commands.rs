//! The IPC surface exposed to the three webviews.
//!
//! Every command is deliberately thin: it reads from a module handle and
//! returns a plain serialisable value. Anything that needs to touch a window or
//! a native resource lives in [`crate::win`] or the module itself, so this file
//! stays a description of *what the UI may ask for*.

use beautify_clipboard::store::{ClipEntry, ClipFilter, ClipKind};
use beautify_core::config::Config;
use beautify_core::model::{FlyoutTab, Lyrics, MediaSnapshot, TaskbarState};
use beautify_core::paths;
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

use beautify_media::session::Command as MediaCommand;
use beautify_todo::{Task, TaskFilter, TaskPatch, TaskStore};

use crate::hotkeys::HotkeyAction;
use crate::state::AppState;
use crate::win;

type R<T> = Result<T, String>;

fn err<E: std::fmt::Display>(context: &str, e: E) -> String {
    tracing::error!("{context}: {e}");
    format!("{context}: {e}")
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_config(state: State<'_, std::sync::Arc<AppState>>) -> Config {
    state.config.get()
}

/// Replace the whole configuration.
///
/// Rust clamps the incoming values and returns what it actually stored, so the
/// UI can never hold a config that disagrees with the running modules.
#[tauri::command]
pub fn update_config(
    app: AppHandle,
    state: State<'_, std::sync::Arc<AppState>>,
    config: Config,
) -> R<Config> {
    let applied = state
        .config
        .update(|current| *current = config)
        .map_err(|e| err("saving configuration", e))?;

    state.apply_config(&applied);
    crate::sync_side_effects(&app, &applied);
    // The other windows need to see the change too.
    let _ = app.emit("config-changed", &applied);
    Ok(applied)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModuleStatus {
    taskbar: bool,
    media: bool,
    spectrum: bool,
    clipboard: bool,
    todo: bool,
}

#[tauri::command]
pub fn get_module_status(state: State<'_, std::sync::Arc<AppState>>) -> ModuleStatus {
    let config = state.config.get();
    ModuleStatus {
        taskbar: config.taskbar.enabled && state.taskbar.is_running(),
        media: config.media.enabled,
        spectrum: state.media.spectrum_running(),
        clipboard: config.clipboard.enabled,
        todo: config.todo.enabled,
    }
}

#[tauri::command]
pub fn get_app_info(state: State<'_, std::sync::Arc<AppState>>) -> std::collections::BTreeMap<String, String> {
    let mut info = std::collections::BTreeMap::new();
    info.insert("版本".into(), env!("CARGO_PKG_VERSION").into());
    info.insert("配置文件".into(), state.config.path().display().to_string());
    info.insert("数据目录".into(), paths::data_dir().display().to_string());
    info.insert("歌词目录".into(), paths::lyrics_dir().display().to_string());
    info.insert("日志目录".into(), paths::logs_dir().display().to_string());
    info.insert(
        "可执行文件".into(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    info
}

// ---------------------------------------------------------------------------
// Taskbar
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_taskbar_state(state: State<'_, std::sync::Arc<AppState>>) -> TaskbarState {
    state.taskbar_state.read().clone()
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_media(state: State<'_, std::sync::Arc<AppState>>) -> MediaSnapshot {
    state.media.snapshot()
}

#[tauri::command]
pub fn get_lyrics(state: State<'_, std::sync::Arc<AppState>>) -> Lyrics {
    state.media.lyrics()
}

#[tauri::command]
pub fn get_lyric_index(state: State<'_, std::sync::Arc<AppState>>) -> Option<usize> {
    state.media.lyric_index()
}

#[tauri::command]
pub fn media_control(state: State<'_, std::sync::Arc<AppState>>, action: String) -> R<()> {
    let command = match action.as_str() {
        "play" => MediaCommand::Play,
        "pause" => MediaCommand::Pause,
        "toggle" => MediaCommand::Toggle,
        "next" => MediaCommand::Next,
        "previous" => MediaCommand::Previous,
        other => return Err(format!("unknown media action: {other}")),
    };
    state.media.control(command);
    Ok(())
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn clipboard_list(
    state: State<'_, std::sync::Arc<AppState>>,
    query: String,
    filter: Option<ClipFilter>,
    limit: u32,
    offset: u32,
) -> R<Vec<ClipEntry>> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    store
        .list(&query, filter.unwrap_or_default(), limit.clamp(1, 500), offset)
        .map_err(|e| err("reading clipboard history", e))
}

#[tauri::command]
pub fn clipboard_copy(state: State<'_, std::sync::Arc<AppState>>, id: i64) -> R<()> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    let item = store
        .get(id)
        .map_err(|e| err("reading clipboard history", e))?
        .ok_or_else(|| format!("clipboard entry {id} no longer exists"))?;

    state
        .clipboard
        .write(item.kind, &item.text, &item.image_path)
        .map_err(|e| err("writing to the clipboard", e))
}

#[tauri::command]
pub fn clipboard_pin(
    state: State<'_, std::sync::Arc<AppState>>,
    id: i64,
    pinned: bool,
) -> R<()> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    store
        .set_pinned(id, pinned)
        .map_err(|e| err("updating the clipboard entry", e))?;
    state.bus.publish(&beautify_core::Event::ClipboardChanged);
    Ok(())
}

#[tauri::command]
pub fn clipboard_delete(state: State<'_, std::sync::Arc<AppState>>, id: i64) -> R<()> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    store
        .delete(id)
        .map_err(|e| err("deleting the clipboard entry", e))?;
    state.bus.publish(&beautify_core::Event::ClipboardChanged);
    Ok(())
}

#[tauri::command]
pub fn clipboard_clear(state: State<'_, std::sync::Arc<AppState>>, include_pinned: bool) -> R<u32> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    let removed = store
        .clear(include_pinned)
        .map_err(|e| err("clearing clipboard history", e))?;
    state.bus.publish(&beautify_core::Event::ClipboardChanged);
    Ok(removed)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClipboardStats {
    total: i64,
    pinned: i64,
    bytes: i64,
}

#[tauri::command]
pub fn clipboard_stats(state: State<'_, std::sync::Arc<AppState>>) -> R<ClipboardStats> {
    let store = state
        .clipboard
        .store()
        .ok_or_else(|| "clipboard history is unavailable".to_string())?;
    let (total, pinned, bytes) = store
        .stats()
        .map_err(|e| err("reading clipboard statistics", e))?;
    Ok(ClipboardStats {
        total,
        pinned,
        bytes,
    })
}

/// Put arbitrary text on the clipboard (used by the export buttons).
#[tauri::command]
pub fn set_clipboard_text(state: State<'_, std::sync::Arc<AppState>>, text: String) -> R<()> {
    state
        .clipboard
        .write(ClipKind::Text, &text, "")
        .map_err(|e| err("writing to the clipboard", e))
}

// ---------------------------------------------------------------------------
// Todo
// ---------------------------------------------------------------------------

fn todo_store(state: &AppState) -> R<std::sync::Arc<TaskStore>> {
    state
        .todo
        .store()
        .ok_or_else(|| "task list storage is unavailable".to_string())
}

#[tauri::command]
pub fn todo_list(state: State<'_, std::sync::Arc<AppState>>, filter: String) -> R<Vec<Task>> {
    let filter = match filter.as_str() {
        "today" => TaskFilter::Today,
        "open" => TaskFilter::Open,
        "all" => TaskFilter::All,
        "done" => TaskFilter::Done,
        other => return Err(format!("unknown task filter: {other}")),
    };
    todo_store(&state)?
        .list(filter)
        .map_err(|e| err("reading the task list", e))
}

#[tauri::command]
pub fn todo_create(
    state: State<'_, std::sync::Arc<AppState>>,
    title: String,
    list: String,
) -> R<Task> {
    let store = todo_store(&state)?;
    let task = store
        .create(&title, &list)
        .map_err(|e| err("adding a task", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(task)
}

#[tauri::command]
pub fn todo_update(
    state: State<'_, std::sync::Arc<AppState>>,
    id: i64,
    patch: Value,
) -> R<Task> {
    let patch: TaskPatch =
        serde_json::from_value(patch).map_err(|e| err("parsing the task patch", e))?;
    let store = todo_store(&state)?;
    let task = store
        .update(id, &patch)
        .map_err(|e| err("updating the task", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(task)
}

#[tauri::command]
pub fn todo_delete(state: State<'_, std::sync::Arc<AppState>>, id: i64) -> R<()> {
    todo_store(&state)?
        .delete(id)
        .map_err(|e| err("deleting the task", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(())
}

#[tauri::command]
pub fn todo_clear_completed(state: State<'_, std::sync::Arc<AppState>>) -> R<u32> {
    let removed = todo_store(&state)?
        .clear_completed()
        .map_err(|e| err("clearing completed tasks", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(removed)
}

#[tauri::command]
pub fn todo_reorder(state: State<'_, std::sync::Arc<AppState>>, ids: Vec<i64>) -> R<()> {
    todo_store(&state)?
        .reorder(&ids)
        .map_err(|e| err("reordering tasks", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(())
}

#[tauri::command]
pub fn todo_open_count(state: State<'_, std::sync::Arc<AppState>>) -> R<i64> {
    todo_store(&state)?
        .open_count()
        .map_err(|e| err("counting open tasks", e))
}

#[tauri::command]
pub fn todo_export(state: State<'_, std::sync::Arc<AppState>>, format: String) -> R<String> {
    let store = todo_store(&state)?;
    match format.as_str() {
        "markdown" => store.export_markdown().map_err(|e| err("exporting markdown", e)),
        "json" => store.export_json().map_err(|e| err("exporting json", e)),
        other => Err(format!("unknown export format: {other}")),
    }
}

#[tauri::command]
pub fn todo_import(state: State<'_, std::sync::Arc<AppState>>, json: String) -> R<u32> {
    let imported = todo_store(&state)?
        .import_json(&json)
        .map_err(|e| err("importing tasks", e))?;
    state.bus.publish(&beautify_core::Event::TodoChanged);
    Ok(imported)
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlyoutTabArg {
    Todo,
    Clipboard,
}

impl From<FlyoutTabArg> for FlyoutTab {
    fn from(value: FlyoutTabArg) -> Self {
        match value {
            FlyoutTabArg::Todo => FlyoutTab::Todo,
            FlyoutTabArg::Clipboard => FlyoutTab::Clipboard,
        }
    }
}

#[tauri::command]
pub fn show_flyout(app: AppHandle, tab: FlyoutTabArg) -> R<()> {
    crate::flyout::show(&app, tab.into());
    Ok(())
}

#[tauri::command]
pub fn hide_flyout(app: AppHandle) {
    crate::flyout::hide(&app);
}

#[tauri::command]
pub fn toggle_flyout(app: AppHandle, tab: FlyoutTabArg) -> R<()> {
    let visible = app
        .state::<std::sync::Arc<AppState>>()
        .flyout_visible
        .load(std::sync::atomic::Ordering::Acquire);
    if visible {
        win::hide_flyout(&app);
    } else {
        win::show_flyout(&app, tab.into()).map_err(|e| err("showing the flyout", e))?;
    }
    Ok(())
}

#[tauri::command]
pub fn flyout_tab(state: State<'_, std::sync::Arc<AppState>>) -> FlyoutTab {
    *state.flyout_tab.read()
}

#[tauri::command]
pub fn open_settings(app: AppHandle) {
    crate::settings::open(&app);
}

#[tauri::command]
pub fn minimize_settings(app: AppHandle) {
    crate::settings::minimize(&app);
}

#[tauri::command]
pub fn close_settings(app: AppHandle) {
    crate::settings::close(&app);
}

/// Start a region capture, from the flyout or the tray.
#[tauri::command]
pub fn start_snip(app: AppHandle) -> R<()> {
    crate::snip::start(&app)
}

/// Dismiss every image pinned to the desktop.
#[tauri::command]
pub fn close_pins() {
    crate::snip::close_pins();
}

/// Pin the clipboard's image to the desktop, or take it away again.
#[tauri::command]
pub fn pin_clipboard_image(app: AppHandle) -> R<String> {
    crate::snip::pin_clipboard(&app)
}

/// Read the clipboard's text, for a paste into the flyout.
#[tauri::command]
pub fn clipboard_text() -> Option<String> {
    beautify_clipboard::capture::clipboard_text()
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    crate::shutdown(&app);
}

/// Reveal a directory in Explorer.
#[tauri::command]
pub fn open_path(path: String) -> R<()> {
    reveal_path(&path)
}

/// The body of [`open_path`], for the native settings window's action rows.
pub fn reveal_path(path: &str) -> R<()> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value <= 32 on failure.
    if result.0 as usize <= 32 {
        return Err(format!("could not open {path}"));
    }
    Ok(())
}

/// Fetch the configured lyric provider with the current track and describe the
/// outcome, so the settings page can say *why* lyrics are empty.
#[tauri::command]
pub fn test_lyric_provider(state: State<'_, std::sync::Arc<AppState>>) -> R<String> {
    test_lyric_provider_impl(state.inner())
}

/// The body of [`test_lyric_provider`], reachable from the native settings
/// window's action rows as well as over IPC.
pub fn test_lyric_provider_impl(state: &std::sync::Arc<AppState>) -> R<String> {
    let config = state.config.get();
    let provider = config.media.lyric_provider;
    if !provider.is_online() {
        return Err("当前歌词来源是「关闭」，不会联网查询".into());
    }
    if provider == beautify_core::config::LyricProvider::Custom
        && config.media.online_api.trim().is_empty()
    {
        return Err("自定义接口地址为空".into());
    }

    let snapshot = state.media.snapshot();
    if snapshot.title.trim().is_empty() {
        return Err("当前没有正在播放的曲目，无法测试".into());
    }

    // Same path the runtime uses, so a success here is a success there. The
    // resolver is new, so it holds no memo: this really is a fresh request.
    let resolver = beautify_media::lyrics::LyricsResolver::new(
        paths::lyrics_dir(),
        provider,
        config.media.online_api.clone(),
    );

    let lyrics = resolver.lookup_now(
        &snapshot.artist,
        &snapshot.title,
        &snapshot.album,
        snapshot.duration_ms,
    );
    if lyrics.is_empty() {
        Err(format!(
            "「{} - {}」在各来源都没有找到歌词",
            snapshot.title, snapshot.artist
        ))
    } else {
        Ok(format!(
            "成功：{} 提供，共 {} 行",
            lyrics.source,
            lyrics.lines.len()
        ))
    }
}

/// Refresh the hotkey registrations from the current configuration.
pub fn refresh_hotkeys(app: &AppHandle, config: &Config) {
    let state = app.state::<std::sync::Arc<AppState>>();
    let mut guard = state.hotkeys.lock();
    // A `HotkeyRegistry` re-registers on construction; dropping the old one
    // unregisters its bindings first.
    *guard = None;
    *guard = crate::hotkeys::HotkeyRegistry::start(
        vec![
            (HotkeyAction::OpenClipboard, config.clipboard.hotkey.clone()),
            (HotkeyAction::OpenTodo, config.todo.hotkey.clone()),
            (HotkeyAction::Snip, config.snip.hotkey.clone()),
            (HotkeyAction::PinClipboard, config.clipboard.pin_hotkey.clone()),
        ],
        app.clone(),
    );
}
