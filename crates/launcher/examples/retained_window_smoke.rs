//! Native regression check for the retained pop-up window lifecycle.
//!
//! GPUI 0.2.2 deadlocked on the second `show` after a `hide` (a Cocoa
//! `windowDidBecomeKey` / `windowDidResignKey` re-entrancy bug patched in
//! `vendor/gpui`). This example drives repeated activation/hide cycles through
//! the same GPUI window APIs the launcher uses, asserts the window reaches the
//! expected active state, and uses a separate-thread watchdog so a main-thread
//! deadlock still produces a nonzero exit instead of hanging forever.

use gpui::{
    App, Application, AsyncApp, Bounds, Context, FocusHandle, Focusable, Render, Timer, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
    prelude::*, px, size,
};
use std::time::Duration;

const CYCLES: usize = 20;
const WATCHDOG: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POLL_ATTEMPTS: usize = 50;

struct SmokeWindow {
    focus: FocusHandle,
}

impl Focusable for SmokeWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SmokeWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .child("GPUI retained-window lifecycle smoke test")
    }
}

fn main() {
    Application::new().run(|cx| {
        let bounds = Bounds::centered(None, size(px(520.), px(120.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    window_background: WindowBackgroundAppearance::Transparent,
                    focus: false,
                    show: false,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| SmokeWindow {
                        focus: cx.focus_handle(),
                    })
                },
            )
            .expect("open smoke window");

        // The watchdog runs on a separate OS thread, so it still fires if the
        // main thread deadlocks inside a Cocoa notification (the exact failure
        // this example guards against). A normal `cx.quit()` terminates the
        // process before this timer elapses.
        std::thread::spawn(|| {
            std::thread::sleep(WATCHDOG);
            eprintln!(
                "retained-window lifecycle timed out after {}s; possible main-thread deadlock",
                WATCHDOG.as_secs()
            );
            std::process::exit(2);
        });

        cx.spawn(async move |cx| {
            for cycle in 1..=CYCLES {
                show(cx, window, cycle).await;
                if !wait_for_active(cx, window, true, cycle, "after show").await {
                    eprintln!("cycle {cycle}: window was not active after show");
                    std::process::exit(1);
                }
                eprintln!("retained-window cycle {cycle}/{CYCLES}: shown (active=true)");

                hide(cx, window, cycle).await;
                if !wait_for_active(cx, window, false, cycle, "after hide").await {
                    eprintln!("cycle {cycle}: window was still active after hide");
                    std::process::exit(1);
                }
                eprintln!("retained-window cycle {cycle}/{CYCLES}: hidden (active=false)");
            }

            match cx.update(|cx| cx.quit()) {
                Ok(()) => eprintln!(
                    "retained-window lifecycle completed {CYCLES}/{CYCLES} cycles; requesting quit"
                ),
                Err(error) => {
                    eprintln!("quit failed: {error}");
                    std::process::exit(1);
                }
            }
        })
        .detach();
    });
}

async fn show(cx: &mut AsyncApp, window: WindowHandle<SmokeWindow>, cycle: usize) {
    let result = cx
        .update(|cx| {
            window.update(cx, |view, window, cx| {
                cx.activate(true);
                window.activate_window();
                window.focus(&view.focus);
            })
        })
        .and_then(|inner| inner);
    if let Err(error) = result {
        eprintln!("cycle {cycle}: show failed: {error}");
        std::process::exit(1);
    }
}

async fn hide(cx: &mut AsyncApp, window: WindowHandle<SmokeWindow>, cycle: usize) {
    let result = cx
        .update(|cx| window.update(cx, |_, _, cx| cx.hide()))
        .and_then(|inner| inner);
    if let Err(error) = result {
        eprintln!("cycle {cycle}: hide failed: {error}");
        std::process::exit(1);
    }
}

async fn wait_for_active(
    cx: &mut AsyncApp,
    window: WindowHandle<SmokeWindow>,
    expected: bool,
    cycle: usize,
    phase: &str,
) -> bool {
    for _ in 0..POLL_ATTEMPTS {
        let state = cx
            .update(|cx| window.update(cx, |_, window, _| window.is_window_active()))
            .and_then(|inner| inner);
        match state {
            Ok(active) if active == expected => return true,
            Ok(_) => {}
            Err(error) => {
                eprintln!("cycle {cycle} {phase}: window state unavailable: {error}");
                return false;
            }
        }
        Timer::after(POLL_INTERVAL).await;
    }
    false
}
