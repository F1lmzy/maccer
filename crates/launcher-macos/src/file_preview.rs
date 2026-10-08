//! Bounded lazy file previews. File content is data, never executable markup.
use anyhow::Result;
use launcher_core::{CancellationToken, Preview};
use std::path::Path;

pub fn preview(path: &Path, token: &CancellationToken) -> Result<Preview> {
    use anyhow::{Context, bail};
    use std::{fs::File, io::Read, process::Command, time::Duration};
    if token.is_cancelled() {
        bail!("preview cancelled");
    }
    let metadata = std::fs::metadata(path).context("reading preview metadata")?;
    if metadata.is_dir() {
        return Ok(Preview::Info("Folder. Enter to open in Finder.".into()));
    }
    if !metadata.is_file() {
        return Ok(Preview::Info(
            "Preview unavailable for this file type".into(),
        ));
    }
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let image = matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "tiff" | "tif" | "heic" | "bmp" | "avif"
    );
    if image || extension == "pdf" {
        if metadata.len() > 64 * 1024 * 1024 {
            return Ok(Preview::Info(
                "File is too large for an inline preview. Use Quick Look.".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        if image {
            let (bgra, width, height) = crate::image_thumbnail::thumbnail_pixels(path, token)?;
            return Ok(Preview::Pixels {
                bgra,
                width,
                height,
            });
        }
        let png = crate::thumbnail_cache::get_or_generate(path, &metadata, token, || {
            let dir = tempfile::tempdir()?;
            let output_path;
            let mut command;
            if image {
                output_path = dir.path().join("preview.png");
                command = Command::new("/usr/bin/sips");
                command
                    .args(["-s", "format", "png", "--resampleHeightWidthMax", "1024"])
                    .arg(path)
                    .arg("--out")
                    .arg(&output_path);
            } else {
                output_path = dir.path().join(format!(
                    "{}.png",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                command = Command::new("/usr/bin/qlmanage");
                command
                    .args(["-t", "-s", "1024", "-o"])
                    .arg(dir.path())
                    .arg(path);
            }
            let out = crate::process::run_bounded(
                &mut command,
                token,
                Duration::from_secs(5),
                64 * 1024,
            )?;
            if !out.status.success() {
                bail!(
                    "thumbnail generation failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let mut png = Vec::new();
            File::open(&output_path)
                .context("reading generated thumbnail")?
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut png)?;
            if png.len() > 4 * 1024 * 1024 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                bail!("invalid or oversized preview thumbnail");
            }
            if token.is_cancelled() {
                bail!("preview cancelled");
            }
            Ok(png)
        })?;
        return Ok(Preview::Image { png });
    }
    const MAX_BYTES: usize = 65536;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if token.is_cancelled() {
        bail!("preview cancelled");
    }
    let truncated = bytes.len() > MAX_BYTES;
    bytes.truncate(MAX_BYTES);
    if bytes.contains(&0) {
        return Ok(Preview::Info(
            "Binary file. Use Quick Look to preview.".into(),
        ));
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(_) => {
            return Ok(Preview::Info(
                "Non-UTF-8 file. Use Quick Look to preview.".into(),
            ));
        }
    };
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Ok(Preview::Info(
            "Binary file. Use Quick Look to preview.".into(),
        ));
    }
    Ok(Preview::Text {
        text: text.into(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn native_image_previews_return_raw_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.png");
        std::fs::write(&image, include_bytes!("../tests/fixtures/pixel.png")).unwrap();
        assert!(matches!(
            preview(&image, &CancellationToken::default()).unwrap(),
            Preview::Pixels { width: 1, height: 1, bgra } if bgra.len() == 4
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_image_pixels_preserve_row_order_and_alpha_and_pdf_stays_png() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image with spaces.png");
        std::fs::write(
            &image,
            include_bytes!("../tests/fixtures/orientation-alpha.png"),
        )
        .unwrap();
        let Preview::Pixels {
            bgra,
            width,
            height,
        } = preview(&image, &CancellationToken::default()).unwrap()
        else {
            panic!("expected raw pixel image preview");
        };
        assert_eq!((width, height), (2, 2));
        assert_eq!(
            bgra,
            [
                0, 0, 255, 255, // top-left red
                0, 255, 0, 128, // top-right half-alpha green, unpremultiplied
                255, 0, 0, 255, // bottom-left blue
                0, 0, 0, 0, // transparent white has canonical zero color
            ]
        );
        let pdf = dir.path().join("document with spaces.pdf");
        let stream = "BT /F1 18 Tf 20 70 Td (maccer preview fixture) Tj ET\n";
        let objects = ["<< /Type /Catalog /Pages 2 0 R >>".into(), "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 120] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(), format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len())];
        let mut content = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(content.len());
            content.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }
        let xref = content.len();
        content.push_str("xref\n0 6\n0000000000 65535 f \n");
        for offset in offsets {
            content.push_str(&format!("{offset:010} 00000 n \n"));
        }
        content.push_str(&format!(
            "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        ));
        std::fs::write(&pdf, content).unwrap();
        let Preview::Image { png } = preview(&pdf, &CancellationToken::default()).unwrap() else {
            panic!("expected PNG-backed PDF thumbnail");
        };
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
    #[test]
    fn previews_text_with_unicode_and_truncates_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello 🦀\n".repeat(20_000)).unwrap();
        let Preview::Text { text, truncated } =
            preview(&path, &CancellationToken::default()).unwrap()
        else {
            panic!("expected text");
        };
        assert!(text.starts_with("hello 🦀"));
        assert!(truncated && text.len() <= 65536);
    }
    #[test]
    fn binary_files_directories_and_cancellation_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.dat");
        std::fs::write(&path, [0, 255, 12, 34]).unwrap();
        assert!(matches!(
            preview(&path, &CancellationToken::default()).unwrap(),
            Preview::Info(_)
        ));
        assert!(matches!(
            preview(dir.path(), &CancellationToken::default()).unwrap(),
            Preview::Info(_)
        ));
        let token = CancellationToken::default();
        token.cancel();
        assert!(preview(&path, &token).is_err());
    }
}
