//! Reading the current clipboard contents into a [`NewClip`].
//!
//! The clipboard is a shared, lock-held resource: another process can own it at
//! any moment, so every open is retried a few times before giving up. Windows
//! also lets a source mark a clip as "do not record" (what password managers
//! do), which we honour unless the user opts in.

use crate::dib;
use crate::store::{self, ClipKind, NewClip};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// How many times to retry `OpenClipboard` before conceding.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Formats Windows uses to tell clipboard monitors to back off.
const EXCLUDE_FORMAT: PCWSTR = w!("ExcludeClipboardContentFromMonitorProcessing");
const CAN_INCLUDE_FORMAT: PCWSTR = w!("CanIncludeInClipboardHistory");
/// Set by clipboard viewers that want to be skipped entirely.
const VIEWER_IGNORE_FORMAT: PCWSTR = w!("Clipboard Viewer Ignore");

/// Bytes we are willing to copy off the clipboard in one go. Guards against a
/// rogue app advertising a multi-gigabyte DIB.
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// Owns the open clipboard; closing is guaranteed even on an early return.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open(hwnd: HWND) -> Option<Self> {
        for attempt in 0..OPEN_ATTEMPTS {
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                return Some(ClipboardGuard);
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        None
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Borrow an `HGLOBAL` clipboard payload for the duration of `f`.
///
/// # Safety contract
///
/// `GlobalLock` must be paired with `GlobalUnlock`, which is why this is the
/// only place either is called.
unsafe fn with_global<R>(handle: HANDLE, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    let hglobal = HGLOBAL(handle.0);
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return None;
    }
    let size = unsafe { GlobalSize(hglobal) }.min(MAX_PAYLOAD);
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) };
    let out = f(slice);
    unsafe {
        let _ = GlobalUnlock(hglobal);
    }
    Some(out)
}

/// Is `format` present on the clipboard right now?
fn format_present(format: u32) -> bool {
    format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// The first DWORD of a clipboard payload, when the format is present.
fn format_dword(format: u32) -> Option<u32> {
    if !format_present(format) {
        return None;
    }
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    unsafe {
        with_global(handle, |bytes| {
            bytes
                .get(0..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        })
        .flatten()
    }
}

/// The decision behind [`is_excluded`], split out so it can be tested without a
/// live clipboard.
///
/// `can_include` is an *opt-out* flag: **absent means "record it"**. Reading an
/// absent flag as a refusal silently disables the whole feature, which is what
/// this signature exists to make impossible to reintroduce.
fn should_exclude(
    viewer_ignore: bool,
    exclude_flag: bool,
    can_include: Option<u32>,
    capture_sensitive: bool,
) -> bool {
    // "Clipboard Viewer Ignore" asks every monitor to skip the clip, so it is
    // honoured even when the user opted into sensitive content.
    if viewer_ignore {
        return true;
    }
    if capture_sensitive {
        return false;
    }
    if exclude_flag {
        return true;
    }
    matches!(can_include, Some(0))
}

/// Should this clip be ignored because its source asked us to?
pub fn is_excluded(capture_sensitive: bool) -> bool {
    // Registering is idempotent and returns the same id the source used.
    let exclude = unsafe { RegisterClipboardFormatW(EXCLUDE_FORMAT) };
    let can_include = unsafe { RegisterClipboardFormatW(CAN_INCLUDE_FORMAT) };
    let viewer_ignore = unsafe { RegisterClipboardFormatW(VIEWER_IGNORE_FORMAT) };

    should_exclude(
        format_present(viewer_ignore),
        format_present(exclude),
        format_dword(can_include),
        capture_sensitive,
    )
}

fn read_text() -> Option<String> {
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }.ok()?;
    unsafe {
        with_global(handle, |bytes| {
            // The payload is UTF-16 including a trailing NUL and possibly a
            // pad byte, so round the length down to whole code units.
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|u| *u != 0)
                .collect();
            let text = String::from_utf16_lossy(&units);
            (!text.is_empty()).then_some(text)
        })
        .flatten()
    }
}

fn read_files() -> Option<(String, Vec<String>)> {
    let handle = unsafe { GetClipboardData(CF_HDROP.0 as u32) }.ok()?;
    let hdrop = HDROP(handle.0);

    let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, None) };
    if count == 0 {
        return None;
    }
    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, i, None) };
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) };
        if written == 0 {
            continue;
        }
        paths.push(String::from_utf16_lossy(&buf[..written as usize]));
    }
    if paths.is_empty() {
        return None;
    }
    let joined = paths.join("\n");
    Some((joined, paths))
}

/// The raw `CF_DIB` payload, exactly as Windows hands it over.
///
/// The clipboard owns the memory, so it is copied out. Used by the snipper: it
/// wants the pixels, not a file.
pub fn clipboard_dib() -> Option<Vec<u8>> {
    let _guard = ClipboardGuard::open(HWND::default())?;
    read_dib()
}

fn read_dib() -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(CF_DIB.0 as u32) }.ok()?;
    unsafe { with_global(handle, |bytes| bytes.to_vec()) }
}

fn read_image(max_bytes: u32) -> Option<(Vec<u8>, i32, i32, i64)> {
    let dib_bytes = read_dib()?;
    if max_bytes > 0 && dib_bytes.len() as u64 > max_bytes as u64 {
        tracing::debug!(
            bytes = dib_bytes.len(),
            limit = max_bytes,
            "clipboard image exceeds the configured size limit"
        );
        return None;
    }
    let (bmp, width, height) = dib::dib_to_bmp(&dib_bytes)?;
    Some((bmp, width, height, dib_bytes.len() as i64))
}

/// Snapshot the clipboard right now.
///
/// The bitmap currently on the clipboard, as BMP bytes.
///
/// Exposed so features that need the image rather than the history — the pin
/// window, for one — do not have to reimplement `CF_DIB` handling. Returns
/// `None` when the clipboard holds no image or another process is holding it
/// open.
pub fn clipboard_image_bmp(max_bytes: u32) -> Option<Vec<u8>> {
    let _guard = ClipboardGuard::open(HWND::default())?;
    read_image(max_bytes).map(|(bmp, _, _, _)| bmp)
}

/// The text currently on the clipboard, if it holds any.
///
/// The same `CF_UNICODETEXT` path the history reads, exposed for the settings
/// window's text fields: those are drawn rather than being child `EDIT`
/// controls, so a paste into one has to be done by hand.
pub fn clipboard_text() -> Option<String> {
    let _guard = ClipboardGuard::open(HWND::default())?;
    read_text()
}

/// Returns `None` when the clipboard is empty, locked by another process, or
/// holds nothing we record.
pub fn capture(hwnd: HWND, capture_sensitive: bool, max_image_bytes: u32) -> Option<NewClip> {
    let _guard = ClipboardGuard::open(hwnd)?;

    if is_excluded(capture_sensitive) {
        tracing::debug!("clipboard update ignored: source marked it as non-recordable");
        return None;
    }

    // Files first: a file copy also publishes the paths as text, and the paths
    // are the more useful thing to keep.
    if let Some((joined, _paths)) = read_files() {
        return Some(NewClip {
            kind: ClipKind::Files,
            bytes: joined.len() as i64,
            text: joined,
            image_bmp: None,
            width: 0,
            height: 0,
        });
    }

    // Then images, which beat the (usually useless) text alternative whatever
    // app put it there.
    if let Some((bmp, width, height, bytes)) = read_image(max_image_bytes) {
        return Some(NewClip {
            kind: ClipKind::Image,
            text: String::new(),
            image_bmp: Some(bmp),
            width,
            height,
            bytes,
        });
    }

    if let Some(text) = read_text() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(NewClip {
            // A clip that is nothing but a URL is filed separately, which is
            // what puts it under "链接" rather than "文本" in the flyout.
            kind: if store::is_link(&text) {
                ClipKind::Link
            } else {
                ClipKind::Text
            },
            bytes: text.len() as i64,
            text,
            image_bmp: None,
            width: 0,
            height: 0,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::should_exclude;

    #[test]
    fn an_absent_can_include_flag_means_record_it() {
        // The regression that this test exists for: reading "flag not present"
        // as "do not record" turns the listener into a no-op.
        assert!(!should_exclude(false, false, None, false));
    }

    #[test]
    fn an_explicit_zero_from_the_source_is_honoured() {
        assert!(should_exclude(false, false, Some(0), false));
        assert!(!should_exclude(false, false, Some(1), false));
    }

    #[test]
    fn the_exclude_flag_always_wins_unless_sensitive_capture_is_on() {
        assert!(should_exclude(false, true, None, false));
        assert!(!should_exclude(false, true, None, true));
    }

    #[test]
    fn viewer_ignore_beats_even_sensitive_capture() {
        assert!(should_exclude(true, false, None, true));
        assert!(should_exclude(true, true, Some(1), true));
    }
}
