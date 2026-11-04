//! Owned-data IPC acceptance fixture for Pill. No native clipboard, browser,
//! hotkey, filesystem discovery, persistent history or theme helpers.
use anyhow::{Context, Result, bail};
use launcher_agent::core::{HistoryStore, ProviderConfig};
use launcher_agent::macos::Platform;
use launcher_agent::providers::clipboard::ClipboardProvider;
use launcher_agent::providers::web::WebProvider;
use launcher_agent::service::{ClipboardServiceProvider, Server, Service, WebServiceProvider};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}
struct OwnedDesktop(Mutex<File>);
impl OwnedDesktop {
    fn record(&self, kind: &str, text: &str) -> Result<()> {
        let mut output = self.0.lock().unwrap();
        serde_json::to_writer(&mut *output, &serde_json::json!({"kind":kind,"text":text}))?;
        writeln!(output)?;
        output.flush()?;
        Ok(())
    }
}
impl Platform for OwnedDesktop {
    fn launch_application(&self, _: &Path) -> Result<()> {
        bail!("unexpected application effect")
    }
    fn reveal_in_finder(&self, _: &Path) -> Result<()> {
        bail!("unexpected reveal effect")
    }
    fn open_url(&self, url: &str) -> Result<()> {
        self.record("open", url)
    }
    fn copy_text(&self, text: &str) -> Result<()> {
        self.record("copy", text)
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        bail!("usage: pill_acceptance_fixture SOCKET NEW_OWNED_EFFECTS_FILE");
    }
    let socket = PathBuf::from(&args[0]);
    let effects = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[1])
        .context("creating owned effects log")?;
    let desktop = Arc::new(OwnedDesktop(Mutex::new(effects)));
    let clipboard = Arc::new(ClipboardProvider::new(desktop.clone(), 20));
    clipboard.capture(1, None);
    clipboard.capture(2, Some("Owned first entry\nFirst preview body"));
    clipboard.capture(3, Some("Owned second entry\nSecond preview body"));
    let web = WebProvider::new(desktop, WebProvider::default_engines(), "google".into())?;
    let config = |prefix: &str| ProviderConfig {
        prefix: Some(prefix.into()),
        max_results: 20,
        ..Default::default()
    };
    let service = Service::new(
        vec![
            (Arc::new(WebServiceProvider::new(web)), config("?")),
            (
                Arc::new(ClipboardServiceProvider::new(clipboard)),
                config(":"),
            ),
        ],
        Arc::new(HistoryStore::in_memory()?),
    )?;
    let server = Server::bind(socket, service)?;
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
    server.serve_until(&STOP)
}
