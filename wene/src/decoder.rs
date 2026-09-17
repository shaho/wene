//! Image I/O decoder: the macOS implementation of the core's
//! ImageDecoder trait. Runs on core worker threads; the CGImageSource
//! never leaves the thread (it is !Send), only the CGImage does.

use std::path::Path;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL};
use objc2_core_graphics::CGImage;
use objc2_image_io::{
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize, CGImageSource,
};
use wene_core::ImageDecoder;

pub struct ImageIoDecoder;

impl ImageDecoder for ImageIoDecoder {
    type Image = CFRetained<CGImage>;

    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        let url = CFURL::from_file_path(path)?;
        unsafe {
            let src = CGImageSource::with_url(&url, None)?;
            let idx = src.primary_image_index();
            let yes: &CFBoolean = CFBoolean::new(true);
            let size = CFNumber::new_i32(max_px);
            // Always + WithTransform: bounded size, EXIF orientation
            // baked in. The embedded-thumb IfAbsent fast path comes
            // with the real cache in the next map.
            let opts: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
                &[
                    kCGImageSourceCreateThumbnailFromImageAlways,
                    kCGImageSourceThumbnailMaxPixelSize,
                    kCGImageSourceCreateThumbnailWithTransform,
                ],
                &[yes.as_ref(), size.as_ref(), yes.as_ref()],
            );
            src.thumbnail_at_index(idx, Some(opts.as_ref()))
        }
    }
}
