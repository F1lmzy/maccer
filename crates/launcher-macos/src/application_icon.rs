//! Resolves small Finder application icons as render-ready pixels or legacy PNG.

use std::path::Path;

/// A small application icon in top-to-bottom BGRA order with straight alpha.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationIconPixels {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Returns application icon pixels without encoding or decoding an image file.
/// The output is at most 64x64 and can be handed directly to GPUI's RenderImage.
pub fn application_icon_pixels(path: &Path) -> Option<ApplicationIconPixels> {
    #[cfg(target_os = "macos")]
    {
        native::application_icon_pixels(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

/// Returns a PNG icon for the application bundle at `path`, at most 64x64.
///
/// Returns `None` when `path` is not an existing bundle, is not valid UTF-8,
/// or macOS cannot produce an icon.
pub fn application_icon(path: &Path) -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        native::application_icon(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::ApplicationIconPixels;
    use std::{ffi::c_void, path::Path, sync::Mutex};

    // NSWorkspace permits background lookups, but cached NSImages and their
    // representations can be shared. Serialize the AppKit snapshot, then draw
    // the retained, immutable CGImages concurrently outside this lock.
    static IMAGE_SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGImageGetWidth(image: *const c_void) -> usize;
        fn CGImageGetHeight(image: *const c_void) -> usize;
        fn CGColorSpaceCreateDeviceRGB() -> *const c_void;
        fn CGColorSpaceRelease(space: *const c_void);
        fn CGBitmapContextCreate(
            data: *mut u8,
            width: usize,
            height: usize,
            bits: usize,
            row_bytes: usize,
            space: *const c_void,
            info: u32,
        ) -> *const c_void;
        #[allow(clashing_extern_declarations)]
        fn CGContextDrawImage(context: *const c_void, rect: Rect, image: *const c_void);
        fn CGContextRelease(context: *const c_void);
    }

    pub(super) fn application_icon_pixels(path: &Path) -> Option<ApplicationIconPixels> {
        if !path.is_dir()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            return None;
        }
        let path = path.to_str()?;
        objc2::rc::autoreleasepool(|_| {
            objc2::exception::catch(|| {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
                let cg_image = {
                    let _guard = IMAGE_SNAPSHOT_LOCK
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let image =
                        NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path));
                    let mut proposed = NSRect::new(NSPoint::new(0., 0.), NSSize::new(64., 64.));
                    // SAFETY: Proposed rectangle is valid and hints/context are nil.
                    unsafe { image.CGImageForProposedRect_context_hints(&mut proposed, None, None) }
                }?;
                // SAFETY: The returned CGImage is retained for this scope. CoreGraphics
                // draws it into our bounded, exclusively-owned pixel buffer.
                let image_ptr = std::ptr::from_ref(&*cg_image).cast::<c_void>();
                unsafe { draw_icon(image_ptr) }
            })
            .ok()
            .flatten()
        })
    }

    unsafe fn draw_icon(image: *const c_void) -> Option<ApplicationIconPixels> {
        const SIDE: usize = 64;
        let source_width = unsafe { CGImageGetWidth(image) };
        let source_height = unsafe { CGImageGetHeight(image) };
        if source_width == 0 || source_height == 0 {
            return None;
        }
        let scale = (SIDE as f64 / source_width as f64).min(SIDE as f64 / source_height as f64);
        let width = ((source_width as f64 * scale).round() as usize).clamp(1, SIDE);
        let height = ((source_height as f64 * scale).round() as usize).clamp(1, SIDE);
        let mut bgra = vec![0u8; SIDE * SIDE * 4];
        let space = unsafe { CGColorSpaceCreateDeviceRGB() };
        if space.is_null() {
            return None;
        }
        // Little-endian premultiplied-first is BGRA. GPUI wants straight alpha,
        // so convert after drawing. Keep the bitmap fixed at 64x64.
        let context = unsafe {
            CGBitmapContextCreate(bgra.as_mut_ptr(), SIDE, SIDE, 8, SIDE * 4, space, 0x2002)
        };
        unsafe {
            CGColorSpaceRelease(space);
        }
        if context.is_null() {
            return None;
        }
        let rect = Rect {
            origin: Point {
                x: ((SIDE - width) / 2) as f64,
                y: ((SIDE - height) / 2) as f64,
            },
            size: Size {
                width: width as f64,
                height: height as f64,
            },
        };
        unsafe {
            CGContextDrawImage(context, rect, image);
            CGContextRelease(context);
        }
        let pixels = ApplicationIconPixels {
            bgra: normalize_pixels(bgra),
            width: SIDE as u32,
            height: SIDE as u32,
        };
        Some(pixels)
    }

    fn normalize_pixels(mut bgra: Vec<u8>) -> Vec<u8> {
        // Drawing the CGImage preserves its top-to-bottom memory order. Do not
        // flip rows; the asymmetric real-CGImage regression test verifies this.
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
        bgra
    }

    #[cfg(test)]
    #[test]
    fn actual_cgimage_drawing_preserves_source_row_order_and_alpha() {
        use objc2::AllocAnyThread;
        use objc2_app_kit::NSImage;
        use objc2_foundation::{NSData, NSPoint, NSRect, NSSize};
        objc2::rc::autoreleasepool(|_| {
            let data =
                NSData::with_bytes(include_bytes!("../tests/fixtures/orientation-alpha.png"));
            let image = NSImage::initWithData(NSImage::alloc(), &data).unwrap();
            let mut rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(64., 64.));
            // SAFETY: rect lives through the call, with no hints/context.
            let source =
                unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }
                    .unwrap();
            let pixels = unsafe { draw_icon(std::ptr::from_ref(&*source).cast()) }.unwrap();
            let at = |x: usize, y: usize| &pixels.bgra[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
            assert_eq!(at(8, 8), &[0, 0, 255, 255], "top-left red");
            assert_eq!(
                at(55, 8),
                &[0, 255, 0, 128],
                "top-right straight-alpha green"
            );
            assert_eq!(at(8, 55), &[255, 0, 0, 255], "bottom-left blue");
            assert_eq!(at(55, 55), &[0, 0, 0, 0], "bottom-right transparent");
        });
    }

    #[cfg(test)]
    #[test]
    fn normalization_unpremultiplies_without_reordering_pixels() {
        let input = vec![25, 50, 100, 128, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0];
        let output = normalize_pixels(input);
        assert_eq!(&output[0..4], &[50, 100, 199, 128]);
        assert_eq!(&output[4..8], &[0, 255, 0, 255]);
        assert_eq!(&output[8..12], &[0, 0, 255, 255]);
        assert_eq!(&output[12..16], &[0, 0, 0, 0]);
    }

    pub(super) fn application_icon(path: &Path) -> Option<Vec<u8>> {
        // NSWorkspace hands out a generic document icon for paths that do not
        // exist, so reject anything that is not an existing bundle first.
        if !path.is_dir()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            return None;
        }
        let path = path.to_str()?;
        // Keep the NSImage on this worker, with its own autorelease pool.
        // Exporting TIFF uses no main-thread-only view or focus/drawing APIs.
        let tiff = objc2::rc::autoreleasepool(|_| {
            // AppKit can raise Objective-C exceptions for malformed bundle
            // metadata; treat those the same as a missing icon.
            objc2::exception::catch(|| {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::NSString;
                let _guard = IMAGE_SNAPSHOT_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                NSWorkspace::sharedWorkspace()
                    .iconForFile(&NSString::from_str(path))
                    .TIFFRepresentation()
            })
            .ok()
            .flatten()
        })?;
        crate::image_thumbnail::icon_thumbnail(&tiff.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_icon_returns_bounded_straight_alpha_bgra_pixels() {
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        let pixels = application_icon_pixels(finder).expect("Finder icon pixels");
        assert!((1..=64).contains(&pixels.width));
        assert!((1..=64).contains(&pixels.height));
        assert_eq!(
            pixels.bgra.len(),
            pixels.width as usize * pixels.height as usize * 4
        );
        assert!(pixels.bgra.chunks_exact(4).any(|pixel| pixel[3] != 0));
        assert!(
            pixels
                .bgra
                .chunks_exact(4)
                .all(|pixel| pixel[3] != 0 || pixel[..3] == [0, 0, 0])
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pixel_api_rejects_missing_and_invalid_paths() {
        assert!(application_icon_pixels(Path::new("/nonexistent/maccer-missing.app")).is_none());
        assert!(application_icon_pixels(Path::new("")).is_none());
        let dir = tempfile::tempdir().unwrap();
        assert!(application_icon_pixels(dir.path()).is_none());
        let file = dir.path().join("not-an-app.txt");
        std::fs::write(&file, "plain file").unwrap();
        assert!(application_icon_pixels(&file).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pixel_api_supports_concurrent_workers() {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..3 {
                        let pixels = application_icon_pixels(Path::new(
                            "/System/Library/CoreServices/Finder.app",
                        ))
                        .unwrap();
                        assert_eq!(
                            pixels.bgra.len(),
                            pixels.width as usize * pixels.height as usize * 4
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_icon_is_a_small_png() {
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        assert!(finder.is_dir(), "Finder bundle is missing");
        let png = application_icon(finder).expect("Finder icon");
        assert!(png.len() > 24, "icon PNG is too short");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert!((1..=64).contains(&width), "icon width {width} exceeds 64");
        assert!(
            (1..=64).contains(&height),
            "icon height {height} exceeds 64"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_or_invalid_paths_have_no_icon() {
        assert!(application_icon(Path::new("/nonexistent/maccer-missing.app")).is_none());
        assert!(application_icon(Path::new("")).is_none());
        let dir = tempfile::tempdir().unwrap();
        assert!(application_icon(dir.path()).is_none());
        let file = dir.path().join("not-an-app.txt");
        std::fs::write(&file, "plain file").unwrap();
        assert!(application_icon(&file).is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_has_no_icon() {
        assert!(application_icon(Path::new("/Applications/Safari.app")).is_none());
    }
}
