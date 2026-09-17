//! Media module: session tracking, lyrics and the spectrum analyser.
//!
//! # Threading
//!
//! One dedicated thread owns every WinRT object. It blocks on a WinRT-backed
//! auto-reset event, which the GSMTC change handlers signal from their RPC
//! threads, so nothing polls the media session layer while the track is
//! unchanged. The wait timeout is recomputed on every pass to land exactly on
//! the next lyric line, which keeps the on-screen text in sync without a
//! 10 Hz timer.
//!
//! The spectrum analyser runs on a second thread of its own (see
//! [`spectrum`]) because it is driven by the audio engine rather than by WinRT.

pub mod http;
pub mod lrc;
pub mod lyrics;
pub mod providers;
pub mod session;
pub mod spectrum;

use beautify_core::config::{Config, LyricProvider};
use beautify_core::event::Event;
use beautify_core::model::{Lyrics, MediaSnapshot, PlaybackStatus, SpectrumFrame};
use beautify_core::module::{Module, ModuleContext, ModuleResult};
use beautify_core::paths;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use lyrics::LyricsResolver;
use session::{Command, GsmtcSession, WakeEvent};
use spectrum::{SpectrumAnalyzer, SpectrumOptions};

/// Never wait longer than this between safety refreshes.
const MAX_WAIT_MS: i32 = 2000;
/// Fastest lyric-tracking cadence, in milliseconds.
const MIN_WAIT_MS: i32 = 60;
/// Refuse to re-read GSMTC more often than this.
const MIN_REFRESH_MS: u128 = 350;

/// Settings the media thread consults.
#[derive(Debug, Clone, PartialEq)]
struct Options {
    enabled: bool,
    show_lyrics: bool,
    show_spectrum: bool,
    provider: LyricProvider,
    online_api: String,
    lyric_offset_ms: i32,
    poll_interval_ms: u64,
    demo_mode: bool,
    spectrum: SpectrumOptions,
}

impl Options {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.media.enabled,
            show_lyrics: cfg.media.show_lyrics,
            show_spectrum: cfg.media.show_spectrum,
            provider: cfg.media.lyric_provider,
            online_api: cfg.media.online_api.clone(),
            lyric_offset_ms: cfg.media.lyric_offset_ms,
            poll_interval_ms: cfg.media.poll_interval_ms,
            demo_mode: cfg.media.demo_mode,
            spectrum: SpectrumOptions {
                bars: beautify_core::model::SPECTRUM_BANDS,
                sensitivity: cfg.media.spectrum_sensitivity,
                smoothing: cfg.media.spectrum_smoothing,
            },
        }
    }
}

/// State shared with the UI layer.
struct Shared {
    bus: RwLock<Option<beautify_core::event::EventBus>>,
    options: RwLock<Options>,
    snapshot: RwLock<MediaSnapshot>,
    lyrics: RwLock<Lyrics>,
    lyric_index: AtomicI64,
    commands: Mutex<Vec<Command>>,
    /// `artist\u{1}title` of the track the current lyrics belong to, so the
    /// resolver is not consulted on every pass.
    resolved_for: RwLock<Option<String>>,
    stop: AtomicBool,
    running: AtomicBool,
}

impl Shared {
    fn publish(&self, event: Event) {
        if let Some(bus) = self.bus.read().clone() {
            bus.publish(&event);
        }
    }

    fn set_snapshot(&self, snapshot: MediaSnapshot) {
        let changed = {
            let mut guard = self.snapshot.write();
            if *guard == snapshot {
                false
            } else {
                // A moving playhead is not worth an event: nothing on screen
                // depends on it, and it changes several times a second.
                let only_position = {
                    let mut probe = guard.clone();
                    probe.position_ms = snapshot.position_ms;
                    probe == snapshot
                };
                *guard = snapshot.clone();
                !only_position
            }
        };
        if changed {
            self.publish(Event::MediaChanged(Arc::new(snapshot)));
        }
    }

    /// Highlight `index`, publishing only when it actually changed.
    fn set_lyric_index(&self, index: Option<usize>) {
        let current = index.map(|i| i as i64).unwrap_or(-1);
        let previous = self.lyric_index.swap(current, Ordering::AcqRel);
        if previous != current {
            tracing::debug!(index = current, "lyric cursor moved");
            self.publish(Event::LyricLineChanged { index });
        }
    }

    /// Forget the memoised track, e.g. after the provider setting changed.
    fn forget_track(&self) {
        *self.resolved_for.write() = None;
    }

    fn set_lyrics(&self, new_lyrics: Lyrics, index: Option<usize>) {
        *self.lyrics.write() = new_lyrics.clone();
        let previous = self
            .lyric_index
            .swap(index.map(|i| i as i64).unwrap_or(-1), Ordering::AcqRel);
        let current = index.map(|i| i as i64).unwrap_or(-1);
        if previous != current {
            self.publish(Event::LyricLineChanged { index });
        }
    }
}

/// The media module.
pub struct MediaModule {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    spectrum: Mutex<Option<SpectrumAnalyzer>>,
    /// Signal used to interrupt the media thread's wait. Held here so the UI
    /// thread can poke it for commands and shutdown.
    wake: RwLock<Option<WakeEvent>>,
}

impl Default for MediaModule {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaModule {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                bus: RwLock::new(None),
                options: RwLock::new(Options::from_config(&Config::default())),
                snapshot: RwLock::new(MediaSnapshot::default()),
                lyrics: RwLock::new(Lyrics::default()),
                lyric_index: AtomicI64::new(-1),
                commands: Mutex::new(Vec::new()),
                resolved_for: RwLock::new(None),
                stop: AtomicBool::new(false),
                running: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
            spectrum: Mutex::new(None),
            wake: RwLock::new(None),
        }
    }

    /// Current "now playing" snapshot.
    pub fn snapshot(&self) -> MediaSnapshot {
        self.shared.snapshot.read().clone()
    }

    /// Lyrics for the current track.
    pub fn lyrics(&self) -> Lyrics {
        self.shared.lyrics.read().clone()
    }

    /// Index of the highlighted lyric line, if any.
    pub fn lyric_index(&self) -> Option<usize> {
        match self.shared.lyric_index.load(Ordering::Acquire) {
            i if i < 0 => None,
            i => Some(i as usize),
        }
    }

    /// Is the spectrum capture actually running? False on machines without an
    /// audio output device.
    pub fn spectrum_running(&self) -> bool {
        self.spectrum
            .lock()
            .as_ref()
            .map(|s| s.is_running())
            .unwrap_or(false)
    }

    /// Queue a transport command. Safe to call from any thread.
    pub fn control(&self, command: Command) {
        self.shared.commands.lock().push(command);
        self.wake();
    }

    fn wake(&self) {
        if let Some(wake) = self.wake.read().as_ref() {
            wake.signal();
        }
    }
}

impl Module for MediaModule {
    fn name(&self) -> &'static str {
        "media"
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.media.enabled
    }

    fn start(&self, ctx: ModuleContext) -> ModuleResult {
        let mut thread_guard = self.thread.lock();
        if thread_guard.is_some() {
            return Ok(());
        }
        let cfg = ctx.config.get();
        *self.shared.bus.write() = Some(ctx.bus.clone());
        *self.shared.options.write() = Options::from_config(&cfg);
        self.shared.stop.store(false, Ordering::Release);
        self.shared.running.store(true, Ordering::Release);

        let wake = WakeEvent::new().ok_or("could not create the media wake event")?;
        *self.wake.write() = Some(wake.clone());

        if cfg.media.show_spectrum {
            let mut spectrum_guard = self.spectrum.lock();
            *spectrum_guard = SpectrumAnalyzer::start(ctx.bus.clone(), Options::from_config(&cfg).spectrum);
            if spectrum_guard.is_none() {
                tracing::warn!("spectrum analyser unavailable (no audio output device?)");
            }
        }

        let shared = Arc::clone(&self.shared);
        let wake_for_thread = wake.clone();
        let resolver = LyricsResolver::new(
            paths::lyrics_dir(),
            cfg.media.lyric_provider,
            cfg.media.online_api.clone(),
        );
        *thread_guard = Some(
            std::thread::Builder::new()
                .name("wb-media".into())
                .spawn(move || {
                    if let Err(e) = run_media_thread(shared, wake_for_thread, resolver) {
                        tracing::error!("media thread exited: {e}");
                    }
                })?,
        );
        Ok(())
    }

    fn apply(&self, config: &Config) -> ModuleResult {
        let next = Options::from_config(config);
        let changed = {
            let mut guard = self.shared.options.write();
            let changed = *guard != next;
            *guard = next.clone();
            changed
        };

        // The spectrum analyser owns a separate capture stream, so it is only
        // rebuilt when its switch actually flips.
        {
            let mut spectrum = self.spectrum.lock();
            match (config.media.show_spectrum, spectrum.is_some()) {
                (true, false) => {
                    if let Some(bus) = self.shared.bus.read().clone() {
                        *spectrum = SpectrumAnalyzer::start(bus, next.spectrum);
                    }
                }
                (false, true) => {
                    if let Some(mut analyzer) = spectrum.take() {
                        analyzer.stop();
                    }
                }
                (true, true) => {
                    if let Some(analyzer) = spectrum.as_ref() {
                        analyzer.set_options(next.spectrum);
                    }
                }
                (false, false) => {}
            }
        }

        if changed {
            self.wake();
        }
        Ok(())
    }

    fn stop(&self) -> ModuleResult {
        if let Some(mut analyzer) = self.spectrum.lock().take() {
            analyzer.stop();
        }
        let Some(handle) = self.thread.lock().take() else {
            return Ok(());
        };
        self.shared.stop.store(true, Ordering::Release);
        self.wake();
        let _ = handle.join();
        *self.wake.write() = None;
        self.shared.running.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for MediaModule {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run_media_thread(
    shared: Arc<Shared>,
    wake: WakeEvent,
    mut resolver: LyricsResolver,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    // MTA, not STA: WinRT delivers completion callbacks on RPC threads, and an
    // STA would need this thread to pump messages for `join()` to ever return.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut session = GsmtcSession::new().ok();
    if session.is_none() {
        tracing::warn!("media session manager unavailable; retrying when config changes");
    }
    let mut last_refresh = std::time::Instant::now() - std::time::Duration::from_secs(60);
    let demo_clock = std::time::Instant::now();

    while !shared.stop.load(Ordering::Acquire) {
        let options = shared.options.read().clone();

        // --- transport commands ------------------------------------------
        let commands: Vec<Command> = std::mem::take(&mut *shared.commands.lock());
        if !commands.is_empty() {
            if session.is_none() {
                session = GsmtcSession::new().ok();
            }
            if let Some(s) = session.as_mut() {
                for command in commands {
                    s.control(command);
                }
                s.refresh(ARTWORK_BUDGET);
                shared.set_snapshot(s.snapshot().clone());
            }
            last_refresh = std::time::Instant::now();
        }

        if !options.enabled {
            shared.set_snapshot(MediaSnapshot::default());
            shared.set_lyrics(Lyrics::default(), None);
            wake.wait(MAX_WAIT_MS);
            continue;
        }

        // --- demo mode short-circuits the real session ---------------------
        if options.demo_mode {
            let snapshot = demo_snapshot(demo_clock.elapsed().as_millis() as i64);
            let lyrics = demo_lyrics();
            let index = lyrics.index_at(snapshot.position_ms);
            shared.set_snapshot(snapshot);
            shared.set_lyrics(lyrics, index);
            publish_demo_spectrum(&shared, &options, demo_clock.elapsed().as_millis());
            wake.wait(100);
            continue;
        }

        // --- refresh the session when Windows says something changed -------
        //
        // Note what this branch does *not* do: it never touches the lyric
        // cursor. Refreshing only re-reads WinRT, whose timeline is a few
        // seconds stale, and treating that as the playhead is what made the
        // lyric sit on one line and then jump.
        let signalled = wake.wait(next_wait_ms(&shared, &options));
        let due = last_refresh.elapsed().as_millis() >= options.poll_interval_ms as u128;
        if signalled || due {
            if session.is_none() {
                session = GsmtcSession::new().ok();
            }
            if let Some(s) = session.as_mut() {
                if last_refresh.elapsed().as_millis() >= MIN_REFRESH_MS || signalled {
                    s.subscribe(wake.clone());
                    let snapshot = s.refresh(ARTWORK_BUDGET);
                    shared.set_snapshot(snapshot);
                    last_refresh = std::time::Instant::now();
                }
            }
        }

        // --- advance the cursor on every pass, refresh or not ---------------
        advance_cursor(&shared, &mut resolver, session.as_mut(), &options);
    }

    drop(session);
    unsafe { CoUninitialize() };
    Ok(())
}

/// How much album art we are willing to base64 into a snapshot.
const ARTWORK_BUDGET: usize = 512 * 1024;

/// Sleep until the next lyric boundary, clamped to a sane range.
fn next_wait_ms(shared: &Shared, options: &Options) -> i32 {
    let snapshot = shared.snapshot.read().clone();
    if !snapshot.has_session || snapshot.status != PlaybackStatus::Playing {
        return (options.poll_interval_ms as i32).clamp(MIN_WAIT_MS, MAX_WAIT_MS);
    }
    if !options.show_lyrics {
        return MAX_WAIT_MS;
    }
    let lyrics = shared.lyrics.read().clone();
    if lyrics.is_empty() {
        return MAX_WAIT_MS;
    }
    let position = snapshot.position_ms + options.lyric_offset_ms as i64;
    match lyrics.next(position) {
        // Wake a hair after the boundary so the line is definitely due.
        Some(line) => (line.time_ms - position).clamp(MIN_WAIT_MS as i64, MAX_WAIT_MS as i64) as i32,
        None => MAX_WAIT_MS,
    }
}

/// Resolve lyrics for the current track if needed, then move the highlight to
/// wherever the interpolated playhead is.
///
/// Called on every pass of the media loop, so it must be cheap: the resolver
/// is consulted once per track and the index lookup is a binary search under
/// a read lock.
fn advance_cursor(
    shared: &Arc<Shared>,
    resolver: &mut LyricsResolver,
    session: Option<&mut GsmtcSession>,
    options: &Options,
) {
    let Some(session) = session else {
        shared.set_lyrics(Lyrics::default(), None);
        shared.forget_track();
        return;
    };
    if !session.has_session() {
        shared.set_lyrics(Lyrics::default(), None);
        shared.forget_track();
        return;
    }

    let snapshot = session.snapshot().clone();
    // A control character as separator, so "A" + "B" cannot collide with
    // a title that happens to contain the separator itself.
    let key = format!("{artist}{title}", artist = snapshot.artist, title = snapshot.title);
    let current = shared.resolved_for.read().clone();

    if !options.show_lyrics {
        if current.is_some() {
            shared.set_lyrics(Lyrics::default(), None);
            shared.forget_track();
        }
    } else if current.as_deref() != Some(key.as_str()) {
        // A new track: resolve, and remember which track these lines belong
        // to so the next pass costs nothing.
        resolver.set_provider(options.provider, options.online_api.clone());
        let lyrics = resolver.resolve(
            &snapshot.artist,
            &snapshot.title,
            &snapshot.album,
            snapshot.duration_ms,
        );
        tracing::debug!(
            track = %key,
            lines = lyrics.lines.len(),
            source = %lyrics.source,
            "lyrics resolved"
        );
        *shared.resolved_for.write() = Some(key);
        shared.set_lyrics(lyrics, None);
    }

    // The interpolated clock, not WinRT's stale reported position.
    let position = session.position_now() + options.lyric_offset_ms as i64;
    // `index_at` returns an index, so the read lock is released here.
    let index = shared.lyrics.read().index_at(position);
    shared.set_lyric_index(index);

    // Keep the stored playhead moving for anything that shows it.
    session.sync_position();
    *shared.snapshot.write() = session.snapshot().clone();
}

// ---------------------------------------------------------------------------
// Preview mode
// ---------------------------------------------------------------------------

const DEMO_TRACK: &str = "夜航西飞";
const DEMO_ARTIST: &str = "WinBeautify";
const DEMO_ALBUM: &str = "Preview";

fn demo_snapshot(elapsed_ms: i64) -> MediaSnapshot {
    const LOOP_MS: i64 = 48_000;
    let position = elapsed_ms % LOOP_MS;
    MediaSnapshot {
        has_session: true,
        title: DEMO_TRACK.to_string(),
        artist: DEMO_ARTIST.to_string(),
        album: DEMO_ALBUM.to_string(),
        source_app: "WinBeautify.Preview".to_string(),
        status: PlaybackStatus::Playing,
        position_ms: position,
        duration_ms: LOOP_MS,
        can_play: true,
        can_pause: true,
        can_skip_next: true,
        can_skip_previous: true,
        artwork: String::new(),
    }
}

/// A short synthetic LRC so preview mode exercises the real lyric pipeline
/// (parsing, offsetting, cursor lookup) rather than a stub.
const DEMO_LRC: &str = "\
[00:00.00]WinBeautify 预览模式
[00:04.00]这段文字用来预览歌词滚动效果
[00:08.50]单行居中显示，超出宽度自动省略
[00:13.00]指针移到组件上会显示播放控制
[00:18.00]右侧的频谱来自真实音频回放捕获
[00:23.00]没有播放声音时它会安静地停下
[00:28.00]把指针移开，控制按钮会让位给歌词
[00:34.00]以上均为演示内容
[00:40.00]在设置中心关闭预览模式即可恢复
[00:45.00]感谢使用 WinBeautify
";

fn demo_lyrics() -> Lyrics {
    let mut parsed = lrc::parse(DEMO_LRC);
    parsed.source = "demo".to_string();
    parsed
}

/// Deterministic pseudo-audio so the preview looks alive without playing
/// anything. A couple of sine oscillators plus a beat envelope is enough to
/// exercise the widget's rendering.
fn publish_demo_spectrum(shared: &Shared, options: &Options, elapsed_ms: u128) {
    if !options.show_spectrum {
        return;
    }
    let t = elapsed_ms as f32 / 1000.0;
    let bars = options.spectrum.bars.max(4);
    let beat = (t * 2.4).fract();
    let kick = (1.0 - beat).powi(6);
    let mut bands = Vec::with_capacity(bars);

    for i in 0..bars {
        let f = i as f32 / bars as f32;
        let tone = (t * (1.1 + f * 3.0) + i as f32 * 0.7).sin() * 0.5 + 0.5;
        // Bass-heavy tilt with the kick drum on the low third.
        let tilt = (1.0 - f).powf(1.6);
        let value = (tone * 0.55 + kick * tilt * 0.75).clamp(0.0, 1.0);
        bands.push(value);
    }

    shared.publish(Event::Spectrum(Arc::new(SpectrumFrame {
        bands,
        level: 0.5,
    })));
}
