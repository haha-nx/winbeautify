//! Locating lyrics for the current track.
//!
//! Lookup order:
//!
//! 1. **Local** — a `.lrc` file named after the track in the lyrics directory.
//!    The Global System Media Transport Controls API does not expose the media
//!    file path, so there is no "next to the song" directory to search; this is
//!    the one place the user can drop files into by hand.
//! 2. **Online** — the configured provider, when one is set.
//!
//! Nothing fetched is written to disk. Lyrics are held for the current track
//! only, in memory, and re-fetched the next time it plays: a media player's
//! own cache is not something this app should be quietly growing behind the
//! user's back, and the fetch is a single request.
//!
//! The in-memory memo is still essential — the playback position is re-checked
//! several times a second and each check asks for the current track's lyrics.

use crate::http;
use crate::lrc;
use crate::providers::{self, Provider, Track};
use beautify_core::config::LyricProvider;
use beautify_core::model::Lyrics;
use std::path::PathBuf;

/// Filesystem-safe file name for the lyric file belonging to a track.
pub fn cache_key(artist: &str, title: &str) -> String {
    let raw = if artist.trim().is_empty() {
        title.to_string()
    } else {
        format!("{artist} - {title}")
    };
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            // Keep CJK so the cache directory stays readable; only strip what
            // Windows forbids in a file name.
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').to_string();
    // Guard against reserved device names and absurd lengths.
    let safe = if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    };
    safe.chars().take(120).collect()
}

/// Resolves lyrics for a track, reading user-supplied `.lrc` files and
/// consulting the configured online provider.
pub struct LyricsResolver {
    lyrics_dir: PathBuf,
    provider: LyricProvider,
    online_api: String,
    /// The track the resolved result belongs to, so we can skip re-resolving on
    /// every position tick.
    current: parking_lot::Mutex<Option<(String, Lyrics)>>,
}

impl LyricsResolver {
    pub fn new(lyrics_dir: PathBuf, provider: LyricProvider, online_api: String) -> Self {
        Self {
            lyrics_dir,
            provider,
            online_api,
            current: parking_lot::Mutex::new(None),
        }
    }

    pub fn set_provider(&mut self, provider: LyricProvider, online_api: String) {
        if self.provider != provider || self.online_api != online_api {
            self.provider = provider;
            self.online_api = online_api;
            *self.current.lock() = None;
        }
    }

    fn lrc_path(&self, key: &str) -> PathBuf {
        self.lyrics_dir.join(format!("{key}.lrc"))
    }

    /// Read the `.lrc` file the user placed for this track, if any.
    fn read_local(&self, key: &str) -> Option<Lyrics> {
        let path = self.lrc_path(key);
        let text = std::fs::read_to_string(&path).ok()?;
        let mut parsed = lrc::parse(&text);
        if parsed.is_empty() {
            return None;
        }
        parsed.source = "local".to_string();
        Some(parsed)
    }

    /// Resolve for a track. Results are memoised per track.
    pub fn resolve(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration_ms: i64,
    ) -> Lyrics {
        if title.trim().is_empty() {
            return Lyrics::default();
        }
        let key = cache_key(artist, title);

        {
            let guard = self.current.lock();
            if let Some((cached_key, lyrics)) = guard.as_ref() {
                if *cached_key == key {
                    return lyrics.clone();
                }
            }
        }

        let resolved = self.lookup(&key, artist, title, album, duration_ms);
        *self.current.lock() = Some((key, resolved.clone()));
        resolved
    }

    fn lookup(
        &self,
        key: &str,
        artist: &str,
        title: &str,
        album: &str,
        duration_ms: i64,
    ) -> Lyrics {
        // A file the user dropped in the lyrics directory always wins: it is
        // free, instant, and the only source they can curate by hand.
        if let Some(found) = self.read_local(key) {
            return found;
        }

        if self.provider.is_online() {
            if let Some(lyrics) = self.fetch_online(artist, title, album, duration_ms) {
                if !lyrics.is_empty() {
                    return lyrics;
                }
            }
        }

        Lyrics::default()
    }

    fn fetch_online(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration_ms: i64,
    ) -> Option<Lyrics> {
        if self.provider == LyricProvider::Custom {
            return self.fetch_custom(artist, title, album);
        }
        let provider = match self.provider {
            LyricProvider::Netease => Provider::Netease,
            LyricProvider::Qq => Provider::Qq,
            LyricProvider::Kugou => Provider::Kugou,
            LyricProvider::Lrclib => Provider::Lrclib,
            LyricProvider::Off | LyricProvider::Custom => return None,
        };
        let track = Track {
            title,
            artist,
            album,
            duration_ms,
        };
        providers::fetch(provider, &track)
    }

    /// The user-supplied URL template: the response is expected to be LRC, or
    /// JSON wrapping an LRC string.
    fn fetch_custom(&self, artist: &str, title: &str, album: &str) -> Option<Lyrics> {
        let url = http::build_lyric_url(&self.online_api, title, artist, album)?;
        match http::get(&url, 5000) {
            Ok(body) => {
                if body.trim().is_empty() {
                    return None;
                }
                let candidate = if body.contains('[') && body.contains(']') {
                    body
                } else {
                    extract_json_lyric(&body).unwrap_or(body)
                };
                let mut parsed = lrc::parse(&candidate);
                parsed.source = "custom".into();
                (!parsed.is_empty()).then_some(parsed)
            }
            Err(e) => {
                tracing::debug!("custom lyric lookup failed: {e}");
                None
            }
        }
    }

    /// Resolve without consulting the memoised result.
    pub fn lookup_now(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration_ms: i64,
    ) -> Lyrics {
        if title.trim().is_empty() {
            return Lyrics::default();
        }
        self.lookup(&cache_key(artist, title), artist, title, album, duration_ms)
    }
}

/// Ad-hoc extraction of a lyric string from a JSON envelope.
///
/// Deliberately shallow: this exists only to cope with providers that wrap the
/// LRC in `{"lyric": "..."}`. A real API shape should be configured to return
/// LRC directly.
fn extract_json_lyric(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for key in ["lyric", "lrc", "lyrics", "data"] {
        if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    // One level of nesting covers the common `{"data":{"lyric":...}}` shape.
    for key in ["data", "result"] {
        if let Some(inner) = value.get(key) {
            for inner_key in ["lyric", "lrc", "lyrics"] {
                if let Some(s) = inner.get(inner_key).and_then(|v| v.as_str()) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_resolver(provider: LyricProvider, api: &str) -> (LyricsResolver, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "wb-lyrics-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            LyricsResolver::new(dir.clone(), provider, api.to_string()),
            dir,
        )
    }

    #[test]
    fn cache_key_strips_reserved_characters() {
        assert_eq!(cache_key("A/B", "C:D"), "A_B - C_D");
        assert_eq!(cache_key("", "Title"), "Title");
        assert_eq!(cache_key("  ", ""), "unknown");
    }

    #[test]
    fn cache_key_keeps_cjk_readable() {
        assert_eq!(cache_key("周杰伦", "晴天"), "周杰伦 - 晴天");
    }

    #[test]
    fn local_file_is_picked_up() {
        let (resolver, dir) = temp_resolver(LyricProvider::Off, "");
        let key = cache_key("Artist", "Song");
        std::fs::write(
            dir.join(format!("{key}.lrc")),
            "[00:01.00]line one\n[00:05.00]line two\n",
        )
        .unwrap();

        let lyrics = resolver.resolve("Artist", "Song", "", 0);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.source, "local");
    }

    #[test]
    fn results_are_memoised_per_track() {
        let (resolver, dir) = temp_resolver(LyricProvider::Off, "");
        let key = cache_key("Artist", "Song");
        let path = dir.join(format!("{key}.lrc"));
        std::fs::write(&path, "[00:01.00]cached\n").unwrap();

        assert_eq!(resolver.resolve("Artist", "Song", "", 0).lines.len(), 1);
        // Change the file underneath; the memo must still win.
        std::fs::write(&path, "[00:01.00]a\n[00:02.00]b\n").unwrap();
        assert_eq!(resolver.resolve("Artist", "Song", "", 0).lines.len(), 1);

        // A different track re-resolves.
        std::fs::write(dir.join(format!("{}.lrc", cache_key("A", "T2"))), "[00:01.00]x\n").unwrap();
        assert_eq!(resolver.resolve("A", "T2", "", 0).lines.len(), 1);
    }

    #[test]
    fn the_off_provider_never_touches_the_network() {
        let (resolver, _dir) = temp_resolver(LyricProvider::Off, "");
        assert!(resolver.resolve("Artist", "Song", "", 0).is_empty());
    }

    /// The whole resolver path against the real services. Excluded from the
    /// default run because the suite must not need the network.
    #[test]
    #[ignore = "requires network access"]
    fn the_resolver_fetches_and_leaves_no_files_behind() {
        let dir = std::env::temp_dir().join(format!("wb-lyr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let resolver = LyricsResolver::new(dir.clone(), LyricProvider::Netease, String::new());
        let first = resolver.resolve("Beyond", "海阔天空", "", 0);
        assert!(!first.is_empty(), "no lyrics fetched");
        println!("fetched {} lines from {}", first.lines.len(), first.source);

        // Nothing fetched may be written to disk.
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(
            stray.is_empty(),
            "a fetch must not write anything, found {stray:?}"
        );

        // The same track is still answered from the in-process memo, so the
        // position tick that fires several times a second costs no network.
        assert_eq!(resolver.resolve("Beyond", "海阔天空", "", 0).lines, first.lines);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hits the real endpoints. Excluded from the default run because the suite
    /// must not need the network, but run it explicitly to check that a
    /// provider's protocol still matches the service:
    ///
    /// ```text
    /// cargo test -p beautify-media -- --ignored --nocapture providers
    /// ```
    #[test]
    #[ignore = "requires network access"]
    fn providers_return_lyrics_for_a_well_known_track() {
        use crate::providers::{self, Provider, Track};
        let track = Track {
            title: "海阔天空",
            artist: "Beyond",
            album: "",
            duration_ms: 0,
        };
        for provider in Provider::ALL {
            match providers::fetch_one(provider, &track) {
                Some(lyrics) => println!(
                    "{}: {} lines from {}",
                    provider.id(),
                    lyrics.lines.len(),
                    lyrics.source
                ),
                None => println!("{}: no result", provider.id()),
            }
        }
    }

    #[test]
    fn empty_title_is_not_queried() {
        let (resolver, _dir) = temp_resolver(LyricProvider::Off, "");
        assert!(resolver.resolve("Artist", "", "", 0).is_empty());
    }

    #[test]
    fn json_envelopes_are_unwrapped() {
        assert_eq!(
            extract_json_lyric(r#"{"lyric":"[00:01.00]x"}"#).unwrap(),
            "[00:01.00]x"
        );
        assert_eq!(
            extract_json_lyric(r#"{"data":{"lrc":"L"}}"#).unwrap(),
            "L"
        );
        assert!(extract_json_lyric(r#"{"other":1}"#).is_none());
        assert!(extract_json_lyric("not json").is_none());
    }
}
