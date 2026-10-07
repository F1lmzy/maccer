//! Spotlight last-opened/use-count metadata, read on provider workers.
use crate::spotlight::FileMetadata;
use std::{path::Path, time::UNIX_EPOCH};

pub fn metadata(path: &Path) -> FileMetadata {
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|t| t.as_secs() as i64);
    let mut metadata = FileMetadata {
        modified,
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    if let Some((last_used, use_count)) = native::usage(path) {
        metadata.last_used = last_used;
        metadata.use_count = use_count;
    }
    metadata
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use core_foundation::{
        base::{CFAllocatorRef, CFType, CFTypeRef, TCFType},
        date::CFDate,
        number::CFNumber,
        string::{CFString, CFStringRef},
    };

    // Signatures/ownership follow the Apple SDK's Metadata.framework/MDItem.h.
    // Create/Copy returns +1 references, including attributes, or NULL.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn MDItemCreate(allocator: CFAllocatorRef, path: CFStringRef) -> CFTypeRef;
        fn MDItemCopyAttribute(item: CFTypeRef, name: CFStringRef) -> CFTypeRef;
    }
    fn attribute(item: &CFType, name: &str) -> Option<CFType> {
        let name = CFString::new(name);
        let raw = unsafe { MDItemCopyAttribute(item.as_CFTypeRef(), name.as_concrete_TypeRef()) };
        (!raw.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(raw) })
    }
    fn unix_date(date: CFDate) -> Option<i64> {
        let seconds = date.abs_time() + 978_307_200.0;
        (seconds.is_finite() && seconds >= 0.0 && seconds < i64::MAX as f64)
            .then_some(seconds as i64)
    }
    pub(super) fn usage(path: &Path) -> Option<(Option<i64>, u64)> {
        let path = CFString::new(path.to_str()?);
        let raw = unsafe { MDItemCreate(std::ptr::null(), path.as_concrete_TypeRef()) };
        if raw.is_null() {
            return None;
        }
        // Keep the metadata item alive until both copied attributes are read.
        let item = unsafe { CFType::wrap_under_create_rule(raw) };
        let last_used = attribute(&item, "kMDItemLastUsedDate")
            .and_then(|v| v.downcast::<CFDate>())
            .and_then(unix_date);
        let use_count = attribute(&item, "kMDItemUseCount")
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|v| v.to_i64())
            .unwrap_or(0)
            .max(0) as u64;
        Some((last_used, use_count))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn core_foundation_dates_convert_to_unix_seconds() {
            assert_eq!(unix_date(CFDate::new(0.0)), Some(978_307_200));
            assert_eq!(unix_date(CFDate::new(f64::NAN)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_metadata_is_harmless() {
        let metadata = metadata(Path::new("/does-not-exist/maccer-nonexistent-file.pdf"));
        assert_eq!(metadata.last_used, None);
        assert_eq!(metadata.modified, None);
        assert_eq!(metadata.use_count, 0);
    }
}
