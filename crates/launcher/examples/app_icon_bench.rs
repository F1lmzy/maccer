//! cargo run -p launcher --example app_icon_bench -- --pixels --parallel
//! Compare with --png, optionally --parallel. Each invocation is a fresh process.
//! Measures six-result native batches, not layout, encoded GPUI decode or GPU upload.
use anyhow::Result;
use launcher_macos::{
    Application, application_icon, application_icon_pixels, discover_applications,
};
use std::time::Instant;

fn load(app: &Application, pixels: bool, first_round: bool) {
    let started = Instant::now();
    let bytes = if pixels {
        let icon = application_icon_pixels(&app.path).expect("native application icon pixels");
        let bytes = icon.bgra.len();
        std::hint::black_box(icon);
        bytes
    } else {
        let png = application_icon(&app.path).expect("native application icon PNG");
        let bytes = png.len();
        // Match the old UI loader, including encoded-image hashing. GPUI's
        // eventual PNG decode/upload is not included in this benchmark.
        std::hint::black_box(gpui::Image::from_bytes(gpui::ImageFormat::Png, png));
        bytes
    };
    if first_round {
        println!(
            "{}: {:.2} ms, {} bytes",
            app.display_name,
            started.elapsed().as_secs_f64() * 1000.,
            bytes
        );
    }
}

fn main() -> Result<()> {
    let pixels = !std::env::args().any(|arg| arg == "--png");
    let parallel = std::env::args().any(|arg| arg == "--parallel");
    let apps: Vec<_> = discover_applications()?.into_iter().take(6).collect();
    println!(
        "bundles: {}",
        apps.iter()
            .map(|app| app.display_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let label = format!(
        "{} {}",
        if pixels { "pixels" } else { "PNG" },
        if parallel { "four workers" } else { "serial" }
    );
    let mut batches = Vec::new();
    for round in 0..11 {
        let batch = Instant::now();
        if parallel {
            std::thread::scope(|scope| {
                for worker in 0..4 {
                    let apps = &apps;
                    scope.spawn(move || {
                        for app in apps.iter().skip(worker).step_by(4) {
                            load(app, pixels, round == 0);
                        }
                    });
                }
            });
        } else {
            for app in &apps {
                load(app, pixels, round == 0);
            }
        }
        batches.push(batch.elapsed());
    }
    println!(
        "{label} first batch: {:.2} ms",
        batches[0].as_secs_f64() * 1000.
    );
    batches.remove(0);
    batches.sort();
    println!(
        "{label} repeated batches: p50 {:.2} ms, max {:.2} ms",
        batches[5].as_secs_f64() * 1000.,
        batches[9].as_secs_f64() * 1000.
    );
    Ok(())
}
