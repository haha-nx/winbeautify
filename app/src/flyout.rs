//! The native panel's link to the application.
//!
//! [`beautify_flyout`] draws a list and asks a [`Host`] for its rows. This is
//! that host: the clipboard store and the task store, reached through the module
//! handles in [`AppState`], plus the two things the panel needs that live
//! elsewhere — the current theme, and whether an image is on screen as a pin.
//!
//! # Pinned images are matched by content
//!
//! The panel has to know which of its rows is already pinned to the desktop, and
//! the pin registry knows images by fingerprint, not by clipboard id. So the
//! first time a row is asked about, its stored `.bmp` is decoded and
//! fingerprinted, and the answer is remembered against the entry id. That keeps
//! the decode to once per entry per session instead of once per frame.

use std::collections::HashMap;
use std::sync::Arc;

use beautify_flyout::{ClipKind, ClipRow, Host, Panel, Tab, TodoRow};
use beautify_todo::{TaskFilter, TaskPatch};
use parking_lot::Mutex;
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Build (once) and show the panel.
pub fn show(app: &AppHandle, tab: beautify_core::model::FlyoutTab) {
    let state = app.state::<Arc<AppState>>();
    *state.flyout_tab.write() = tab;

    let mut panel = state.flyout_panel.lock();
    let panel = panel.get_or_insert_with(|| {
        Panel::new(Arc::new(FlyoutHost {
            app: app.clone(),
            fingerprints: Mutex::new(HashMap::new()),
        }))
    });

    let config = state.config.get();
    let logical = (config.widget.flyout_width, config.widget.flyout_height);
    let bar = crate::win::bar_rect(app, &config);
    // Scale first, then place: the flip and the screen clamp compare the panel's
    // real height against the monitor edge, and the panel is created in physical
    // pixels while the config is in logical ones.
    let (width, height) = beautify_flyout::scaled_size(bar.left, bar.top, logical);
    let (x, y) = crate::win::flyout_position(&config, bar, (width, height));
    panel.show(x, y, width, height);
    state
        .flyout_visible
        .store(true, std::sync::atomic::Ordering::Release);
    // The launcher draws itself lit while the panel is up.
    state.widget.set_flyout_open(true);
}

/// Take the panel down.
pub fn hide(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let guard = state.flyout_panel.lock();
    if let Some(panel) = guard.as_ref() {
        panel.hide();
    }
    state
        .flyout_visible
        .store(false, std::sync::atomic::Ordering::Release);
    state.widget.set_flyout_open(false);
}

/// Tell the panel its data changed, if it is up.
pub fn refresh(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let guard = state.flyout_panel.lock();
    if let Some(panel) = guard.as_ref() {
        panel.refresh();
    }
}

struct FlyoutHost {
    app: AppHandle,
    /// Entry id to image fingerprint, filled in as rows are asked about.
    fingerprints: Mutex<HashMap<i64, u64>>,
}

impl FlyoutHost {
    fn state(&self) -> tauri::State<'_, Arc<AppState>> {
        self.app.state::<Arc<AppState>>()
    }

    /// The fingerprint of a stored image, decoding it only once.
    fn fingerprint(&self, id: i64, path: &str) -> Option<u64> {
        if let Some(known) = self.fingerprints.lock().get(&id) {
            return Some(*known);
        }
        let shot = shot_from_file(path)?;
        let fingerprint = shot.fingerprint();
        self.fingerprints.lock().insert(id, fingerprint);
        Some(fingerprint)
    }
}

/// Decode a stored `.bmp` into a shot.
///
/// The store writes a real BMP file — a 14-byte `BITMAPFILEHEADER` in front of a
/// `CF_DIB` — so the decoding is the DIB path with the header stepped over.
fn shot_from_file(path: &str) -> Option<beautify_snip::Shot> {
    let bytes = std::fs::read(path).ok()?;
    let dib = bytes
        .strip_prefix(b"BM")
        .filter(|_| bytes.len() > 14)
        .map(|_| &bytes[14..])?;
    beautify_snip::Shot::from_dib(dib)
}

impl Host for FlyoutHost {
    fn clipboard_rows(&self, query: &str, limit: u32) -> Vec<ClipRow> {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return Vec::new();
        };
        let entries = match store.list(query, Default::default(), limit, 0) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("could not read clipboard history: {e}");
                return Vec::new();
            }
        };
        let now = beautify_clipboard::store::now_millis();
        entries
            .into_iter()
            .map(|entry| {
                let kind = match entry.kind {
                    beautify_clipboard::store::ClipKind::Text => ClipKind::Text,
                    beautify_clipboard::store::ClipKind::Link => ClipKind::Link,
                    beautify_clipboard::store::ClipKind::Files => ClipKind::Files,
                    beautify_clipboard::store::ClipKind::Image => ClipKind::Image,
                };
                let title = if entry.kind == beautify_clipboard::store::ClipKind::Image {
                    // An image has no text of its own; the OCR result is the only
                    // thing that can tell two screenshots apart at a glance.
                    if entry.ocr_text.is_empty() {
                        if entry.width > 0 {
                            format!("图片 {}×{}", entry.width, entry.height)
                        } else {
                            "图片".to_string()
                        }
                    } else {
                        entry.ocr_text.lines().next().unwrap_or("图片").to_string()
                    }
                } else {
                    entry.preview.lines().next().unwrap_or("").to_string()
                };
                let subtitle = match entry.kind {
                    beautify_clipboard::store::ClipKind::Image if entry.width > 0 => format!(
                        "{} · {}",
                        relative_time(now - entry.created_at),
                        human_bytes(entry.bytes)
                    ),
                    _ => relative_time(now - entry.created_at),
                };
                let pinned_to_screen = entry.kind == beautify_clipboard::store::ClipKind::Image
                    && self
                        .fingerprint(entry.id, &entry.image_path)
                        .is_some_and(beautify_snip::is_pinned);
                ClipRow {
                    id: entry.id,
                    kind,
                    title,
                    subtitle,
                    image_path: entry.image_path,
                    favourite: entry.pinned,
                    pinned_to_screen,
                }
            })
            .collect()
    }

    fn clipboard_stats(&self) -> (i64, i64) {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return (0, 0);
        };
        match store.stats() {
            Ok((total, favourites, _bytes)) => (total, favourites),
            Err(e) => {
                tracing::warn!("could not read clipboard statistics: {e}");
                (0, 0)
            }
        }
    }

    fn copy_clip(&self, id: i64) {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return;
        };
        let Ok(Some(entry)) = store.get(id) else {
            return;
        };
        if let Err(e) = state.clipboard.write(entry.kind, &entry.text, &entry.image_path) {
            tracing::warn!("could not put an entry back on the clipboard: {e}");
        }
    }

    fn delete_clip(&self, id: i64) {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return;
        };
        match store.delete(id) {
            Ok(()) => {
                self.fingerprints.lock().remove(&id);
                state.bus.publish(&beautify_core::Event::ClipboardChanged);
            }
            Err(e) => tracing::warn!("could not delete a clipboard entry: {e}"),
        }
    }

    fn set_favourite(&self, id: i64, favourite: bool) {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return;
        };
        match store.set_pinned(id, favourite) {
            Ok(()) => state.bus.publish(&beautify_core::Event::ClipboardChanged),
            Err(e) => tracing::warn!("could not change a clipboard favourite: {e}"),
        }
    }

    fn is_pinned_to_screen(&self, id: i64, image_path: &str) -> bool {
        self.fingerprint(id, image_path)
            .is_some_and(beautify_snip::is_pinned)
    }

    fn toggle_pinned_to_screen(&self, id: i64, image_path: &str) -> bool {
        let Some(shot) = shot_from_file(image_path) else {
            tracing::warn!(id, "the stored image could not be decoded for pinning");
            return false;
        };
        let fingerprint = shot.fingerprint();
        self.fingerprints.lock().insert(id, fingerprint);
        if beautify_snip::is_pinned(fingerprint) {
            beautify_snip::close_pinned_image(fingerprint);
            return false;
        }
        let pinned = beautify_snip::pin(shot, None);
        if pinned {
            let state = self.state();
            *state.last_action.write() = "已贴图".to_string();
        }
        pinned
    }

    fn clear_unpinned_clips(&self) -> u32 {
        let state = self.state();
        let Some(store) = state.clipboard.store() else {
            return 0;
        };
        match store.clear(false) {
            Ok(removed) => {
                self.fingerprints.lock().clear();
                state.bus.publish(&beautify_core::Event::ClipboardChanged);
                removed
            }
            Err(e) => {
                tracing::warn!("could not clear the clipboard history: {e}");
                0
            }
        }
    }

    fn todo_rows(&self) -> Vec<TodoRow> {
        let state = self.state();
        let Some(store) = state.todo.store() else {
            return Vec::new();
        };
        // `TodayAll`, not `Today`: see the variant's comment — a ticked task
        // stays visible so it can be un-ticked.
        match store.list(TaskFilter::TodayAll) {
            Ok(tasks) => tasks
                .into_iter()
                .map(|task| TodoRow {
                    id: task.id,
                    title: task.title,
                    done: task.done,
                })
                .collect(),
            Err(e) => {
                tracing::warn!("could not read the task list: {e}");
                Vec::new()
            }
        }
    }

    fn add_todo(&self, title: &str) {
        let state = self.state();
        let Some(store) = state.todo.store() else {
            return;
        };
        match store.create(title, "today") {
            Ok(_) => state.bus.publish(&beautify_core::Event::TodoChanged),
            Err(e) => tracing::warn!("could not add a task: {e}"),
        }
    }

    fn set_todo_done(&self, id: i64, done: bool) {
        self.patch(id, TaskPatch {
            done: Some(done),
            ..Default::default()
        });
    }

    fn set_todo_title(&self, id: i64, title: &str) {
        self.patch(id, TaskPatch {
            title: Some(title.to_string()),
            ..Default::default()
        });
    }

    fn delete_todo(&self, id: i64) {
        let state = self.state();
        let Some(store) = state.todo.store() else {
            return;
        };
        match store.delete(id) {
            Ok(()) => state.bus.publish(&beautify_core::Event::TodoChanged),
            Err(e) => tracing::warn!("could not delete a task: {e}"),
        }
    }

    fn clear_completed_todos(&self) -> u32 {
        let state = self.state();
        let Some(store) = state.todo.store() else {
            return 0;
        };
        match store.clear_completed() {
            Ok(removed) => {
                state.bus.publish(&beautify_core::Event::TodoChanged);
                removed
            }
            Err(e) => {
                tracing::warn!("could not clear completed tasks: {e}");
                0
            }
        }
    }

    fn remembered_tab(&self) -> Tab {
        match *self.state().flyout_tab.read() {
            beautify_core::model::FlyoutTab::Clipboard => Tab::Clipboard,
            beautify_core::model::FlyoutTab::Todo => Tab::Todo,
        }
    }

    fn remember_tab(&self, tab: Tab) {
        let state = self.state();
        *state.flyout_tab.write() = match tab {
            Tab::Clipboard => beautify_core::model::FlyoutTab::Clipboard,
            Tab::Todo => beautify_core::model::FlyoutTab::Todo,
        };
    }

    /// The colours the panel draws with: the UI theme decides the surface, and
    /// the accent is the configured one.
    ///
    /// This is what "the panel follows the settings" means — read on every
    /// frame, so a change shows up as soon as the app asks for a redraw.
    fn palette(&self) -> beautify_flyout::Palette {
        let config = self.state().config.get();
        let light = match config.ui.theme {
            beautify_core::config::Theme::Light => true,
            beautify_core::config::Theme::Dark => false,
            beautify_core::config::Theme::Auto => crate::settings::system_is_light(),
        };
        beautify_flyout::Palette::resolve(config.ui.accent, light)
    }

    /// The panel lost the foreground. That is the "clicked outside" that closes
    /// a popup, and it has to move both pieces of state the launcher reads.
    fn dismissed(&self) {
        self.state()
            .flyout_visible
            .store(false, std::sync::atomic::Ordering::Release);
        self.state().widget.set_flyout_open(false);
    }
}

impl FlyoutHost {
    fn patch(&self, id: i64, patch: TaskPatch) {
        let state = self.state();
        let Some(store) = state.todo.store() else {
            return;
        };
        match store.update(id, &patch) {
            Ok(_) => state.bus.publish(&beautify_core::Event::TodoChanged),
            Err(e) => tracing::warn!("could not update a task: {e}"),
        }
    }
}

/// Milliseconds as something a person reads: "刚刚", "12 分钟前", "3 天前".
fn relative_time(age_ms: i64) -> String {
    let seconds = age_ms.max(0) / 1000;
    match seconds {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", seconds / 60),
        3600..=86_399 => format!("{} 小时前", seconds / 3600),
        86_400..=2_591_999 => format!("{} 天前", seconds / 86_400),
        _ => format!("{} 个月前", seconds / 2_592_000),
    }
}

/// Bytes as something a person reads.
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
    fn times_read_the_way_people_say_them() {
        assert_eq!(relative_time(0), "刚刚");
        assert_eq!(relative_time(30_000), "刚刚");
        assert_eq!(relative_time(60_000), "1 分钟前");
        assert_eq!(relative_time(3 * 3600_000), "3 小时前");
        assert_eq!(relative_time(2 * 86_400_000), "2 天前");
        assert_eq!(relative_time(70 * 86_400_000), "2 个月前");
        // A clock that jumped backwards must not produce "-3 分钟前".
        assert_eq!(relative_time(-5000), "刚刚");
    }

    #[test]
    fn sizes_read_the_way_people_write_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
