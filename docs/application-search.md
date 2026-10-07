# Application search performance

## Cause and fix

Application metadata was already discovered at startup, but the first icon implementation called `NSWorkspace::iconForFile`, exported TIFF, and decoded/downscaled/encoded PNG for each result **inside `ApplicationProvider::search`**. Search held the icon-cache mutex during native work and returned the application batch only after all icons were ready. Broad queries requested up to 50 sequential icon conversions. Cancellation could not interrupt a native call already in progress.

Search now uses prebuilt item templates, UTF-32 fuzzy-match strings, and normalized exact titles. It performs no native icon calls or filesystem access, ranks references to those records, and clones only the retained results. Name, path, bundle-id and executable matching, exact-name priority and deterministic tie-breaking are preserved.

Results carry `IconDescriptor::ApplicationBundle(PathBuf)`. After accepting results, the UI schedules one background icon load. Both native extraction and `gpui::Image` creation/hashing happen off the UI thread. Completion caches the image and redraws without re-running search. The next load is chosen from the **current** ranked results: query changes do not enqueue stale batches or start overlapping native calls. Hiding the launcher stops subsequent loads; an existing load may finish and populate the cache. Successful images and failures are cached for the process lifetime.

The worker can load all retained results, not just the six visible rows. A placeholder is displayed until each image is ready. Icons remain bounded to 64 pixels per side and 256 KiB; improving native icon throughput is separate from result-delivery latency.

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

The example times discovery, index construction, first search, 100 warm searches, and 20 coordinator searches. It does not time UI input-to-paint, mixed-provider completion, icon throughput, or rapid-typing behavior. Provider tests assert that even a real Finder bundle returns a lazy descriptor rather than PNG bytes. UI tests cover single-flight scheduling, selection from current results, hidden-window behavior, and native background loading/caching. Timing assertions are intentionally not embedded in unit tests because native hosts and CI load vary.

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
