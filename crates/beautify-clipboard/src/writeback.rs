//! Putting a stored entry back onto the Windows clipboard.
//!
//! All three shapes (`CF_UNICODETEXT`, `CF_HDROP`, `CF_DIB`) use the same
//! `HGLOBAL` handoff: allocate movable memory, fill it, then hand the handle to
//! the clipboard. Ownership transfers on success — freeing the handle
//! afterwards would corrupt the clipboard, so the error path is the only place
//! `GlobalFree` appears.

use crate::store::ClipKind;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};

/// `DROPFILES` from `<shellapi.h>`. `windows-rs` does not expose it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DropFiles {
    /// Offset from the start of the structure to the file list.
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    /// Non-zero for a UTF-16 file list.
    f_wide: i32,
}

impl DropFiles {
    const SIZE: usize = std::mem::size_of::<DropFiles>();
}

#[derive(Debug)]
pub enum WriteError {
    /// Another process is holding the clipboard open.
    Busy,
    /// Win32 refused an allocation or a clipboard operation.
    Win32(&'static str),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Busy => write!(f, "clipboard is in use by another application"),
            WriteError::Win32(op) => write!(f, "{op} failed"),
        }
    }
}

impl std::error::Error for WriteError {}

struct ClipboardGuard;

impl ClipboardGuard {
    fn open(hwnd: HWND) -> Result<Self, WriteError> {
        for attempt in 0..5 {
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                return Ok(ClipboardGuard);
            }
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        Err(WriteError::Busy)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Copy `bytes` into a freshly allocated `GMEM_MOVEABLE` block.
///
/// The returned handle is *not* freed on success — it belongs to the clipboard.
unsafe fn alloc_and_fill(bytes: &[u8]) -> Result<HANDLE, WriteError> {
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }
        .map_err(|_| WriteError::Win32("GlobalAlloc"))?;
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        unsafe { let _ = GlobalFree(Some(hglobal)); }
        return Err(WriteError::Win32("GlobalLock"));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    Ok(HANDLE(hglobal.0))
}

/// Encode a UTF-16 file list for `CF_HDROP`: a `DROPFILES` header followed by
/// the paths, each NUL-terminated, with an extra NUL closing the list.
fn encode_hdrop(paths: &[String]) -> Vec<u8> {
    let header = DropFiles {
        p_files: DropFiles::SIZE as u32,
        pt_x: 0,
        pt_y: 0,
        f_nc: 0,
        f_wide: 1,
    };
    let mut units: Vec<u16> = Vec::new();
    for path in paths {
        units.extend(path.encode_utf16());
        units.push(0);
    }
    units.push(0); // list terminator

    let mut out = Vec::with_capacity(DropFiles::SIZE + units.len() * 2);
    out.extend_from_slice(&header.p_files.to_le_bytes());
    out.extend_from_slice(&header.pt_x.to_le_bytes());
    out.extend_from_slice(&header.pt_y.to_le_bytes());
    out.extend_from_slice(&header.f_nc.to_le_bytes());
    out.extend_from_slice(&header.f_wide.to_le_bytes());
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

fn encode_utf16_z(text: &str) -> Vec<u8> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let mut out = Vec::with_capacity(units.len() * 2);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// Strip the 14-byte `BITMAPFILEHEADER` we added when storing the image.
fn bmp_to_dib(bmp: &[u8]) -> Option<&[u8]> {
    if bmp.len() <= 14 || &bmp[0..2] != b"BM" {
        return None;
    }
    Some(&bmp[14..])
}

/// Place a stored entry on the clipboard.
///
/// `image_bmp_path` is read lazily so text and file copies never touch the disk.
pub fn put(hwnd: HWND, kind: ClipKind, text: &str, image_bmp_path: &str) -> Result<(), WriteError> {
    let _guard = ClipboardGuard::open(hwnd)?;
    if unsafe { EmptyClipboard() }.is_err() {
        return Err(WriteError::Win32("EmptyClipboard"));
    }

    match kind {
        // A link goes back as the text it is.
        ClipKind::Text | ClipKind::Link => {
            let payload = encode_utf16_z(text);
            let handle = unsafe { alloc_and_fill(&payload)? };
            // From here the clipboard owns `handle`; do not free it.
            unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(handle)) }
                .map_err(|_| WriteError::Win32("SetClipboardData(CF_UNICODETEXT)"))?;
        }
        ClipKind::Files => {
            let paths: Vec<String> = text
                .lines()
                .map(|l| l.trim_end_matches('\r').to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if paths.is_empty() {
                return Err(WriteError::Win32("empty file list"));
            }
            let payload = encode_hdrop(&paths);
            let handle = unsafe { alloc_and_fill(&payload)? };
            unsafe { SetClipboardData(CF_HDROP.0 as u32, Some(handle)) }
                .map_err(|_| WriteError::Win32("SetClipboardData(CF_HDROP)"))?;
            // Offering the paths as text as well is what Explorer does, and it
            // makes pasting into a terminal work.
            if let Ok(text_handle) = unsafe { alloc_and_fill(&encode_utf16_z(&paths.join("\r\n"))) } {
                let _ = unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(text_handle)) };
            }
        }
        ClipKind::Image => {
            let bmp = std::fs::read(image_bmp_path).map_err(|_| WriteError::Win32("read image"))?;
            let dib = bmp_to_dib(&bmp).ok_or(WriteError::Win32("decode image"))?;
            let handle = unsafe { alloc_and_fill(dib)? };
            unsafe { SetClipboardData(CF_DIB.0 as u32, Some(handle)) }
                .map_err(|_| WriteError::Win32("SetClipboardData(CF_DIB)"))?;
        }
    }
    Ok(())
}

/// Put a finished `CF_DIB` payload on the clipboard, replacing its contents.
///
/// Used by the snipper, which has the pixels in memory and no reason to write
/// them to a file first. The payload has to be a complete `CF_DIB` — a
/// `BITMAPINFOHEADER` followed by the pixel data — because that is what goes on
/// the clipboard verbatim.
pub fn put_dib(hwnd: HWND, dib: &[u8]) -> Result<(), WriteError> {
    if dib.is_empty() {
        return Err(WriteError::Win32("empty DIB"));
    }
    let _guard = ClipboardGuard::open(hwnd)?;
    if unsafe { EmptyClipboard() }.is_err() {
        return Err(WriteError::Win32("EmptyClipboard"));
    }
    let handle = unsafe { alloc_and_fill(dib)? };
    // From here the clipboard owns `handle`; do not free it.
    unsafe { SetClipboardData(CF_DIB.0 as u32, Some(handle)) }
        .map_err(|_| WriteError::Win32("SetClipboardData(CF_DIB)"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_header_and_terminators() {
        let bytes = encode_hdrop(&["C:\\a.txt".to_string(), "D:\\b.txt".to_string()]);
        assert!(bytes.len() > DropFiles::SIZE);

        // pFiles points just past the header.
        let p_files = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        assert_eq!(p_files as usize, DropFiles::SIZE);
        // fWide must be non-zero or Explorer reads ANSI.
        let f_wide = i32::from_le_bytes(bytes[16..20].try_into().unwrap());
        assert_eq!(f_wide, 1);

        let units: Vec<u16> = bytes[DropFiles::SIZE..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let expected: Vec<u16> = "C:\\a.txt\0D:\\b.txt\0\0".encode_utf16().collect();
        assert_eq!(units, expected);
    }

    #[test]
    fn utf16_text_is_nul_terminated() {
        let bytes = encode_utf16_z("hi");
        assert_eq!(bytes.len(), 6);
        assert_eq!(&bytes[4..6], &[0, 0]);
    }

    #[test]
    fn bmp_header_is_stripped_for_cf_dib() {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0u8; 12]);
        bmp.extend_from_slice(b"payload");
        assert_eq!(bmp_to_dib(&bmp).unwrap(), b"payload");
        assert!(bmp_to_dib(b"XX").is_none());
    }
}
