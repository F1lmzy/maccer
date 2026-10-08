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

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef;
    fn CGBitmapContextCreate(
        data: *mut u8,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: CFTypeRef,
        bitmap_info: u32,
    ) -> CFTypeRef;
    fn CGContextDrawImage(context: CFTypeRef, rect: CGRect, image: CFTypeRef);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const PREVIEW_MAX_PIXEL_SIZE: i32 = 800;
#[cfg(test)]
const PREVIEW_MAX_BYTES: usize = 4 * 1024 * 1024;
const ICON_MAX_PIXEL_SIZE: i32 = 64;
const ICON_MAX_BYTES: usize = 256 * 1024;

// `tracing` labels for timed stages. The preview variants stay distinct from
// icon extraction so icon timing is never mistaken for a preview.
#[cfg(test)]
const PIPELINE_PNG: &str = "png";
const PIPELINE_RAW_PIXELS: &str = "raw_pixels";
const PIPELINE_ICON: &str = "icon";

/// Times one pipeline stage and emits a structured `tracing` event when the
/// stage ends. The guard logs on drop, so a stage that exits early through `?`
/// still reports its duration; `fail` attaches the stage's error message.
struct StageTimer {
    pipeline: &'static str,
    stage: &'static str,
    started: Instant,
    error: Option<String>,
    emitted: bool,
}

impl StageTimer {
    fn start(pipeline: &'static str, stage: &'static str) -> Self {
        Self {
            pipeline,
            stage,
            started: Instant::now(),
            error: None,
            emitted: false,
        }
    }

    /// Records why this stage failed. The first message wins.
    fn fail(&mut self, error: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(error.into());
        }
    }

    /// Emits the stage event now, for stages whose values outlive the timed
    /// scope (such as a decode permit held for the whole pipeline).
    fn finish(mut self) {
        self.emit();
    }

    fn emit(&mut self) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        // `as_secs_f64` keeps microsecond resolution: debug builds need
        // sub-millisecond stages such as metadata reads.
        let duration_ms = self.started.elapsed().as_secs_f64() * 1_000.0;
        match &self.error {
            Some(error) => tracing::debug!(
                pipeline = self.pipeline,
                stage = self.stage,
                duration_ms,
                error = %error,
                "image stage"
            ),
            None => tracing::debug!(
                pipeline = self.pipeline,
                stage = self.stage,
                duration_ms,
                "image stage"
            ),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        self.emit();
    }
}

/// Like `ensure!`, but records the failing message on a `StageTimer`. The
/// returned error text and control flow are identical to `ensure!`.
macro_rules! stage_ensure {
    ($timer:expr, $condition:expr, $message:expr) => {
        if !$condition {
            ($timer).fail($message);
            anyhow::bail!($message);
        }
    };
}

/// Decodes one image from `source` and re-encodes it as a PNG thumbnail.
///
/// `pipeline` only labels the emitted timing events.
///
/// # Safety
///
/// `source` must be a live `CGImageSource` object.
unsafe fn png_thumbnail(
    source: &CFType,
    token: &CancellationToken,
    pipeline: &'static str,
    max_pixel_size: i32,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    // SAFETY: all native objects below follow CF create/get ownership rules,
    // remain alive through their calls, and are released by their Rust wrappers.
    // ImageIO functions are used on this worker only, never across an await.
    unsafe {
        let key = |value| CFString::wrap_under_get_rule(value);
        let mut metadata = StageTimer::start(pipeline, "metadata");
        let properties_ref =
            CGImageSourceCopyPropertiesAtIndex(source.as_CFTypeRef(), 0, ptr::null());
        stage_ensure!(
            &mut metadata,
            !properties_ref.is_null(),
            "reading image dimensions failed"
        );
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
        stage_ensure!(
            &mut metadata,
            width > 0
                && height > 0
                && width
                    .checked_mul(height)
                    .is_some_and(|pixels| pixels <= 100_000_000),
            "image dimensions are too large for an inline preview. Use Quick Look."
        );
        stage_ensure!(&mut metadata, !token.is_cancelled(), "preview cancelled");
        metadata.finish();

        let mut decode = StageTimer::start(pipeline, "imageio_decode_resize");
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
        stage_ensure!(
            &mut decode,
            !image_ref.is_null(),
            "image thumbnail decoding failed"
        );
        let image = CFType::wrap_under_create_rule(image_ref);
        stage_ensure!(&mut decode, !token.is_cancelled(), "preview cancelled");
        decode.finish();

        let mut encode = StageTimer::start(pipeline, "png_encode");
        let data_ref = CFDataCreateMutable(kCFAllocatorDefault, 0);
        stage_ensure!(
            &mut encode,
            !data_ref.is_null(),
            "allocating thumbnail buffer failed"
        );
        let data = CFData::wrap_under_create_rule(data_ref.cast());
        let kind = CFString::new("public.png");
        let destination_ref = CGImageDestinationCreateWithData(
            data.as_CFTypeRef(),
            kind.as_concrete_TypeRef(),
            1,
            ptr::null(),
        );
        stage_ensure!(
            &mut encode,
            !destination_ref.is_null(),
            "creating PNG encoder failed"
        );
        let destination = CFType::wrap_under_create_rule(destination_ref);
        CGImageDestinationAddImage(
            destination.as_CFTypeRef(),
            image.as_CFTypeRef(),
            ptr::null(),
        );
        stage_ensure!(
            &mut encode,
            CGImageDestinationFinalize(destination.as_CFTypeRef()),
            "encoding thumbnail failed"
        );
        stage_ensure!(&mut encode, !token.is_cancelled(), "preview cancelled");
        stage_ensure!(
            &mut encode,
            data.len() as usize <= max_bytes,
            "oversized preview thumbnail"
        );
        encode.finish();

        let _copy = StageTimer::start(pipeline, "buffer_copy");
        Ok(data.bytes().to_vec())
    }
}

static DECODERS: DecodeSlots = DecodeSlots(AtomicUsize::new(0));

#[cfg(test)]
fn thumbnail(path: &Path, token: &CancellationToken) -> Result<Vec<u8>> {
    let slot = StageTimer::start(PIPELINE_PNG, "decode_slot_wait");
    let _permit = DECODERS.acquire(token)?;
    slot.finish();
    let mut source_open = StageTimer::start(PIPELINE_PNG, "source_open");
    let url = CFURL::from_path(path, false).context("creating preview file URL")?;
    // SAFETY: native objects follow CF create/get ownership rules and stay alive
    // through their calls; ImageIO runs on this worker thread only.
    unsafe {
        let key = |value| CFString::wrap_under_get_rule(value);
        let source_options = CFDictionary::from_CFType_pairs(&[(
            key(kCGImageSourceShouldCache),
            CFBoolean::false_value().as_CFType(),
        )]);
        // ImageIO reads the file lazily; this only opens the source. The bytes
        // are pulled in by the decode/resize stage below.
        let source_ref =
            CGImageSourceCreateWithURL(url.as_concrete_TypeRef(), source_options.as_CFTypeRef());
        stage_ensure!(
            &mut source_open,
            !source_ref.is_null(),
            "unsupported image preview"
        );
        let source = CFType::wrap_under_create_rule(source_ref);
        source_open.finish();
        png_thumbnail(
            &source,
            token,
            PIPELINE_PNG,
            PREVIEW_MAX_PIXEL_SIZE,
            PREVIEW_MAX_BYTES,
        )
    }
}

/// Returns a target-sized, top-to-bottom BGRA buffer with straight alpha.
/// ImageIO applies the source orientation transform before CoreGraphics draws it.
pub(crate) fn thumbnail_pixels(
    path: &Path,
    token: &CancellationToken,
) -> Result<(Vec<u8>, u32, u32)> {
    let slot = StageTimer::start(PIPELINE_RAW_PIXELS, "decode_slot_wait");
    let _permit = DECODERS.acquire(token)?;
    slot.finish();
    let mut source_open = StageTimer::start(PIPELINE_RAW_PIXELS, "source_open");
    let url = CFURL::from_path(path, false).context("creating preview file URL")?;
    // SAFETY: ImageIO/CoreGraphics objects are owned by CF wrappers and are
    // used only on this worker. The temporary pixel buffer outlives its context.
    unsafe {
        let key = |value| CFString::wrap_under_get_rule(value);
        let source_options = CFDictionary::from_CFType_pairs(&[(
            key(kCGImageSourceShouldCache),
            CFBoolean::false_value().as_CFType(),
        )]);
        // ImageIO reads the file lazily; this only opens the source. The bytes
        // are pulled in by the decode/resize stage below.
        let source_ref =
            CGImageSourceCreateWithURL(url.as_concrete_TypeRef(), source_options.as_CFTypeRef());
        stage_ensure!(
            &mut source_open,
            !source_ref.is_null(),
            "unsupported image preview"
        );
        let source = CFType::wrap_under_create_rule(source_ref);
        source_open.finish();

        let mut metadata = StageTimer::start(PIPELINE_RAW_PIXELS, "metadata");
        let properties_ref =
            CGImageSourceCopyPropertiesAtIndex(source.as_CFTypeRef(), 0, ptr::null());
        stage_ensure!(
            &mut metadata,
            !properties_ref.is_null(),
            "reading image dimensions failed"
        );
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
        stage_ensure!(
            &mut metadata,
            width > 0
                && height > 0
                && width
                    .checked_mul(height)
                    .is_some_and(|pixels| pixels <= 100_000_000),
            "image dimensions are too large for an inline preview. Use Quick Look."
        );
        stage_ensure!(&mut metadata, !token.is_cancelled(), "preview cancelled");
        metadata.finish();

        let mut decode = StageTimer::start(PIPELINE_RAW_PIXELS, "imageio_decode_resize");
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
                CFNumber::from(PREVIEW_MAX_PIXEL_SIZE).as_CFType(),
            ),
        ]);
        let image_ref =
            CGImageSourceCreateThumbnailAtIndex(source.as_CFTypeRef(), 0, options.as_CFTypeRef());
        stage_ensure!(
            &mut decode,
            !image_ref.is_null(),
            "image thumbnail decoding failed"
        );
        let image = CFType::wrap_under_create_rule(image_ref);
        stage_ensure!(&mut decode, !token.is_cancelled(), "preview cancelled");
        decode.finish();

        let mut draw = StageTimer::start(PIPELINE_RAW_PIXELS, "bgra_draw");
        let width = CGImageGetWidth(image.as_CFTypeRef());
        let height = CGImageGetHeight(image.as_CFTypeRef());
        ensure!(width > 0 && height > 0 && width <= 800 && height <= 800);
        let row_bytes = width.checked_mul(4).context("image row is too wide")?;
        let byte_len = row_bytes
            .checked_mul(height)
            .context("image is too large")?;
        let mut bgra = vec![0u8; byte_len];
        let color_space_ref = CGColorSpaceCreateDeviceRGB();
        stage_ensure!(
            &mut draw,
            !color_space_ref.is_null(),
            "creating image color space failed"
        );
        let color_space = CFType::wrap_under_create_rule(color_space_ref);
        // Little-endian premultiplied-first gives in-memory BGRA.
        const PREMULTIPLIED_FIRST: u32 = 2;
        const BYTE_ORDER_32_LITTLE: u32 = 0x2000;
        let context_ref = CGBitmapContextCreate(
            bgra.as_mut_ptr(),
            width,
            height,
            8,
            row_bytes,
            color_space.as_CFTypeRef(),
            PREMULTIPLIED_FIRST | BYTE_ORDER_32_LITTLE,
        );
        stage_ensure!(
            &mut draw,
            !context_ref.is_null(),
            "creating image bitmap context failed"
        );
        let context = CFType::wrap_under_create_rule(context_ref);
        // CGContextDrawImage returns pixels in top-to-bottom memory order,
        // matching the row order GPUI expects.
        CGContextDrawImage(
            context.as_CFTypeRef(),
            CGRect {
                origin: CGPoint { x: 0., y: 0. },
                size: CGSize {
                    width: width as f64,
                    height: height as f64,
                },
            },
            image.as_CFTypeRef(),
        );
        stage_ensure!(&mut draw, !token.is_cancelled(), "preview cancelled");
        draw.finish();

        let _alpha = StageTimer::start(PIPELINE_RAW_PIXELS, "alpha_unpremultiply");
        for pixel in bgra.chunks_exact_mut(4) {
            let alpha = u16::from(pixel[3]);
            if alpha == 0 {
                pixel[..3].fill(0);
            } else if alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        Ok((bgra, width as u32, height as u32))
    }
}

/// Renders in-memory image data (for example an `NSImage` TIFF representation)
/// as a PNG icon no larger than 64x64.
pub(crate) fn icon_thumbnail(image_data: &[u8]) -> Result<Vec<u8>> {
    let data = CFData::from_buffer(image_data);
    let mut source_open = StageTimer::start(PIPELINE_ICON, "source_open");
    // SAFETY: `data` owns a valid CFData; the created source is released by
    // `source`; ImageIO runs on this thread only.
    unsafe {
        let source_ref = CGImageSourceCreateWithData(data.as_CFTypeRef(), ptr::null());
        stage_ensure!(
            &mut source_open,
            !source_ref.is_null(),
            "unsupported icon image"
        );
        let source = CFType::wrap_under_create_rule(source_ref);
        source_open.finish();
        png_thumbnail(
            &source,
            &CancellationToken::default(),
            PIPELINE_ICON,
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
    fn raw_pixels_apply_exif_orientation_and_release_slots_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotated.tiff");
        std::fs::write(&path, include_bytes!("../tests/fixtures/rotated.tiff")).unwrap();
        let (bgra, width, height) = thumbnail_pixels(&path, &CancellationToken::default()).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(bgra, [255, 0, 0, 255, 0, 0, 255, 255]);
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(
            thumbnail_pixels(&path, &cancelled)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        std::fs::write(&path, b"not an image").unwrap();
        for _ in 0..3 {
            assert!(
                !thumbnail_pixels(&path, &CancellationToken::default())
                    .unwrap_err()
                    .to_string()
                    .contains("workers are busy")
            );
        }
        std::fs::write(&path, include_bytes!("../tests/fixtures/rotated.tiff")).unwrap();
        assert!(thumbnail_pixels(&path, &CancellationToken::default()).is_ok());
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

    /// Runs `run` with a thread-local `tracing` subscriber that captures emitted
    /// stage events as formatted lines.
    fn capture_stage_events(run: impl FnOnce()) -> Vec<String> {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for Capture {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                CaptureWriter(self.0.clone())
            }
        }
        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(buffer.clone()))
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Asserts a timing event exists for every stage, and that each carries a
    /// `duration_ms`. Extra stages are allowed.
    fn assert_stage_events(events: &[String], pipeline: &str, stages: &[&str]) {
        let label = format!("pipeline=\"{pipeline}\"");
        for stage in stages {
            let stage_label = format!("stage=\"{stage}\"");
            let line = events
                .iter()
                .find(|line| line.contains(&stage_label) && line.contains(&label))
                .unwrap_or_else(|| panic!("no {pipeline} timing for stage {stage}: {events:#?}"));
            assert!(line.contains("duration_ms="), "missing duration: {line}");
        }
    }

    #[test]
    fn raw_pixels_pipeline_times_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, include_bytes!("../tests/fixtures/pixel.png")).unwrap();
        let events = capture_stage_events(|| {
            thumbnail_pixels(&path, &CancellationToken::default()).unwrap();
        });
        assert_stage_events(
            &events,
            "raw_pixels",
            &[
                "decode_slot_wait",
                "source_open",
                "metadata",
                "imageio_decode_resize",
                "bgra_draw",
                "alpha_unpremultiply",
            ],
        );
    }

    #[test]
    fn png_pipeline_times_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, include_bytes!("../tests/fixtures/pixel.png")).unwrap();
        let events = capture_stage_events(|| {
            thumbnail(&path, &CancellationToken::default()).unwrap();
        });
        assert_stage_events(
            &events,
            "png",
            &[
                "decode_slot_wait",
                "source_open",
                "metadata",
                "imageio_decode_resize",
                "png_encode",
                "buffer_copy",
            ],
        );
    }

    #[test]
    fn icon_pipeline_is_labelled_separately_from_preview() {
        let events = capture_stage_events(|| {
            icon_thumbnail(include_bytes!("../tests/fixtures/pixel.png")).unwrap();
        });
        assert_stage_events(
            &events,
            "icon",
            &[
                "source_open",
                "metadata",
                "imageio_decode_resize",
                "png_encode",
                "buffer_copy",
            ],
        );
        assert!(
            events.iter().all(|line| !line.contains("pipeline=\"png\"")),
            "icon timing must not be labelled as a preview: {events:#?}"
        );
    }

    #[test]
    fn failing_stage_records_the_error_on_its_timing_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.png");
        std::fs::write(&path, b"not an image").unwrap();
        let events = capture_stage_events(|| {
            assert!(thumbnail_pixels(&path, &CancellationToken::default()).is_err());
        });
        let line = events
            .iter()
            .find(|line| line.contains("pipeline=\"raw_pixels\"") && line.contains("error="))
            .unwrap_or_else(|| panic!("expected an error-bearing stage event: {events:#?}"));
        assert!(
            line.contains("stage=\"source_open\"") || line.contains("stage=\"metadata\""),
            "{line}"
        );
    }
}
