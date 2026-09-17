//! Image I/O decoder: the macOS implementation of the core's
//! ImageDecoder trait. Runs on core worker threads; the CGImageSource
//! never leaves the thread (it is !Send), only the CGImage does.

use std::path::Path;
use std::time::SystemTime;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL};
use objc2_core_graphics::CGImage;
use objc2_image_io::{
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
    kCGImagePropertyGIFDelayTime, kCGImagePropertyGIFDictionary,
    kCGImagePropertyGIFUnclampedDelayTime, kCGImagePropertyPixelHeight,
    kCGImagePropertyPixelWidth, kCGImagePropertyWebPDelayTime, kCGImagePropertyWebPDictionary,
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

/// EXIF capture date from the header, no decode.
fn exif_date(path: &Path) -> Option<SystemTime> {
    let url = CFURL::from_file_path(path)?;
    unsafe {
        let src = CGImageSource::with_url(&url, None)?;
        let props = src.properties_at_index(src.primary_image_index(), None)?;
        let props: CFRetained<CFDictionary<CFString, CFType>> =
            CFRetained::cast_unchecked(props);
        let exif = props
            .get(kCGImagePropertyExifDictionary)?
            .downcast::<CFDictionary>()
            .ok()?;
        let exif: CFRetained<CFDictionary<CFString, CFType>> = CFRetained::cast_unchecked(exif);
        let value = exif
            .get(kCGImagePropertyExifDateTimeOriginal)?
            .downcast::<CFString>()
            .ok()?;
        wene_core::parse_exif_datetime(&value.to_string())
    }
}

/// When the file landed in its folder (Finder's "date added").
fn date_added(path: &Path) -> Option<SystemTime> {
    use objc2_foundation::{NSURLAddedToDirectoryDateKey, NSArray, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    let keys = NSArray::from_slice(&[unsafe { NSURLAddedToDirectoryDateKey }]);
    let values = url.resourceValuesForKeys_error(&keys).ok()?;
    let date = values.objectForKey(unsafe { NSURLAddedToDirectoryDateKey })?;
    let date = date.downcast::<objc2_foundation::NSDate>().ok()?;
    let secs = date.timeIntervalSince1970();
    (secs >= 0.0).then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(secs))
}

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

    fn file_dates(&self, path: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
        (exif_date(path), date_added(path))
    }

    /// Animated GIF/WebP: decode every frame with its delay. Files
    /// whose decoded frames would blow the budget play as a static
    /// first frame instead.
    fn decode_frames(&self, path: &Path) -> Option<Vec<(Self::Image, f64)>> {
        if !is_animated_ext(path) {
            return None;
        }
        let url = CFURL::from_file_path(path)?;
        unsafe {
            let src = CGImageSource::with_url(&url, None)?;
            let count = src.count();
            if count < 2 {
                return None;
            }
            // Budget: whole animation decoded up front.
            // ponytail: streaming/looping decode if real GIFs hit it.
            const FRAME_BUDGET_BYTES: usize = 256 * 1024 * 1024;
            let (w, h) = image_dimensions(path)?;
            if (w as usize) * (h as usize) * 4 * count > FRAME_BUDGET_BYTES {
                return None;
            }
            let mut frames = Vec::with_capacity(count);
            for index in 0..count {
                let image = src.image_at_index(index, None)?;
                frames.push((image, frame_delay(&src, index)));
            }
            Some(frames)
        }
    }
}

pub fn is_animated_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            e == "gif" || e == "webp"
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_gif_frames_decode() {
        let path = std::env::temp_dir().join("wene-anim-selfcheck.gif");
        std::fs::write(&path, crate::e2e::ANIMATED_GIF).unwrap();
        let frames = ImageIoDecoder.decode_frames(&path).expect("gif is animated");
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|(_, delay)| *delay > 0.0));
        // Static file: no frames path.
        assert!(ImageIoDecoder.decode_frames(std::path::Path::new("/x/a.jpg")).is_none());
        let _ = std::fs::remove_file(&path);
    }
}

/// Per-frame delay in seconds. GIF prefers the unclamped value;
/// zero/absent falls back to the 0.1 s browsers use.
fn frame_delay(src: &CGImageSource, index: usize) -> f64 {
    let delay = unsafe {
        let props = src.properties_at_index(index, None);
        props.and_then(|props| {
            let props: CFRetained<CFDictionary<CFString, CFType>> =
                CFRetained::cast_unchecked(props);
            let number = |dict: &CFDictionary<CFString, CFType>, key: &CFString| {
                dict.get(key)
                    .and_then(|v| v.downcast::<CFNumber>().ok())
                    .and_then(|n| n.as_f64())
            };
            let sub = |key: &'static CFString| {
                props
                    .get(key)
                    .and_then(|v| v.downcast::<CFDictionary>().ok())
                    .map(|d| {
                        CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(d)
                    })
            };
            if let Some(gif) = sub(kCGImagePropertyGIFDictionary) {
                number(&gif, kCGImagePropertyGIFUnclampedDelayTime)
                    .filter(|&d| d > 0.0)
                    .or_else(|| number(&gif, kCGImagePropertyGIFDelayTime))
            } else if let Some(webp) = sub(kCGImagePropertyWebPDictionary) {
                number(&webp, kCGImagePropertyWebPDelayTime)
            } else {
                None
            }
        })
    };
    match delay {
        Some(d) if d > 0.011 => d,
        _ => 0.1,
    }
}
