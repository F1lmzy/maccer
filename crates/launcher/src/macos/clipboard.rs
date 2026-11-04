//! Clipboard history support.
//!
//! [`snapshot_if_changed`] reads plain text from the general pasteboard for a
//! history provider. It refuses content that password managers and clipboard
//! utilities mark as sensitive, file URLs, non-text payloads, and anything over
//! 64 KiB. Unmarked secrets can still be ordinary text, so this is not a
//! password detector.
//!
//! [`paste_to_application`] writes history text back to the pasteboard and
//! synthesizes Command-V into a chosen application. It requires macOS
//! Accessibility permission, verifies the target is frontmost before sending
//! the keystroke, and never pastes into another application by mistake: if the
//! target cannot be confirmed, it leaves the text on the clipboard and returns
//! an error.
#![cfg_attr(not(target_os = "macos"), allow(unused_imports))]

use anyhow::Result;

/// Largest text accepted into history or written back for pasting.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Current pasteboard change count plus accepted plain text, if any.
///
/// A `None` `text` still reports the change count so a poller can update its
/// tracked count without re-reading hidden content on every tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub change_count: i64,
    pub text: Option<String>,
}

/// Poll the general pasteboard, returning `None` when `previous` already
/// matches its change count. Pass the change count from the last snapshot (or
/// `None` on the first poll) and ignore the returned text if you are only
/// establishing the baseline.
#[cfg(target_os = "macos")]
pub fn snapshot_if_changed(previous: Option<i64>) -> Result<Option<ClipboardSnapshot>> {
    Ok(native::snapshot_from(
        &native::general_pasteboard(),
        previous,
    ))
}
#[cfg(not(target_os = "macos"))]
pub fn snapshot_if_changed(_previous: Option<i64>) -> Result<Option<ClipboardSnapshot>> {
    Ok(None)
}

/// Process identifier of the frontmost application, for saving the target
/// before the launcher takes focus.
#[cfg(target_os = "macos")]
pub fn frontmost_application_pid() -> Option<i32> {
    native::frontmost_pid()
}
#[cfg(not(target_os = "macos"))]
pub fn frontmost_application_pid() -> Option<i32> {
    None
}

/// Write `text` to the pasteboard, activate `pid`, then immediately synthesize
/// Command-V. Activation is asynchronous, so the keystroke often cannot be
/// confirmed in the same call; prefer [`prepare_paste`] plus a delayed
/// [`send_paste`] for interactive pasting. This call never pastes into the
/// wrong application: on any uncertainty it copies the text and returns an
/// error.
#[cfg(target_os = "macos")]
pub fn paste_to_application(text: &str, pid: i32) -> Result<()> {
    native::prepare_paste(text, pid)?;
    native::send_paste(pid)
}
#[cfg(not(target_os = "macos"))]
pub fn paste_to_application(_text: &str, _pid: i32) -> Result<()> {
    anyhow::bail!("pasting into another application requires macOS")
}

/// Validate the target, check Accessibility permission, write `text` to the
/// pasteboard, and ask `pid` to activate. Split from [`send_paste`] so the UI
/// can wait for activation without blocking on a sleep.
#[cfg(target_os = "macos")]
pub fn prepare_paste(text: &str, pid: i32) -> Result<()> {
    native::prepare_paste(text, pid)
}
#[cfg(not(target_os = "macos"))]
pub fn prepare_paste(_text: &str, _pid: i32) -> Result<()> {
    anyhow::bail!("pasting into another application requires macOS")
}

/// Synthesize Command-V after confirming `pid` is running and frontmost. Any
/// mismatch returns an error without sending a keystroke, so text is never
/// pasted into the wrong application.
#[cfg(target_os = "macos")]
pub fn send_paste(pid: i32) -> Result<()> {
    native::send_paste(pid)
}
#[cfg(not(target_os = "macos"))]
pub fn send_paste(_pid: i32) -> Result<()> {
    anyhow::bail!("pasting into another application requires macOS")
}

#[cfg(target_os = "macos")]
mod native {
    use super::{ClipboardSnapshot, MAX_TEXT_BYTES};
    use anyhow::{Context, Result, bail};
    use core::ffi::c_void;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSApplicationActivationOptions, NSPasteboard, NSPasteboardTypeString, NSRunningApplication,
        NSWorkspace,
    };
    use objc2_foundation::NSString;

    /// `public.utf8-plain-text`, the value behind `NSPasteboardTypeString`.
    const STRING_TYPE: &str = "public.utf8-plain-text";
    /// `public.file-url`, the pasteboard type Finder uses for copied files.
    const FILE_URL_TYPE: &str = "public.file-url";
    /// Legacy Finder file list type; still rejected for completeness.
    const LEGACY_FILENAMES_TYPE: &str = "NSFilenamesPboardType";
    /// UTF-16 length guard applied before converting to UTF-8, so a huge
    /// pasteboard string is not fully converted just to be discarded.
    const MAX_TEXT_UTF16_UNITS: usize = MAX_TEXT_BYTES;

    /// Non-standard pasteboard markers that mean "do not record this".
    const SENSITIVE_MARKERS: [&str; 5] = [
        "org.nspasteboard.ConcealedType",
        "org.nspasteboard.TransientType",
        "org.nspasteboard.AutoGeneratedType",
        "de.petermaurer.TransientPasteboardType",
        "com.typeit4me.clipping",
    ];

    // ApplicationServices exposes `AXIsProcessTrusted` for the Accessibility
    // permission check.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }

    // CoreGraphics synthesizes the Command-V keystroke.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(
            source: *mut c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> *mut c_void;
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventPostToPid(pid: i32, event: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// `kCGEventFlagMaskCommand`.
    const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
    /// `kVK_ANSI_V`.
    const K_VK_ANSI_V: u16 = 0x09;

    pub fn general_pasteboard() -> Retained<NSPasteboard> {
        NSPasteboard::generalPasteboard()
    }

    pub fn frontmost_pid() -> Option<i32> {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        Some(app.processIdentifier())
    }

    /// Read the pasteboard when its change count differs from `previous`.
    pub fn snapshot_from(
        pasteboard: &NSPasteboard,
        previous: Option<i64>,
    ) -> Option<ClipboardSnapshot> {
        let change_count = pasteboard.changeCount() as i64;
        if previous == Some(change_count) {
            return None;
        }
        let text = read_plain_text(pasteboard);
        let final_count = pasteboard.changeCount() as i64;
        // Another process may replace the pasteboard between checking markers
        // and reading its string. Discard that racing snapshot, fail closed.
        Some(ClipboardSnapshot {
            change_count: final_count,
            text: if final_count == change_count {
                text
            } else {
                None
            },
        })
    }

    /// Plain UTF-8 text, or `None` for anything sensitive, non-text, or oversized.
    fn read_plain_text(pasteboard: &NSPasteboard) -> Option<String> {
        let types = pasteboard.types()?;
        let mut has_plain_text = false;
        for declared in types.iter() {
            let name = declared.to_string();
            if SENSITIVE_MARKERS.contains(&name.as_str())
                || name == FILE_URL_TYPE
                || name == LEGACY_FILENAMES_TYPE
            {
                return None;
            }
            if name == STRING_TYPE {
                has_plain_text = true;
            }
        }
        if !has_plain_text {
            return None;
        }
        let text = pasteboard.stringForType(unsafe { NSPasteboardTypeString })?;
        if text.len() > MAX_TEXT_UTF16_UNITS {
            return None;
        }
        let text = text.to_string();
        if text.len() > MAX_TEXT_BYTES {
            return None;
        }
        Some(text)
    }

    pub fn prepare_paste(text: &str, pid: i32) -> Result<()> {
        if text.is_empty() {
            bail!("cannot paste empty clipboard text");
        }
        if text.len() > MAX_TEXT_BYTES {
            bail!("clipboard text exceeds the {MAX_TEXT_BYTES} byte limit");
        }
        // Validate the target before touching the clipboard or prompting for
        // permission, so a bad target can never copy over the user's clipboard.
        let target = target_application(pid)?;
        if !is_process_trusted() {
            bail!(
                "macOS Accessibility permission is required to paste into other applications; \
                 enable maccer in System Settings > Privacy & Security > Accessibility"
            );
        }
        write_text(text)?;
        if !target.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows) {
            bail!("macOS could not bring the target application to the front");
        }
        Ok(())
    }

    pub fn send_paste(pid: i32) -> Result<()> {
        let _ = target_application(pid)?;
        let frontmost = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        if frontmost != Some(pid) {
            bail!("target application is not frontmost; the paste keystroke was not sent");
        }
        if !is_process_trusted() {
            bail!("macOS Accessibility permission is required to send the paste keystroke");
        }
        post_command_v(pid)
    }

    fn target_application(pid: i32) -> Result<Retained<NSRunningApplication>> {
        let own_pid = std::process::id() as i32;
        if pid <= 0 || pid == own_pid {
            bail!("refusing to paste into the launcher's own process or an invalid pid");
        }
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .context("target application is no longer running")?;
        if app.isTerminated() {
            bail!("target application has already terminated");
        }
        Ok(app)
    }

    fn write_text(text: &str) -> Result<()> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if !pasteboard
            .setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
        {
            bail!("could not write text to the macOS pasteboard");
        }
        Ok(())
    }

    fn is_process_trusted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    fn post_command_v(pid: i32) -> Result<()> {
        unsafe {
            let down = CGEventCreateKeyboardEvent(std::ptr::null_mut(), K_VK_ANSI_V, true);
            let up = CGEventCreateKeyboardEvent(std::ptr::null_mut(), K_VK_ANSI_V, false);
            if down.is_null() || up.is_null() {
                if !down.is_null() {
                    CFRelease(down);
                }
                if !up.is_null() {
                    CFRelease(up);
                }
                bail!("could not create the paste keystroke");
            }
            for event in [down, up] {
                CGEventSetFlags(event, K_CG_EVENT_FLAG_MASK_COMMAND);
                // Deliver to the validated process, not whichever app happens
                // to gain focus between the frontmost check and event posting.
                CGEventPostToPid(pid, event);
                CFRelease(event);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::native;
        use super::super::{MAX_TEXT_BYTES, prepare_paste};
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
        use objc2_foundation::NSString;

        const MARKERS: [&str; 5] = [
            "org.nspasteboard.ConcealedType",
            "org.nspasteboard.TransientType",
            "org.nspasteboard.AutoGeneratedType",
            "de.petermaurer.TransientPasteboardType",
            "com.typeit4me.clipping",
        ];

        fn write_string(pb: &NSPasteboard, text: &str) {
            pb.clearContents();
            assert!(
                pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
            );
        }

        fn add_type(pb: &NSPasteboard, ty: &str) {
            assert!(pb.setString_forType(&NSString::from_str("sentinel"), &NSString::from_str(ty)));
        }

        #[test]
        fn private_pasteboard_accepts_plain_text_and_tracks_change_count() {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            write_string(&pb, "hello 🦀 world");
            let first = native::snapshot_from(&pb, None).expect("first poll is always a change");
            assert_eq!(first.text.as_deref(), Some("hello 🦀 world"));
            assert!(native::snapshot_from(&pb, Some(first.change_count)).is_none());
            write_string(&pb, "second");
            let second =
                native::snapshot_from(&pb, Some(first.change_count)).expect("content changed");
            assert_eq!(second.text.as_deref(), Some("second"));
            assert_ne!(first.change_count, second.change_count);
        }

        #[test]
        fn concealed_markers_are_rejected() {
            for marker in MARKERS {
                let pb = NSPasteboard::pasteboardWithUniqueName();
                write_string(&pb, "hunter2");
                add_type(&pb, marker);
                let snapshot = native::snapshot_from(&pb, None).expect("changed");
                assert_eq!(snapshot.text, None, "marker {marker} must hide text");
            }
        }

        #[test]
        fn non_text_content_is_rejected() {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            pb.clearContents();
            add_type(&pb, "public.png");
            let snapshot = native::snapshot_from(&pb, None).expect("changed");
            assert_eq!(snapshot.text, None);
        }

        #[test]
        fn file_urls_are_rejected_even_with_a_string_representation() {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            write_string(&pb, "/Users/someone/secret.txt");
            add_type(&pb, "public.file-url");
            let snapshot = native::snapshot_from(&pb, None).expect("changed");
            assert_eq!(snapshot.text, None);
        }

        #[test]
        fn oversized_text_is_rejected() {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            write_string(&pb, &"a".repeat(MAX_TEXT_BYTES + 1));
            let snapshot = native::snapshot_from(&pb, None).expect("changed");
            assert_eq!(snapshot.text, None);
        }

        #[test]
        fn exactly_64_kib_of_text_is_accepted() {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            let text = "a".repeat(MAX_TEXT_BYTES);
            write_string(&pb, &text);
            let snapshot = native::snapshot_from(&pb, None).expect("changed");
            assert_eq!(snapshot.text.as_deref(), Some(text.as_str()));
        }

        #[test]
        fn prepare_paste_rejects_own_process_before_touching_the_clipboard() {
            let error = prepare_paste("secret", std::process::id() as i32).unwrap_err();
            assert!(error.to_string().contains("own process"), "{error}");
            let error = prepare_paste("secret", -1).unwrap_err();
            assert!(error.to_string().contains("own process"), "{error}");
        }

        #[test]
        fn prepare_paste_rejects_oversized_and_empty_text() {
            let error = prepare_paste(&"a".repeat(MAX_TEXT_BYTES + 1), 999_999).unwrap_err();
            assert!(error.to_string().contains("limit"), "{error}");
            let error = prepare_paste("", 999_999).unwrap_err();
            assert!(error.to_string().contains("empty"), "{error}");
        }

        /// Reads the real general pasteboard (safe, no mutation) and calls the
        /// paste entry points with targets that must fail before any write or
        /// keystroke. This also forces the linker to resolve the ApplicationServices,
        /// CoreGraphics, and CoreFoundation symbols.
        #[test]
        fn public_api_reads_and_rejects_bad_targets_without_mutating_the_clipboard() {
            let snapshot = super::super::snapshot_if_changed(Some(-1)).unwrap();
            assert!(snapshot.is_some());
            let _ = super::super::frontmost_application_pid();
            assert!(super::super::prepare_paste("secret", i32::MAX).is_err());
            assert!(super::super::send_paste(i32::MAX).is_err());
            assert!(super::super::prepare_paste("secret", std::process::id() as i32).is_err());
        }

        #[test]
        fn prepare_paste_rejects_a_missing_target_without_writing_the_clipboard() {
            let error = prepare_paste("secret", i32::MAX).unwrap_err();
            assert!(error.to_string().contains("no longer running"), "{error}");
        }
    }
}
