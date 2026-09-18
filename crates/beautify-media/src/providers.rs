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

/// The title with decoration stripped: every bracketed segment — `(Official
/// Video)`, `[MV]`, `【字幕】` — and anything after a ` - ` tail like
/// `- Official Music Video`.
///
/// Streaming sources hand GSMTC titles that carry this junk, and it breaks
/// keyword search. Used for the *first* search attempt, with the raw title
/// kept as the retry: the odd song that lives inside brackets loses nothing
/// but one request.
fn clean_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' | '（' | '【' | '〔' | '［' => depth += 1,
            ')' | ']' | '）' | '】' | '〕' | '］' => {
                depth = depth.saturating_sub(1);
            }
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // A ` - ` tail is usually a source annotation ("Song - Official MV"), not
    // part of the title. Only a tail that carries one of the known annotation
    // words is stripped, so a genuine two-part title survives; the raw-title
    // retry catches whatever this misses.
    const ANNOTATIONS: [&str; 16] = [
        "mv", "official", "live", "remaster", "version", "lyrics", "lyric", "video", "audio",
        "hd", "4k", "cover", "instrumental", "字幕", "伴奏", "翻唱",
    ];
    for dash in [" - ", " – ", " — ", " － "] {
        if let Some((head, tail)) = out.split_once(dash) {
            let tail_lower = tail.to_lowercase();
            if ANNOTATIONS.iter().any(|kw| tail_lower.contains(kw)) {
                out = head.to_string();
            }
        }
    }
    let cleaned = out.trim().trim_matches(['-', '–', '—', '－']).trim();
    if cleaned.is_empty() {
        // Degenerate: everything was decoration. The raw title still matches.
        title.trim().to_string()
    } else {
        cleaned.to_string()
    }
}

/// The artist string split into the individual names a provider list may
/// match. GSMTC concatenates multiple artists with every separator in use.
fn split_artists(artist: &str) -> Vec<String> {
    artist
        .split(['/', ';', ',', '&', '、', '，', '；', '&'])
        .flat_map(|part| part.split_once(" feat.").map(|(head, _)| head).or(Some(part)))
        .flat_map(|part| part.split_once(" ft.").map(|(head, _)| head).or(Some(part)))
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// How well a search candidate matches the track being looked up.
///
/// Providers rank by their own relevance, which mixes covers, live versions
/// and karaoke tracks in with the original. A positive score means at least
/// one piece of evidence (title, artist or duration) agrees; the caller picks
/// the best and only falls back to unscored results when nothing matched.
fn score_candidate(candidate_title: &str, candidate_artists: &[String], duration_ms: i64, track: &Track<'_>) -> i32 {
    let mut score = 0;
    if !track.title.trim().is_empty()
        && names_match(candidate_title, &clean_title(track.title))
    {
        score += 4;
    }
    if !track.artist.trim().is_empty() {
        let wanted = split_artists(track.artist);
        if wanted
            .iter()
            .any(|name| candidate_artists.iter().any(|other| names_match(other, name)))
        {
            score += 3;
        }
    }
    if track.duration_ms > 0 && duration_ms > 0 {
        let delta = (duration_ms - track.duration_ms).abs();
        if delta <= 3000 {
            score += 2;
        } else if delta <= 8000 {
            score += 1;
        }
    }
    score
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
    // Three searches, in increasing desperation: the cleaned title with the
    // artist (the usual hit), the raw title with the artist (titles that live
    // inside brackets), and the cleaned title alone (artists GSMTC cannot
    // spell the way the provider does). The scored pick below keeps a search
    // that *did* return the song from grabbing a cover instead.
    for term in netease_terms(track) {
        if let Some(lyrics) = netease_search(&term, track) {
            return Some(lyrics);
        }
    }
    None
}

/// Search strings for NetEase, most specific first.
fn netease_terms(track: &Track<'_>) -> Vec<String> {
    let cleaned = clean_title(track.title);
    let raw = track.title.trim();
    let artist = track.artist.trim();
    let mut terms = Vec::new();
    if !cleaned.is_empty() && !artist.is_empty() {
        terms.push(format!("{cleaned} {artist}"));
    }
    if !raw.is_empty() && !artist.is_empty() && raw != cleaned {
        terms.push(format!("{raw} {artist}"));
    }
    if !cleaned.is_empty() {
        terms.push(cleaned);
    }
    if !raw.is_empty() {
        terms.push(raw.to_string());
    }
    terms.dedup();
    terms
}

fn netease_search(term: &str, track: &Track<'_>) -> Option<Lyrics> {
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

    // Score every candidate and take the best: the endpoint ranks by its own
    // relevance, which puts covers and live cuts on top more often than it
    // should. A tie keeps the endpoint's own order.
    let mut best: Option<(i32, usize)> = None;
    for (index, song) in songs.iter().enumerate() {
        let title = song.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let artists: Vec<String> = song
            .get("artists")
            .and_then(|a| a.as_array())
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let duration = song.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
        let score = score_candidate(title, &artists, duration, track);
        if best.map(|(best_score, _)| score > best_score).unwrap_or(true) {
            best = Some((score, index));
        }
    }
    let index = best?.1;
    let song_id = songs
        .get(index)?
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

// ---------------------------------------------------------------------------
// QQ Music
// ---------------------------------------------------------------------------

fn qq(track: &Track<'_>) -> Option<Lyrics> {
    for term in netease_terms(track) {
        if let Some(lyrics) = qq_search(&term, track) {
            return Some(lyrics);
        }
    }
    None
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
        let title = song.get("songname").and_then(|v| v.as_str()).unwrap_or("");
        let artists: Vec<String> = song
            .get("singer")
            .and_then(|v| v.as_array())
            .map(|singers| {
                singers
                    .iter()
                    .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let interval = song.get("interval").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
        let score = score_candidate(title, &artists, interval, track);
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
    // Keyword search first: one request, but it frequently returns nothing at
    // all — the endpoint answers 200 with an empty candidate list — so the
    // hash route below is not a rare fallback, it is the usual path. The
    // cleaned title goes first for the same reason as everywhere else.
    let raw = track.title.trim();
    let cleaned = clean_title(track.title);
    let mut keywords = Vec::new();
    if !raw.is_empty() && raw != cleaned {
        keywords.push(raw.to_string());
    }
    keywords.push(cleaned.clone());
    for keyword in keywords {
        if let Some(lyrics) = kugou_search_by_keyword(&keyword, track.duration_ms) {
            return Some(lyrics);
        }
    }
    kugou_search_by_hash(&cleaned, track)
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
    // The lyric index lists every transcription of the song it has; the first
    // entry is the one its ranker liked, which is not always the one that
    // matches the track's duration. Candidates carry `duration` in ms, so the
    // closest one wins when the track duration is known.
    let mut best: Option<(i64, &Value)> = None;
    for candidate in &candidates {
        let candidate_duration = candidate.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
        let delta = if duration_ms > 0 && candidate_duration > 0 {
            (candidate_duration - duration_ms).abs()
        } else {
            i64::MAX
        };
        if best.map(|(best_delta, _)| delta < best_delta).unwrap_or(true) {
            best = Some((delta, candidate));
        }
    }
    kugou_download(best?.1)
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

    // Same shared scoring as the other providers: the song index also mixes
    // covers and live versions in, and `Duration` here is whole seconds.
    let mut best: Option<(i32, &Value)> = None;
    for song in lists {
        let title = song.get("SongName").and_then(|v| v.as_str()).unwrap_or("");
        let artists: Vec<String> = song
            .get("SingerName")
            .and_then(|v| v.as_str())
            .map(split_artists)
            .unwrap_or_default();
        let duration = song.get("Duration").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
        let score = score_candidate(title, &artists, duration, track);
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
    // `/get` is an exact-match endpoint. The cleaned title goes first for the
    // same reason as the keyword searches; the raw title retries.
    let cleaned = clean_title(track.title);
    for title in [cleaned.as_str(), track.title.trim()] {
        if title.is_empty() {
            continue;
        }
        let mut params = vec![("track_name", title.to_string())];
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
    }

    // `/search` is fuzzy. Score what comes back — the list is unordered with
    // respect to covers, and entries carry the names and duration needed to
    // tell the original apart.
    let url = format!("https://lrclib.net/api/search?{}", query(&[("q", search_term(track))]));
    let json = get_json(&url, None)?;
    let entries = json.as_array()?;
    let mut best: Option<(i32, String)> = None;
    for entry in entries {
        let Some(text) = entry.get("syncedLyrics").and_then(|v| v.as_str()) else {
            continue;
        };
        let title = entry.get("track_name").and_then(|v| v.as_str()).unwrap_or("");
        let artists: Vec<String> = entry
            .get("artist_name")
            .and_then(|v| v.as_str())
            .map(split_artists)
            .unwrap_or_default();
        let duration = entry.get("duration").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
        let score = score_candidate(title, &artists, duration, track);
        if score > best.as_ref().map(|(s, _)| *s).unwrap_or(-1) {
            best = Some((score, text.to_string()));
        }
    }
    best.and_then(|(_, text)| from_lrc(&text))
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
    fn clean_title_strips_decoration_but_not_real_titles() {
        assert_eq!(clean_title("晴天 (Official MV)"), "晴天");
        assert_eq!(clean_title("Song [HD Lyrics]"), "Song");
        assert_eq!(clean_title("夜曲【字幕版】"), "夜曲");
        assert_eq!(clean_title("Song - Official Music Video"), "Song");
        // An annotation is stripped only when the tail carries one of the
        // known words — a genuine two-part title keeps its tail.
        assert_eq!(clean_title("爱 - Love"), "爱 - Love");
        // Everything stripped degenerates back to the raw title.
        assert_eq!(clean_title("(())"), "(())");
    }

    #[test]
    fn split_artists_covers_the_separators_gmtc_uses() {
        assert_eq!(
            split_artists("周杰伦 / 费玉清"),
            vec!["周杰伦", "费玉清"]
        );
        assert_eq!(split_artists("A、B，C"), vec!["A", "B", "C"]);
        assert_eq!(split_artists("D feat. E"), vec!["D"]);
        assert_eq!(split_artists("Solo"), vec!["Solo"]);
    }

    #[test]
    fn scoring_prefers_the_original_over_covers_and_live_cuts() {
        let track = Track {
            title: "海阔天空 (Live)",
            artist: "Beyond",
            album: "",
            duration_ms: 326_000,
        };

        let original = score_candidate("海阔天空", &["Beyond".into()], 325_000, &track);
        let cover = score_candidate("海阔天空", &["Someone Else".into()], 325_000, &track);
        let live_by_original = score_candidate("海阔天空 (Live)", &["Beyond".into()], 400_000, &track);
        assert!(original >= 6, "title + artist + duration: {original}");
        assert!(cover < original, "a cover must not outrank the original");
        assert!(
            live_by_original < original,
            "a live cut at a different length must not outrank the studio take"
        );
    }

    #[test]
    fn scoring_needs_no_duration_when_the_track_has_none() {
        let track = Track {
            title: "Song",
            artist: "",
            album: "",
            duration_ms: 0,
        };
        let scored = score_candidate("Song", &["Anyone".into()], 0, &track);
        assert_eq!(scored, 4);
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
    fn search_terms_put_the_cleaned_title_first_and_keep_the_raw_one() {
        let track = Track {
            title: "晴天 (Official MV)",
            artist: "周杰伦",
            album: "",
            duration_ms: 0,
        };
        let terms = netease_terms(&track);
        assert_eq!(terms.first().map(String::as_str), Some("晴天 周杰伦"));
        assert!(
            terms.iter().any(|term| term.contains("晴天 (Official MV)")),
            "the raw title must be retried: {terms:?}"
        );
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
