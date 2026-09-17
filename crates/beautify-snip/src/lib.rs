//! Screen capture for the screenshot feature: grabbing a region and pinning the
//! result to the desktop.
//!
//! # Layout of the crate
//!
//! * [`capture`] — the pixel layer: grabbing the screen, cropping, dimming,
//!   scaling and encoding. No windows, no message loops, fully tested.
//! * `overlay` — the full-screen region selector.
//! * `pin` — the pinned image windows.
//!
//! # Threading
//!
//! Everything here is fire-and-forget. [`capture`] and [`pin`] return as soon as
//! the work has been handed to a thread, because both end up owning a window and
//! a message loop, and the caller (a global hotkey handler, a tray menu) must
//! not block on that. [`Host::completed`] is called back on the capture thread
//! once the result is known, which is where a caller reports what happened.

pub mod capture;
mod overlay;
mod pin;

use std::sync::Arc;

use beautify_core::geometry::Color;
use windows::Win32::Foundation::HWND;

pub use capture::{grab, monitor_rect_at, virtual_screen, virtual_screen_rect, Shot};
pub use pin::{
    close_all as close_all_pins, close_fingerprint as close_pinned_image, is_pinned,
    on_change as on_pins_changed, pinned as pinned_images,
};

/// How a capture should behave.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// How far the unselected area is darkened, 0.0 .. 0.85.
    pub dim: f32,
    /// Border, badge and selection-frame colour.
    pub accent: Color,
    /// Put the finished capture on the clipboard.
    pub copy_to_clipboard: bool,
    /// Pin the capture to the desktop as well, where it was taken.
    pub auto_pin: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dim: 0.45,
            accent: Color::rgb(0x6C, 0x8C, 0xFF),
            copy_to_clipboard: true,
            auto_pin: false,
        }
    }
}

/// What came of a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Escape, a right-click, or a click without a drag.
    Cancelled,
    /// The region was taken. `delivered` says how many of the delivery steps
    /// actually happened, so the caller can report a clipboard that refused the
    /// image rather than claiming success.
    Captured {
        width: i32,
        height: i32,
        copied: bool,
        pinned: bool,
    },
}

/// The caller's side of a capture.
pub trait Host: Send + Sync + 'static {
    /// Called once per capture, on the capture thread, after the selector window
    /// is gone. Never called for a request that was refused because one was
    /// already running.
    fn completed(&self, outcome: &Outcome);
}

/// Start a region capture. Returns false when one is already in progress.
pub fn capture(host: Arc<dyn Host>, options: Options) -> bool {
    // The selector owns a full-screen window and its message loop, so it needs a
    // thread that pumps. `overlay::run` declines the request outright if another
    // session is already up.
    std::thread::Builder::new()
        .name("wb-snip".into())
        .spawn(move || match overlay::run(options) {
            overlay::Finished::Refused => {
                tracing::debug!("a capture request was ignored: one is already running");
            }
            overlay::Finished::Failed => {
                tracing::warn!("the screen could not be captured");
            }
            overlay::Finished::Cancelled => host.completed(&Outcome::Cancelled),
            overlay::Finished::Captured(captured) => {
                let copied = options.copy_to_clipboard && copy_to_clipboard(&captured.shot);
                let pinned = match captured.pin_at {
                    Some((x, y)) if options.auto_pin => {
                        pin::pin(captured.shot.clone(), Some((x, y)))
                    }
                    _ => false,
                };
                host.completed(&Outcome::Captured {
                    width: captured.shot.width,
                    height: captured.shot.height,
                    copied,
                    pinned,
                });
            }
        })
        .is_ok()
}

/// Pin an already-captured image to the desktop. Returns false when the thread
/// could not be started.
pub fn pin(shot: Shot, at: Option<(i32, i32)>) -> bool {
    pin::pin(shot, at)
}

/// Put a capture on the clipboard as a `CF_DIB`.
///
/// `HWND::default()` as the owner: this runs on the capture thread, which has no
/// window of its own, and the clipboard does not need one.
pub(crate) fn copy_to_clipboard(shot: &Shot) -> bool {
    match beautify_clipboard::writeback::put_dib(HWND::default(), &shot.to_dib()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("could not copy the capture to the clipboard: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_options_actually_deliver_something() {
        let options = Options::default();
        assert!(options.copy_to_clipboard, "a capture nobody can paste is a bug");
        assert!((0.0..0.9).contains(&options.dim));
    }

    #[test]
    fn the_outcome_distinguishes_cancel_from_delivery() {
        assert_ne!(Outcome::Cancelled, Outcome::Captured {
            width: 1,
            height: 1,
            copied: true,
            pinned: false,
        });
    }
}
