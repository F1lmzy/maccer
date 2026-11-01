//! Native GPUI window lifecycle against an already-running service.
//! Enter is sent only on empty home, so it navigates a provider, not a native action.
use anyhow::{Context, Result, bail};
use gpui::{Application, Keystroke, Timer};
use launcher_agent::connection::ServiceConnection;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let socket = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: service_frontend_smoke ABSOLUTE_SOCKET_PATH")?;
    // Preflight is outside the GUI loop. It never starts or replaces a backend.
    let probe = ServiceConnection::connect(&socket)?;
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(event) = probe.try_recv() {
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("ready") => break,
                Some("error") => bail!("service preflight: {}", event["message"]),
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            bail!("service preflight timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(probe);
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("service frontend smoke timed out");
        std::process::exit(2);
    });
    let focus_failed = Arc::new(AtomicBool::new(false));
    let check_focus_failed = focus_failed.clone();
    Application::new().run(move |cx| {
        launcher_macos::configure_accessory_app().unwrap();
        let window = launcher_ui::open_service_launcher(
            &socket,
            launcher_ui::LauncherOptions {
                width: 480.,
                max_results: 50,
            },
            cx,
        )
        .unwrap();
        cx.spawn(async move |cx| {
            for cycle in 1..=3 {
                cx.update(|cx| window.update(cx, |launcher, window, cx| launcher.show(window, cx)))
                    .unwrap()
                    .unwrap();
                Timer::after(Duration::from_millis(500)).await;
                let active = cx
                    .update(|cx| window.update(cx, |_, window, _| window.is_window_active()))
                    .unwrap()
                    .unwrap();
                if !active {
                    check_focus_failed.store(true, Ordering::Release);
                    cx.update(|cx| cx.quit()).unwrap();
                    return;
                }
                // Home -> provider browsing -> Home. Never invoke a scoped item.
                for key in ["cmd-p", "enter", "cmd-p"] {
                    cx.update(|cx| {
                        window.update(cx, |_, window, cx| {
                            window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx);
                        })
                    })
                    .unwrap()
                    .unwrap();
                    Timer::after(Duration::from_millis(300)).await;
                }
                cx.update(|cx| window.update(cx, |launcher, _, cx| launcher.hide(cx)))
                    .unwrap()
                    .unwrap();
                Timer::after(Duration::from_millis(150)).await;
                let active = cx
                    .update(|cx| window.update(cx, |_, window, _| window.is_window_active()))
                    .unwrap()
                    .unwrap();
                assert!(!active, "cycle {cycle}: service frontend did not hide");
                eprintln!("service frontend cycle {cycle}: show, navigation keys, hide passed");
            }
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
    if focus_failed.load(Ordering::Acquire) {
        bail!(
            "native window did not become active; foreground focus needs interactive verification"
        );
    }
    Ok(())
}
