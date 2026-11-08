//! A WKWebView drawn over the launcher body.
//!
//! maccer's GPUI frontend can show a real search page without teaching GPUI to
//! render HTML. The web view is a native subview above the GPUI content view,
//! hidden until a web result asks for it. Escape, Cmd-Return and Cmd-[ are
//! intercepted before the page so the launcher keeps its keyboard contract.
//!
//! The view is created once and kept for the life of the launcher: WebKit keeps
//! its helper processes pooled after the view is released, so tearing it down
//! saves no memory and only pays process start-up again. `warm` loads an empty
//! document while the user is still choosing, `hide` keeps the loaded page (a
//! hidden page burns no CPU), and `release_page` drops the document after an
//! idle delay. See `docs/web-preview-performance.md`.

#[cfg(target_os = "macos")]
pub use native::{WebHost, WebPreview, WebPreviewEvent};

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{Context, Result, bail};
    use async_channel::Sender;
    use block2::RcBlock;
    use objc2::{
        MainThreadMarker, msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject, Bool},
    };
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSEvent, NSEventMask, NSEventModifierFlags, NSView, NSWorkspace,
    };
    use objc2_foundation::{NSBundle, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        ffi::{CStr, c_char},
        ptr::NonNull,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    const TOP_INSET: f64 = 52.0;
    const BOTTOM_INSET: f64 = 22.0;
    const SIDE_INSET: f64 = 1.0;
    const CORNER_RADIUS: f64 = 8.0;

    /// Borrowed AppKit host views for the launcher window. Keeping the GPUI
    /// responder lets Escape restore keyboard focus without asking GPUI.
    #[derive(Clone)]
    pub struct WebHost {
        content: Retained<NSView>,
        responder: Retained<NSView>,
    }

    impl WebHost {
        pub fn from_window(window: &gpui::Window) -> Result<Self> {
            MainThreadMarker::new()
                .context("the web preview must be created on the main thread")?;
            let handle = HasWindowHandle::window_handle(window)
                .map_err(|_| anyhow::anyhow!("the GPUI window has no native handle"))?;
            let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                bail!("the GPUI window is not backed by AppKit");
            };
            let responder = unsafe { Retained::retain(appkit.ns_view.as_ptr() as *mut NSView) }
                .context("retaining the GPUI view")?;
            let content = unsafe { responder.superview() }
                .context("the GPUI view has no containing AppKit view")?;
            Ok(Self { content, responder })
        }

        fn frame(&self) -> NSRect {
            frame_for(self.content.bounds().size)
        }
    }

    fn frame_for(size: NSSize) -> NSRect {
        let width = (size.width - SIDE_INSET * 2.0).max(1.0);
        let height = (size.height - TOP_INSET - BOTTOM_INSET).max(1.0);
        NSRect::new(
            NSPoint::new(SIDE_INSET, BOTTOM_INSET),
            NSSize::new(width, height),
        )
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum WebPreviewEvent {
        /// The page was hidden by the native key monitor, so the launcher can
        /// repaint and shrink back to the result list.
        Hidden,
    }

    pub struct WebPreview {
        web: Retained<AnyObject>,
        host: WebHost,
        visible: Arc<AtomicBool>,
        /// A navigation has been started at least once, so the WebContent
        /// process exists and `warm` has nothing left to do.
        warmed: bool,
        monitor: Option<Retained<AnyObject>>,
        handler: Option<RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>>,
    }

    impl WebPreview {
        pub fn new(host: &WebHost, events: Sender<WebPreviewEvent>) -> Result<Self> {
            MainThreadMarker::new()
                .context("the web preview must be created on the main thread")?;
            load_webkit()?;
            let configuration = create_configuration()?;
            let web = create_web_view(&configuration)?;
            let web_view = unsafe { &*(Retained::as_ptr(&web) as *const NSView) };
            web_view.setFrame(host.frame());
            web_view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            web_view.setWantsLayer(true);
            web_view.setHidden(true);
            let layer: *mut AnyObject = unsafe { msg_send![web_view, layer] };
            if !layer.is_null() {
                unsafe {
                    let _: () = msg_send![layer, setCornerRadius: CORNER_RADIUS];
                    let _: () = msg_send![layer, setMasksToBounds: Bool::YES];
                }
            }
            host.content.addSubview(web_view);

            let visible = Arc::new(AtomicBool::new(false));
            let handler = monitor_handler(
                Retained::as_ptr(&web) as *mut AnyObject,
                Retained::as_ptr(&host.content) as *mut NSView,
                Retained::as_ptr(&host.responder) as *mut NSView,
                visible.clone(),
                events.clone(),
            );
            let monitor = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                    NSEventMask::KeyDown,
                    &handler,
                )
            }
            .context("installing the web preview key monitor")?;

            Ok(Self {
                web,
                host: host.clone(),
                visible,
                warmed: false,
                monitor: Some(monitor),
                handler: Some(handler),
            })
        }

        /// Start the WebKit helper processes ahead of the first search. The
        /// WebContent process is spawned by the first navigation, not by the
        /// view, so an empty document here removes that cost from the frame
        /// where the user presses Enter. Repeated calls are no-ops.
        pub fn warm(&mut self) -> Result<()> {
            if self.warmed {
                return Ok(());
            }
            let text = NSString::from_str("about:blank");
            let url = NSURL::URLWithString(&text).context("about:blank is not a valid URL")?;
            let request = NSURLRequest::requestWithURL(&url);
            unsafe {
                let _: *mut AnyObject = msg_send![&*self.web, loadRequest: &*request];
            }
            self.warmed = true;
            Ok(())
        }

        pub fn show(&mut self, url: &str) -> Result<()> {
            let text = NSString::from_str(url);
            let url =
                NSURL::URLWithString(&text).context("the web preview URL is not a valid URL")?;
            let request = NSURLRequest::requestWithURL(&url);
            let web_view = unsafe { &*(Retained::as_ptr(&self.web) as *const NSView) };
            web_view.setFrame(self.host.frame());
            web_view.setHidden(false);
            unsafe {
                let _: *mut AnyObject = msg_send![&*self.web, loadRequest: &*request];
                if let Some(window) = self.host.content.window() {
                    let _: Bool =
                        msg_send![&*window, makeFirstResponder: Retained::as_ptr(&self.web)];
                }
            }
            self.visible.store(true, Ordering::Release);
            self.warmed = true;
            Ok(())
        }

        pub fn hide(&mut self) {
            hide_raw(
                Retained::as_ptr(&self.web) as *mut AnyObject,
                &self.host.content,
                &self.host.responder,
                &self.visible,
            );
        }

        /// Drop the live document without dropping the WebKit processes. The
        /// caller decides when an instant re-open is unlikely enough to pay a
        /// reload later.
        pub fn release_page(&mut self) {
            if self.visible.load(Ordering::Acquire) {
                return;
            }
            release_raw(Retained::as_ptr(&self.web) as *mut AnyObject);
        }

        pub fn is_visible(&self) -> bool {
            self.visible.load(Ordering::Acquire)
        }

        pub fn update_frame(&self) {
            let web_view = unsafe { &*(Retained::as_ptr(&self.web) as *const NSView) };
            web_view.setFrame(self.host.frame());
        }
    }

    impl Drop for WebPreview {
        fn drop(&mut self) {
            if let Some(monitor) = self.monitor.take() {
                unsafe { NSEvent::removeMonitor(&monitor) };
            }
            self.hide();
            let web_view = unsafe { &*(Retained::as_ptr(&self.web) as *const NSView) };
            web_view.removeFromSuperview();
            // The handler must outlive the monitor, so it drops with the struct.
            let _ = &self.handler;
        }
    }

    fn create_configuration() -> Result<Retained<AnyObject>> {
        let class = AnyClass::get(c"WKWebViewConfiguration")
            .context("WebKit did not provide WKWebViewConfiguration")?;
        let configuration: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let configuration: *mut AnyObject = unsafe { msg_send![configuration, init] };
        let configuration = unsafe { Retained::from_raw(configuration) }
            .context("creating a WKWebViewConfiguration")?;
        let user_agent = NSString::from_str(&safari_user_agent());
        unsafe {
            let _: () = msg_send![&*configuration, setApplicationNameForUserAgent: &*user_agent];
            // A panel the user may be ignoring must not start media decoders on
            // its own; require a click, as Safari's default does.
            let _: () = msg_send![
                &*configuration,
                setMediaTypesRequiringUserActionForPlayback: usize::MAX
            ];
        }
        Ok(configuration)
    }

    fn create_web_view(configuration: &AnyObject) -> Result<Retained<AnyObject>> {
        let class = AnyClass::get(c"WKWebView").context("WebKit did not provide WKWebView")?;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        let web: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let web: *mut AnyObject =
            unsafe { msg_send![web, initWithFrame: frame, configuration: configuration] };
        let web = unsafe { Retained::from_raw(web) }.context("creating a WKWebView")?;
        unsafe {
            let _: () = msg_send![
                &*web,
                setAllowsBackForwardNavigationGestures: Bool::YES
            ];
            // The remote inspector opens a mach connection per view and this
            // panel is not a developer surface.
            let responds: Bool = msg_send![&*web, respondsToSelector: objc2::sel!(setInspectable:)];
            if responds.as_bool() {
                let _: () = msg_send![&*web, setInspectable: Bool::NO];
            }
        }
        Ok(web)
    }

    fn monitor_handler(
        web: *mut AnyObject,
        content: *mut NSView,
        responder: *mut NSView,
        visible: Arc<AtomicBool>,
        events: Sender<WebPreviewEvent>,
    ) -> RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent> {
        RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            let event_ptr = event.as_ptr();
            if !visible.load(Ordering::Acquire) {
                return event_ptr;
            }
            let event = unsafe { event.as_ref() };
            let key_code = event.keyCode();
            let command = event
                .modifierFlags()
                .contains(NSEventModifierFlags::Command);
            match (key_code, command) {
                // Escape, Cmd-Return and Cmd-[ beat the page, as in nanolight.
                (53, _) => {
                    let content = unsafe { &*content };
                    let responder = unsafe { &*responder };
                    hide_raw(web, content, responder, &visible);
                    let _ = events.try_send(WebPreviewEvent::Hidden);
                    std::ptr::null_mut()
                }
                (36 | 76, true) => {
                    open_current(web);
                    let content = unsafe { &*content };
                    let responder = unsafe { &*responder };
                    hide_raw(web, content, responder, &visible);
                    let _ = events.try_send(WebPreviewEvent::Hidden);
                    std::ptr::null_mut()
                }
                (33, true) => {
                    unsafe {
                        let _: () = msg_send![web, goBack];
                    }
                    std::ptr::null_mut()
                }
                _ => event_ptr,
            }
        })
    }

    /// Hide the panel and leave the document alone. A hidden page measured
    /// 0.00 CPU-seconds over ten seconds and WebKit returns its memory within
    /// seconds, so unloading here would only cost a reload on the next open.
    fn hide_raw(web: *mut AnyObject, content: &NSView, responder: &NSView, visible: &AtomicBool) {
        let web_view = unsafe { &*(web as *const NSView) };
        web_view.setHidden(true);
        unsafe {
            let _: () = msg_send![web, stopLoading];
        }
        visible.store(false, Ordering::Release);
        if let Some(window) = content.window() {
            unsafe {
                let _: Bool = msg_send![&*window, makeFirstResponder: responder];
            }
        }
    }

    fn release_raw(web: *mut AnyObject) {
        unsafe {
            let _: () = msg_send![web, stopLoading];
            let empty = NSString::from_str("");
            let _: *mut AnyObject = msg_send![
                web,
                loadHTMLString: &*empty,
                baseURL: std::ptr::null_mut::<AnyObject>()
            ];
        }
    }

    fn open_current(web: *mut AnyObject) {
        unsafe {
            let url: *mut NSURL = msg_send![web, URL];
            if url.is_null() {
                return;
            }
            let _ = NSWorkspace::sharedWorkspace().openURL(&*url);
        }
    }

    fn safari_user_agent() -> String {
        let version = safari_version().unwrap_or_else(|| "17.4".into());
        format!("Version/{version} Safari/605.1.15")
    }

    fn safari_version() -> Option<String> {
        for path in [
            "/System/Cryptexes/App/System/Applications/Safari.app",
            "/Applications/Safari.app",
        ] {
            let path = NSString::from_str(path);
            let Some(bundle) = NSBundle::bundleWithPath(&path) else {
                continue;
            };
            let key = NSString::from_str("CFBundleShortVersionString");
            let Some(value) = bundle.objectForInfoDictionaryKey(&key) else {
                continue;
            };
            let Some(value) = value.downcast_ref::<NSString>() else {
                continue;
            };
            let version = value.to_string();
            if !version.is_empty() && version.len() < 16 {
                return Some(version);
            }
        }
        None
    }

    fn load_webkit() -> Result<()> {
        static LOADED: std::sync::OnceLock<std::result::Result<(), String>> =
            std::sync::OnceLock::new();
        LOADED
            .get_or_init(|| {
                let path = c"/System/Library/Frameworks/WebKit.framework/WebKit";
                let handle =
                    unsafe { libc::dlopen(path.as_ptr() as *const c_char, libc::RTLD_LAZY) };
                if handle.is_null() {
                    let error = unsafe { libc::dlerror() };
                    if error.is_null() {
                        return Err("dlopen(WebKit) failed".into());
                    }
                    return Err(unsafe { CStr::from_ptr(error) }
                        .to_string_lossy()
                        .into_owned());
                }
                Ok(())
            })
            .clone()
            .map_err(anyhow::Error::msg)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn frame_keeps_the_input_and_footer_visible() {
            let frame = frame_for(NSSize::new(480.0, 400.0));
            assert_eq!(frame.origin.x, 1.0);
            assert_eq!(frame.origin.y, 22.0);
            assert_eq!(frame.size.width, 478.0);
            assert_eq!(frame.size.height, 400.0 - 52.0 - 22.0);
        }

        #[test]
        fn frame_never_collapses_below_one_point() {
            let frame = frame_for(NSSize::new(1.0, 1.0));
            assert!(frame.size.width >= 1.0);
            assert!(frame.size.height >= 1.0);
        }

        #[test]
        fn safari_user_agent_looks_like_safari() {
            let user_agent = safari_user_agent();
            assert!(user_agent.starts_with("Version/"));
            assert!(user_agent.ends_with("Safari/605.1.15"));
        }

        #[test]
        fn webkit_framework_loads_and_exposes_wkwebview() {
            load_webkit().unwrap();
            assert!(AnyClass::get(c"WKWebView").is_some());
            assert!(AnyClass::get(c"WKWebViewConfiguration").is_some());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{Result, bail};
    use async_channel::Sender;

    #[derive(Clone)]
    pub struct WebHost;
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum WebPreviewEvent {
        Hidden,
    }
    pub struct WebPreview;

    impl WebHost {
        pub fn from_window(_: &gpui::Window) -> Result<Self> {
            bail!("web previews require macOS")
        }
    }
    impl WebPreview {
        pub fn new(_: &WebHost, _: Sender<WebPreviewEvent>) -> Result<Self> {
            bail!("web previews require macOS")
        }
        pub fn warm(&mut self) -> Result<()> {
            bail!("web previews require macOS")
        }
        pub fn show(&mut self, _: &str) -> Result<()> {
            bail!("web previews require macOS")
        }
        pub fn hide(&mut self) {}
        pub fn release_page(&mut self) {}
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn update_frame(&self) {}
    }
}
#[cfg(not(target_os = "macos"))]
pub use stub::{WebHost, WebPreview, WebPreviewEvent};
