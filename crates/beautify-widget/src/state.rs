//! What the widget bar is currently showing.
//!
//! A snapshot of everything the renderer and the hit-tester read. The pump
//! thread owns one and the event bus writes into it, so it is kept behind a
//! lock and cloned only for the fields that are cheap to clone.

use beautify_core::config::{Config, LyricStyle};
use beautify_core::model::{Lyrics, MediaSnapshot, SpectrumFrame};
use std::sync::Arc;

use crate::layout::{Content, Hit, Limits};

/// Everything the widget draws from.
#[derive(Clone)]
pub struct WidgetState {
    pub config: Arc<Config>,
    pub media: Arc<MediaSnapshot>,
    pub lyrics: Arc<Lyrics>,
    pub lyric_index: Option<usize>,
    pub spectrum: Arc<SpectrumFrame>,
    /// Open task count, for the launcher badge.
    pub open_tasks: i64,
    pub hover: Option<Hit>,
    pub flyout_open: bool,
}

impl WidgetState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            media: Arc::new(MediaSnapshot::default()),
            lyrics: Arc::new(Lyrics::default()),
            lyric_index: None,
            spectrum: Arc::new(SpectrumFrame::default()),
            open_tasks: 0,
            hover: None,
            flyout_open: false,
        }
    }

    pub fn limits(&self) -> Limits {
        Limits::from_config(&self.config)
    }

    /// Which parts of the bar are laid out at all.
    pub fn content(&self) -> Content {
        let cfg = &self.config;
        Content {
            audio: self.has_audio(),
            show_spectrum: cfg.media.enabled && cfg.media.show_spectrum,
            show_lyrics: cfg.media.enabled && cfg.media.show_lyrics,
            show_badge: cfg.todo.enabled && cfg.todo.show_badge && self.open_tasks > 0,
            launcher_trailing: cfg.widget.anchor.flyout_button_trailing(),
        }
    }

    pub fn has_audio(&self) -> bool {
        self.config.media.enabled && self.media.has_session
    }

    /// The lyric line to show, or `None` when there is nothing timed yet.
    pub fn current_lyric(&self) -> Option<&str> {
        if !self.content().show_lyrics {
            return None;
        }
        let index = self.lyric_index?;
        let line = self.lyrics.lines.get(index)?;
        (!line.text.trim().is_empty()).then_some(line.text.as_str())
    }

    /// `歌曲名 - 歌手`, shown while a track has no timed lyric yet.
    pub fn track_line(&self) -> Option<String> {
        if !self.media.has_session {
            return None;
        }
        let title = self.media.title.trim();
        let artist = self.media.artist.trim();
        let line = match (title.is_empty(), artist.is_empty()) {
            (false, false) => format!("{title} - {artist}"),
            (false, true) => title.to_string(),
            (true, false) => artist.to_string(),
            (true, true) => return None,
        };
        Some(line)
    }

    /// The lines the bar draws, in order, each with whether it is the dimmed
    /// fallback rather than live content.
    ///
    /// 「歌名+歌词」 splits the display in two — 「歌名 - 歌手」 on top, the
    /// timed lyric below. 「仅歌词」 keeps the historic single line. Both fall
    /// back to the track line alone while nothing timed is on screen, and both
    /// draw nothing when lyrics are off or no session is playing.
    pub fn display_lines(&self) -> Vec<(String, bool)> {
        if !self.content().show_lyrics {
            return Vec::new();
        }
        let track = self.track_line();
        match (self.config.media.lyric_style, self.current_lyric()) {
            (LyricStyle::TitleAndLyrics, Some(lyric)) => {
                // The title line only earns a row when there is a lyric under
                // it; on its own it is just the single-line fallback.
                let mut lines = Vec::with_capacity(2);
                if let Some(t) = track {
                    lines.push((t, true));
                }
                lines.push((lyric.to_string(), false));
                lines
            }
            (_, Some(lyric)) => vec![(lyric.to_string(), false)],
            (_, None) => track
                .map(|t| vec![(t, true)])
                .unwrap_or_default(),
        }
    }

    pub fn is_playing(&self) -> bool {
        self.media.status == beautify_core::model::PlaybackStatus::Playing
    }

    /// Spectrum bands, padded or truncated to the fixed band count.
    pub fn spectrum_bars(&self) -> Vec<f32> {
        let mut bands = self.spectrum.bands.clone();
        bands.resize(crate::layout::SPECTRUM_BARS, 0.0);
        bands.truncate(crate::layout::SPECTRUM_BARS);
        bands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beautify_core::model::{LyricLine, PlaybackStatus};

    fn with_media() -> WidgetState {
        let mut state = WidgetState::new(Config::default());
        state.media = Arc::new(MediaSnapshot {
            has_session: true,
            title: "夜航西飞".into(),
            artist: "WinBeautify".into(),
            status: PlaybackStatus::Playing,
            ..Default::default()
        });
        state
    }

    #[test]
    fn a_track_without_lyrics_falls_back_to_title_and_artist() {
        let state = with_media();
        assert_eq!(
            state.display_lines(),
            vec![("夜航西飞 - WinBeautify".to_string(), true)],
            "the caller needs to know to dim it"
        );
    }

    #[test]
    fn title_and_lyrics_is_two_lines_once_something_is_timed() {
        let mut state = with_lyrics();
        state.lyric_index = Some(1);
        assert_eq!(
            state.display_lines(),
            vec![
                ("夜航西飞 - WinBeautify".to_string(), true),
                ("第二句".to_string(), false),
            ]
        );
    }

    #[test]
    fn lyrics_only_keeps_the_historic_single_line() {
        let mut state = with_lyrics();
        state.lyric_index = Some(1);
        let mut cfg = (*state.config).clone();
        cfg.media.lyric_style = LyricStyle::LyricsOnly;
        state.config = Arc::new(cfg);
        assert_eq!(state.display_lines(), vec![("第二句".to_string(), false)]);
    }

    #[test]
    fn the_title_line_only_exists_when_there_is_a_lyric_under_it() {
        // The default style is title-and-lyrics; with nothing timed yet the
        // track name must not appear twice or claim a second row for itself.
        let mut state = with_lyrics();
        state.lyric_index = None;
        assert_eq!(
            state.display_lines(),
            vec![("夜航西飞 - WinBeautify".to_string(), true)]
        );

        // A track with neither title nor artist cannot produce the top line.
        state.lyric_index = Some(0);
        state.media = Arc::new(MediaSnapshot {
            has_session: true,
            title: String::new(),
            artist: String::new(),
            status: PlaybackStatus::Playing,
            ..Default::default()
        });
        assert_eq!(
            state.display_lines(),
            vec![("第一句".to_string(), false)],
            "a nameless track degrades to the lyric alone"
        );
    }

    fn with_lyrics() -> WidgetState {
        let mut state = with_media();
        state.lyrics = Arc::new(Lyrics {
            lines: vec![
                LyricLine {
                    time_ms: 0,
                    text: "第一句".into(),
                },
                LyricLine {
                    time_ms: 4000,
                    text: "第二句".into(),
                },
            ],
            source: "local".into(),
        });
        state
    }

    #[test]
    fn an_index_before_the_first_line_falls_back() {
        let mut state = with_lyrics();
        state.lyrics = Arc::new(Lyrics {
            lines: vec![LyricLine {
                time_ms: 5000,
                text: "intro".into(),
            }],
            source: String::new(),
        });
        state.lyric_index = None;
        assert_eq!(
            state.display_lines(),
            vec![("夜航西飞 - WinBeautify".to_string(), true)]
        );
    }

    #[test]
    fn disabling_lyrics_shows_nothing_even_with_a_timed_line() {
        let mut state = with_lyrics();
        let mut cfg = (*state.config).clone();
        cfg.media.show_lyrics = false;
        state.config = Arc::new(cfg);
        state.lyric_index = Some(0);
        assert!(state.current_lyric().is_none());
        assert!(
            state.display_lines().is_empty(),
            "lyrics off means no lyric lines at all"
        );
    }

    #[test]
    fn no_session_means_no_audio_component_at_all() {
        let state = WidgetState::new(Config::default());
        assert!(!state.has_audio());
        assert!(!state.content().audio);
        assert!(state.display_lines().is_empty());
    }

    #[test]
    fn the_badge_only_shows_when_there_is_something_to_count() {
        let mut state = WidgetState::new(Config::default());
        assert!(!state.content().show_badge);
        state.open_tasks = 3;
        assert!(state.content().show_badge);
    }

    #[test]
    fn spectrum_is_padded_and_truncated_to_the_requested_bar_count() {
        let mut state = WidgetState::new(Config::default());
        state.spectrum = Arc::new(SpectrumFrame {
            bands: vec![0.1, 0.2, 0.3],
            level: 0.3,
        });
        let bars = state.spectrum_bars();
        assert_eq!(bars.len(), crate::layout::SPECTRUM_BARS);
        assert_eq!(bars[0], 0.1);
        assert_eq!(bars[3], 0.0, "missing bands pad with silence");
    }

    /// The default bar has no pill, so the theme's pill colour must not leak
    /// into it. This is the regression that made a faded-out background
    /// unreadable: the glyph colour used to be derived from the pill.
    #[test]
    fn the_default_bar_has_no_background_to_derive_a_colour_from() {
        let state = WidgetState::new(Config::default());
        assert_eq!(state.config.widget.opacity, 0.0);
        let theme = crate::theme::Theme::resolve(&state.config);
        assert_eq!(theme.pill.a, 0.0);
        assert!(
            theme.foreground.a > 0.9,
            "the glyphs are still there: they are the whole bar"
        );
    }
}
