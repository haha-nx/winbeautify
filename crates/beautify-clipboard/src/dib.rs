//! Wrapping a `CF_DIB` payload into a real `.bmp` file.
//!
//! Windows hands out a `BITMAPINFOHEADER` plus a colour table plus pixels. A
//! `.bmp` file is exactly that, prefixed with a 14-byte `BITMAPFILEHEADER`. So
//! the whole conversion is a header splice plus one alpha fixup — no encoder
//! needed, and the webview can render the result directly.

/// `BITMAPINFOHEADER` fields we actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DibHeader {
    pub size: u32,
    pub width: i32,
    /// Negative for a top-down bitmap.
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: u32,
}

pub const BI_RGB: u32 = 0;
pub const BI_BITFIELDS: u32 = 3;
pub const BI_ALPHABITFIELDS: u32 = 6;

pub const BITMAPFILEHEADER_SIZE: usize = 14;

pub fn parse_header(dib: &[u8]) -> Option<DibHeader> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let u16_at = |o: usize| u16::from_le_bytes(dib[o..o + 2].try_into().unwrap());
    let header = DibHeader {
        size: u32_at(0),
        width: i32::from_le_bytes(dib[4..8].try_into().unwrap()),
        height: i32::from_le_bytes(dib[8..12].try_into().unwrap()),
        planes: u16_at(12),
        bit_count: u16_at(14),
        compression: u32_at(16),
    };
    // Sizes below 40 are the ancient BITMAPCOREHEADER, whose fields are 16-bit;
    // nothing modern produces those, so treat them as unsupported.
    if header.size < 40 || header.planes != 1 {
        return None;
    }
    Some(header)
}

/// Bytes between the end of the DIB header and the first pixel.
///
/// Covers the colour table for palettised images and the bit-field masks that
/// an old-style `BI_BITFIELDS` header stores *after* itself.
pub fn pixel_offset(header: &DibHeader) -> usize {
    let extra = match header.compression {
        BI_RGB if header.bit_count <= 8 => (1usize << header.bit_count) * 4,
        BI_BITFIELDS => {
            if header.size >= 52 {
                0 // masks live inside the V2+ header
            } else if header.size == 40 {
                12
            } else {
                16
            }
        }
        BI_ALPHABITFIELDS => 16,
        _ => 0,
    };
    BITMAPFILEHEADER_SIZE + header.size as usize + extra
}

/// Convert a `CF_DIB` buffer into a `.bmp` file image.
///
/// Returns `(bytes, width, height)`. `None` when the header is not something we
/// can safely describe.
pub fn dib_to_bmp(dib: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
    let header = parse_header(dib)?;
    let width = header.width.unsigned_abs();
    let height = header.height.unsigned_abs();
    if width == 0 || height == 0 || header.width < 0 {
        // Negative width is not a thing; negative *height* is (top-down) and is
        // preserved as-is.
        return None;
    }

    let offset = pixel_offset(&header);
    // `pixel_offset` is relative to the start of the finished *file*, so the
    // 14-byte file header has to come off before comparing against the DIB.
    if offset < BITMAPFILEHEADER_SIZE || offset - BITMAPFILEHEADER_SIZE > dib.len() {
        return None;
    }

    let mut out = Vec::with_capacity(BITMAPFILEHEADER_SIZE + dib.len());
    let file_size = (BITMAPFILEHEADER_SIZE + dib.len()) as u32;
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // bfReserved1 + bfReserved2
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    out.extend_from_slice(dib);

    // 32bpp screenshots routinely carry an all-zero alpha channel. Browsers
    // honour it, so the image would render as a fully transparent rectangle.
    // If every alpha byte is zero the channel is meaningless — force it opaque.
    if header.bit_count == 32 && header.compression == BI_RGB {
        let pixels = &mut out[offset..];
        let has_alpha = pixels.chunks_exact(4).any(|px| px[3] != 0);
        if !has_alpha {
            for px in pixels.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
    }

    Some((out, width as i32, height as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 32bpp BI_RGB DIB of `w`x`h` with a constant colour.
    fn make_dib(w: i32, h: i32, bgra: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&32u16.to_le_bytes());
        v.extend_from_slice(&BI_RGB.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        v.extend_from_slice(&[0u8; 16]); // resolution + palette counts
        for _ in 0..(w * h) {
            v.extend_from_slice(&bgra);
        }
        v
    }

    #[test]
    fn header_round_trips() {
        let dib = make_dib(3, 2, [1, 2, 3, 255]);
        let h = parse_header(&dib).unwrap();
        assert_eq!((h.width, h.height, h.bit_count), (3, 2, 32));
        assert_eq!(pixel_offset(&h), 54);
    }

    #[test]
    fn rejects_short_and_core_headers() {
        assert!(parse_header(&[0u8; 10]).is_none());
        let mut core = vec![0u8; 40];
        core[0..4].copy_from_slice(&12u32.to_le_bytes()); // BITMAPCOREHEADER
        assert!(parse_header(&core).is_none());
    }

    #[test]
    fn bmp_header_points_at_the_pixels() {
        let dib = make_dib(2, 2, [10, 20, 30, 255]);
        let (bmp, w, h) = dib_to_bmp(&dib).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 14 + dib.len());
        let off = u32::from_le_bytes(bmp[10..14].try_into().unwrap());
        assert_eq!(off as usize, 54);
        // First pixel, BGRA
        assert_eq!(&bmp[off as usize..off as usize + 4], &[10, 20, 30, 255]);
    }

    #[test]
    fn zero_alpha_channel_is_forced_opaque() {
        let dib = make_dib(2, 1, [5, 6, 7, 0]);
        let (bmp, _, _) = dib_to_bmp(&dib).unwrap();
        assert_eq!(&bmp[54..58], &[5, 6, 7, 255]);
    }

    #[test]
    fn real_alpha_is_preserved() {
        let mut dib = make_dib(2, 1, [5, 6, 7, 0]);
        // Give the second pixel a meaningful alpha.
        let n = dib.len();
        dib[n - 1] = 0x40;
        let (bmp, _, _) = dib_to_bmp(&dib).unwrap();
        assert_eq!(bmp[54 + 3], 0x00, "fully transparent pixel stays transparent");
        assert_eq!(bmp[58 + 3], 0x40, "partial alpha is kept");
    }

    #[test]
    fn palette_size_is_accounted_for() {
        let mut h = DibHeader {
            size: 40,
            width: 4,
            height: 4,
            planes: 1,
            bit_count: 8,
            compression: BI_RGB,
        };
        // 8bpp: 256 palette entries * 4 bytes
        assert_eq!(pixel_offset(&h), 14 + 40 + 1024);
        h.bit_count = 24;
        assert_eq!(pixel_offset(&h), 54);
        h.compression = BI_BITFIELDS;
        assert_eq!(pixel_offset(&h), 14 + 40 + 12);
        h.size = 52;
        assert_eq!(pixel_offset(&h), 14 + 52);
    }

    #[test]
    fn top_down_bitmap_keeps_negative_height_but_reports_positive() {
        let dib = make_dib(2, -2, [1, 1, 1, 255]);
        let (_, w, h) = dib_to_bmp(&dib).unwrap();
        assert_eq!((w, h), (2, 2));
    }

    #[test]
    fn a_dib_shorter_than_its_own_header_is_rejected() {
        let mut dib = make_dib(8, 8, [0, 0, 0, 255]);
        dib.truncate(30); // shorter than the 40-byte BITMAPINFOHEADER
        assert!(dib_to_bmp(&dib).is_none());
    }

    #[test]
    fn a_truncated_pixel_tail_is_tolerated() {
        // A short pixel array cannot be detected reliably — plenty of producers
        // leave `biSizeImage` at zero — and a short BMP renders as blank in the
        // webview rather than breaking anything, so the header is all we check.
        let mut dib = make_dib(8, 8, [0, 0, 0, 255]);
        dib.truncate(50);
        let (bmp, w, h) = dib_to_bmp(&dib).expect("header and palette are intact");
        assert_eq!((w, h), (8, 8));
        assert_eq!(bmp.len(), BITMAPFILEHEADER_SIZE + 50);
    }

    #[test]
    fn zero_sized_images_are_rejected() {
        let dib = make_dib(0, 0, [0, 0, 0, 0]);
        assert_eq!(dib.len(), 40);
        assert!(dib_to_bmp(&dib).is_none());
    }
}
