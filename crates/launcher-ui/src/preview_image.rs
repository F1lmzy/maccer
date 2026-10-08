//! Render-ready image previews. Decode on workers; look up on the UI thread.
use anyhow::{Context, Result, ensure};
use launcher_core::Preview;
use std::{collections::VecDeque, io::Cursor, sync::Arc};

pub(crate) type PreviewKey = (String, String, u64);

pub(crate) struct PreparedPreview {
    pub preview: Preview,
    pub image: Option<Arc<gpui::RenderImage>>,
}
impl PreparedPreview {
    /// Call on a worker for image data. No decoding is left for GPUI's img().
    pub fn new(preview: Preview) -> Result<Self> {
        let (buffer, pipeline) = match preview {
            Preview::Pixels {
                bgra,
                width,
                height,
            } => {
                let started = std::time::Instant::now();
                ensure!(
                    width > 0 && height > 0 && width <= 1024 && height <= 1024,
                    "invalid preview dimensions"
                );
                ensure!(
                    bgra.len() == width as usize * height as usize * 4,
                    "invalid preview pixel buffer"
                );
                // RgbaImage is just a four-channel buffer here. These bytes
                // deliberately stay BGRA, as RenderImage expects.
                let buffer = image::RgbaImage::from_raw(width, height, bgra)
                    .context("invalid preview pixel buffer")?;
                tracing::debug!(
                    pipeline = "raw_pixels",
                    stage = "pixel_buffer_wrap",
                    duration_ms = started.elapsed().as_secs_f64() * 1000.,
                    "preview stage"
                );
                (buffer, "raw_pixels")
            }
            Preview::Image { png } => {
                ensure!(png.len() <= 4 * 1024 * 1024, "preview PNG is too large");
                let mut reader =
                    image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(1024);
                limits.max_image_height = Some(1024);
                limits.max_alloc = Some(8 * 1024 * 1024);
                reader.limits(limits);
                let started = std::time::Instant::now();
                let decoded = reader.decode().map(|image| image.into_rgba8());
                tracing::debug!(
                    pipeline = "png",
                    stage = "png_decode",
                    component = "preview_worker",
                    duration_ms = started.elapsed().as_secs_f64() * 1000.,
                    success = decoded.is_ok(),
                    "preview stage"
                );
                let mut buffer = decoded?;
                let started = std::time::Instant::now();
                for pixel in buffer.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                tracing::debug!(
                    pipeline = "png",
                    stage = "rgba_to_bgra",
                    duration_ms = started.elapsed().as_secs_f64() * 1000.,
                    "preview stage"
                );
                (buffer, "png")
            }
            preview => {
                return Ok(Self {
                    preview,
                    image: None,
                });
            }
        };
        let started = std::time::Instant::now();
        let image = Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buffer)]));
        tracing::debug!(
            pipeline,
            stage = "gpui_render_image_create",
            duration_ms = started.elapsed().as_secs_f64() * 1000.,
            encoded_decode_required = false,
            "preview stage"
        );
        Ok(Self {
            preview: Preview::Image { png: vec![] },
            image: Some(image),
        })
    }
}

pub(crate) struct PreviewImageCache {
    entries: VecDeque<(PreviewKey, Arc<gpui::RenderImage>)>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    evicted: Vec<Arc<gpui::RenderImage>>,
}
impl Default for PreviewImageCache {
    fn default() -> Self {
        Self::new(16, 32 * 1024 * 1024)
    }
}
impl PreviewImageCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            evicted: vec![],
        }
    }
    pub fn get(&mut self, key: &PreviewKey) -> Option<Arc<gpui::RenderImage>> {
        let index = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(index)?;
        let image = entry.1.clone();
        self.entries.push_back(entry);
        Some(image)
    }
    pub fn insert(&mut self, key: PreviewKey, image: Arc<gpui::RenderImage>) {
        let bytes = image.as_bytes(0).map_or(0, |pixels| pixels.len());
        if bytes > self.max_bytes || self.max_entries == 0 {
            return;
        }
        // Replace prior revisions, not just identical keys.
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].0.0 == key.0 && self.entries[index].0.1 == key.1 {
                let (_, old) = self.entries.remove(index).unwrap();
                self.bytes -= old.as_bytes(0).map_or(0, |bytes| bytes.len());
                self.evicted.push(old);
            } else {
                index += 1;
            }
        }
        while self.entries.len() >= self.max_entries || self.bytes + bytes > self.max_bytes {
            let (_, old) = self.entries.pop_front().unwrap();
            self.bytes -= old.as_bytes(0).map_or(0, |bytes| bytes.len());
            self.evicted.push(old);
        }
        self.bytes += bytes;
        self.entries.push_back((key, image));
    }
    /// Drain at render time with the current Window so its GPU atlas is freed too.
    pub fn take_evicted(&mut self) -> Vec<Arc<gpui::RenderImage>> {
        std::mem::take(&mut self.evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> Arc<gpui::RenderImage> {
        PreparedPreview::new(Preview::Pixels {
            bgra: vec![0, 0, 255, 128],
            width: 1,
            height: 1,
        })
        .unwrap()
        .image
        .unwrap()
    }
    #[test]
    fn raw_pixels_are_render_ready_without_changing_color_or_alpha() {
        let image = image();
        assert_eq!(image.as_bytes(0).unwrap(), &[0, 0, 255, 128]);
        assert_eq!(image.frame_count(), 1);
        assert!(
            PreparedPreview::new(Preview::Pixels {
                bgra: vec![0; 4],
                width: 0,
                height: 1
            })
            .is_err()
        );
        assert!(
            PreparedPreview::new(Preview::Pixels {
                bgra: vec![0; 3],
                width: 1,
                height: 1
            })
            .is_err()
        );
        assert!(
            PreparedPreview::new(Preview::Pixels {
                bgra: vec![],
                width: u32::MAX,
                height: u32::MAX
            })
            .is_err()
        );
    }
    #[test]
    fn preparation_logs_distinguish_png_decode_from_raw_handoff() {
        #[derive(Clone)]
        struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let capture = |preview| {
            let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = Writer(bytes.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let span = tracing::debug_span!("preview", request_id = 42);
                span.in_scope(|| PreparedPreview::new(preview).unwrap());
            });
            let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert!(output.contains("request_id=42"));
            assert!(output.contains("duration_ms="));
            assert!(output.contains("gpui_render_image_create"));
            assert!(output.contains("encoded_decode_required=false"));
            output
        };
        let raw = capture(Preview::Pixels {
            bgra: vec![0, 0, 255, 255],
            width: 1,
            height: 1,
        });
        assert!(raw.contains("pixel_buffer_wrap"));
        assert!(!raw.contains("png_decode"));
        let png = capture(Preview::Image {
            png: include_bytes!("../../launcher-macos/tests/fixtures/pixel.png").to_vec(),
        });
        assert!(png.contains("png_decode"));
        assert!(png.contains("rgba_to_bgra"));
        assert!(!png.contains("pixel_buffer_wrap"));
    }

    #[test]
    fn cache_is_bounded_refreshes_recency_and_invalidates_revision() {
        let mut cache = PreviewImageCache::new(2, 8);
        let a = ("files".into(), "a".into(), 0);
        let b = ("files".into(), "b".into(), 0);
        let c = ("files".into(), "c".into(), 0);
        let first = image();
        cache.insert(a.clone(), first.clone());
        cache.insert(b.clone(), image());
        assert!(Arc::ptr_eq(&cache.get(&a).unwrap(), &first));
        cache.insert(c.clone(), image());
        assert!(cache.get(&b).is_none());
        assert_eq!(cache.bytes, 8);
        cache.insert(("files".into(), "a".into(), 1), image());
        assert!(cache.get(&a).is_none());
        assert_eq!(cache.entries.len(), 2);
        assert!(!cache.take_evicted().is_empty());
        let mut tiny = PreviewImageCache::new(2, 3);
        tiny.insert(a, image());
        assert!(tiny.entries.is_empty());
        assert_eq!(tiny.bytes, 0);
    }
}
