# Application search performance

## Cause and fix

Application metadata is discovered at startup and refreshed after debounced filesystem changes. The first icon implementation called `NSWorkspace::iconForFile`, exported TIFF, and decoded/downscaled/encoded PNG for each result **inside `ApplicationProvider::search`**. Search held the icon-cache mutex during native work and returned the application batch only after all icons were ready. Broad queries requested up to 50 sequential icon conversions. Cancellation could not interrupt a native call already in progress.

Search now uses prebuilt item templates, UTF-32 fuzzy-match strings, and normalized exact titles. It performs no native icon calls or filesystem access, ranks references to those records, and clones only the retained results. Name, path, bundle-id and executable matching, exact-name priority and deterministic tie-breaking are preserved.

The live application catalog now follows the directory-discovery and snapshot-refresh architecture described in [Vicinae-style application catalog](vicinae-app-implementation.md). Finder-localized names keep their raw bundle names as search aliases, and configured app roots take precedence over default roots. Catalog refresh builds new searchable records on the watcher thread and publishes an immutable snapshot; unchanged catalogs do not increment the revision. The UI's existing revision poll refreshes current results after catalog changes. Static providers remain available when `[apps].watch = false`.

Results carry `IconDescriptor::ApplicationBundle(PathBuf)`. The UI now fills at most four background icon-load slots. Each bundle has at most one active load, even across query changes. The uniform list reports its actual viewport after mouse or keyboard scrolling; those rows take priority over offscreen warming. Completion selects new work from current results instead of retaining an old query's queue. Hidden, action and output screens stop new loads. Existing loads may finish and populate the process-lifetime cache.

The native worker obtains an NSWorkspace icon and a retained CGImage snapshot under a short AppKit lock, because NSImage backing representations may be shared and thread-unsafe. CoreGraphics then draws outside that lock into a 64x64 BGRA buffer. The worker converts premultiplied alpha to straight alpha and creates an `Arc<gpui::RenderImage>`. This bypasses TIFF export, ImageIO resize/PNG encoding, PNG hashing and GPUI PNG decoding. Rows only look up cached render-ready images; missing images use placeholders. Each pixel buffer is 16 KiB. The legacy PNG API remains available for compatibility and comparison, but application rows no longer use it.

## Measurements

Measured on this macOS development machine with **101 installed applications**, debug builds, a fresh provider for each query, a 50-result limit, and no GUI window. Before and after use the same example and machine; results are not universal latency guarantees.

| Query | Results | Cold search before | Cold search after | Warm search after, p95 |
| --- | ---: | ---: | ---: | ---: |
| `safari` | 5 | 1,350.43 ms | 0.57 ms | 0.25 ms |
| `a` | 50 | 30,232.96 ms | 0.66 ms | 0.37 ms |

Before: warm p95 was 0.71 ms (`safari`) and 1.26 ms (`a`). After: startup discovery was 115–201 ms and index construction 0.69 ms; neither runs on each query. Coordinator delivery of complete application results, including worker startup and ranking/history, was 2.92 ms p95 (`safari`) and 2.90 ms p95 (`a`), over 20 runs per query.

Reproduce with:

```sh
cargo run -p launcher --example app_search_bench -- safari
cargo run -p launcher --example app_search_bench -- a
# Use --release for production-build measurements; numbers above are debug.
```

The example times discovery, index construction, first search, 100 warm searches, and 20 coordinator searches. It does not time UI input-to-paint, mixed-provider completion, icon throughput, or rapid-typing behavior. Provider tests assert that even a real Finder bundle returns a lazy descriptor rather than PNG bytes. UI tests cover the four-load limit, per-bundle single-flight, selection from current results and actual scrolled viewport, hidden-window behavior, and native background loading/caching. Timing assertions are intentionally not embedded in unit tests because native hosts and CI load vary.

## Direct-pixel icon measurements

The next optimization addressed icons appearing after the result text. Measurements used the same first six discovered applications, debug builds and a fresh process per invocation. Native framework initialization and OS cache state vary between runs.

| Pipeline | First six-icon batch | Repeated native batch, p50 |
| --- | ---: | ---: |
| Old serial TIFF/PNG path | 1,337 ms initially; 4,112 ms on a later run | 560–643 ms |
| Direct pixels, four worker slots | 231 ms on the final synchronized-snapshot implementation | 3.43 ms |

These are native preparation timings, not input-to-paint or GPU-upload latency. The old measurement includes encoded-image hashing but excludes GPUI's subsequent PNG decode. The pixel measurement excludes RenderImage buffer wrapping. UI tests separately confirm that cached application icons are render-ready 64x64 buffers. No comparison benchmark against Vicinae was run.

```sh
cargo run -p launcher --example app_icon_bench -- --png
cargo run -p launcher --example app_icon_bench -- --pixels --parallel
```

Verification passed 89 native library tests, 43 UI tests and 12 application-provider tests, along with affected-target clippy and formatting checks. The asymmetric real-CGImage fixture checks row orientation, color channels, alpha and transparent pixels; a preliminary row-flip assumption failed that test and was removed. Native tests exercise four concurrent calls, while UI tests verify that query changes cannot exceed four slots and that wheel/keyboard scrolling changes viewport priority.

The rebuilt native launcher was restarted successfully. Its first four icon preparation events completed in 29–34 ms each, followed by 3–8 ms preparation events, with `encoded_decode_required=false`. Its 480x348 window was present. Window capture was unavailable, so this task does not claim a screenshot or measured paint latency.

AppKit API/threading references: [NSWorkspace icon lookup](https://developer.apple.com/documentation/appkit/nsworkspace/icon(forfile:)), [NSImage CGImage snapshot](https://developer.apple.com/documentation/appkit/nsimage/cgimage(forproposedrect:context:hints:)), and [Apple's thread-safety summary](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html). The code serializes AppKit snapshot calls and parallelizes work on retained immutable CoreGraphics images. Persistent disk caching and GPU-atlas eviction policy are unchanged.

## Upstream implementations examined

Sources were inspected at pinned commits; neither upstream launcher was benchmarked here.

### Vicinae

Commit `b66cc7241a33bf1b1181cbac6de4f9e00042d9cb`:

- [mac-app-database.mm](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/services/app-service/macos/mac-app-database.mm): scans bundle metadata to build the application database.
- [root-item-manager.cpp](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/services/root-item-manager/root-item-manager.cpp): `updateIndex` prebuilds searchable records; `search` fuzzy-matches in memory using a thread-local matcher and sorts scores. This is not filesystem discovery per keystroke.
- [mac-app.mm](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/services/app-service/macos/mac-app.mm): `iconUrl` returns a bundle-path image identifier, not decoded icon bytes.
- [image-stream.cpp](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/image/image-stream.cpp): checks image caches and dispatches `MacBundle` icon rendering to a decoding pool, separate from matching.
- [mac-file-icon-loader.mm](https://github.com/vicinaehq/vicinae/blob/b66cc7241a33bf1b1181cbac6de4f9e00042d9cb/src/server/src/ui/image/mac-file-icon-loader.mm): resolves NSWorkspace icons in the image-rendering worker.

maccer adopts the separation between matching and icons, not Vicinae's matcher, complete cache hierarchy, or rendering implementation.

### Sol

Commit `5a3034b511ba83c3225f662496f78fe5e3b8f661` in [ospfranco/sol](https://github.com/ospfranco/sol/tree/5a3034b511ba83c3225f662496f78fe5e3b8f661): `ApplicationSearcher.getAllApplications` enumerates native application metadata, the JSI bridge returns name/localized name/URL/running state without icon bytes, and `src/stores/ui.store.tsx` builds and searches a MiniSearch index. Filesystem events trigger application re-indexing rather than rediscovery on every query.

The common principle is to keep the keystroke path in memory and defer expensive presentation assets. Switching fuzzy-match libraries would not have fixed maccer's measured icon bottleneck.
