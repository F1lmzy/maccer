# Web preview cost

A web result can open inside the launcher in a native `WKWebView` instead of the default browser. The panel is optional (`[web].inline_preview`), the WebKit processes are started before the user commits (`[web].prewarm`), and a hidden page keeps its document until an idle delay expires (`[web].idle_release_seconds`).

## What changed

- `WebPreview::warm` loads `about:blank` while a web result is selected. The first navigation, not `WKWebView` creation, is what spawns the WebContent process, so the warm load removes process start-up from the frame where the user presses Enter.
- Prewarming is gated on the selected row being a `web` result and on inline previews being enabled. Selecting an app, file, calculator or clipboard result never starts WebKit.
- `hide` no longer loads an empty document. It hides the view, stops loading and restores keyboard focus. A hidden page measured 0.00 CPU-seconds over ten seconds and WebKit returned its memory within seconds on its own, so unloading only added a reload on the next open.
- `release_page` performs the old empty-document load, but only after `idle_release_seconds` (default 300) with the panel hidden. It keeps the WebKit processes, so the next search still skips start-up.
- The view is never destroyed. WebKit keeps its helper processes pooled after the view is released, so tearing it down saves no memory and pays start-up again on the next search.
- The configuration requires a user gesture for media playback and turns the remote inspector off, so a panel the user ignores starts no decoders and no inspector connection. The persistent website data store is kept: a non-persistent store measured no memory win and discarded the HTTP cache.

## Measurements

A minimal host that mirrors `web_preview.rs` (frame-zero `WKWebView`, Safari user agent, same hide/release calls) on macOS 26.4 with Safari 26 WebKit. RSS comes from `ps`, sampled for the WebKit XPC processes each run started; each row is a fresh process, 2–4 runs.

| stage | cost |
| --- | --- |
| `WKWebView` creation (blocking) | 0.1–0.2 ms |
| GPU + Networking XPC spawn | 16–26 MB, immediately at creation |
| `about:blank` with the view hidden | 122–280 ms, starts the WebContent process (26–27 MB, ≈55–58 MB across all three XPC processes) |
| first SERP first paint, cold | 449–921 ms |
| first SERP first paint, prewarmed | 63–104 ms |
| first SERP `didFinish`, cold | 533–1412 ms |
| first SERP `didFinish`, prewarmed | 473–547 ms |
| live SERP, all WebKit XPC | 56–250 MB, dominated by the page Google served |
| warmed but idle (view hidden) | ~55 MB at warm-up, settling to ~25 MB |
| hidden page kept | 28 MB after 8 s, 0.00 CPU-s over 10 s |
| hidden page after the empty-document load | ~25 MB, next open reloads (~500 ms) |
| view released, processes pooled | WebContent still alive after 60 s at ~0 MB; recreate is as slow as cold |

The live figure varies 4x between Google result variants (ads, consent pages, A/B layouts). Compare variants against the same page and treat the minimum as the same-content estimate.

## Harness

```sh
swiftc -O scripts/web_preview_bench.swift -o /tmp/web_preview_bench
/tmp/web_preview_bench prewarm   # cold vs warm, and two warm searches
/tmp/web_preview_bench prewarm-hidden # warm while the view is hidden, as maccer does
/tmp/web_preview_bench reopen    # hide/re-show with and without the reload
/tmp/web_preview_bench hiddencpu # idle CPU of a hidden page
/tmp/web_preview_bench kill      # release the view, then recreate it
/tmp/web_preview_bench cold      # first search with nothing warm
```

Google's page weight makes single samples useless for memory comparisons. The harness keeps every measurement in a separate process for that reason.

The native calls themselves have a smoke test that runs the real code path (capture the AppKit host, warm while hidden, load, hide, release, quit) and exits nonzero on a hang:

```sh
cargo run -p launcher --example web_preview_smoke
```

## Debug timings

With `RUST_LOG=debug`, the launcher emits:

1. `web_preview_create` — configuring the view and installing the key monitor. Sub-millisecond per the table above.
2. `web_preview_warm` — issuing the empty-document load. This is not the navigation time; WebKit reports that asynchronously.
3. `web_preview_release` — the idle timer firing and dropping the document.

These correlate with the existing `preview{request_id=...}` spans only through the log's timestamps; the web panel is independent of the file preview pipeline.

## Left open

Both of the following change what the page renders, so they need the harness above before they earn a default:

- A `WKContentRuleList` blocking known ad and tracker hosts. It compiles in ~15 ms, but the measured memory spread between Google variants is larger than the effect being measured, so it needs an A/B with fresh processes per sample.
- Per-engine JavaScript-off mode. `html.duckduckgo.com/html/?q=` renders in 962 ms at 33 MB of WebContent, but Google without JavaScript serves a 272-character "enable JavaScript" wall. Any such flag must be per engine, and the service frontend only carries a URL for a web result, so it also needs a protocol field.
