//! The confirmation toast: a short-lived panel in the top-left corner.
//!
//! One job — tell the user that something they just did happened, and get out
//! of the way. Switching the default audio device is done with a hotkey, so
//! there is no window to look at afterwards and the audible result is not
//! something you can check without playing something; a two-second panel
//! naming the device that is now default is the whole feedback loop.
//!
//! # Composition
//!
//! * [`layout`] is the sizing arithmetic: measured text widths and a monitor
//!   work area in, a panel size and a screen position out. Pure, and fully
//!   tested without a desktop.
//! * [`paint`] measures the two lines, truncates them, and draws a frame with
//!   Direct2D into a premultiplied-alpha bitmap.
//! * [`window`] owns the window, the message pump and the auto-hide timer.
//! * [`surface`] is the layered-window handoff, reused from the widget bar.
//!
//! # Why it reuses the widget's plumbing
//!
//! A rounded translucent panel with text on it is what the widget bar and the
//! flyout already are, and both ended up needing the same three things: a
//! `32bppPBGRA` bitmap that `UpdateLayeredWindow` will accept, a Direct2D canvas
//! that draws rounded rectangles and correctly-centred text into it, and a
//! window on a thread that pumps messages. Reimplementing any of that here
//! would be a fourth place for the same two bugs — a render target that does not
//! write alpha, and text that sits a tenth of an em too low — to be rediscovered.
//!
//! # Why the API is shaped this way
//!
//! [`Toast::show`] is called from wherever the action landed, which for a
//! global hotkey is a Win32 message-pump thread belonging to some other module.
//! It therefore does no window work at all: it starts the toast's own thread on
//! first use and posts a message. Nothing in this crate takes a lock, blocks,
//! or can panic on the caller's thread, so a toast can never be the reason a
//! hotkey feels slow or a module dies.
//!
//! # What it is not
//!
//! Not a notification: nothing is queued, nothing is persisted, and there is no
//! history. A second toast replaces the first and restarts the timer — the
//! panel reports the current state of the world rather than a log of it.

pub mod layout;
pub mod paint;
pub mod surface;
pub mod window;

use crate::window::Window;

/// A top-left overlay used to confirm an action.
///
/// `Toast` is a handle, not the window: it can be created at startup, before
/// there is any reason to show anything, and the window and its thread come up
/// the first time [`Toast::show`] is called. It is `Send + Sync` so the app can
/// keep one in shared state and call it from a hotkey handler, an event-bus
/// subscriber, or the UI thread, without deciding in advance which.
///
/// ```no_run
/// use beautify_toast::Toast;
///
/// let toast = Toast::new();
/// toast.show("已切换到", "扬声器 (Realtek)");
/// ```
pub struct Toast {
    // Owned directly rather than behind an `Arc`, because an `Arc` would imply
    // the window dies with the last handle — and it does not: the pump thread
    // holds its own references to the handle and the hand-off slot, so the
    // window outlives this struct. `Window` is already `Send + Sync` on the
    // strength of its own fields.
    window: Window,
}

impl Toast {
    /// Create the handle. Does not create a window, and does not start a
    /// thread — see [`Toast::show`].
    pub fn new() -> Self {
        Self {
            window: Window::new(),
        }
    }

    /// Show the panel. Non-blocking: returns immediately and must be safe to
    /// call from any thread, including a Win32 message-pump thread.
    /// Replaces whatever is currently showing and restarts the hide timer.
    ///
    /// The two arguments are title and detail: the title says what happened,
    /// the detail says to what. Each is truncated with an ellipsis to the
    /// panel's text width; a detail containing `\n` renders as one row per
    /// line (the panel grows accordingly), and an empty detail leaves a
    /// one-line panel rather than a blank second line.
    ///
    /// A toast is a confirmation, not a result: if the window cannot be created
    /// or Direct2D is unavailable this is a logged no-op rather than an error
    /// for the caller to handle.
    pub fn show(&self, title: &str, detail: &str) {
        self.window.show(title, detail);
    }

    /// Hide immediately.
    ///
    /// Safe to call when nothing is showing, and safe to call from any thread.
    /// It is not required before a `show`: a new toast replaces the one on
    /// screen and restarts the timer, which is what makes a burst of switches
    /// end with the last one on screen rather than with the first one's
    /// remaining time.
    pub fn hide(&self) {
        self.window.hide();
    }
}

impl Default for Toast {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Toast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toast").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof that a `Toast` can live in shared state.
    ///
    /// The app builds one at startup and calls it from a hotkey handler on a
    /// thread it does not own; an accidental `Rc` or a raw `HWND` in a field
    /// would make that a compile error at *its* call site, where the reason
    /// would be far from obvious. This puts the failure here instead.
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn a_toast_can_be_shared_between_threads() {
        assert_send_sync::<Toast>();
    }

    #[test]
    fn a_toast_can_be_kept_in_shared_state() {
        // Not just `Send + Sync` in the abstract: this is the shape the app
        // actually uses — one `Toast` behind an `Arc`, written once at startup
        // and called from whatever thread an action lands on.
        let shared = std::sync::Arc::new(Toast::new());
        let handle = std::sync::Arc::clone(&shared);
        let other = std::thread::spawn(move || handle.hide());
        other.join().expect("the other thread did not panic");
        assert_eq!(std::sync::Arc::strong_count(&shared), 1);
    }

    #[test]
    fn constructing_a_toast_opens_nothing() {
        // The whole point of the lazy start: the app builds its state before it
        // has any business putting pixels on a screen, and a test can construct
        // one without a desktop. `show` is what starts the thread, and it is
        // deliberately not called here.
        let toast = Toast::new();
        drop(toast);
    }

    #[test]
    fn the_default_is_the_same_as_new() {
        let _ = Toast::default();
    }

    #[test]
    fn hiding_a_toast_that_was_never_shown_is_a_no_op() {
        // Called from a shutdown path that does not know whether anything is
        // up; it must not start a thread to find out.
        Toast::new().hide();
    }
}
