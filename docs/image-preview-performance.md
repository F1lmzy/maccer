# Image preview performance

Image previews use a synchronous decoded-image cache, immediate background decoding, and render-ready pixels. File discovery, search scope, and result ranking are unchanged.

## What changed

- Removed the 40 ms preview debounce. Cold previews start background work immediately.
- macOS ImageIO downsizes images to at most 800 pixels on the longest side and applies orientation metadata. CoreGraphics produces straight-alpha BGRA pixels for GPUI, avoiding PNG compression followed by another PNG decode.
- The UI keeps decoded `Arc<RenderImage>` values in a 16-entry, 32 MiB LRU. Hits reuse the same image synchronously in the selection frame. Selection changes and hiding the launcher retain this cache; restarting clears it.
- Keys include provider, item identity, and `preview_revision`. The file provider publishes file-change stamps through its background index, without stat calls on the UI thread. A changed revision misses the cache. Changes may remain visible until the existing filesystem watcher publishes its next index snapshot.
- Evicted images release their GPU textures, including the currently rendered window's atlas. The memory budget counts decoded pixels, not compressed PNG size. GPU allocation overhead is additional.
- The pane stays blank for short cold loads rather than flashing "Loading preview". Loading text appears after 150 ms if work is still pending; the existing two-second slow-preview notice remains. No previously selected image is shown under the new selection.

Native decoding still uses a shared two-worker limit, cancellation checks between stages, a 100-megapixel dimension guard, and a 64 MiB source-file limit. ImageIO cannot interrupt a decode already in progress. PDFs retain their cancellable Quick Look subprocess, PNG thumbnail cache, and five-second deadline. The UI decodes their PNG on a worker and caches the render-ready image too. Text and clipboard previews are not retained in the decoded-image cache.

## Per-stage debug timings

Run with `RUST_LOG=debug`. Preview work is correlated by a `preview{request_id=..., provider=..., revision=...}` span, propagated into the background worker and UI completion. `duration_ms` is the duration of that stage (floating-point milliseconds); `elapsed_ms` is cumulative time from the preview request. Nested timings overlap: `provider_preview` includes the native image stages and must not be added to them.

Production raw-pixel previews emit:

1. `decoded_cache_lookup`: decoded UI cache hit/miss. A hit skips the worker.
2. `worker_queue_wait`: time before the background job starts.
3. `decode_slot_wait`: waiting for a native decode permit.
4. `source_open`: creating the file URL and ImageIO source.
5. `metadata`: source dimensions and safety checks.
6. `imageio_decode_resize`: ImageIO thumbnail creation and orientation transform.
7. `bgra_draw`: buffer allocation, CoreGraphics context setup and drawing.
8. `alpha_unpremultiply`: converting premultiplied BGRA to straight alpha.
9. `pixel_buffer_wrap` and `gpui_render_image_create`: wrapping the pixels as render-ready GPUI data.
10. `ui_dispatch_wait`, `ui_apply` and `ui_render_wait`: worker-to-UI scheduling, applying the result, and waiting for the render callback.

ImageIO performs lazy reads. `source_open` is **not** a measurement of reading the entire original image; disk I/O and some decoding can occur during thumbnail creation or drawing. Separate disk-read-only time cannot be inferred from these native calls without changing the pipeline or using an I/O profiler.

The legacy `thumbnail()` PNG path is test-only now. It emits `pipeline="png"` stages for source open, metadata, decode/resize, `png_encode` and `buffer_copy`. PNG previews from other backends (such as PDFs) emit `png_decode` on the preview worker and `rgba_to_bgra` before `gpui_render_image_create`. GPUI receives an `Arc<RenderImage>`, so it does **not** decode that PNG again. Icon encoding is labelled `pipeline="icon"`, not as preview work.

`preview render callback reached` measures GPUI beginning to build the ready element tree, not GPU upload completion or screen presentation. To identify a bottleneck, compare per-stage timings across several cold and warm requests; one first-call sample can include ImageIO framework initialization.

For native-only stage logs:

```sh
RUST_LOG=debug cargo run -p launcher-macos --example preview_bench -- /path/to/image.png
```

Tests capture real debug events for both native pipelines, icon labelling, a failing native stage, and PNG versus raw-pixel GPUI preparation, including request-span correlation.

## Vicinae source comparison

Inspected [Vicinae at b66cc724](https://github.com/vicinaehq/vicinae/tree/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb), without running or benchmarking it:

- [`view-utils.cpp`](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/views/view-utils.cpp) selects a local image source for image files. File-search images do not use Quick Look thumbnails.
- [`image-stream.cpp`](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/image/image-stream.cpp) checks a size-keyed decoded QImage cache synchronously in `start()`. A miss dispatches immediately, without a debounce. Its decoded cache budget is 64 MiB.
- [`image-renderer.cpp`](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/image/image-renderer.cpp) uses scaled QImageReader decoding on a four-thread pool.
- [`ImagePreview.qml`](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/qml/detail/ImagePreview.qml) has no inline spinner. A separate full-screen viewer does show a loading indicator.

The transferable mechanism is synchronous decoded-cache reuse, not a promise of instant cold decoding. Maccer retains its existing lower concurrency bound and adds a delayed loading explanation for slow files.

## Measurements

Debug builds on generated 3840x2160 PNG/JPEG fixtures, on the same Mac. Cold backend runs preview six temporary copies; attempt one also includes framework initialization.

| Measurement | Before | After |
| --- | --- | --- |
| PNG backend attempts 2–6 | 87.8–89.0 ms | 83.7–84.8 ms |
| JPEG backend attempts 2–6 | 42.0–42.5 ms | 23.3–24.5 ms |
| PNG selection to ready element tree, one native UI sample | 201 ms | 145 ms |
| PNG revisit in that UI sample | 48–50 ms, worker dispatched | Synchronous decoded-cache hit, no worker |

Backend figures exclude UI scheduling, GPU upload, and presentation. The native UI measurement ends at GPUI building the ready element tree, not screen presentation; it is not a p95 or broad hardware benchmark. These generated fixtures do not represent every codec. Cold images still require file I/O and decoding; PNG decompression remains the dominant cost in this sample.

```sh
cargo run -p launcher-macos --example preview_bench -- /path/to/image.png --cold
RUST_LOG=debug cargo run -p launcher --example preview_ui_smoke -- /path/to/image.png
```

The native smoke run also checks centered resizing while toggling the preview pane. Tests cover no-debounce scheduling, synchronous Arc reuse, revision invalidation, cache count/byte eviction, loading-label grace and stale timers, stale completion rejection, malformed pixel buffers, native alpha/row order, TIFF orientation tag 6, cancellation, and decode-slot release. GPU memory under prolonged navigation and photographic color fidelity have not been profiled.
