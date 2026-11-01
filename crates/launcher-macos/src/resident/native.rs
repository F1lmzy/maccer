use super::{LoginStatus, ResidentEvent};
use anyhow::{Context, Result, bail};
use async_channel::Sender;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueMixed,
    NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSBundle, NSObject, NSObjectProtocol, NSOperatingSystemVersion, NSProcessInfo, NSString,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

fn login_available() -> bool {
    let version = NSOperatingSystemVersion {
        majorVersion: 13,
        minorVersion: 0,
        patchVersion: 0,
    };
    let bundle = NSBundle::mainBundle();
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version)
        && bundle.bundleIdentifier().is_some()
        && bundle.bundlePath().to_string().ends_with(".app")
}

fn login_status() -> LoginStatus {
    if !login_available() {
        return LoginStatus::Unavailable;
    }
    // Availability checked above. Only the main app is registered, never a helper.
    match unsafe { SMAppService::mainAppService().status() } {
        SMAppServiceStatus::NotRegistered => LoginStatus::NotRegistered,
        SMAppServiceStatus::Enabled => LoginStatus::Enabled,
        SMAppServiceStatus::RequiresApproval => LoginStatus::RequiresApproval,
        SMAppServiceStatus::NotFound => LoginStatus::NotFound,
        _ => LoginStatus::Unavailable,
    }
}

struct MenuIvars {
    events: Sender<ResidentEvent>,
    login_item: Retained<NSMenuItem>,
    /// Whether this process owns the login item. A service-mode frontend does
    /// not, so it never reads or mutates ServiceManagement.
    owns_login: bool,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(openLauncher:))]
        fn open_launcher(&self, _: &NSMenuItem) {
            let _ = self.ivars().events.try_send(ResidentEvent::OpenLauncher);
        }
        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _: &NSMenuItem) {
            let _ = self.ivars().events.try_send(ResidentEvent::ToggleLaunchAtLogin);
        }
        #[unsafe(method(quitLauncher:))]
        fn quit_launcher(&self, _: &NSMenuItem) {
            let _ = self.ivars().events.try_send(ResidentEvent::Quit);
        }
    }

    unsafe impl NSMenuDelegate for MenuTarget {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _: &NSMenu) {
            self.refresh();
        }
    }
);

impl MenuTarget {
    fn refresh(&self) {
        let item = &self.ivars().login_item;
        if !self.ivars().owns_login {
            // Login state is owned by the shared background service. A
            // frontend must not query SMAppService or register the main app.
            item.setEnabled(false);
            item.setState(NSControlStateValueOff);
            item.setToolTip(Some(&NSString::from_str(
                "Launch at Login requires separate background service installation.",
            )));
            return;
        }
        let status = login_status();
        item.setEnabled(status != LoginStatus::Unavailable);
        item.setState(match status {
            LoginStatus::Enabled => NSControlStateValueOn,
            LoginStatus::RequiresApproval => NSControlStateValueMixed,
            _ => NSControlStateValueOff,
        });
        let hint = match status {
            LoginStatus::Unavailable => {
                Some("Requires macOS 13 or newer and a signed maccer.app bundle.")
            }
            LoginStatus::RequiresApproval => Some(
                "Awaiting approval in System Settings > General > Login Items. Click to disable.",
            ),
            LoginStatus::NotFound => {
                Some("macOS could not find the login item. Click to try registering it again.")
            }
            _ => None,
        };
        item.setToolTip(hint.map(NSString::from_str).as_deref());
    }
}

/// Main-thread owner of the status item and its weakly referenced action target.
pub struct ResidentMenu {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<MenuTarget>,
}

impl ResidentMenu {
    /// Owns the login item: reads and mutates the system Launch at Login state.
    pub fn new(events: Sender<ResidentEvent>) -> Result<Self> {
        Self::build(events, true)
    }

    /// Frontend-only menu: the same Open and Quit controls, but login state
    /// belongs to the shared background service, so this process never reads
    /// or mutates it. The login item is disabled with a service tooltip and
    /// `toggle_launch_at_login` fails before any register/unregister call.
    pub fn new_frontend(events: Sender<ResidentEvent>) -> Result<Self> {
        Self::build(events, false)
    }

    fn build(events: Sender<ResidentEvent>, owns_login: bool) -> Result<Self> {
        let mtm = MainThreadMarker::new().context("menu bar controls require the main thread")?;
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        // Target methods match their selectors and are retained for the menu's lifetime.
        let open = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Open Launcher"),
                Some(sel!(openLauncher:)),
                &NSString::from_str(""),
            )
        };
        let login_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Launch at Login"),
                Some(sel!(toggleLogin:)),
                &NSString::from_str(""),
            )
        };
        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit"),
                Some(sel!(quitLauncher:)),
                &NSString::from_str("q"),
            )
        };
        let target = MenuTarget::alloc(mtm).set_ivars(MenuIvars {
            events,
            login_item,
            owns_login,
        });
        let target: Retained<MenuTarget> = unsafe { msg_send![super(target), init] };
        for item in [&open, &target.ivars().login_item, &quit] {
            unsafe {
                item.setTarget(Some(&target));
            }
            menu.addItem(item);
        }
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        target.refresh();
        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = status_item
            .button(mtm)
            .context("creating menu bar button")?;
        button.setTitle(&NSString::from_str("maccer"));
        button.setToolTip(Some(&NSString::from_str("maccer launcher")));
        status_item.setMenu(Some(&menu));
        Ok(Self {
            status_item,
            menu,
            target,
        })
    }

    /// Read the OS state on every change. Do not persist a competing local bool.
    /// Source: https://developer.apple.com/documentation/servicemanagement/smappservice
    pub fn toggle_launch_at_login(&self) -> Result<()> {
        if !self.target.ivars().owns_login {
            bail!(
                "This frontend cannot configure background service login; separate installation is required."
            );
        }
        let enable = login_status().toggle_enables()?;
        let result = unsafe {
            let service = SMAppService::mainAppService();
            if enable {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        self.target.refresh(); // Also refresh after failure; the OS may have changed state.
        if login_status() == LoginStatus::RequiresApproval {
            let mtm = self.target.mtm();
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str("Launch at Login needs approval"));
            alert.setInformativeText(&NSString::from_str(
                "Allow maccer in System Settings > General > Login Items. The menu shows a dash until macOS approves it.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Open System Settings"));
            alert.addButtonWithTitle(&NSString::from_str("Not Now"));
            activate_for_alert(mtm);
            if alert.runModal() == NSAlertFirstButtonReturn {
                unsafe {
                    SMAppService::openSystemSettingsLoginItems();
                }
            }
        }
        result.map_err(|error| anyhow::anyhow!("Could not change Launch at Login: {error}"))
    }
}

impl Drop for ResidentMenu {
    fn drop(&mut self) {
        self.menu.setDelegate(None);
        self.status_item.setMenu(None);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
    }
}

fn activate_for_alert(mtm: MainThreadMarker) {
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// Also works before GPUI starts, so configuration/history failures are visible.
/// Must run on the main thread, outside any GPUI app/window borrow.
pub fn show_error_alert(title: &str, detail: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("cannot show error alert off the main thread");
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    activate_for_alert(mtm);
    alert.runModal();
}

/// Main-thread Cocoa integration test. Dispatch actions, but never change login items.
#[cfg(test)]
pub(super) fn menu_smoke() {
    let mtm = MainThreadMarker::new().expect("native menu test runs on main thread");
    let app = NSApplication::sharedApplication(mtm);
    let (tx, rx) = async_channel::unbounded();
    objc2::rc::autoreleasepool(|_| {
        let owner = ResidentMenu::new(tx).unwrap();
        assert_eq!(owner.menu.numberOfItems(), 3);
        for (index, title, expected) in [
            (0, "Open Launcher", ResidentEvent::OpenLauncher),
            (1, "Launch at Login", ResidentEvent::ToggleLaunchAtLogin),
            (2, "Quit", ResidentEvent::Quit),
        ] {
            let item = owner.menu.itemAtIndex(index).unwrap();
            assert_eq!(item.title().to_string(), title);
            // Force native dispatch even for the disabled login item to verify the
            // target/selector bridge. The received action is NOT executed.
            assert!(unsafe {
                app.sendAction_to_from(
                    item.action().unwrap(),
                    item.target().as_deref(),
                    Some(&item),
                )
            });
            assert_eq!(rx.try_recv().unwrap(), expected);
        }
        unsafe {
            let _: () = msg_send![&*owner.target, menuWillOpen: &*owner.menu];
        }
        assert_eq!(login_status(), LoginStatus::Unavailable);
        assert!(!owner.target.ivars().login_item.isEnabled());
        assert!(owner.toggle_launch_at_login().is_err());
        drop(owner);
    });
    assert!(rx.is_closed(), "dropping menu releases action sender");
    println!(
        "native resident menu: labels, Cocoa dispatch, disabled unbundled login, cleanup passed"
    );
}

/// Frontend-only menu smoke: same owned NSMenu and Cocoa dispatch, but login
/// state belongs to the shared service, so this process never reads or mutates
/// ServiceManagement and the login item stays disabled.
#[cfg(test)]
pub(super) fn frontend_menu_smoke() {
    let _mtm = MainThreadMarker::new().expect("frontend menu test runs on main thread");
    let (tx, rx) = async_channel::unbounded();
    objc2::rc::autoreleasepool(|_| {
        let owner = ResidentMenu::new_frontend(tx).unwrap();
        assert_eq!(owner.menu.numberOfItems(), 3);
        let login_item = owner.menu.itemAtIndex(1).unwrap();
        assert_eq!(login_item.title().to_string(), "Launch at Login");
        assert!(!login_item.isEnabled());
        let tooltip = login_item
            .toolTip()
            .map(|value| value.to_string())
            .unwrap_or_default();
        assert!(
            tooltip.contains("background service"),
            "frontend tooltip should point at the service, got: {tooltip}"
        );
        // Must fail before any register/unregister call. In this unbundled
        // process the own-login path would instead report login Unavailable.
        let error = owner.toggle_launch_at_login().unwrap_err();
        assert!(
            error.to_string().contains("background service"),
            "unexpected frontend toggle error: {error}"
        );
        drop(owner);
    });
    assert!(
        rx.is_closed(),
        "dropping frontend menu releases action sender"
    );
    println!(
        "native frontend menu: disabled login, service tooltip, no ServiceManagement mutation passed"
    );
}

/// Prove a GPUI callback can run while an error alert spins Cocoa's modal loop.
#[cfg(test)]
pub(super) fn alert_smoke(cx: &mut gpui::App) {
    use objc2_app_kit::NSModalPanelRunLoopMode;
    use objc2_foundation::{NSRunLoop, NSTimer};
    use std::{
        cell::{Cell, RefCell},
        ptr::NonNull,
        rc::Rc,
        time::Duration,
    };
    let callback_ran = Rc::new(Cell::new(false));
    let callback_check = callback_ran.clone();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        eprintln!("native alert smoke timed out");
        std::process::exit(2);
    });
    let async_cx = RefCell::new(cx.to_async());
    let callback = block2::RcBlock::new(move |_: NonNull<NSTimer>| {
        async_cx
            .borrow_mut()
            .update(|_| callback_ran.set(true))
            .unwrap();
        NSApplication::sharedApplication(MainThreadMarker::new().unwrap()).abortModal();
    });
    // GPUI's dispatch timers do not run in every modal-loop mode. Use an
    // explicit modal timer to exercise the reentrant app update.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.3, false, &callback) };
    unsafe {
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSModalPanelRunLoopMode);
    }
    cx.spawn(async move |cx| {
        gpui::Timer::after(Duration::ZERO).await;
        show_error_alert(
            "maccer alert smoke test",
            "This test alert closes automatically.",
        );
        assert!(
            callback_check.get(),
            "GPUI callback did not run during alert"
        );
        println!("native error alert: GPUI modal reentrancy passed");
        cx.update(|cx| cx.quit()).unwrap();
    })
    .detach();
}
