# Wallpaper preview performance

The wallpaper picker paints a five-frame strip from the active pack. Its
thumbnail work was serialized end to end, so a pack's strip took about a second
to fill and the frames on the left stayed blank longest. Browsing, ranking and
the protocol are unchanged.

## Where the time went

Measured on an 8 GB M1 MacBook Air against the `bellerophon` pack (sixteen
2880x1800 WebP files, 0.6–1.2 MB each) and the release build:

| Stage | Measured |
| --- | --- |
| One `previewBgra8` decode (2880x1800 → 384 px edge) | 56–98 ms, ~70 ms typical |
| Sixteen previews through `AssetStore::get`, one after another | **1090–1240 ms** |
| The same sixteen from 2 or 4 client threads, before any change | 1084–1091 ms (no gain) |
| Path resolution for the whole pack (2x `canonicalize` + `stat` per file) | 0.3 ms |

Three findings mattered:

1. `AssetStore` held one global `resolve_lock` across the whole decode. Any
   number of callers therefore decoded one file at a time; adding concurrency
   changed nothing, as the third row shows.
2. The agent fetched previews one at a time, in catalog order, and only when its
   command channel was idle. At the frame the strip opens on, the two left-hand
   neighbours are the *last* published wallpapers, so they arrived after every
   other picture had been decoded — the strip looked half-loaded for a second
   even though five frames were all that was on screen.
3. Every new snapshot re-queued the same asset references, and the agent had no
   "I already have these pixels" check. Each keystroke in the picker re-requested
   up to 24 previews — ~370 KiB each — over the socket, and the frontend
   re-decoded every cached thumbnail into a fresh render image on the UI thread.

## What changed

- **Previews decode concurrently, one decode per file revision.** The claim on
  an asset is now a per-entry state (`Empty` / `Resolving` / `Settled`) instead
  of a global mutex. Icon extraction keeps its serialization because it calls
  AppKit; ImageIO thumbnails are thread-safe and are limited by the existing
  decode permits. A second request for a reference already being decoded waits
  for that result instead of paying for the file twice, and a claim left behind
  by a panicking decode is released so later requests cannot wait forever.
- **The strip is warmed when the picker opens.** `WallpaperServiceProvider`
  starts a bounded background pass over the active pack, ordered by distance
  around the strip ring, so the focused frame and its two neighbours on each
  side exist first. It runs once per (catalog revision, active pack), so a
  keystroke never restarts it, and switching packs cancels the previous pass.
  The frontend still asks for pixels itself; those requests are now store hits.
- **Decode permits went from two to four.** `image_thumbnail` documents the
  measurements that fix the ceiling: the same sixteen files take 1160 ms with
  one permit, 596 ms with two and 324 ms with four, and four is where the curve
  flattens. Three of the four permits are used by the prefetch, leaving one for
  a demand request so it never queues behind the background pass.
- **The agent reuses pixels it already has.** Before issuing `assets.get` it
  checks its own cache for that asset id and revision and re-announces the
  existing token. This removes the per-keystroke re-download of the whole strip.
- **The frontend stops rebuilding thumbnails it already holds.** A snapshot no
  longer copies pixel buffers and allocates render images for preview sources
  that are already painted, and the wallpaper image cache evicts in insertion
  order rather than dropping an arbitrary key the strip may still be drawing.

The preview cap also went from 24 to 32 previews per view, matching the store
and frontend caches: packs with more pictures than the old cap left frames
permanently empty because nothing ever asked for them again.

## Measurements after the change

Same machine, same pack:

| Measurement | Before | After |
| --- | --- | --- |
| Whole strip decoded (16 previews) | 1090–1240 ms, serialized | **401 ms** across three prefetch workers |
| Demand fetch for a warmed preview | 56–98 ms (decode) | 0.4 ms for all sixteen (copy) |
| Eight callers asking for one reference at once | one decode, and every other asset waits behind the global lock | one decode, 56 ms; unrelated decodes proceed |
| Keystroke in the picker (24 previews) | 24 socket round trips, ~9 MB transferred | no transfer, cached tokens re-announced |

The visible effect is that the five frames on screen are the first jobs in the
pass, so they arrive in the first wave or two instead of the last: measured
against a live service, the first tile has pixels at 90 ms and the whole strip
is done at 464 ms, where the old path spent over a second before the last tile
— including the two left-hand neighbours the strip draws on open.

```sh
# Per-stage ImageIO timings for one file, cold and warm.
RUST_LOG=debug cargo run -p launcher --example preview_bench -- /path/to/wallpaper.webp --cold

# Delivery over a live service socket, one request per tile.
cargo run --release -p launcher --example socket_preview_bench -- \
    ~/Library/Caches/maccer-service/rpc
```

## Deliberately unchanged

- Previews stay at a 384 px edge (`MAX_PREVIEW_EDGE`). The focused frame is
  drawn at 320x226 CSS px, so this is already slightly soft on a 2x display;
  raising it would multiply both the decode and the transfer cost, which is the
  opposite of the problem being fixed.
- Decoding still goes through `CGImageSourceCreateThumbnailAtIndex` with
  `...FromImageAlways`. WebP has no scalable shortcut, so the source is decoded
  in full either way; the win came from parallelism, not from a cheaper codec
  path.
- Paths still never cross the protocol, and each pack identity is revalidated
  the same way.

Tests cover the shared decode (eight callers, one reference), prefetch warm-up
and reference reuse, retry-after-failure, eviction during a decode, the strip
prefetch order, and the one-pass-per-pack schedule. Against a running service
with a cold store, `socket_preview_bench` reports the whole 16-picture strip in
464 ms, first pixels at 90 ms (including ImageIO's first-call setup) and a
keystroke re-query in 6 ms. GPU upload cost and memory under a pack larger than
the 32-preview cap have not been profiled.
