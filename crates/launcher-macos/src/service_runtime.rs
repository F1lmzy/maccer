//! Main-thread run-loop host for the shared Maccer service.
//!
//! The shared backend runs on a process main thread that has no GPUI or Qt
//! window. AppKit operations that must execute on the main thread (for example
//! `NSWorkspace::openURL` in [`crate::NativePlatform`]) are dispatched through
//! the libdispatch main queue by `run_on_main`. That main queue is only drained
//! while the Cocoa main run loop is running. Carbon hotkeys additionally need
//! application-event dispatch, not just CF sources. The live service must keep
//! this host active whenever it accepts native work or global shortcuts.

use anyhow::Result;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;

/// How long a single run-loop pass waits before the stop flag is checked again.
/// The main dispatch queue wakes the loop immediately, so this only bounds
/// shutdown latency.
#[cfg(target_os = "macos")]
const RUN_LOOP_PASS: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs the shared service main loop on the calling thread until `stop` is set.
///
/// Call this from the process main thread. It configures a non-activating
/// `NSApplication` (`Prohibited` activation policy, so no Dock icon or menu bar)
/// and pumps the Cocoa application event loop. While the loop runs, work submitted to
/// the libdispatch main queue executes on this thread, which is what
/// [`crate::NativePlatform`]'s main-thread operations rely on.
///
/// Shutdown: the loop polls `stop` between bounded run-loop passes. The caller
/// owns the flag and must set it to request shutdown; no signal handling is
/// installed here. Returning does not tear down the `NSApplication`; the process
/// is expected to exit next.
///
/// Threading: this function must not be called from a worker thread. On macOS
/// that returns an error instead of silently running without a main run loop.
/// It performs no hotkey, clipboard, window or application changes itself.
#[cfg(target_os = "macos")]
pub fn run_service_loop(stop: &AtomicBool) -> Result<()> {
    run_service_loop_with(stop, || {})
}

/// Pump the native service loop and invoke `tick` on the process main thread.
/// Keep callbacks short and nonblocking; this is intended for hotkey polling.
#[cfg(target_os = "macos")]
pub fn run_service_loop_with(stop: &AtomicBool, mut tick: impl FnMut()) -> Result<()> {
    use anyhow::Context;
    use objc2::{MainThreadMarker, rc::autoreleasepool};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

    let mtm = MainThreadMarker::new()
        .context("the Maccer service run loop must run on the process main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // Prohibited keeps the service hidden: no Dock icon, menu bar or activation.
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    while !stop.load(Ordering::Acquire) {
        autoreleasepool(|_| {
            // A CF-only loop services dispatch sources but does not dispatch the
            // Carbon application events used by global-hotkey. Dequeuing through
            // AppKit handles those events and also services the main run loop.
            // https://developer.apple.com/documentation/appkit/nsapplication/nextevent(matching:until:inmode:dequeue:)
            let deadline = NSDate::dateWithTimeIntervalSinceNow(RUN_LOOP_PASS.as_secs_f64());
            // SAFETY: NSDefaultRunLoopMode is a valid static Foundation mode.
            if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&deadline),
                unsafe { NSDefaultRunLoopMode },
                true,
            ) {
                app.sendEvent(&event);
            }
            if !stop.load(Ordering::Acquire) {
                tick();
            }
        });
    }
    Ok(())
}

/// Non-macOS stub: the service host is AppKit-specific.
#[cfg(not(target_os = "macos"))]
pub fn run_service_loop(_stop: &AtomicBool) -> Result<()> {
    run_service_loop_with(_stop, || {})
}

#[cfg(not(target_os = "macos"))]
pub fn run_service_loop_with(_stop: &AtomicBool, _tick: impl FnMut()) -> Result<()> {
    anyhow::bail!("the Maccer service run loop requires macOS")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn service_loop_refuses_to_run_off_the_main_thread() {
        // libtest runs each test on a worker thread, so the main-thread guard
        // must reject the call rather than block.
        let stop = AtomicBool::new(false);
        let error = run_service_loop(&stop).expect_err("worker thread must be rejected");
        assert!(
            error.to_string().contains("main thread"),
            "unexpected error: {error}"
        );
    }
}
