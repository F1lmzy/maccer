//! Measures backend preview latency (excludes UI debounce and GPU rendering).
//! cargo run -p launcher-macos --example preview_bench -- /path/to/image.png [--cold]
//! --cold uses temporary copies to bypass the thumbnail cache without editing the source.
use anyhow::{Result, bail};
use launcher_core::{CancellationToken, Preview};
use std::{path::PathBuf, time::Instant};

fn main() -> Result<()> {
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
        let result = launcher_macos::file_preview::preview(&input, &CancellationToken::default())?;
        let Preview::Image { png } = result else {
            bail!("expected image preview");
        };
        println!(
            "attempt {attempt}: {:.2} ms, {} PNG bytes",
            start.elapsed().as_secs_f64() * 1000.,
            png.len()
        );
    }
    Ok(())
}
