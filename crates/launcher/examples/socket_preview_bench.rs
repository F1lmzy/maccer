//! Times wallpaper preview delivery from a live service, the way the frontend
//! asks for it: one `assets.get` per tile, in published order.
//!
//! Point it at a running service (`maccer-service --socket ...`) and keep the
//! picker closed so the measurement starts from whatever the store already
//! holds. See docs/wallpaper-preview-performance.md.
//!
//! ```sh
//! cargo run --release -p launcher --example socket_preview_bench -- \
//!     ~/Library/Caches/maccer-service/rpc
//! ```
use anyhow::{Context, Result, bail};
use launcher_agent::client::Client;
use launcher_agent::protocol::{Icon, Route};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let socket = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("pass the service socket path")?;
    let mut client = Client::connect(socket)?;
    let opened = Instant::now();
    let view = client.open_view(
        Route::Provider {
            provider_id: "wallpapers".into(),
        },
        "",
        20,
    )?;
    let rows = view
        .sections
        .iter()
        .flat_map(|section| &section.items)
        .collect::<Vec<_>>();
    println!(
        "snapshot: {} wallpapers in {:.2} ms",
        rows.len(),
        opened.elapsed().as_secs_f64() * 1000.
    );
    if rows.is_empty() {
        bail!("the picker published no wallpapers");
    }
    let mut first_frame = None;
    for (index, row) in rows.iter().enumerate() {
        let Some(Icon::Asset { asset, .. }) = row.icon.as_ref() else {
            continue;
        };
        let item = Instant::now();
        let data = client.get_asset(asset)?;
        let ready = opened.elapsed();
        first_frame.get_or_insert(ready);
        println!(
            "  {index}: {} {:.2} ms{}",
            row.title,
            item.elapsed().as_secs_f64() * 1000.,
            if data.bytes.is_empty() {
                " (empty)"
            } else {
                ""
            }
        );
    }
    let total = opened.elapsed();
    println!(
        "first frame pixels {:.2} ms after the view opened, whole strip {:.2} ms",
        first_frame.unwrap_or_default().as_secs_f64() * 1000.,
        total.as_secs_f64() * 1000.
    );
    // A second snapshot is the keystroke path: the service answers from its
    // warmed store instead of decoding again, and the frontend still has to
    // re-announce what it already holds.
    let requery = Instant::now();
    let _ = client.query_view(&view.view_id, "")?;
    println!(
        "re-query (keystroke path): {:.2} ms",
        requery.elapsed().as_secs_f64() * 1000.
    );
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}
