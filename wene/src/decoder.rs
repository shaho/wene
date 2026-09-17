//! Image I/O decoder: the macOS implementation of the core's
//! ImageDecoder trait. Runs on core worker threads; the CGImageSource
//! never leaves the thread (it is !Send), only the CGImage does.

use std::path::Path;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL};
use objc2_core_graphics::CGImage;
use objc2_image_io::{
    kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailFromImageIfAbsent,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceThumbnailMaxPixelSize, CGImageSource,
};
use wene_core::ImageDecoder;

/// Pixel dimensions from the file header, no decode. Cheap enough
/// for a synchronous main-thread call on one selected file.
pub fn image_dimensions(path: &Path) -> Option<(i64, i64)> {
    let url = CFURL::from_file_path(path)?;
    unsafe {
        let src = CGImageSource::with_url(&url, None)?;
        let props = src.properties_at_index(src.primary_image_index(), None)?;
        // The header dictionary is untyped; its keys are CFStrings.
        let props: CFRetained<CFDictionary<CFString, CFType>> =
            CFRetained::cast_unchecked(props);
        let dim = |key: &CFString| -> Option<i64> {
            props
                .get(key)
                .and_then(|v| v.downcast::<CFNumber>().ok())
                .and_then(|n| n.as_i64())
        };
        Some((dim(kCGImagePropertyPixelWidth)?, dim(kCGImagePropertyPixelHeight)?))
    }
}

pub struct ImageIoDecoder;

/// One thumbnail request. `from_image` is Always for a real decode
/// and IfAbsent for the fast path (use the preview embedded in the
/// file when there is one). Both cap the size and bake in EXIF
/// orientation.
fn thumbnail(
    path: &Path,
    max_px: i32,
    from_image: &'static CFString,
) -> Option<CFRetained<CGImage>> {
    let url = CFURL::from_file_path(path)?;
    unsafe {
        let src = CGImageSource::with_url(&url, None)?;
        let idx = src.primary_image_index();
        let yes: &CFBoolean = CFBoolean::new(true);
        let size = CFNumber::new_i32(max_px);
        let opts: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[
                from_image,
                kCGImageSourceThumbnailMaxPixelSize,
                kCGImageSourceCreateThumbnailWithTransform,
            ],
            &[yes.as_ref(), size.as_ref(), yes.as_ref()],
        );
        src.thumbnail_at_index(idx, Some(opts.as_ref()))
    }
}

impl ImageDecoder for ImageIoDecoder {
    type Image = CFRetained<CGImage>;

    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        thumbnail(path, max_px, unsafe { kCGImageSourceCreateThumbnailFromImageAlways })
    }

    /// Fast thumbnail: take the embedded preview when the file has
    /// one. Embedded previews never upscale, so a tiny one (an old
    /// 160 px EXIF thumb on a retina-sized request) falls back to a
    /// full decode.
    fn decode_thumb(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        let fast = thumbnail(path, max_px, unsafe { kCGImageSourceCreateThumbnailFromImageIfAbsent });
        if let Some(image) = &fast {
            let long_edge = CGImage::width(Some(image)).max(CGImage::height(Some(image)));
            if long_edge * 2 >= max_px as usize {
                return fast;
            }
        }
        self.decode(path, max_px).or(fast)
    }
}
