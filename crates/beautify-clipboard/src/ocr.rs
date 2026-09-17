//! Reading text *inside* clipboard images.
//!
//! Uses the OCR engine that ships with Windows (`Windows.Media.Ocr`), so there
//! is no model to download, no network call and no extra binary — the engine and
//! its language packs are already installed for the handwriting and screenshot
//! features of the OS itself.
//!
//! # Why a worker thread
//!
//! Recognition takes tens to hundreds of milliseconds, which is far too long for
//! the clipboard listener's message loop: it has to answer `WM_CLIPBOARDUPDATE`
//! promptly or the copying application blocks. So images are recognised on a
//! dedicated thread that is initialised once, and the result is written back to
//! the row afterwards.
//!
//! # Why the BMP goes through a decoder
//!
//! The engine wants a `SoftwareBitmap`. Our images are BMP files on disk, so the
//! bytes are handed to `BitmapDecoder` through an in-memory stream — the same
//! path a file-backed stream would take, minus the file handle — and the decoded
//! bitmap is converted to the one pixel format the engine accepts.

use beautify_core::model::base64;
use std::path::Path;

/// Longest recognised text we keep per image.
///
/// A screenshot of a document can run to thousands of characters. This is a
/// search index and a preview, not an archive, so it is capped.
const MAX_TEXT_CHARS: usize = 2000;

/// What OCR produced for one image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recognised {
    /// Whitespace-normalised text, empty when nothing was found.
    pub text: String,
    /// Whether an engine existed at all. Distinguishes "no text in this image"
    /// from "this machine cannot do OCR", which are different problems.
    pub engine_available: bool,
}

impl Recognised {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Is OCR usable on this machine?
pub fn available() -> bool {
    !languages().is_empty()
}

/// The recogniser languages installed, as BCP-47 tags.
pub fn languages() -> Vec<String> {
    use windows::Media::Ocr::OcrEngine;

    let mut out = Vec::new();
    if let Ok(list) = OcrEngine::AvailableRecognizerLanguages() {
        for language in list {
            if let Ok(tag) = language.LanguageTag() {
                out.push(tag.to_string());
            }
        }
    }
    out
}

/// Recognise the text in a BMP file.
///
/// Returns `None` only when the image itself could not be decoded. A machine
/// with no recogniser, or an image containing no text, both yield a
/// [`Recognised`] with empty text so the caller can tell the cases apart.
pub fn recognise_file(path: &Path) -> Option<Recognised> {
    let bytes = std::fs::read(path).ok()?;
    recognise_bytes(&bytes)
}

/// Recognise the text in BMP bytes.
pub fn recognise_bytes(bytes: &[u8]) -> Option<Recognised> {
    let engine = match engine() {
        Some(engine) => engine,
        None => {
            return Some(Recognised {
                text: String::new(),
                engine_available: false,
            })
        }
    };

    let bitmap = decode(bytes)?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .ok()
        .and_then(|op| op.join().ok())?;

    let mut text = String::new();
    if let Ok(lines) = result.Lines() {
        for line in lines {
            if let Ok(line_text) = line.Text() {
                let owned = line_text.to_string();
                let trimmed = owned.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(trimmed);
                if text.chars().count() >= MAX_TEXT_CHARS {
                    break;
                }
            }
        }
    }

    Some(Recognised {
        text: cap(text),
        engine_available: true,
    })
}

/// The first engine that will work, preferring the user's own languages.
///
/// Falling back to English matters on a machine whose display language has no
/// recogniser installed: recognising Latin text is better than refusing.
fn engine() -> Option<windows::Media::Ocr::OcrEngine> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine;

    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Some(engine);
    }
    for tag in ["en-US", "zh-Hans-CN"] {
        if let Ok(language) = Language::CreateLanguage(&windows::core::HSTRING::from(tag)) {
            if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&language) {
                return Some(engine);
            }
        }
    }
    None
}

/// Decode BMP bytes into the pixel format the recogniser accepts.
///
/// # Why the format conversion is not optional
///
/// `RecognizeAsync` takes `Bgra8`; a BMP from the clipboard is usually 24- or
/// 32-bit and may carry a palette. Handing over a `SoftwareBitmap` in any other
/// format fails at the call rather than degrading, so the conversion is explicit.
fn decode(bytes: &[u8]) -> Option<windows::Graphics::Imaging::SoftwareBitmap> {
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, SoftwareBitmap,
    };
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

    let stream = InMemoryRandomAccessStream::new().ok()?;
    let writer = DataWriter::CreateDataWriter(&stream).ok()?;
    writer.WriteBytes(bytes).ok()?;
    writer.StoreAsync().ok()?.join().ok()?;
    writer.FlushAsync().ok()?.join().ok()?;
    // The decoder reads from the stream's current position.
    stream.Seek(0).ok()?;

    let decoder = BitmapDecoder::CreateAsync(&stream).ok()?.join().ok()?;
    let bitmap = decoder.GetSoftwareBitmapAsync().ok()?.join().ok()?;

    if bitmap.BitmapPixelFormat().ok()? == BitmapPixelFormat::Bgra8
        && bitmap.BitmapAlphaMode().ok()? == BitmapAlphaMode::Premultiplied
    {
        return Some(bitmap);
    }
    SoftwareBitmap::ConvertWithAlpha(
        &bitmap,
        BitmapPixelFormat::Bgra8,
        BitmapAlphaMode::Premultiplied,
    )
    .ok()
}

/// Is this a character the recogniser treats as its own word?
///
/// The Windows engine joins words with a space, and for Chinese, Japanese and
/// Korean each *character* is a word — so `华为` comes back as `华 为`.
fn is_ideographic(ch: char) -> bool {
    matches!(ch,
        '\u{3000}'..='\u{303F}'   // CJK punctuation
        | '\u{3040}'..='\u{30FF}' // kana
        | '\u{3400}'..='\u{4DBF}' // unified ideographs, extension A
        | '\u{4E00}'..='\u{9FFF}' // unified ideographs
        | '\u{F900}'..='\u{FAFF}' // compatibility ideographs
        | '\u{FF00}'..='\u{FFEF}' // fullwidth forms
    )
}

/// Drop the spaces the recogniser inserts *between* ideographs.
///
/// Left in, they break the only thing this text is for. The stored value is
/// searched with `LIKE`, so `华 为` would never match a search for `华为` — and
/// the flyout would show the text spaced out for no reason. Spaces between
/// Latin words are untouched.
fn join_ideographs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (index, &ch) in chars.iter().enumerate() {
        if ch == ' ' {
            let before = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let after = chars.get(index + 1).copied();
            if before.is_some_and(is_ideographic) && after.is_some_and(is_ideographic) {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Collapse whitespace runs and stop at the cap.
///
/// A run that contains a newline becomes a single newline, so the line
/// structure the recogniser found survives; any other run becomes one space.
/// The result is searched with `LIKE`, where runs of padding are only noise.
fn cap(text: String) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_TEXT_CHARS * 2));
    let mut length = 0usize;
    // The separator owed to the next visible character. Held back so a run
    // cannot emit two in a row, or one at the very start.
    let mut pending: Option<char> = None;

    for ch in text.chars() {
        if ch.is_whitespace() {
            pending = Some(if ch == '\n' || pending == Some('\n') {
                '\n'
            } else {
                ' '
            });
            continue;
        }
        if length >= MAX_TEXT_CHARS {
            break;
        }
        if let Some(separator) = pending.take() {
            if length > 0 {
                out.push(separator);
                length += 1;
            }
        }
        out.push(ch);
        length += 1;
    }
    join_ideographs(&out)
}

/// A single line of recognised text, for a list row.
pub fn to_preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(160).collect()
}

/// Base64 of the recognised text, for the webview.
///
/// The text can be any language, and the front end's transport is JSON, so it is
/// kept as a plain string — this exists only for symmetry with the model's other
/// binary helpers and is used by the tests.
#[allow(dead_code)]
pub fn encode_for_transport(text: &str) -> String {
    base64::encode(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_collapsed_but_lines_are_kept() {
        let out = cap("hello    world\n\n  second   line  ".to_string());
        assert_eq!(out, "hello world\nsecond line");
    }

    #[test]
    fn the_cap_bounds_the_stored_text() {
        let long = "字".repeat(MAX_TEXT_CHARS * 2);
        assert_eq!(cap(long).chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn spaces_between_ideographs_are_dropped() {
        // The engine's actual output for a Chinese line: one space per
        // character. Searching for 华为 has to find it.
        assert_eq!(join_ideographs("华 为 赛 力 斯"), "华为赛力斯");
        // Latin words keep their spaces.
        assert_eq!(join_ideographs("hello world"), "hello world");
        // Mixed: the CJK run is joined, the Latin words are not.
        assert_eq!(join_ideographs("华 为 Mate 60"), "华为 Mate 60");
        // A space between two Latin words next to CJK stays put.
        assert_eq!(join_ideographs("版 本 1.0 beta"), "版本 1.0 beta");
    }

    #[test]
    fn recognised_chinese_becomes_searchable_text() {
        let out = cap("华 为 、 赛 力 斯 的 智 选 车\n合 作 模 式".to_string());
        assert!(out.contains("华为"), "got {out:?}");
        assert!(out.contains("合作模式"), "got {out:?}");
        assert!(out.contains('\n'), "the line break survives");
    }

    #[test]
    fn the_preview_is_a_single_line() {
        let preview = to_preview("first line\nsecond line");
        assert_eq!(preview, "first line second line");
        assert!(!preview.contains('\n'));
    }

    #[test]
    fn the_preview_is_bounded() {
        assert!(to_preview(&"x".repeat(500)).chars().count() <= 160);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(cap(String::new()), "");
        assert_eq!(to_preview(""), "");
        assert!(Recognised::default().is_empty());
    }

    /// Reports what this machine can do. Not a hard assertion: a Windows install
    /// without any OCR language pack is a legitimate configuration, and the
    /// feature has to degrade rather than fail.
    #[test]
    fn this_machine_reports_its_recogniser_languages() {
        let langs = languages();
        println!("OCR languages: {langs:?} (available: {})", available());
    }
}
