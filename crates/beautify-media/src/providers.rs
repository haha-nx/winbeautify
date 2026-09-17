//! Online lyric providers.
//!
//! Each provider is a two-step lookup: search by name to find a song id, then
//! fetch the lyric for that id. None of them require an API key, a signature or
//! a cookie — they are the same public endpoints the various open-source
//! players use, which is why a plain HTTP client is enough.
//!
//! Two things are worth knowing before reading the code:
//!
//! * **These are undocumented endpoints.** They can change shape without
//!   warning, so every parse is written to fail soft: a provider that returns
//!   something unexpected yields `None` and the caller falls through to the
//!   next source, rather than erroring out.
//! * **They all send the track name and artist to a third party.** That is why
//!   the provider is a user choice and `Off` is a first-class option.

use beautify_core::model::Lyrics;
use serde_json::Value;

use crate::http;
use crate::lrc;

/// Where lyrics may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Netease,
    Qq,
    Kugou,
    Lrclib,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::Netease,
        Provider::Qq,
        Provider::Kugou,
        Provider::Lrclib,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Provider::Netease => "netease",
            Provider::Qq => "qq",
            Provider::Kugou => "kugou",
            Provider::Lrclib => "lrclib",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Provider::Netease => "网易云音乐",
            Provider::Qq => "QQ 音乐",
            Provider::Kugou => "酷狗音乐",
            Provider::Lrclib => "LRCLIB（国际曲库）",
        }
    }

    /// Providers to try when this one has nothing.
    ///
    /// Lyrics databases have very different coverage depending on the genre and
    /// the region, so a miss on one is not a miss overall.
    const fn fallbacks(self) -> &'static [Provider] {
        match self {
            Provider::Netease => &[Provider::Qq, Provider::Kugou, Provider::Lrclib],
            Provider::Qq => &[Provider::Netease, Provider::Kugou, Provider::Lrclib],
            Provider::Kugou => &[Provider::Netease, Provider::Qq, Provider::Lrclib],
            Provider::Lrclib => &[Provider::Netease, Provider::Qq, Provider::Kugou],
        }
    }
}

/// A track to look up.
#[derive(Debug, Clone)]
pub struct Track<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// Milliseconds; used to disambiguate covers and live versions.
    pub duration_ms: i64,
}

/// Look up `track`, trying `primary` and then its fallbacks.
///
/// Returns the first non-empty lyric found, tagged with its source.
pub fn fetch(primary: Provider, track: &Track<'_>) -> Option<Lyrics> {
    if let Some(lyrics) = fetch_one(primary, track) {
        return Some(lyrics);
    }
    for fallback in primary.fallbacks() {
        if let Some(lyrics) = fetch_one(*fallback, track) {
            tracing::debug!(provider = fallback.id(), "lyrics found via fallback provider");
            return Some(lyrics);
        }
    }
    None
}

/// Look up `track` on one provider, with no fallback.
pub fn fetch_one(provider: Provider, track: &Track<'_>) -> Option<Lyrics> {
    let result = match provider {
        Provider::Netease => netease(track),
        Provider::Qq => qq(track),
        Provider::Kugou => kugou(track),
        Provider::Lrclib => lrclib(track),
    };
    match result {
        Some(lyrics) if !lyrics.is_empty() => {
            let mut lyrics = lyrics;
            lyrics.source = provider.id().to_string();
            Some(lyrics)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A browser User-Agent. Several of these endpoints return an empty body or a
/// redirect for clients that do not look like a browser.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

fn get_json(url: &str, referer: Option<&str>) -> Option<Value> {
    let mut headers = vec![("User-Agent", USER_AGENT)];
    if let Some(referer) = referer {
        headers.push(("Referer", referer));
    }
    let body = http::get_with_headers(url, 8000, &headers).ok()?;
    serde_json::from_str(&body).ok()
}

fn query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", k, http::encode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The shared search string: title plus artist, which is what all four
/// providers match on.
fn search_term(track: &Track<'_>) -> String {
    if track.artist.trim().is_empty() {
        track.title.trim().to_string()
    } else {
        format!("{} {}", track.title.trim(), track.artist.trim())
    }
}

/// Case-insensitive "do these two names refer to the same thing", ignoring
/// punctuation and bracketed suffixes like `(Live)` or `【官方】`.
fn names_match(a: &str, b: &str) -> bool {
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect::<String>()
    };
    let (a, b) = (normalize(a), normalize(b));
    !a.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

/// Build a `Lyrics` from an LRC document, rejecting anything unparseable.
fn from_lrc(text: &str) -> Option<Lyrics> {
    let parsed = lrc::parse(text);
    (!parsed.is_empty()).then_some(parsed)
}

// ---------------------------------------------------------------------------
// NetEase Cloud Music
// ---------------------------------------------------------------------------

fn netease(track: &Track<'_>) -> Option<Lyrics> {
    if let Some(lyrics) = netease_search(&search_term(track), track.artist) {
        return Some(lyrics);
    }
    // A featured artist in the search string often breaks the match; retrying
    // with the title alone costs one request and rescues a lot of tracks.
    netease_search(track.title.trim(), track.artist)
}

fn netease_search(term: &str, artist: &str) -> Option<Lyrics> {
    if term.trim().is_empty() {
        return None;
    }
    let url = format!(
        "https://music.163.com/api/search/get/web?{}",
        query(&[
            ("s", term.to_string()),
            ("type", "1".into()),
            ("offset", "0".into()),
            ("total", "true".into()),
            ("limit", "10".into()),
        ])
    );
    let json = get_json(&url, None)?;
    let songs = json.get("result")?.get("songs")?.as_array()?;

    // Prefer the candidate whose artist matches, falling back to the first hit
    // — the endpoint ranks by its own relevance, which is usually right.
    let song_id = songs
        .iter()
        .find(|song| song_artist_matches(song, artist))
        .or_else(|| songs.first())?
        .get("id")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))?;

    let url = format!(
        "https://music.163.com/api/song/lyric?{}",
        query(&[
            ("id", song_id.to_string()),
            ("lv", "1".into()),
            ("kv", "1".into()),
            ("tv", "-1".into()),
        ])
    );
    let json = get_json(&url, None)?;
    from_lrc(json.get("lrc")?.get("lyric")?.as_str()?)
}

fn song_artist_matches(song: &Value, artist: &str) -> bool {
    song.get("artists")
        .and_then(|a| a.as_array())
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .any(|name| names_match(name, artist))
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// QQ Music
// ---------------------------------------------------------------------------

fn qq(track: &Track<'_>) -> Option<Lyrics> {
    if let Some(lyrics) = qq_search(&search_term(track), track) {
        return Some(lyrics);
    }
    qq_search(track.title.trim(), track)
}

fn qq_search(term: &str, track: &Track<'_>) -> Option<Lyrics> {
    if term.trim().is_empty() {
        return None;
    }
    let url = format!(
        "https://c.y.qq.com/soso/fcgi-bin/client_search_cp?{}",
        query(&[
            ("format", "json".into()),
            ("p", "1".into()),
            ("n", "20".into()),
            ("w", term.to_string()),
        ])
    );
    let json = get_json(&url, Some("https://y.qq.com/"))?;
    let list = json.get("data")?.get("song")?.get("list")?.as_array()?;

    // Score candidates rather than taking the first hit: the search returns
    // covers and live versions mixed in with the original.
    let mut best: Option<(i32, &Value)> = None;
    for song in list {
        let mut score = 0;
        if song
            .get("songname")
            .and_then(|v| v.as_str())
            .is_some_and(|name| names_match(name, track.title))
        {
            score += 4;
        }
        if song
            .get("singer")
            .and_then(|v| v.as_array())
            .is_some_and(|singers| {
                singers
                    .iter()
                    .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
                    .any(|name| names_match(name, track.artist))
            })
        {
            score += 2;
        }
        if track.duration_ms > 0 {
            let interval = song.get("interval").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
            if interval > 0 && (interval - track.duration_ms).abs() <= 5000 {
                score += 1;
            }
        }
        if score > best.map(|(s, _)| s).unwrap_or(-1) {
            best = Some((score, song));
        }
    }

    let song_mid = best?.1.get("songmid")?.as_str()?;
    let url = format!(
        "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?{}",
        query(&[
            ("songmid", song_mid.to_string()),
            ("format", "json".into()),
            // `nobase64=1` returns plain LRC; the default is a base64 blob.
            ("nobase64", "1".into()),
            ("g_tk", "5381".into()),
        ])
    );
    let json = get_json(&url, Some("https://y.qq.com/"))?;
    if json.get("retcode").and_then(|v| v.as_i64()).unwrap_or(0) != 0 {
        return None;
    }
    from_lrc(json.get("lyric")?.as_str()?)
}

// ---------------------------------------------------------------------------
// Kugou
// ---------------------------------------------------------------------------

fn kugou(track: &Track<'_>) -> Option<Lyrics> {
    let keyword = track.title.trim();
    if keyword.is_empty() {
        return None;
    }

    // Keyword search first. It is one request, but it frequently returns
    // nothing at all — the endpoint answers 200 with an empty candidate list —
    // so the hash route below is not a rare fallback, it is the usual path.
    if let Some(lyrics) = kugou_search_by_keyword(keyword, track.duration_ms) {
        return Some(lyrics);
    }
    kugou_search_by_hash(keyword, track)
}

fn kugou_candidates(url: &str) -> Option<Vec<Value>> {
    let json = match get_json(url, None) {
        Some(json) => json,
        None => {
            tracing::debug!(url, "kugou: request failed or was not json");
            return None;
        }
    };
    let candidates = json
        .get("candidates")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    tracing::debug!(url, count = candidates.len(), "kugou: candidates");
    Some(candidates)
}

fn kugou_search_by_keyword(keyword: &str, duration_ms: i64) -> Option<Lyrics> {
    let url = format!(
        "https://lyrics.kugou.com/search?{}",
        query(&[
            ("ver", "1".into()),
            ("man", "yes".into()),
            ("client", "pc".into()),
            ("keyword", keyword.to_string()),
            ("duration", duration_ms.max(0).to_string()),
        ])
    );
    let candidates = kugou_candidates(&url)?;
    kugou_download(candidates.first()?)
}

/// Resolve the lyric through the song-search index instead of the lyric index.
///
/// `songsearch` needs no special headers and always answers, so it supplies the
/// file hash that the lyric search accepts reliably.
fn kugou_search_by_hash(keyword: &str, track: &Track<'_>) -> Option<Lyrics> {
    let url = format!(
        "https://songsearch.kugou.com/song_search_v2?{}",
        query(&[
            ("keyword", keyword.to_string()),
            ("page", "1".into()),
            ("pagesize", "20".into()),
            ("platform", "WebFilter".into()),
            ("filter", "2".into()),
            ("iscorrection", "1".into()),
            ("privilege_filter", "0".into()),
        ])
    );
    let json = match get_json(&url, None) {
        Some(json) => json,
        None => {
            tracing::debug!("kugou: song search failed");
            return None;
        }
    };
    let lists = json.get("data").and_then(|d| d.get("lists")).and_then(|l| l.as_array())?;
    tracing::debug!(count = lists.len(), "kugou: song search hits");

    let mut best: Option<(i32, &Value)> = None;
    for song in lists {
        let mut score = 0;
        if song
            .get("SongName")
            .and_then(|v| v.as_str())
            .is_some_and(|name| names_match(name, track.title))
        {
            score += 4;
        }
        if song
            .get("SingerName")
            .and_then(|v| v.as_str())
            .is_some_and(|name| names_match(name, track.artist))
        {
            score += 2;
        }
        if score > best.map(|(s, _)| s).unwrap_or(-1) {
            best = Some((score, song));
        }
    }
    let hash = best?.1.get("FileHash")?.as_str()?;
    tracing::debug!(hash, "kugou: resolved file hash");

    let url = format!(
        "https://lyrics.kugou.com/search?{}",
        query(&[
            ("ver", "1".into()),
            ("man", "yes".into()),
            ("client", "pc".into()),
            ("hash", hash.to_string()),
        ])
    );
    let candidates = kugou_candidates(&url)?;
    kugou_download(candidates.first()?)
}

fn kugou_download(candidate: &Value) -> Option<Lyrics> {
    // Kugou is inconsistent about this: `id` arrives as a number in some
    // responses and as a numeric string in others.
    let id = candidate.get("id").and_then(|v| {
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })?;
    let accesskey = candidate.get("accesskey").and_then(|v| v.as_str())?;
    let url = format!(
        "https://lyrics.kugou.com/download?{}",
        query(&[
            ("ver", "1".into()),
            ("client", "pc".into()),
            ("id", id.to_string()),
            ("accesskey", accesskey.to_string()),
            ("fmt", "lrc".into()),
            ("charset", "utf8".into()),
        ])
    );
    let json = get_json(&url, None)?;
    // Kugou hands the LRC back base64-encoded even when asked for utf8.
    let content = json.get("content")?.as_str()?;
    let decoded = beautify_core::model::base64::decode(content)?;
    from_lrc(&String::from_utf8_lossy(&decoded))
}

// ---------------------------------------------------------------------------
// LRCLIB
// ---------------------------------------------------------------------------

fn lrclib(track: &Track<'_>) -> Option<Lyrics> {
    if track.title.trim().is_empty() {
        return None;
    }
    let mut params = vec![("track_name", track.title.trim().to_string())];
    if !track.artist.trim().is_empty() {
        params.push(("artist_name", track.artist.trim().to_string()));
    }
    if track.duration_ms > 0 {
        params.push(("duration", (track.duration_ms / 1000).to_string()));
    }
    let url = format!("https://lrclib.net/api/get?{}", query(&params));
    if let Some(lyrics) = get_json(&url, None)
        .and_then(|json| json.get("syncedLyrics").and_then(|v| v.as_str()).map(str::to_string))
        .and_then(|text| from_lrc(&text))
    {
        return Some(lyrics);
    }

    // `/get` is an exact-match endpoint; `/search` is fuzzy.
    let url = format!("https://lrclib.net/api/search?{}", query(&[("q", search_term(track))]));
    let json = get_json(&url, None)?;
    json.as_array()?
        .iter()
        .find_map(|entry| entry.get("syncedLyrics")?.as_str().and_then(from_lrc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_ignores_case_punctuation_and_suffixes() {
        assert!(names_match("Hello, World!", "hello world"));
        assert!(names_match("夜曲", "夜曲"));
        assert!(names_match("Song (Live)", "song"));
        assert!(!names_match("Song A", "Song B"));
        assert!(!names_match("", "anything"));
    }

    #[test]
    fn every_provider_has_a_stable_id_and_a_fallback_chain() {
        let mut ids: Vec<&str> = Provider::ALL.iter().map(|p| p.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Provider::ALL.len(), "ids must be unique");

        for provider in Provider::ALL {
            assert!(!provider.label().is_empty());
            assert!(
                !provider.fallbacks().contains(&provider),
                "{} must not fall back to itself",
                provider.id()
            );
        }
    }

    #[test]
    fn the_search_term_omits_a_missing_artist() {
        let track = Track {
            title: "Song",
            artist: "",
            album: "",
            duration_ms: 0,
        };
        assert_eq!(search_term(&track), "Song");
        let track = Track {
            artist: "Artist",
            ..track
        };
        assert_eq!(search_term(&track), "Song Artist");
    }

    #[test]
    fn a_parse_failure_is_not_mistaken_for_a_hit() {
        // Providers return HTML error pages or empty objects under load; those
        // must not be reported as lyrics.
        assert!(from_lrc("").is_none());
        assert!(from_lrc("<html>error</html>").is_none());
    }

    #[test]
    fn query_encoding_escapes_reserved_characters() {
        let encoded = query(&[("s", "A & B".to_string()), ("type", "1".into())]);
        assert_eq!(encoded, "s=A%20%26%20B&type=1");
    }
}
