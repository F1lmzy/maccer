//! Resolves the Finder icon for an installed application bundle as PNG bytes.

use std::path::Path;

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
    use std::path::Path;

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
