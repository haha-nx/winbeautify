//! Native taskbar widget bar.
//!
//! Replaces the WebView2 widget window with a Direct2D one. The reason is
//! memory: any WebView2 window brings a whole Chromium process tree with it
//! (~340 MB measured), while this draws the same pill — launcher, album art,
//! lyric and spectrum — inside the host process for a few megabytes.
//!
//! # Composition
//!
//! * [`state`] is a snapshot of what to show, written by the event bus.
//! * [`layout`] turns that into rectangles. Pure arithmetic, fully tested.
//! * [`theme`] resolves configuration colours into what the painter needs.
//! * [`paint`] draws a frame with Direct2D into a premultiplied-alpha bitmap.
//! * [`surface`] pushes that bitmap to a layered window.
//! * [`window`] owns the window, the message pump, hover and clicks.
//!
//! The module is an ordinary [`beautify_core::Module`], so the host starts and
//! stops it exactly like the taskbar or clipboard modules.

pub mod canvas;
pub mod images;
pub mod layout;
pub mod paint;
pub mod state;
pub mod surface;
pub mod theme;
pub mod window;

use beautify_core::config::{Config, WidgetRenderer};
use beautify_core::event::Event;
use beautify_core::geometry::Rect;
use beautify_core::model::Lyrics;
use beautify_core::module::{Module, ModuleContext, ModuleResult};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use crate::layout::Transport;
use crate::state::WidgetState;

/// Transport buttons map onto these; the host turns them into media commands,
/// which keeps this crate free of a dependency on `beautify-media`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAction {
    Previous,
    Toggle,
    Next,
}

/// Something the user did to the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetAction {
    /// The launcher was clicked.
    ToggleFlyout,
    /// Somewhere else on the bar was clicked while the flyout was open.
    HideFlyout,
    Transport(TransportAction),
}

impl From<Transport> for TransportAction {
    fn from(value: Transport) -> Self {
        match value {
            Transport::Previous => TransportAction::Previous,
            Transport::Toggle => TransportAction::Toggle,
            Transport::Next => TransportAction::Next,
        }
    }
}

type ActionHandler = Box<dyn Fn(WidgetAction) + Send + Sync>;

/// State shared between the host thread and the widget's pump thread.
pub struct Shared {
    pub state: RwLock<WidgetState>,
    /// Window handle of the pump, so the host can poke it. Zero when stopped.
    pub hwnd: AtomicIsize,
    /// Where the bar currently is, for the flyout to anchor against.
    pub rect: RwLock<Option<Rect>>,
    action: RwLock<Option<ActionHandler>>,
    /// Bus subscriptions; kept alive here because dropping one unsubscribes.
    subscriptions: Mutex<Vec<beautify_core::Subscription>>,
}

impl Shared {
    fn dispatch(&self, action: WidgetAction) {
        if let Some(handler) = self.action.read().as_ref() {
            handler(action);
        }
    }

    /// Tell the pump thread that the displayed state changed.
    pub(crate) fn invalidate(&self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let hwnd = windows::Win32::Foundation::HWND(raw as *mut core::ffi::c_void);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                window::WM_APP_DIRTY,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }

    /// The snapshot the bar is currently showing.
    pub fn snapshot(&self) -> WidgetState {
        self.state.read().clone()
    }

    /// Record where the bar ended up, so the flyout can anchor to it.
    pub fn set_rect(&self, rect: Option<Rect>) {
        *self.rect.write() = rect;
    }
}

/// The native widget bar.
pub struct WidgetModule {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for WidgetModule {
    fn default() -> Self {
        Self::new()
    }
}

impl WidgetModule {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: RwLock::new(WidgetState::new(Config::default())),
                hwnd: AtomicIsize::new(0),
                rect: RwLock::new(None),
                action: RwLock::new(None),
                subscriptions: Mutex::new(Vec::new()),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Install the callback that receives launcher and transport clicks.
    ///
    /// Set before [`Module::start`]. Without it the bar still draws, it just
    /// does nothing when clicked.
    pub fn set_action_handler(&self, handler: impl Fn(WidgetAction) + Send + Sync + 'static) {
        *self.shared.action.write() = Some(Box::new(handler));
    }

    /// Where the bar is right now, in physical screen pixels.
    ///
    /// `None` while the bar is not on screen.
    pub fn rect(&self) -> Option<Rect> {
        *self.shared.rect.read()
    }

    /// Lyrics for the current track. Pushed by the host, which already tracks
    /// them for the webview front end.
    pub fn set_lyrics(&self, lyrics: Lyrics, index: Option<usize>) {
        update(&self.shared, |state| {
            state.lyrics = Arc::new(lyrics);
            state.lyric_index = index;
        });
    }

    /// Open task count, for the launcher badge.
    pub fn set_open_tasks(&self, count: i64) {
        if self.shared.state.read().open_tasks == count {
            return;
        }
        update(&self.shared, |state| state.open_tasks = count);
    }

    /// Tell the bar whether the flyout is up, so the launcher can stay lit.
    pub fn set_flyout_open(&self, open: bool) {
        if self.shared.state.read().flyout_open == open {
            return;
        }
        update(&self.shared, |state| state.flyout_open = open);
    }

    /// True once the pump thread has a window and is rendering.
    /// Ask the bar to redraw.
    ///
    /// Needed after something on another thread has covered it: a layered window
    /// is composited from the surface the process last pushed, so a full-screen
    /// overlay that was taken down over it leaves the shell to put the pixels
    /// back — and a window that is *not* repainted keeps whatever the compositor
    /// last had, which after a topmost overlay has been and gone is nothing.
    pub fn refresh(&self) {
        self.shared.invalidate();
    }

    pub fn is_running(&self) -> bool {
        self.shared.hwnd.load(Ordering::Acquire) != 0
    }

    fn subscribe(&self, bus: &beautify_core::EventBus) {
        let mut subscriptions = self.shared.subscriptions.lock();
        subscriptions.clear();

        // A weak handle so the subscription cannot keep the module alive.
        let weak = Arc::downgrade(&self.shared);
        subscriptions.push(bus.subscribe(move |event| {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            match event {
                Event::MediaChanged(snapshot) => {
                    update(&shared, |state| state.media = Arc::clone(snapshot));
                }
                // The lyric *document* is pushed in by the host, but which line
                // is current is published on the bus as the playhead moves.
                // Without this arm the bar kept whichever line was current when
                // the track changed and never advanced from it.
                Event::LyricLineChanged { index } => {
                    advance_lyric(&shared, *index);
                }
                Event::Spectrum(frame) => {
                    // A silent session keeps publishing near-zero frames. Redrawing
                    // for those would keep the bar busy while nothing is playing.
                    if !is_visually_different(&shared.state.read().spectrum, frame) {
                        return;
                    }
                    update(&shared, |state| state.spectrum = Arc::clone(frame));
                }
                _ => {}
            }
        }));
    }
}

/// Would this frame change what the spectrum bars look like?
///
/// The threshold is well below one pixel of bar height, so a frame that passes
/// is one a viewer could actually notice.
fn is_visually_different(
    previous: &beautify_core::model::SpectrumFrame,
    next: &beautify_core::model::SpectrumFrame,
) -> bool {
    const EPSILON: f32 = 0.004;
    if previous.bands.len() != next.bands.len() {
        return true;
    }
    previous
        .bands
        .iter()
        .zip(next.bands.iter())
        .any(|(a, b)| (a - b).abs() > EPSILON)
}

/// Move the highlighted lyric line, redrawing only when it actually moves.
///
/// Returns whether the state changed. The bus delivers this event several times
/// a second and an unchanged index means the same pixels, so the pump is left
/// alone — the bar draws a frame for every invalidation it receives.
fn advance_lyric(shared: &Arc<Shared>, index: Option<usize>) -> bool {
    {
        let mut state = shared.state.write();
        if state.lyric_index == index {
            return false;
        }
        state.lyric_index = index;
    }
    shared.invalidate();
    true
}

/// Apply `f` to the state and wake the pump.
fn update(shared: &Arc<Shared>, f: impl FnOnce(&mut WidgetState)) {
    f(&mut shared.state.write());
    shared.invalidate();
}

impl Module for WidgetModule {
    fn name(&self) -> &'static str {
        "widget"
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.widget.enabled
            && config.widget.renderer == WidgetRenderer::Native
            && config.any_widget_source()
    }

    fn start(&self, ctx: ModuleContext) -> ModuleResult {
        let mut guard = self.thread.lock();
        if guard.is_some() {
            return Ok(());
        }
        *self.shared.state.write() = WidgetState::new(ctx.config.get());
        self.subscribe(&ctx.bus);

        let shared = Arc::clone(&self.shared);
        *guard = Some(
            std::thread::Builder::new()
                .name("wb-widget".into())
                .spawn(move || {
                    if let Err(e) = window::run(shared) {
                        tracing::error!("widget window exited: {e}");
                    }
                })?,
        );
        Ok(())
    }

    fn apply(&self, config: &Config) -> ModuleResult {
        update(&self.shared, |state| {
            state.config = Arc::new(config.clone());
        });
        Ok(())
    }

    fn stop(&self) -> ModuleResult {
        let Some(handle) = self.thread.lock().take() else {
            return Ok(());
        };
        let raw = self.shared.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let hwnd = windows::Win32::Foundation::HWND(raw as *mut core::ffi::c_void);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(hwnd),
                    window::WM_APP_SHUTDOWN,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
        let _ = handle.join();
        self.shared.subscriptions.lock().clear();
        *self.shared.rect.write() = None;
        Ok(())
    }
}

impl Drop for WidgetModule {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            state: RwLock::new(WidgetState::new(Config::default())),
            hwnd: AtomicIsize::new(0),
            rect: RwLock::new(None),
            action: RwLock::new(None),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn a_lyric_move_advances_the_highlighted_line() {
        let shared = shared();
        assert!(advance_lyric(&shared, Some(3)));
        assert_eq!(shared.state.read().lyric_index, Some(3));
    }

    #[test]
    fn a_repeated_index_is_not_a_redraw() {
        let shared = shared();
        assert!(advance_lyric(&shared, Some(1)));
        // The playback position is republished several times a second; only the
        // line actually changing may cost a frame.
        assert!(!advance_lyric(&shared, Some(1)));
        assert!(advance_lyric(&shared, Some(2)));
        // Clearing back to the fallback line is a change too.
        assert!(advance_lyric(&shared, None));
        assert!(!advance_lyric(&shared, None));
    }

    /// The regression that mattered: the media module published the moving
    /// lyric cursor and the bar subscribed to media and spectrum events only,
    /// so it kept showing whichever line was current when the track changed.
    #[test]
    fn a_published_lyric_move_reaches_the_bar() {
        use beautify_core::event::EventBus;

        let module = WidgetModule::new();
        let bus = EventBus::new();
        module.subscribe(&bus);

        bus.publish(&Event::LyricLineChanged { index: Some(7) });
        assert_eq!(module.shared.state.read().lyric_index, Some(7));

        bus.publish(&Event::LyricLineChanged { index: Some(8) });
        assert_eq!(
            module.shared.state.read().lyric_index,
            Some(8),
            "the bar must follow the cursor, not just the first value"
        );

        // Unsubscribing on stop must stop the updates.
        module.shared.subscriptions.lock().clear();
        bus.publish(&Event::LyricLineChanged { index: Some(9) });
        assert_eq!(module.shared.state.read().lyric_index, Some(8));
    }
}
