//! Measures backend preview latency (excludes UI scheduling and GPU rendering).
//! cargo run -p launcher-macos --example preview_bench -- /path/to/image.png [--cold]
//! --cold uses temporary copies to bypass the PDF PNG cache without editing the source.
//! Native images always decode here; their decoded cache lives in the UI.
use anyhow::{Result, bail};
use launcher_core::{CancellationToken, Preview};
use std::{path::PathBuf, time::Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        bail!("provide an image path");
    };
    let copies = if std::env::args_os().nth(2).as_deref() == Some(std::ffi::OsStr::new("--cold")) {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    for attempt in 1..=6 {
        let input = if let Some(dir) = &copies {
            let copy = dir.path().join(format!(
                "attempt-{attempt}.{}",
                path.extension().unwrap_or_default().to_string_lossy()
            ));
            std::fs::copy(&path, &copy)?;
            copy
        } else {
            path.clone()
        };
        let start = Instant::now();
        let span = tracing::debug_span!("preview", request_id = attempt);
        let result = span.in_scope(|| {
            launcher_macos::file_preview::preview(&input, &CancellationToken::default())
        })?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
        match result {
            Preview::Pixels {
                bgra,
                width,
                height,
            } => println!(
                "attempt {attempt}: {elapsed_ms:.2} ms, raw BGRA {width}x{height} ({} bytes)",
                bgra.len()
            ),
            Preview::Image { png } => println!(
                "attempt {attempt}: {elapsed_ms:.2} ms, PNG backend ({} bytes)",
                png.len()
            ),
            _ => bail!("expected image preview"),
        }
    }
    Ok(())
}
