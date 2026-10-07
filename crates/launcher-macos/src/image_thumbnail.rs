//! In-process ImageIO thumbnails, called only from preview worker threads.
//! API/option contracts: Apple's ImageIO.framework SDK headers and
//! https://developer.apple.com/documentation/imageio/cgimagesourcecreatethumbnailatindex(_:_:_:)
use anyhow::{Context, Result, ensure};
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType, kCFAllocatorDefault},
    boolean::CFBoolean,
    data::{CFData, CFDataCreateMutable},
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
    url::{CFURL, CFURLRef},
};
use launcher_core::CancellationToken;
use std::{
    path::Path,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

// ImageIO cannot interrupt an active synchronous decode. Limit expensive work
// to two images, and let cancelled requests stop while waiting for a slot.
struct DecodeSlots(AtomicUsize);
struct DecodePermit<'a>(&'a DecodeSlots);
impl DecodeSlots {
    fn acquire(&self, token: &CancellationToken) -> Result<DecodePermit<'_>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            ensure!(!token.is_cancelled(), "preview cancelled");
            if self
                .0
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                    (used < 2).then_some(used + 1)
                })
                .is_ok()
            {
                return Ok(DecodePermit(self));
            }
            ensure!(
                Instant::now() < deadline,
                "image preview workers are busy. Use Quick Look."
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Release);
    }
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageSourceShouldCache: CFStringRef;
    static kCGImageSourceCreateThumbnailFromImageAlways: CFStringRef;
    static kCGImageSourceCreateThumbnailWithTransform: CFStringRef;
    static kCGImageSourceThumbnailMaxPixelSize: CFStringRef;
    static kCGImagePropertyPixelWidth: CFStringRef;
    static kCGImagePropertyPixelHeight: CFStringRef;
    fn CGImageSourceCopyPropertiesAtIndex(
        source: CFTypeRef,
        index: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
    fn CGImageSourceCreateWithData(data: CFTypeRef, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceCreateWithURL(url: CFURLRef, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceCreateThumbnailAtIndex(
        source: CFTypeRef,
        index: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
    fn CGImageDestinationCreateWithData(
        data: CFTypeRef,
        kind: CFStringRef,
        count: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
    fn CGImageDestinationAddImage(destination: CFTypeRef, image: CFTypeRef, properties: CFTypeRef);
    fn CGImageDestinationFinalize(destination: CFTypeRef) -> bool;
}

const PREVIEW_MAX_PIXEL_SIZE: i32 = 800;
const PREVIEW_MAX_BYTES: usize = 4 * 1024 * 1024;
const ICON_MAX_PIXEL_SIZE: i32 = 64;
const ICON_MAX_BYTES: usize = 256 * 1024;

/// Decodes one image from `source` and re-encodes it as a PNG thumbnail.
///
/// # Safety
///
/// `source` must be a live `CGImageSource` object.
unsafe fn png_thumbnail(
    source: &CFType,
    token: &CancellationToken,
    max_pixel_size: i32,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    // SAFETY: all native objects below follow CF create/get ownership rules,
    // remain alive through their calls, and are released by their Rust wrappers.
    // ImageIO functions are used on this worker only, never across an await.
    unsafe {
        let key = |value| CFString::wrap_under_get_rule(value);
        let properties_ref =
            CGImageSourceCopyPropertiesAtIndex(source.as_CFTypeRef(), 0, ptr::null());
        ensure!(!properties_ref.is_null(), "reading image dimensions failed");
        let properties =
            CFDictionary::<CFString, CFType>::wrap_under_create_rule(properties_ref.cast());
        let dimension = |name| {
            properties
                .find(key(name))
                .and_then(|value| value.downcast::<CFNumber>())
                .and_then(|value| value.to_i64())
        };
        let width = dimension(kCGImagePropertyPixelWidth).context("missing image width")?;
        let height = dimension(kCGImagePropertyPixelHeight).context("missing image height")?;
        ensure!(
            width > 0
                && height > 0
                && width
                    .checked_mul(height)
                    .is_some_and(|pixels| pixels <= 100_000_000),
            "image dimensions are too large for an inline preview. Use Quick Look."
        );
        ensure!(!token.is_cancelled(), "preview cancelled");
        let options = CFDictionary::from_CFType_pairs(&[
            (
                key(kCGImageSourceCreateThumbnailFromImageAlways),
                CFBoolean::true_value().as_CFType(),
            ),
            (
                key(kCGImageSourceCreateThumbnailWithTransform),
                CFBoolean::true_value().as_CFType(),
            ),
            (
                key(kCGImageSourceThumbnailMaxPixelSize),
                CFNumber::from(max_pixel_size).as_CFType(),
            ),
        ]);
        let image_ref =
            CGImageSourceCreateThumbnailAtIndex(source.as_CFTypeRef(), 0, options.as_CFTypeRef());
        ensure!(!image_ref.is_null(), "image thumbnail decoding failed");
        let image = CFType::wrap_under_create_rule(image_ref);
        ensure!(!token.is_cancelled(), "preview cancelled");
        let data_ref = CFDataCreateMutable(kCFAllocatorDefault, 0);
        ensure!(!data_ref.is_null(), "allocating thumbnail buffer failed");
        let data = CFData::wrap_under_create_rule(data_ref.cast());
        let kind = CFString::new("public.png");
        let destination_ref = CGImageDestinationCreateWithData(
            data.as_CFTypeRef(),
            kind.as_concrete_TypeRef(),
            1,
            ptr::null(),
        );
        ensure!(!destination_ref.is_null(), "creating PNG encoder failed");
        let destination = CFType::wrap_under_create_rule(destination_ref);
        CGImageDestinationAddImage(
            destination.as_CFTypeRef(),
            image.as_CFTypeRef(),
            ptr::null(),
        );
        ensure!(
            CGImageDestinationFinalize(destination.as_CFTypeRef()),
            "encoding thumbnail failed"
        );
        ensure!(!token.is_cancelled(), "preview cancelled");
        ensure!(
            data.len() as usize <= max_bytes,
            "oversized preview thumbnail"
        );
        Ok(data.bytes().to_vec())
    }
}

pub(crate) fn thumbnail(path: &Path, token: &CancellationToken) -> Result<Vec<u8>> {
    static DECODERS: DecodeSlots = DecodeSlots(AtomicUsize::new(0));
    let _permit = DECODERS.acquire(token)?;
    let url = CFURL::from_path(path, false).context("creating preview file URL")?;
    // SAFETY: native objects follow CF create/get ownership rules and stay alive
    // through their calls; ImageIO runs on this worker thread only.
    unsafe {
        let key = |value| CFString::wrap_under_get_rule(value);
        let source_options = CFDictionary::from_CFType_pairs(&[(
            key(kCGImageSourceShouldCache),
            CFBoolean::false_value().as_CFType(),
        )]);
        let source_ref =
            CGImageSourceCreateWithURL(url.as_concrete_TypeRef(), source_options.as_CFTypeRef());
        ensure!(!source_ref.is_null(), "unsupported image preview");
        let source = CFType::wrap_under_create_rule(source_ref);
        png_thumbnail(&source, token, PREVIEW_MAX_PIXEL_SIZE, PREVIEW_MAX_BYTES)
    }
}

/// Renders in-memory image data (for example an `NSImage` TIFF representation)
/// as a PNG icon no larger than 64x64.
pub(crate) fn icon_thumbnail(image_data: &[u8]) -> Result<Vec<u8>> {
    let data = CFData::from_buffer(image_data);
    // SAFETY: `data` owns a valid CFData; the created source is released by
    // `source`; ImageIO runs on this thread only.
    unsafe {
        let source_ref = CGImageSourceCreateWithData(data.as_CFTypeRef(), ptr::null());
        ensure!(!source_ref.is_null(), "unsupported icon image");
        let source = CFType::wrap_under_create_rule(source_ref);
        png_thumbnail(
            &source,
            &CancellationToken::default(),
            ICON_MAX_PIXEL_SIZE,
            ICON_MAX_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_decode_slots_are_bounded_and_released() {
        let slots = DecodeSlots(AtomicUsize::new(0));
        let token = CancellationToken::default();
        let first = slots.acquire(&token).unwrap();
        let second = slots.acquire(&token).unwrap();
        assert_eq!(slots.0.load(Ordering::Relaxed), 2);
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(slots.acquire(&cancelled).is_err());
        drop(first);
        let third = slots.acquire(&token).unwrap();
        drop((second, third));
        assert_eq!(slots.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn waiting_for_a_decode_slot_observes_cancellation() {
        let slots = std::sync::Arc::new(DecodeSlots(AtomicUsize::new(0)));
        let token = CancellationToken::default();
        let first = slots.acquire(&token).unwrap();
        let second = slots.acquire(&token).unwrap();
        let waiting_slots = slots.clone();
        let waiting_token = token.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            tx.send(()).unwrap();
            waiting_slots
                .acquire(&waiting_token)
                .err()
                .unwrap()
                .to_string()
        });
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        token.cancel();
        assert!(waiter.join().unwrap().contains("cancelled"));
        assert_eq!(slots.0.load(Ordering::Relaxed), 2);
        drop((first, second));
        assert_eq!(slots.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn native_images_are_png_and_malformed_inputs_release_decode_slots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"not an image").unwrap();
        for _ in 0..3 {
            let error = thumbnail(&path, &CancellationToken::default()).unwrap_err();
            assert!(!error.to_string().contains("workers are busy"));
        }
        std::fs::write(&path, include_bytes!("../tests/fixtures/pixel.png")).unwrap();
        let png = thumbnail(&path, &CancellationToken::default()).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert!(width > 0 && width <= 800 && height > 0 && height <= 800);
    }
}
