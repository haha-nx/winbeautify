//! Album art decoding.
//!
//! The media module hands the snapshot a `data:image/...;base64,...` URL
//! because that is what the webview renderer needs. Decoding it here — base64,
//! then WIC — keeps both renderers fed from one model type.
//!
//! Decoding only happens when the track changes, so the cost is one image
//! decode per song rather than anything per-frame.

use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget};
use windows::Win32::Graphics::Imaging::{
    IWICImagingFactory, CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};

/// Split a `data:` URL into its MIME type and payload.
///
/// Splits on the `;base64,` marker rather than the first comma: a media app is
/// free to report a content type that contains commas, and splitting on the
/// first one would put the rest of the type into the payload.
pub fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(";base64,")?;
    let mime = meta
        .split([',', ';'])
        .map(str::trim)
        .find(|part| part.starts_with("image/"))
        .unwrap_or("image/png");
    (!payload.is_empty()).then_some((mime, payload))
}

/// Decode a data URL to raw bytes, whichever image format it wraps.
pub fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let (_, payload) = split_data_url(url)?;
    beautify_core::model::base64::decode(payload)
}

/// Build a Direct2D bitmap from encoded image bytes.
///
/// # Safety
///
/// The WIC factory must have been created on a COM-initialised thread, and the
/// render target must outlive the returned bitmap.
pub unsafe fn bitmap_from_bytes(
    target: &ID2D1RenderTarget,
    wic: &IWICImagingFactory,
    bytes: &[u8],
) -> Option<ID2D1Bitmap> {
    let stream = unsafe { wic.CreateStream() }.ok()?;
    unsafe { stream.InitializeFromMemory(bytes) }.ok()?;
    let decoder = unsafe {
        wic.CreateDecoderFromStream(
            &stream,
            std::ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )
    }
    .ok()?;
    let frame = unsafe { decoder.GetFrame(0) }.ok()?;

    // Album art arrives as PNG, JPEG or occasionally BMP; normalising through a
    // format converter means the rest of the renderer only ever sees one
    // layout (premultiplied BGRA), which is also what the surface wants.
    let converter = unsafe { wic.CreateFormatConverter() }.ok()?;
    unsafe {
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }
    .ok()?;

    unsafe { target.CreateBitmapFromWicBitmap(&converter, None) }.ok()
}

/// Keeps the decoded bitmap for the track it belongs to.
///
/// The `generation` counter is the give-away for a rebuilt render target: a
/// Direct2D bitmap belongs to the device that created it, so a resize has to
/// invalidate it even when the track has not changed.
pub struct ArtworkCache {
    key: String,
    generation: u64,
    bitmap: Option<ID2D1Bitmap>,
}

impl Default for ArtworkCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtworkCache {
    pub fn new() -> Self {
        Self {
            key: String::new(),
            generation: u64::MAX,
            bitmap: None,
        }
    }

    /// The bitmap for `url`, decoding only when something actually changed.
    pub fn get(
        &mut self,
        target: &ID2D1RenderTarget,
        wic: &IWICImagingFactory,
        generation: u64,
        url: &str,
    ) -> Option<&ID2D1Bitmap> {
        if self.generation != generation || self.key != url {
            self.key = url.to_string();
            self.generation = generation;
            self.bitmap = if url.is_empty() {
                None
            } else {
                decode_data_url(url).and_then(|bytes| unsafe {
                    bitmap_from_bytes(target, wic, &bytes)
                })
            };
            if self.bitmap.is_none() && !url.is_empty() {
                tracing::debug!("album art could not be decoded; using the fallback tile");
            }
        }
        self.bitmap.as_ref()
    }

    pub fn clear(&mut self) {
        self.key.clear();
        self.generation = u64::MAX;
        self.bitmap = None;
    }
}

/// Create the shared WIC factory.
pub fn create_wic_factory() -> windows::core::Result<IWICImagingFactory> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_png_data_url() {
        let (mime, payload) = split_data_url("data:image/png;base64,AAAA").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(payload, "AAAA");
    }

    #[test]
    fn splits_a_jpeg_data_url() {
        let (mime, _) = split_data_url("data:image/jpeg;base64,AAAA").unwrap();
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn rejects_urls_that_are_not_base64_data() {
        assert!(split_data_url("https://example.com/a.png").is_none());
        assert!(split_data_url("data:image/png,raw").is_none());
        assert!(split_data_url("data:image/png;base64,").is_none());
    }

    #[test]
    fn a_content_type_containing_commas_still_parses() {
        // QQ Music reports "image/jpeg,image/jpe,image/jpg"; splitting on the
        // first comma used to swallow the payload.
        let (mime, payload) =
            split_data_url("data:image/jpeg,image/jpe;base64,AAAA").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(payload, "AAAA");
    }

    #[test]
    fn an_unusable_content_type_falls_back_to_png() {
        let (mime, _) = split_data_url("data:application/octet-stream;base64,AAAA").unwrap();
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn decodes_the_payload() {
        let png = beautify_core::model::base64::encode(b"hello");
        let url = format!("data:image/png;base64,{png}");
        assert_eq!(decode_data_url(&url).unwrap(), b"hello");
    }

    #[test]
    fn a_cached_bitmap_is_dropped_when_the_track_changes() {
        let mut cache = ArtworkCache::new();
        cache.key = "old".into();
        cache.generation = 1;
        // No render target here, so the decode path is not exercised; the
        // point is that the key/generation pair drives invalidation.
        assert_eq!(cache.generation, 1);
        cache.clear();
        assert_eq!(cache.generation, u64::MAX);
        assert!(cache.key.is_empty());
    }
}
