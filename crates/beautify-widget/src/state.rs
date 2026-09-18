//! What the widget bar is currently showing.
//!
//! A snapshot of everything the renderer and the hit-tester read. The pump
//! thread owns one and the event bus writes into it, so it is kept behind a
//! lock and cloned only for the fields that are cheap to clone.

use beautify_core::config::Config;
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

    /// The single line the bar shows, plus whether it is the fallback.
    pub fn display_line(&self) -> (String, bool) {
        match self.current_lyric() {
            Some(line) => (line.to_string(), false),
            None => (self.track_line().unwrap_or_default(), true),
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
        let (line, fallback) = state.display_line();
        assert_eq!(line, "夜航西飞 - WinBeautify");
        assert!(fallback, "the caller needs to know to dim it");
    }

    #[test]
    fn a_timed_lyric_wins_over_the_track_name() {
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
        state.lyric_index = Some(1);
        let (line, fallback) = state.display_line();
        assert_eq!(line, "第二句");
        assert!(!fallback);
    }

    #[test]
    fn an_index_before_the_first_line_falls_back() {
        let mut state = with_media();
        state.lyrics = Arc::new(Lyrics {
            lines: vec![LyricLine {
                time_ms: 5000,
                text: "intro".into(),
            }],
            source: String::new(),
        });
        state.lyric_index = None;
        let (line, fallback) = state.display_line();
        assert_eq!(line, "夜航西飞 - WinBeautify");
        assert!(fallback);
    }

    #[test]
    fn disabling_lyrics_falls_back_even_with_a_timed_line() {
        let mut state = with_media();
        let mut cfg = (*state.config).clone();
        cfg.media.show_lyrics = false;
        state.config = Arc::new(cfg);
        state.lyrics = Arc::new(Lyrics {
            lines: vec![LyricLine {
                time_ms: 0,
                text: "第一句".into(),
            }],
            source: String::new(),
        });
        state.lyric_index = Some(0);
        assert!(state.current_lyric().is_none());
        assert!(state.display_line().1, "should show the fallback instead");
    }

    #[test]
    fn no_session_means_no_audio_component_at_all() {
        let state = WidgetState::new(Config::default());
        assert!(!state.has_audio());
        assert!(!state.content().audio);
        assert_eq!(state.display_line().0, "");
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
