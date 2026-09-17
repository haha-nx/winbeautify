//! LRC parsing.
//!
//! Handles the shapes real `.lrc` files use in the wild: `[mm:ss]`,
//! `[mm:ss.xx]`, `[mm:ss.xxx]`, several timestamps sharing one line (repeat
//! choruses), `[offset:±ms]` metadata, and `[ti:]`/`[ar:]`/`[al:]` tags that
//! must not be mistaken for lyrics.

use beautify_core::model::{LyricLine, Lyrics};

/// Parse an LRC document. Never fails: unparseable lines are dropped.
pub fn parse(text: &str) -> Lyrics {
    let mut lines: Vec<LyricLine> = Vec::new();
    // `[offset:+500]` shifts every timestamp; LRC defines positive as "earlier",
    // so it is subtracted.
    let mut offset_ms: i64 = 0;

    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }

        let mut stamps: Vec<i64> = Vec::new();
        let mut rest = line;

        // Consume the leading run of `[..]` groups; they are all timestamps
        // unless they are metadata tags.
        loop {
            let trimmed = rest.trim_start();
            if !trimmed.starts_with('[') {
                rest = trimmed;
                break;
            }
            let Some(close) = trimmed.find(']') else {
                rest = trimmed;
                break;
            };
            let inside = &trimmed[1..close];
            match parse_timestamp(inside) {
                Some(ms) => {
                    stamps.push(ms);
                    rest = &trimmed[close + 1..];
                }
                None => {
                    // Metadata like `[ti:...]`, `[ar:...]`, `[al:...]`, `[by:...]`.
                    if let Some(value) = inside.split_once(':').map(|(_, v)| v.trim()) {
                        if inside.starts_with("offset:") {
                            offset_ms = value.parse().unwrap_or(0);
                        }
                    }
                    rest = &trimmed[close + 1..];
                }
            }
        }

        let text = rest.trim();
        if stamps.is_empty() || text.is_empty() {
            continue;
        }
        for ms in stamps {
            lines.push(LyricLine {
                time_ms: ms,
                text: text.to_string(),
            });
        }
    }

    if offset_ms != 0 {
        for line in &mut lines {
            line.time_ms -= offset_ms;
        }
    }

    // A file with out-of-order timestamps is common enough that sorting is
    // cheaper than trusting the source.
    lines.sort_by_key(|l| l.time_ms);
    // Collapse exact duplicates, which appear when a line carries the same
    // timestamp twice.
    lines.dedup_by(|a, b| a.time_ms == b.time_ms && a.text == b.text);

    Lyrics {
        lines,
        source: String::new(),
    }
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, `h:mm:ss.xx`.
fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let mut parts = s.split(':');
    let first = parts.next()?;

    // Determine whether this is `mm:ss` or `h:mm:ss` by how many fields follow.
    let second = parts.next();
    let (hours, minutes, seconds_part) = match (second, parts.next()) {
        (Some(sec), None) => (0i64, first, sec),
        (Some(min), Some(sec)) => (first.parse().ok()?, min, sec),
        _ => return None,
    };
    let minutes: i64 = minutes.parse().ok()?;

    let (secs, frac) = match seconds_part.split_once(['.', ',']) {
        Some((a, b)) => (a, Some(b)),
        None => (seconds_part, None),
    };
    let seconds: i64 = secs.trim().parse().ok()?;
    if seconds >= 60 {
        return None;
    }

    let millis = match frac {
        Some(f) => {
            let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                0
            } else {
                // ".5" means 500ms, ".05" means 50ms, ".005" means 5ms.
                let value: i64 = digits.parse().ok()?;
                match digits.len() {
                    1 => value * 100,
                    2 => value * 10,
                    3 => value,
                    _ => value / 10i64.pow(digits.len() as u32 - 3),
                }
            }
        }
        None => 0,
    };

    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_timestamps() {
        let lrc = "[00:01.00]first\n[00:12.50]second\n[01:05.25]third\n";
        let out = parse(lrc);
        assert_eq!(out.lines.len(), 3);
        assert_eq!(out.lines[0].time_ms, 1000);
        assert_eq!(out.lines[1].time_ms, 12_500);
        assert_eq!(out.lines[2].time_ms, 65_250);
        assert_eq!(out.lines[2].text, "third");
    }

    #[test]
    fn fraction_width_does_not_change_meaning() {
        assert_eq!(parse("[00:01.5]a").lines[0].time_ms, 1500);
        assert_eq!(parse("[00:01.05]a").lines[0].time_ms, 1050);
        assert_eq!(parse("[00:01.005]a").lines[0].time_ms, 1005);
        assert_eq!(parse("[00:01]a").lines[0].time_ms, 1000);
    }

    #[test]
    fn metadata_tags_are_not_lyrics() {
        let lrc = "[ti:Song]\n[ar:Artist]\n[al:Album]\n[by:me]\n[00:01.00]real line\n";
        let out = parse(lrc);
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].text, "real line");
    }

    #[test]
    fn multiple_timestamps_share_one_text() {
        let out = parse("[00:10.00][00:40.00]chorus\n");
        assert_eq!(out.lines.len(), 2);
        assert!(out.lines.iter().all(|l| l.text == "chorus"));
        assert_eq!(out.lines[0].time_ms, 10_000);
        assert_eq!(out.lines[1].time_ms, 40_000);
    }

    #[test]
    fn offset_shifts_every_line_earlier() {
        // LRC defines a positive offset as "shift earlier".
        let out = parse("[offset:500]\n[00:10.00]a\n");
        assert_eq!(out.lines[0].time_ms, 9_500);
    }

    #[test]
    fn out_of_order_input_is_sorted() {
        let out = parse("[00:30.00]c\n[00:10.00]a\n[00:20.00]b\n");
        let times: Vec<i64> = out.lines.iter().map(|l| l.time_ms).collect();
        assert_eq!(times, vec![10_000, 20_000, 30_000]);
    }

    #[test]
    fn junk_lines_are_dropped() {
        let out = parse("not a lyric\n\n[00:05.00]\n[]\n[ab:cd]x\n[00:06.00]ok\n");
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].text, "ok");
    }

    #[test]
    fn hour_long_timestamps_work() {
        let out = parse("[1:02:03.00]long\n");
        assert_eq!(out.lines[0].time_ms, (3600 + 120 + 3) * 1000);
    }

    #[test]
    fn index_at_walks_the_cursor() {
        let out = parse("[00:01.00]a\n[00:05.00]b\n[00:09.00]c\n");
        assert_eq!(out.index_at(0), None, "before the first line");
        assert_eq!(out.index_at(1000), Some(0));
        assert_eq!(out.index_at(4000), Some(0));
        assert_eq!(out.index_at(5000), Some(1));
        assert_eq!(out.index_at(99_000), Some(2));

        assert_eq!(out.current(5000).map(|l| l.text.as_str()), Some("b"));
        assert_eq!(out.next(5000).map(|l| l.text.as_str()), Some("c"));
        assert_eq!(out.next(99_000), None);
    }

    #[test]
    fn empty_input_yields_empty_lyrics() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
    }
}
