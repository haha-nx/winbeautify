//! WinBeautify — entry point.
//!
//! Startup order matters:
//!
//! 1. Load (or repair) the configuration, then install logging at the level it
//!    asks for.
//! 2. Build the module graph and start the enabled modules. Each one owns its
//!    own threads; the widget window comes up afterwards so it can position
//!    itself against a taskbar whose geometry the taskbar module has already
//!    measured.
//! 3. Bridge the module event bus to the webviews, install the tray icon and
//!    register global hotkeys.
//!
//! Shutdown is the reverse: drop the tray, unregister hotkeys, stop the
//! modules — which is what restores the taskbar — and only then let the
//! process exit.

// The app links against `windows` for the shell helpers, so a crate-root
// module named `windows` would shadow it; `win` is the window manager.
mod autostart;
mod commands;
mod hotkeys;
mod settings;
mod snip;
mod state;
mod tray;
mod win;

use beautify_core::config::ConfigManager;
use beautify_core::event::Event;
use beautify_core::{paths, APP_NAME};
use beautify_media::session::Command as MediaCommand;
use beautify_widget::WidgetAction;
use state::AppState;
use beautify_widget::TransportAction;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, RunEvent};

fn main() {
    let config_path = paths::config_path();
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("{APP_NAME}: cannot create {}: {e}", paths::data_dir().display());
    }

    let manager = Arc::new(ConfigManager::new(config_path));
    let config = match manager.load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{APP_NAME}: falling back to defaults ({e})");
            manager.set(Default::default());
            manager.get()
        }
    };
    beautify_core::logging::init(&config.ui.log_level, config.ui.file_logging);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting {APP_NAME}");

    let state = AppState::new(Arc::clone(&manager));

    let app = tauri::Builder::default()
        .manage(Arc::clone(&state))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::update_config,
            commands::get_module_status,
            commands::get_app_info,
            commands::test_lyric_provider,
            commands::get_taskbar_state,
            commands::get_media,
            commands::get_lyrics,
            commands::get_lyric_index,
            commands::media_control,
            commands::clipboard_list,
            commands::clipboard_copy,
            commands::clipboard_pin,
            commands::clipboard_delete,
            commands::clipboard_clear,
            commands::clipboard_stats,
            commands::set_clipboard_text,
            commands::todo_list,
            commands::todo_create,
            commands::todo_update,
            commands::todo_delete,
            commands::todo_clear_completed,
            commands::todo_reorder,
            commands::todo_open_count,
            commands::todo_export,
            commands::todo_import,
            commands::show_flyout,
            commands::hide_flyout,
            commands::toggle_flyout,
            commands::flyout_tab,
            commands::resize_widget,
            commands::widget_height,
            commands::open_settings,
            commands::minimize_settings,
            commands::close_settings,
            commands::start_snip,
            commands::close_pins,
            commands::pin_clipboard_image,
            commands::clipboard_text,
            commands::quit_app,
            commands::open_path,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            wire_widget_actions(&handle);
            bridge_events(&handle);
            state.start_modules();
            push_widget_state(&handle);

            if let Err(e) = tray::install(&handle) {
                tracing::error!("tray icon unavailable: {e}");
            }
            if let Err(e) = win::ensure_widget(&handle) {
                tracing::error!("could not create the widget bar: {e}");
            }
            crate::sync_side_effects(&handle, &manager.get());

            // Overwrite the registry state from reality rather than trusting
            // the config file, which the user may have edited by hand.
            let autostart_actual = autostart::current().is_some();
            if autostart_actual != manager.read(|c| c.general.autostart) {
                let _ = manager.update(|c| c.general.autostart = autostart_actual);
            }

            if manager.read(|c| !c.general.start_minimized) {
                settings::open(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the WinBeautify application");

    app.run(move |app_handle, event| match event {
        // Closing the last window must not take the daemon down: the widget bar
        // and the tray icon are the app, and "退出" is the only way out.
        RunEvent::ExitRequested { api, code, .. } if code.is_none() => {
            api.prevent_exit();
            let _ = app_handle;
        }
        RunEvent::Exit => {
            let state = app_handle.state::<Arc<AppState>>();
            state.stop_modules();
            tracing::info!("{APP_NAME} stopped");
        }
        _ => {}
    });
}

/// Connect the native widget bar's clicks to the same actions the webview
/// launcher used to trigger.
fn wire_widget_actions(app: &AppHandle) {
    let handle = app.clone();
    app.state::<Arc<AppState>>()
        .widget
        .set_action_handler(move |action| {
            // This runs on the widget bar's pump thread. Every Tauri call that
            // round-trips to the main thread — `is_visible`, window creation —
            // must therefore be dispatched rather than called inline, or the
            // pump stops servicing mouse input while it waits.
            let app = handle.clone();
            let dispatched = app.clone();
            let _ = handle.run_on_main_thread(move || match action {
                WidgetAction::ToggleFlyout => {
                    let visible = dispatched
                        .state::<Arc<AppState>>()
                        .flyout_visible
                        .load(std::sync::atomic::Ordering::Acquire);
                    // Both of these tell the bar what happened; nothing further
                    // is needed here.
                    if visible {
                        win::hide_flyout(&dispatched);
                    } else {
                        let tab = *dispatched.state::<Arc<AppState>>().flyout_tab.read();
                        if let Err(e) = win::show_flyout(&dispatched, tab) {
                            tracing::error!("could not show the flyout: {e}");
                        }
                    }
                }
                WidgetAction::HideFlyout => win::hide_flyout(&dispatched),
                WidgetAction::Transport(action) => {
                    let command = match action {
                        TransportAction::Previous => MediaCommand::Previous,
                        TransportAction::Toggle => MediaCommand::Toggle,
                        TransportAction::Next => MediaCommand::Next,
                    };
                    dispatched.state::<Arc<AppState>>().media.control(command);
                }
            });
            let _ = app;
        });
}

/// Re-push the flyout's visibility to the native bar from the recorded flag.
///
/// The show/hide paths in [`win`] set both, so this only matters after the bar
/// itself was restarted — a config change rebuilds its state — where the flag is
/// the one thing that survived.
fn sync_flyout_state(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let visible = state
        .flyout_visible
        .load(std::sync::atomic::Ordering::Acquire);
    state.widget.set_flyout_open(visible);
}

/// Push the pieces of state the native bar needs but the bus does not carry.
///
/// Lyrics and the open-task count are already tracked here for the webview
/// front end, so the native bar reuses them rather than subscribing to a
/// second source of truth.
pub fn push_widget_state(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let index = state.media.lyric_index();
    state.widget.set_lyrics(state.media.lyrics(), index);
    sync_flyout_state(app);
    if let Some(store) = state.todo.store() {
        if let Ok(count) = store.open_count() {
            state.widget.set_open_tasks(count);
        }
    }
}

/// Forward module events into the webviews.
///
/// Spectrum frames are the hot path at ~30/s, so they are addressed to the
/// widget window only — broadcasting them would wake the flyout and settings
/// webviews for data they never render.
fn bridge_events(app: &AppHandle) {
    let handle = app.clone();
    let state = app.state::<Arc<AppState>>();
    let subscription = state.bus.subscribe(move |event| {
        if handle.state::<Arc<AppState>>().is_shutting_down() {
            return;
        }
        match event {
            Event::MediaChanged(snapshot) => {
                let _ = handle.emit("media-changed", snapshot.as_ref());
            }
            Event::LyricLineChanged { index } => {
                let _ = handle.emit("lyric-index", serde_json::json!({ "index": index }));
            }
            Event::Spectrum(frame) => {
                if handle.get_webview_window(win::WIDGET).is_some() {
                    let _ = handle.emit_to(win::WIDGET, "spectrum", frame.as_ref());
                }
            }
            Event::TaskbarChanged(taskbar) => {
                *handle.state::<Arc<AppState>>().taskbar_state.write() = taskbar.as_ref().clone();
                let _ = handle.emit("taskbar-changed", taskbar.as_ref());
                // The taskbar has moved or resized; the bar has to follow.
                let width = win::current_widget_width(&handle);
                win::reposition_widget(&handle, width);
            }
            Event::ClipboardChanged => {
                let _ = handle.emit("clipboard-changed", ());
            }
            Event::TodoChanged => {
                if let Some(store) = handle.state::<Arc<AppState>>().todo.store() {
                    if let Ok(count) = store.open_count() {
                        handle.state::<Arc<AppState>>().widget.set_open_tasks(count);
                    }
                }
                let _ = handle.emit("todo-changed", ());
            }
            Event::ThemeChanged | Event::ConfigChanged => {
                let _ = handle.emit("config-changed", handle.state::<Arc<AppState>>().config.get());
            }
        }
    });
    // Held for the lifetime of the process.
    state.subscriptions.lock().push(subscription);

    // Lyrics arrive per track rather than per position, so they are pushed from
    // a small poll of the media module instead of the event bus.
    let lyrics_handle = app.clone();
    std::thread::Builder::new()
        .name("wb-lyrics-bridge".into())
        .spawn(move || {
            let mut last_source = String::new();
            let mut last_len = usize::MAX;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let state = lyrics_handle.state::<Arc<AppState>>();
                if state.is_shutting_down() {
                    break;
                }
                let lyrics = state.media.lyrics();
                if lyrics.source != last_source || lyrics.lines.len() != last_len {
                    last_source = lyrics.source.clone();
                    last_len = lyrics.lines.len();
                    crate::push_widget_state(&lyrics_handle);
                    let _ = lyrics_handle.emit("lyrics-changed", &lyrics);
                }
            }
        })
        .ok();
}

/// Keep the side effects that are not modules in step with the config:
/// autostart and the global hotkeys.
pub fn sync_side_effects(app: &AppHandle, config: &beautify_core::Config) {
    autostart::sync(config.general.autostart);
    commands::refresh_hotkeys(app, config);

    // A disabled widget bar should disappear rather than linger. The native
    // bar is stopped and started by the module registry instead, which rebuilds
    // its state from scratch — so re-push what only the host still knows.
    if config.widget.renderer == beautify_core::config::WidgetRenderer::Native {
        sync_flyout_state(app);
        push_widget_state(app);
        return;
    }
    if let Some(window) = app.get_webview_window(win::WIDGET) {
        if !config.widget.enabled || !config.any_widget_source() {
            let _ = window.hide();
        } else {
            let width = win::current_widget_width(app);
            win::reposition_widget(app, width);
        }
    } else if config.widget.enabled && config.any_widget_source() {
        if let Err(e) = win::ensure_widget(app) {
            tracing::error!("could not create the widget bar: {e}");
        }
    }
}

/// Tear everything down and exit. Safe to call more than once.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    if state.shutting_down.swap(true, Ordering::AcqRel) {
        return;
    }
    tracing::info!("shutting down");
    if let Some(window) = app.get_webview_window(win::FLYOUT) {
        let _ = window.hide();
    }
    state.stop_modules();
    app.exit(0);
}
