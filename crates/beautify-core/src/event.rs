//! A tiny synchronous publish/subscribe bus.
//!
//! Modules run on their own threads (Win32 message pumps, WASAPI capture, the
//! Tauri event loop) so the bus has to be callable from anywhere without an
//! async runtime. Handlers are invoked inline on the publisher's thread, which
//! keeps the hot path allocation-free and means the bus never buffers — but it
//! also means a slow handler slows the publisher, so handlers must be cheap:
//! copy a value, push to a channel, return.

use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::model::{MediaSnapshot, SpectrumFrame, TaskbarState};

/// Events broadcast across module boundaries.
#[derive(Debug, Clone)]
pub enum Event {
    /// A settings field changed; modules re-read what they care about.
    ConfigChanged,
    /// The media session snapshot changed (track, status, capabilities).
    MediaChanged(Arc<MediaSnapshot>),
    /// The highlighted lyric line changed. Carries only the index; the UI
    /// already holds the lyric document.
    LyricLineChanged {
        index: Option<usize>,
    },
    /// A new spectrum frame is ready (~30 fps while audio plays).
    Spectrum(Arc<SpectrumFrame>),
    /// The taskbar module's visual state changed.
    TaskbarChanged(Arc<TaskbarState>),
    /// A new clipboard entry was stored.
    ClipboardChanged,
    /// The todo list was mutated.
    TodoChanged,
    /// Windows light/dark theme changed.
    ThemeChanged,
}

impl Event {
    /// Coarse name used for logging and for skipping work in handlers.
    pub const fn name(&self) -> &'static str {
        match self {
            Event::ConfigChanged => "config-changed",
            Event::MediaChanged(_) => "media-changed",
            Event::LyricLineChanged { .. } => "lyric-line-changed",
            Event::Spectrum(_) => "spectrum",
            Event::TaskbarChanged(_) => "taskbar-changed",
            Event::ClipboardChanged => "clipboard-changed",
            Event::TodoChanged => "todo-changed",
            Event::ThemeChanged => "theme-changed",
        }
    }

    /// High-frequency events are never logged at info level.
    pub const fn is_high_frequency(&self) -> bool {
        matches!(self, Event::Spectrum(_))
    }
}

type Handler = Arc<dyn Fn(&Event) + Send + Sync + 'static>;

#[derive(Default)]
struct Inner {
    handlers: Vec<(u64, Handler)>,
}

/// Fan-out event bus. Cheap to clone; clones share the same subscriber list.
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<RwLock<Inner>>,
    next_id: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler and get back a subscription handle. Dropping the
    /// handle unsubscribes.
    pub fn subscribe(&self, handler: impl Fn(&Event) + Send + Sync + 'static) -> Subscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.write().handlers.push((id, Arc::new(handler)));
        Subscription {
            bus: self.clone(),
            id,
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner.read().handlers.len()
    }

    /// Deliver an event to every handler.
    ///
    /// The handler list is snapshotted under the read lock and released before
    /// dispatch, so a handler may subscribe or unsubscribe without deadlocking.
    /// A panicking handler is caught and dropped: one broken module must not
    /// take down the publisher thread of a long-running daemon.
    pub fn publish(&self, event: &Event) {
        let handlers: Vec<Handler> = {
            let guard = self.inner.read();
            guard.handlers.iter().map(|(_, h)| Arc::clone(h)).collect()
        };
        for handler in handlers {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(event))).is_err() {
                tracing::error!(event = event.name(), "event handler panicked; skipping");
            }
        }
    }

    fn remove(&self, id: u64) {
        self.inner.write().handlers.retain(|(hid, _)| *hid != id);
    }
}

/// Keeps a handler registered for as long as it is alive.
pub struct Subscription {
    bus: EventBus,
    id: u64,
}

impl Subscription {
    /// Stop receiving events before the handle is dropped.
    pub fn unsubscribe(self) {
        // `Drop` would do the same thing; this exists for explicitness.
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn publishes_to_all_subscribers() {
        let bus = EventBus::new();
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));

        let a2 = a.clone();
        let _s1 = bus.subscribe(move |_| {
            a2.fetch_add(1, Ordering::SeqCst);
        });
        let b2 = b.clone();
        let _s2 = bus.subscribe(move |_| {
            b2.fetch_add(1, Ordering::SeqCst);
        });

        bus.publish(&Event::ConfigChanged);
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropped_subscription_stops_receiving() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = count.clone();
        let sub = bus.subscribe(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        bus.publish(&Event::TodoChanged);
        drop(sub);
        bus.publish(&Event::TodoChanged);

        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn a_panicking_handler_does_not_stop_the_others() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));

        let _bad = bus.subscribe(|_| panic!("boom"));
        let c2 = count.clone();
        let _good = bus.subscribe(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        // Silence the default panic hook noise for this expected panic.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        bus.publish(&Event::ConfigChanged);
        std::panic::set_hook(prev);

        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
