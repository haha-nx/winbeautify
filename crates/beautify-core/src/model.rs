//! Cross-module data model.
//!
//! These types are the wire format between native modules and the web UI, so
//! they carry `serde` derives and keep field names stable.

use serde::{Deserialize, Serialize};

/// Transport state reported by the Global System Media Transport Controls API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaybackStatus {
    /// No app is currently publishing a media session.
    #[default]
    Closed,
    Stopped,
    Paused,
    Playing,
    /// The session exists but Windows has not reported a status yet.
    Unknown,
}

impl PlaybackStatus {
    pub const fn is_active(self) -> bool {
        matches!(self, PlaybackStatus::Playing | PlaybackStatus::Paused)
    }
}

/// A snapshot of "what is playing right now".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaSnapshot {
    pub has_session: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Source app's `AppUserModelId`, e.g. `Spotify.exe`.
    pub source_app: String,
    pub status: PlaybackStatus,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_skip_next: bool,
    pub can_skip_previous: bool,
    /// `data:image/png;base64,...` for the album art, empty when unavailable.
    pub artwork: String,
}

impl MediaSnapshot {
    /// The string the widget shows when no lyric is available.
    pub fn display_line(&self) -> String {
        match (self.title.is_empty(), self.artist.is_empty()) {
            (false, false) => format!("{} - {}", self.title, self.artist),
            (false, true) => self.title.clone(),
            (true, false) => self.artist.clone(),
            (true, true) => String::new(),
        }
    }

    pub fn is_playing(&self) -> bool {
        self.status == PlaybackStatus::Playing
    }
}

/// One timestamped lyric line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    /// Milliseconds from the start of the track.
    pub time_ms: i64,
    pub text: String,
}

/// A parsed lyric document plus the current playback cursor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lyrics {
    /// Sorted ascending by `time_ms`.
    pub lines: Vec<LyricLine>,
    /// Where the text came from: `"local"`, `"cache"`, `"online"` or `""`.
    pub source: String,
}

impl Lyrics {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Index of the line that should be highlighted at `position_ms`.
    ///
    /// Returns `None` before the first timestamp so the widget can keep showing
    /// the track name during the intro.
    pub fn index_at(&self, position_ms: i64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        // `partition_point` gives the count of lines strictly before the
        // cursor, which is exactly the index we want.
        let idx = self.lines.partition_point(|l| l.time_ms <= position_ms);
        if idx == 0 {
            None
        } else {
            Some(idx - 1)
        }
    }

    pub fn current(&self, position_ms: i64) -> Option<&LyricLine> {
        self.index_at(position_ms).map(|i| &self.lines[i])
    }

    pub fn next(&self, position_ms: i64) -> Option<&LyricLine> {
        self.lines
            .iter()
            .find(|l| l.time_ms > position_ms)
    }
}

/// How many bands the spectrum has, end to end.
///
/// Shared so the analyser and the renderer cannot disagree: producing 16
/// bands for a display that draws 8 would just waste FFT work.
pub const SPECTRUM_BANDS: usize = 8;

/// One frame of the spectrum analyser.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpectrumFrame {
    /// Normalised band magnitudes in `0.0..=1.0`, low frequency first.
    pub bands: Vec<f32>,
    /// Overall level, used to fade the bars out on silence.
    pub level: f32,
}

/// Why the taskbar is currently in a particular visual state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskbarVisualState {
    /// The configured mode is applied.
    Applied,
    /// Dynamic mode kicked in because a window is maximised.
    Dynamic,
    /// Auto-hide has retracted the taskbar.
    Hidden,
    /// A fullscreen app is foreground and the taskbar is untouched.
    Fullscreen,
    /// The feature is switched off or the shell is not running.
    Disabled,
}

/// Everything the settings UI needs to describe the taskbar module's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskbarState {
    pub state: TaskbarVisualState,
    pub mode: String,
    pub secondary_bars: u32,
    /// Taskbar rectangle in physical pixels, when known.
    pub rect: Option<crate::geometry::Rect>,
    pub autohide: bool,
    /// True when this Windows build draws the taskbar itself and ignores the
    /// composition API, so `mode` has no visible effect. See
    /// `beautify_taskbar::winver`.
    pub shell_managed: bool,
}

impl Default for TaskbarState {
    fn default() -> Self {
        Self {
            state: TaskbarVisualState::Disabled,
            mode: "normal".to_string(),
            secondary_bars: 0,
            rect: None,
            autohide: false,
            shell_managed: false,
        }
    }
}

/// Which flyout tab is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlyoutTab {
    #[default]
    Todo,
    Clipboard,
}

/// Minimal base64 (standard alphabet, with padding).
///
/// Hand-rolled because it is the only encoding WinBeautify needs and a
/// dependency for 30 lines of table lookup is not worth the build time.
pub mod base64 {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;

            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Decode standard base64, ignoring whitespace and tolerating missing
    /// padding. Returns `None` on any character outside the alphabet.
    pub fn decode(input: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(input.len() / 4 * 3);
        let mut accumulator: u32 = 0;
        let mut bits = 0u32;
        let mut padding = 0usize;

        for byte in input.bytes() {
            if byte.is_ascii_whitespace() {
                continue;
            }
            if byte == b'=' {
                padding += 1;
                continue;
            }
            // Anything after padding is malformed.
            if padding > 0 {
                return None;
            }
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            } as u32;

            accumulator = (accumulator << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((accumulator >> bits) as u8);
            }
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rfc4648_vectors() {
            assert_eq!(encode(b""), "");
            assert_eq!(encode(b"f"), "Zg==");
            assert_eq!(encode(b"fo"), "Zm8=");
            assert_eq!(encode(b"foo"), "Zm9v");
            assert_eq!(encode(b"foob"), "Zm9vYg==");
            assert_eq!(encode(b"fooba"), "Zm9vYmE=");
            assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        }

        #[test]
        fn decode_is_the_inverse_of_encode() {
            for case in [
                &b""[..],
                b"f",
                b"fo",
                b"foo",
                b"foob",
                b"fooba",
                b"foobar",
                &[0u8, 255, 128, 1, 2, 3][..],
            ] {
                let encoded = encode(case);
                assert_eq!(decode(&encoded).as_deref(), Some(case), "{encoded}");
            }
        }

        #[test]
        fn decode_tolerates_whitespace_and_missing_padding() {
            assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
            assert_eq!(decode("Zm9v
 YmFy").unwrap(), b"foobar");
            assert_eq!(decode("Zg").unwrap(), b"f");
        }

        #[test]
        fn decode_rejects_junk() {
            assert!(decode("not base64!").is_none());
            assert!(decode("Zm9v=YmFy").is_none(), "data after padding");
        }
    }
}
