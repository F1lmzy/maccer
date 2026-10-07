use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::{Command, Stdio},
};

/// OS operations used by providers. Calls may run on provider worker threads.
pub trait Platform: Send + Sync {
    fn launch_application(&self, path: &Path) -> Result<()>;
    fn reveal_in_finder(&self, path: &Path) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
    fn copy_text(&self, text: &str) -> Result<()>;
    fn copy_file(&self, _path: &Path) -> Result<()> {
        bail!("file copying is unavailable on this platform")
    }
    /// Must run on the main thread while processing a mouse drag event.
    fn begin_file_drag(&self, _path: &Path) -> Result<()> {
        bail!("native dragging is unavailable on this platform")
    }
    fn open_path(&self, path: &Path) -> Result<()> {
        self.launch_application(path)
    }
    fn quick_look(&self, _path: &Path) -> Result<()> {
        bail!("Quick Look is unavailable on this platform")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativePlatform;

impl Platform for NativePlatform {
    fn copy_file(&self, path: &Path) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let path = path.to_path_buf();
            run_on_main(move || crate::file_transfer::copy(&path))
        }
        #[cfg(not(target_os = "macos"))]
        crate::file_transfer::copy(path)
    }
    fn begin_file_drag(&self, path: &Path) -> Result<()> {
        crate::file_transfer::drag(path)
    }
    fn quick_look(&self, path: &Path) -> Result<()> {
        if !path.is_absolute() || !path.exists() {
            bail!("Quick Look requires an existing absolute path");
        }
        let mut child = Command::new("/usr/bin/qlmanage")
            .arg("-p")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting Quick Look")?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    fn launch_application(&self, path: &Path) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let path = path.to_path_buf();
            run_on_main(move || {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::{NSString, NSURL};
                let path = path
                    .to_str()
                    .context("application path is not valid UTF-8")?;
                let path = NSString::from_str(path);
                let url = NSURL::fileURLWithPath(&path);
                let opened = NSWorkspace::sharedWorkspace().openURL(&url);
                if !opened {
                    bail!("NSWorkspace could not open application {}", path);
                }
                Ok(())
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            bail!("macOS launcher services require macOS")
        }
    }

    fn reveal_in_finder(&self, path: &Path) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let path = path.to_path_buf();
            run_on_main(move || {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::NSString;
                let path_text = path.to_str().context("file path is not valid UTF-8")?;
                let parent = path.parent().context("file path has no parent directory")?;
                let path_string = NSString::from_str(path_text);
                let parent_text = parent.to_str().context("parent path is not valid UTF-8")?;
                let parent_string = NSString::from_str(parent_text);
                if !NSWorkspace::sharedWorkspace()
                    .selectFile_inFileViewerRootedAtPath(Some(&path_string), &parent_string)
                {
                    bail!("NSWorkspace could not reveal {} in Finder", path_text);
                }
                Ok(())
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            bail!("macOS launcher services require macOS")
        }
    }

    fn open_url(&self, raw: &str) -> Result<()> {
        let url = validate_http_url(raw)?;
        #[cfg(target_os = "macos")]
        {
            run_on_main(move || {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::{NSString, NSURL};
                let value = NSString::from_str(url.as_str());
                let native_url = NSURL::URLWithString(&value).context("invalid NSURL")?;
                if !NSWorkspace::sharedWorkspace().openURL(&native_url) {
                    bail!("NSWorkspace rejected URL {}", url);
                }
                Ok(())
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = url;
            bail!("macOS launcher services require macOS")
        }
    }

    /// Clipboard integration stays on the fixed system utility until NSPasteboard
    /// ownership and main-thread behavior are needed by a feature.
    fn copy_text(&self, text: &str) -> Result<()> {
        let mut child = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .context("starting pbcopy")?;
        use std::io::Write;
        child
            .stdin
            .take()
            .context("opening pbcopy stdin")?
            .write_all(text.as_bytes())
            .context("writing clipboard text")?;
        let status = child.wait().context("waiting for pbcopy")?;
        if !status.success() {
            bail!("pbcopy exited with {status}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn run_on_main<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    use std::sync::mpsc;
    if objc2::MainThreadMarker::new().is_some() {
        return objc2::rc::autoreleasepool(|_| work());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let result = objc2::rc::autoreleasepool(|_| work());
        let _ = tx.send(result);
    });
    rx.recv()
        .context("waiting for macOS main-thread operation")?
}

fn validate_http_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).context("invalid URL")?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        bail!("only absolute HTTP and HTTPS URLs can be opened");
    }
    Ok(url)
}

/// Configure the accessory policy. Call from GPUI's app callback on the main thread.
#[cfg(target_os = "macos")]
pub fn configure_accessory_app() -> Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let mtm =
        MainThreadMarker::new().context("accessory activation must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn configure_accessory_app() -> Result<()> {
    bail!("macOS launcher services require macOS")
}

/// Returns the pointer's display bounds in Cocoa points (bottom-left origin).
#[cfg(target_os = "macos")]
pub fn mouse_display_bounds() -> Option<(f32, f32, f32, f32)> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen};
    let mtm = MainThreadMarker::new()?;
    let point = NSEvent::mouseLocation();
    for screen in NSScreen::screens(mtm).iter() {
        let frame = screen.frame();
        if point.x >= frame.origin.x
            && point.x < frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y < frame.origin.y + frame.size.height
        {
            return Some((
                frame.origin.x as f32,
                frame.origin.y as f32,
                frame.size.width as f32,
                frame.size.height as f32,
            ));
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
pub fn mouse_display_bounds() -> Option<(f32, f32, f32, f32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_absolute_http_urls() {
        assert_eq!(
            validate_http_url("https://example.com/search?q=rust")
                .unwrap()
                .host_str(),
            Some("example.com")
        );
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
        assert!(validate_http_url("https://").is_err());
    }
    #[test]
    fn file_paths_round_trip_spaces_and_unicode() {
        let path = std::path::Path::new("/Applications/My App/例.app");
        let native = url::Url::from_file_path(path).unwrap();
        assert_eq!(native.to_file_path().unwrap(), path);
        assert!(native.as_str().contains("%20"));
    }
}
