//! Global System Media Transport Controls (GSMTC) wrapper.
//!
//! One thing to know before reading further: WinRT events do **not** give you
//! the current position continuously. Sessions report timeline updates every
//! few seconds at best, so the position shown here is interpolated from the
//! last reported value plus wall-clock time while playing. Everything else
//! (track, transport state, capabilities) is event-driven.

use beautify_core::model::{MediaSnapshot, PlaybackStatus};
use std::sync::Arc;
use std::time::Instant;
use windows::core::{Interface, Result as WinResult};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinStatus,
};
use windows::Storage::Streams::{DataReader, IInputStream};

/// A transport command issued by the widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

/// A `HANDLE` that any thread may signal.
///
/// WinRT delivers event callbacks on RPC threads, so the event used to wake the
/// watcher has to cross the thread boundary. A kernel event handle is safe to
/// signal from anywhere; only the Rust type is not `Send` by default. Cloning
/// shares the same handle and the last clone closes it.
#[derive(Clone)]
pub struct WakeEvent(Arc<WakeInner>);

struct WakeInner(isize);

// SAFETY: a Win32 event handle is a process-wide kernel object. `SetEvent` is
// documented as callable from any thread, and the handle is closed only after
// the last clone is dropped — which happens after the watcher thread joins.
unsafe impl Send for WakeInner {}
unsafe impl Sync for WakeInner {}

impl WakeEvent {
    /// Create a manual-reset event, so a signal arriving while the watcher is
    /// busy is not lost.
    pub fn new() -> Option<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        let handle = unsafe { CreateEventW(None, true, false, None) }.ok()?;
        Some(Self(Arc::new(WakeInner(handle.0 as isize))))
    }

    /// The underlying Win32 handle, for `WaitForMultipleObjects`.
    pub fn raw_handle(&self) -> *mut core::ffi::c_void {
        self.0 .0 as *mut core::ffi::c_void
    }

    pub fn signal(&self) {
        use windows::Win32::System::Threading::SetEvent;
        unsafe {
            let _ = SetEvent(windows::Win32::Foundation::HANDLE(self.raw_handle()));
        }
    }

    pub fn reset(&self) {
        use windows::Win32::System::Threading::ResetEvent;
        unsafe {
            let _ = ResetEvent(windows::Win32::Foundation::HANDLE(self.raw_handle()));
        }
    }

    /// Wait until signalled, up to `timeout_ms`.
    ///
    /// Returns true when the event was signalled (and resets it).
    pub fn wait(&self, timeout_ms: i32) -> bool {
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::WaitForSingleObject;
        let result = unsafe {
            WaitForSingleObject(HANDLE(self.raw_handle()), timeout_ms.max(0) as u32)
        };
        if result == WAIT_OBJECT_0 {
            self.reset();
            true
        } else {
            false
        }
    }
}

impl Drop for WakeInner {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            let _ = CloseHandle(HANDLE(self.0 as *mut core::ffi::c_void));
        }
    }
}

/// Live connection to the Windows media session layer.
///
/// Must be created, used and dropped on the same thread — it holds WinRT
/// objects whose apartment is that thread's.
pub struct GsmtcSession {
    manager: Manager,
    session: Option<Session>,
    /// Registration tokens for the handlers currently attached to `session`.
    /// WinRT hands back an opaque `i64`; the matching `Remove*` method takes it
    /// back.
    tokens: Vec<i64>,
    manager_token: Option<i64>,
    /// Last reported position and when we read it, for interpolation.
    anchor: Option<(i64, Instant)>,
    /// The position Windows reported on the previous refresh.
    ///
    /// Sessions repeat the same value for seconds at a time. Re-anchoring the
    /// interpolation on a repeat would pin the playhead to that stale value,
    /// which is exactly what stopped the lyric cursor from advancing.
    last_reported_ms: i64,
    /// Artwork for the track it belongs to, so we only decode it once.
    artwork_key: String,
    artwork: String,
    /// Album art often is not ready when the metadata arrives, so a failed
    /// fetch is retried a few times before the track is given up on.
    artwork_attempts: u32,
    artwork_next_try: Option<Instant>,
    last: MediaSnapshot,
    session_id: String,
}

impl GsmtcSession {
    pub fn new() -> WinResult<Self> {
        let manager = Manager::RequestAsync()?.join()?;
        Ok(Self {
            manager,
            session: None,
            tokens: Vec::new(),
            manager_token: None,
            anchor: None,
            last_reported_ms: 0,
            artwork_key: String::new(),
            artwork: String::new(),
            artwork_attempts: 0,
            artwork_next_try: None,
            last: MediaSnapshot::default(),
            session_id: String::new(),
        })
    }

    /// Attach change handlers that only signal `wake`.
    ///
    /// Callbacks run on RPC threads and must not touch the session objects, so
    /// signalling the event is the entire body of every handler.
    pub fn subscribe(&mut self, wake: WakeEvent) {
        if self.manager_token.is_none() {
            let w = wake.clone();
            let handler = TypedEventHandler::new(move |_, _| {
                w.signal();
                Ok(())
            });
            if let Ok(token) = self.manager.CurrentSessionChanged(&handler) {
                self.manager_token = Some(token);
            }
        }
        self.attach_session_handlers(wake);
    }

    fn attach_session_handlers(&mut self, wake: WakeEvent) {
        self.detach_session_handlers();
        let Some(session) = self.session.as_ref() else {
            return;
        };

        // The three subscriptions are identical apart from which event they
        // hang off, so they are written out rather than abstracted over.
        let w = wake.clone();
        let props = TypedEventHandler::new(move |_, _| {
            w.signal();
            Ok(())
        });
        if let Ok(token) = session.MediaPropertiesChanged(&props) {
            self.tokens.push(token);
        }
        let w = wake.clone();
        let playback = TypedEventHandler::new(move |_, _| {
            w.signal();
            Ok(())
        });
        if let Ok(token) = session.PlaybackInfoChanged(&playback) {
            self.tokens.push(token);
        }
        let w = wake.clone();
        let timeline = TypedEventHandler::new(move |_, _| {
            w.signal();
            Ok(())
        });
        if let Ok(token) = session.TimelinePropertiesChanged(&timeline) {
            self.tokens.push(token);
        }
    }

    fn detach_session_handlers(&mut self) {
        if let Some(session) = self.session.as_ref() {
            for token in self.tokens.drain(..) {
                // The token is only meaningful to the event it came from, but
                // removing an unknown token is a harmless no-op, so clearing
                // all three is safe and keeps the bookkeeping simple.
                let _ = session.RemoveMediaPropertiesChanged(token);
                let _ = session.RemovePlaybackInfoChanged(token);
                let _ = session.RemoveTimelinePropertiesChanged(token);
            }
        }
        self.tokens.clear();
    }

    /// Re-read everything from Windows and return the current snapshot.
    ///
    /// `artwork_budget` caps how many bytes of album art we are willing to
    /// base64 into the snapshot.
    pub fn refresh(&mut self, artwork_budget: usize) -> MediaSnapshot {
        // Re-resolve the session first: apps come and go constantly, and
        // `GetCurrentSession` is the only way to notice.
        match self.manager.GetCurrentSession() {
            Ok(session) => {
                let id = session
                    .SourceAppUserModelId()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                let changed = id != self.session_id || self.session.is_none();
                if changed {
                    self.detach_session_handlers();
                    self.session_id = id.clone();
                    self.session = Some(session);
                    self.anchor = None;
                    self.last_reported_ms = 0;
                    self.artwork_key.clear();
                    self.artwork.clear();
                    self.artwork_attempts = 0;
                    self.artwork_next_try = None;
                }
            }
            Err(_) => {
                // No session at all: `GetCurrentSession` reports a null pointer
                // as an error, which is the normal "nothing is playing" path.
                if self.session.is_some() {
                    self.detach_session_handlers();
                    self.session = None;
                    self.session_id.clear();
                    self.anchor = None;
                    self.last_reported_ms = 0;
                    self.artwork_key.clear();
                    self.artwork.clear();
                    self.artwork_attempts = 0;
                    self.artwork_next_try = None;
                }
                self.last = MediaSnapshot::default();
                return self.last.clone();
            }
        }

        let Some(session) = self.session.as_ref() else {
            self.last = MediaSnapshot::default();
            return self.last.clone();
        };

        let mut snapshot = MediaSnapshot {
            has_session: true,
            source_app: self.session_id.clone(),
            ..Default::default()
        };

        // --- track metadata -------------------------------------------------
        let mut artwork_source = None;
        match session.TryGetMediaPropertiesAsync().and_then(|op| op.join()) {
            Ok(props) => {
                snapshot.title = props.Title().map(|s| s.to_string_lossy()).unwrap_or_default();
                snapshot.artist = props.Artist().map(|s| s.to_string_lossy()).unwrap_or_default();
                snapshot.album = props
                    .AlbumTitle()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                artwork_source = props.Thumbnail().ok();
            }
            Err(e) => {
                tracing::debug!("media properties unavailable: {e}");
            }
        }

        // --- transport state and capabilities -------------------------------
        if let Ok(info) = session.GetPlaybackInfo() {
            if let Ok(status) = info.PlaybackStatus() {
                snapshot.status = match status {
                    WinStatus::Playing => PlaybackStatus::Playing,
                    WinStatus::Paused => PlaybackStatus::Paused,
                    WinStatus::Stopped => PlaybackStatus::Stopped,
                    WinStatus::Closed => PlaybackStatus::Closed,
                    _ => PlaybackStatus::Unknown,
                };
            }
            if let Ok(controls) = info.Controls() {
                snapshot.can_play = controls.IsPlayEnabled().unwrap_or(false);
                snapshot.can_pause = controls.IsPauseEnabled().unwrap_or(false);
                snapshot.can_skip_next = controls.IsNextEnabled().unwrap_or(false);
                snapshot.can_skip_previous = controls.IsPreviousEnabled().unwrap_or(false);
            }
        }

        // --- timeline, with interpolation ------------------------------------
        let mut reported_position = 0i64;
        if let Ok(timeline) = session.GetTimelineProperties() {
            if let Ok(pos) = timeline.Position() {
                reported_position = pos.Duration / 10_000;
            }
            if let Ok(end) = timeline.EndTime() {
                snapshot.duration_ms = end.Duration / 10_000;
            }
        }
        // Re-anchor only when Windows actually moved the playhead. A session
        // repeats the same reported position for seconds at a time, and
        // re-anchoring on every repeat would keep resetting the interpolated
        // clock back to that stale value — the playhead would never advance.
        if self.anchor.is_none() || reported_position != self.last_reported_ms {
            self.anchor = Some((reported_position, Instant::now()));
        }
        self.last_reported_ms = reported_position;
        snapshot.position_ms = self.position_now();

        // --- artwork, re-read only when the track changes ---------------------
        let key = format!(
            "{}\u{1}{}\u{1}{}",
            snapshot.title, snapshot.artist, snapshot.album
        );
        if key != self.artwork_key {
            self.artwork_key = key;
            self.artwork.clear();
            self.artwork_attempts = 0;
            // Media apps publish the metadata before the artwork is readable, so
            // the first attempt usually fails. Give it a moment, then retry.
            self.artwork_next_try = Some(Instant::now() + ARTWORK_FIRST_DELAY);
        }

        let due = self
            .artwork_next_try
            .map(|at| Instant::now() >= at)
            .unwrap_or(false);
        if self.artwork.is_empty() && due && self.artwork_attempts < ARTWORK_MAX_ATTEMPTS {
            self.artwork_attempts += 1;
            self.artwork = artwork_source
                .and_then(|reference| read_artwork(&reference, artwork_budget))
                .unwrap_or_default();
            self.artwork_next_try = if self.artwork.is_empty() {
                Some(Instant::now() + ARTWORK_RETRY_DELAY)
            } else {
                None
            };
            if self.artwork.is_empty() {
                tracing::debug!(
                    attempt = self.artwork_attempts,
                    "album art not readable yet; will retry"
                );
            } else {
                tracing::debug!(
                    attempt = self.artwork_attempts,
                    encoded_bytes = self.artwork.len(),
                    "album art resolved"
                );
            }
        }
        snapshot.artwork = self.artwork.clone();

        self.last = snapshot.clone();
        snapshot
    }

    /// Where playback is *right now*, interpolated from the last report.
    ///
    /// Sessions report their timeline only every few seconds, so the value
    /// WinRT hands back goes stale almost immediately. Anything that has to
    /// follow the music — the lyric cursor above all — must use this instead.
    pub fn position_now(&self) -> i64 {
        if !self.last.has_session {
            return 0;
        }
        let Some((base, at)) = self.anchor else {
            return self.last.position_ms;
        };
        interpolate(
            base,
            at.elapsed().as_millis() as i64,
            self.last.status == PlaybackStatus::Playing,
            self.last.duration_ms,
        )
    }

    /// Fold the interpolated position back into the stored snapshot, without
    /// touching WinRT.
    pub fn sync_position(&mut self) {
        if !self.last.has_session {
            return;
        }
        let position = self.position_now();
        if self.last.duration_ms > 0 {
            self.last.position_ms = position.min(self.last.duration_ms);
        } else {
            self.last.position_ms = position;
        }
    }

    pub fn has_session(&self) -> bool {
        self.last.has_session
    }

    pub fn snapshot(&self) -> &MediaSnapshot {
        &self.last
    }

    pub fn control(&self, command: Command) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let result = match command {
            Command::Play => session.TryPlayAsync().and_then(|op| op.join()),
            Command::Pause => session.TryPauseAsync().and_then(|op| op.join()),
            Command::Toggle => {
                if self.last.status == PlaybackStatus::Playing {
                    session.TryPauseAsync().and_then(|op| op.join())
                } else {
                    session.TryPlayAsync().and_then(|op| op.join())
                }
            }
            Command::Next => session.TrySkipNextAsync().and_then(|op| op.join()),
            Command::Previous => session.TrySkipPreviousAsync().and_then(|op| op.join()),
        };
        if let Err(e) = result {
            tracing::debug!("media command {command:?} failed: {e}");
        }
    }
}

/// Advance a reported playhead by the time since it was reported.
///
/// Split out from [`GsmtcSession::position_now`] because this is the piece that
/// decides whether lyrics follow the music, and it should be testable without
/// a live media session.
pub fn interpolate(base_ms: i64, elapsed_ms: i64, playing: bool, duration_ms: i64) -> i64 {
    if !playing {
        // Paused: the playhead is wherever the last report left it.
        return base_ms;
    }
    let position = base_ms.saturating_add(elapsed_ms.max(0));
    if duration_ms > 0 {
        position.min(duration_ms)
    } else {
        position
    }
}

/// How long to wait before the first album-art attempt, and between retries.
///
/// Apps publish `Title`/`Artist` well before `Thumbnail` is readable, so an
/// immediate first try fails for almost every track change.
const ARTWORK_FIRST_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const ARTWORK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(600);
const ARTWORK_MAX_ATTEMPTS: u32 = 12;

/// Read the album art into a `data:` URL the webview can render directly.
fn read_artwork(
    reference: &windows::Storage::Streams::IRandomAccessStreamReference,
    budget: usize,
) -> Option<String> {
    let stream = match reference.OpenReadAsync().ok().and_then(|op| op.join().ok()) {
        Some(stream) => stream,
        None => {
            tracing::debug!("artwork: OpenReadAsync failed");
            return None;
        }
    };
    // Media apps are not consistent here. QQ Music reports
    // "image/jpeg,image/jpe,image/jpg" — a comma-separated list — which would
    // corrupt the `data:` URL, because everything after the first comma is read
    // as the payload. Keep only the first type and insist it is an image.
    let content_type = stream
        .ContentType()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let content_type = content_type
        .split([',', ';'])
        .map(str::trim)
        .find(|part| part.starts_with("image/"))
        .unwrap_or("image/png")
        .to_string();
    let size = stream.Size().ok()? as usize;
    if size == 0 || size > budget {
        tracing::debug!(size, budget, "artwork: size rejected");
        return None;
    }

    let input: IInputStream = match stream.cast() {
        Ok(input) => input,
        Err(e) => {
            tracing::debug!("artwork: IInputStream cast failed: {e:?}");
            return None;
        }
    };
    let reader = DataReader::CreateDataReader(&input).ok()?;
    // `LoadAsync` is capped at u32; the budget check above keeps this honest.
    match reader.LoadAsync(size as u32).ok().and_then(|op| op.join().ok()) {
        Some(_) => {}
        None => {
            tracing::debug!("artwork: LoadAsync failed");
            return None;
        }
    }
    let mut buffer = vec![0u8; size];
    reader.ReadBytes(&mut buffer).ok()?;

    Some(format!(
        "data:{content_type};base64,{}",
        beautify_core::model::base64::encode(&buffer)
    ))
}

#[cfg(test)]
mod interpolation_tests {
    use super::interpolate;

    #[test]
    fn a_playing_playhead_advances_with_the_clock() {
        // The bug this guards: using the reported position directly makes the
        // lyric sit still between reports and then jump.
        assert_eq!(interpolate(1_000, 500, true, 0), 1_500);
        assert_eq!(interpolate(1_000, 0, true, 0), 1_000);
    }

    #[test]
    fn a_paused_playhead_does_not_move() {
        assert_eq!(interpolate(4_200, 9_999, false, 0), 4_200);
    }

    #[test]
    fn the_playhead_never_passes_the_end_of_the_track() {
        assert_eq!(interpolate(179_000, 5_000, true, 180_000), 180_000);
    }

    #[test]
    fn an_unknown_duration_is_not_treated_as_zero_length() {
        assert_eq!(interpolate(0, 12_345, true, 0), 12_345);
    }

    #[test]
    fn a_negative_elapsed_time_cannot_rewind_the_playhead() {
        // `Instant::elapsed` cannot go backwards, but a clock change could.
        assert_eq!(interpolate(1_000, -500, true, 0), 1_000);
    }
}
