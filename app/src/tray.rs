//! Tray icon.
//!
//! A background app with no windows at rest needs a way back in and a way out;
//! the tray menu is both. Left-clicking the icon also opens the settings, which
//! is what people try first.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::win;

const ID: &str = "winbeautify-tray";

/// Build and install the tray icon.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "settings", "设置中心", true, None::<&str>)?,
            &MenuItem::with_id(app, "flyout-todo", "任务清单", true, None::<&str>)?,
            &MenuItem::with_id(app, "flyout-clipboard", "剪贴板历史", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "snip", "截图", true, None::<&str>)?,
            &MenuItem::with_id(app, "close-pins", "关闭全部贴图", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "reload", "重新载入配置", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "退出 WinBeautify", true, None::<&str>)?,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("WinBeautify — Windows 桌面美化")
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event);

    // The window icon is embedded by `tauri-build` from `icons/32x32.png`.
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        "settings" => crate::settings::open(app),
        "flyout-todo" => show(beautify_core::model::FlyoutTab::Todo, app),
        "flyout-clipboard" => show(beautify_core::model::FlyoutTab::Clipboard, app),
        "snip" => {
            if let Err(e) = crate::snip::start(app) {
                tracing::warn!("could not start a capture: {e}");
            }
        }
        "close-pins" => crate::snip::close_pins(),
        "reload" => reload(app),
        "quit" => crate::shutdown(app),
        _ => {}
    }
}

fn show(tab: beautify_core::model::FlyoutTab, app: &AppHandle) {
    if let Err(e) = win::show_flyout(app, tab) {
        tracing::error!("could not show the flyout: {e}");
    }
}

fn on_tray_event(tray: &tauri::tray::TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        crate::settings::open(tray.app_handle());
    }
}

/// Re-read `config.toml` from disk and apply it. Useful while experimenting
/// with the file by hand.
fn reload(app: &AppHandle) {
    use crate::state::AppState;
    let state = app.state::<std::sync::Arc<AppState>>();
    match state.config.load() {
        Ok(config) => {
            state.apply_config(&config);
            crate::sync_side_effects(app, &config);
            // The settings window holds its own copy of the config; without
            // this it would keep showing the file as it was when it opened.
            crate::settings::refresh(app);
            let _ = app.emit("config-changed", &config);
            tracing::info!("configuration reloaded from disk");
        }
        Err(e) => tracing::error!("could not reload configuration: {e}"),
    }
}
