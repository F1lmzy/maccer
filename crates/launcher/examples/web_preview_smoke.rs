//! Manual macOS smoke test for the native web preview.
//!
//! Drives the same calls the launcher makes: capture the AppKit host, create
//! the view, warm it while hidden, load a page, hide it without unloading, and
//! release the document. `cargo run -p launcher --example web_preview_smoke`
//! exits nonzero on the first failure and uses a watchdog so a main-thread hang
//! cannot pass silently.

#[cfg(target_os = "macos")]
fn main() {
    use anyhow::Result;
    use gpui::{
        App, Application, AsyncApp, Bounds, Context, FocusHandle, Focusable, Render, Timer, Window,
        WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
        prelude::*, px, size,
    };
    use launcher_agent::macos::{WebHost, WebPreview, WebPreviewEvent};
    use std::time::Duration;

    struct SmokeWindow {
        focus: FocusHandle,
        preview: Option<WebPreview>,
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
                .child("maccer web preview smoke test")
        }
    }

    const URL: &str = "https://example.com/";

    fn step<T>(
        cx: &mut AsyncApp,
        window: WindowHandle<SmokeWindow>,
        run: impl FnOnce(&mut SmokeWindow, &mut Window, &mut Context<SmokeWindow>) -> Result<T>,
    ) -> Result<T> {
        cx.update(|cx| window.update(cx, run))?
            .and_then(|inner| inner)
    }

    fn fail(error: anyhow::Error) -> ! {
        eprintln!("web preview smoke: {error:#}");
        std::process::exit(1);
    }

    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(25));
        eprintln!("web preview smoke: timed out");
        std::process::exit(2);
    });

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(720.), px(520.)), cx);
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
                        preview: None,
                    })
                },
            )
            .expect("opening the smoke window");
        window
            .update(cx, |_, window, cx| {
                cx.activate(true);
                window.activate_window();
            })
            .expect("showing the smoke window");

        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(200)).await;

            // The launcher warms while the view is hidden and the user is still
            // choosing a result.
            let (sender, receiver) = async_channel::unbounded();
            cx.spawn(async move |_| {
                while let Ok(WebPreviewEvent::Hidden) = receiver.recv().await {
                    println!("web preview smoke: hidden by the native key monitor");
                }
            })
            .detach();
            let warmed = step(cx, window, move |node, window, _| {
                let host = WebHost::from_window(window)?;
                let mut preview = WebPreview::new(&host, sender)?;
                preview.warm()?;
                let visible = preview.is_visible();
                node.preview = Some(preview);
                Ok(visible)
            });
            match warmed {
                Ok(false) => println!("web preview smoke: warmed while hidden"),
                Ok(true) => fail(anyhow::anyhow!("warm() made the view visible")),
                Err(error) => fail(error),
            }

            Timer::after(Duration::from_millis(800)).await;
            let shown = step(cx, window, |node, _, _| {
                let preview = node.preview.as_mut().expect("preview");
                preview.show(URL)?;
                Ok(preview.is_visible())
            });
            match shown {
                Ok(true) => println!("web preview smoke: showed {URL}"),
                Ok(false) => fail(anyhow::anyhow!("preview did not report itself visible")),
                Err(error) => fail(error),
            }

            Timer::after(Duration::from_millis(1500)).await;
            let hidden = step(cx, window, |node, _, _| {
                let preview = node.preview.as_mut().expect("preview");
                preview.hide();
                Ok(preview.is_visible())
            });
            match hidden {
                Ok(false) => println!("web preview smoke: hidden without unloading"),
                Ok(true) => fail(anyhow::anyhow!("preview stayed visible after hide()")),
                Err(error) => fail(error),
            }

            // A hidden page keeps its document, so a second hide is a no-op and
            // the idle release is the only thing that unloads it.
            Timer::after(Duration::from_millis(300)).await;
            if let Err(error) = step(cx, window, |node, _, _| {
                let preview = node.preview.as_mut().expect("preview");
                preview.hide();
                preview.release_page();
                preview.update_frame();
                Ok(())
            }) {
                fail(error);
            }
            println!("web preview smoke: ok");

            match cx.update(|cx| cx.quit()) {
                Ok(()) => {}
                Err(error) => fail(error.into()),
            }
        })
        .detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("web preview smoke: macOS only");
    std::process::exit(1);
}
