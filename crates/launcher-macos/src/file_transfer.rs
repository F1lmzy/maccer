//! Native file-URL pasteboard and copy-only external drag sessions.
use anyhow::{Context, Result, bail};
use std::path::Path;

pub fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.exists() || path.to_str().is_none() {
        bail!("file transfer requires an existing absolute UTF-8 path");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use objc2::{
        AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
        runtime::ProtocolObject,
    };
    use objc2_app_kit::{
        NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
        NSDraggingSource, NSPasteboard, NSPasteboardWriting, NSWorkspace,
    };
    use objc2_foundation::{
        NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
    };
    use std::cell::OnceCell;

    define_class!(
        // NSObject has no additional subclassing requirements.
        #[unsafe(super = NSObject)]
        #[thread_kind = MainThreadOnly]
        struct FileDragSource;
        unsafe impl NSObjectProtocol for FileDragSource {}
        unsafe impl NSDraggingSource for FileDragSource {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn operations(
                &self,
                _session: &NSDraggingSession,
                _context: NSDraggingContext,
            ) -> NSDragOperation {
                NSDragOperation::Copy
            }
        }
    );
    thread_local! { static DRAG_SOURCE: OnceCell<Retained<FileDragSource>> = const { OnceCell::new() }; }
    pub fn copy(path: &Path) -> Result<()> {
        write_file(&NSPasteboard::generalPasteboard(), path)
    }
    pub(super) fn write_file(pasteboard: &NSPasteboard, path: &Path) -> Result<()> {
        validate_path(path)?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        pasteboard.clearContents();
        if !pasteboard.writeObjects(&NSArray::from_slice(&[writer])) {
            bail!("could not copy file to the macOS pasteboard");
        }
        Ok(())
    }
    pub fn drag(path: &Path) -> Result<()> {
        validate_path(path)?;
        let mtm = MainThreadMarker::new().context("file dragging requires the UI thread")?;
        let app = NSApplication::sharedApplication(mtm);
        let event = app
            .currentEvent()
            .context("file dragging requires a current mouse event")?;
        let window = event.window(mtm).context("drag event has no window")?;
        let view = window
            .contentView()
            .context("drag window has no content view")?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        let icon =
            NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path.to_str().unwrap()));
        let location = view.convertPoint_fromView(event.locationInWindow(), None);
        // Contents is an NSImage, as required by setDraggingFrame:contents:.
        unsafe {
            item.setDraggingFrame_contents(
                NSRect::new(
                    NSPoint::new(location.x - 16., location.y - 16.),
                    NSSize::new(32., 32.),
                ),
                Some(&icon),
            );
        }
        DRAG_SOURCE.with(|source| {
            // Retain the drag source for the full application lifetime. AppKit
            // may call it after beginDraggingSession has returned.
            let source =
                source.get_or_init(|| unsafe { msg_send![FileDragSource::alloc(mtm), init] });
            view.beginDraggingSessionWithItems_event_source(
                &NSArray::from_retained_slice(&[item]),
                &event,
                ProtocolObject::from_ref(&**source),
            );
        });
        Ok(())
    }
}
#[cfg(target_os = "macos")]
pub use native::{copy, drag};
#[cfg(not(target_os = "macos"))]
pub fn copy(_path: &Path) -> Result<()> {
    bail!("native file copying requires macOS")
}
#[cfg(not(target_os = "macos"))]
pub fn drag(_path: &Path) -> Result<()> {
    bail!("native file dragging requires macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn file_urls_round_trip_through_a_private_native_pasteboard() {
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::NSString;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("File 🦀 with spaces.txt");
        std::fs::write(&path, "fixture").unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        for path in [&path, dir.path()] {
            super::native::write_file(&pasteboard, path).unwrap();
            let copied = pasteboard
                .stringForType(&NSString::from_str("public.file-url"))
                .unwrap();
            assert_eq!(
                url::Url::parse(&copied.to_string())
                    .unwrap()
                    .to_file_path()
                    .unwrap(),
                path
            );
        }
    }
    #[test]
    fn native_transfer_rejects_relative_and_missing_paths() {
        assert!(validate_path(Path::new("relative.txt")).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_path(&dir.path().join("missing")).is_err());
        let file = dir.path().join("Unicode 🦀 file.txt");
        std::fs::write(&file, "fixture").unwrap();
        validate_path(&file).unwrap();
        assert_eq!(
            url::Url::from_file_path(&file)
                .unwrap()
                .to_file_path()
                .unwrap(),
            file
        );
    }
}
