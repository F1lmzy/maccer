# Runtime verification

This document records what was actually exercised on macOS during development, and what was not. It exists so future work does not mistake compile/unit coverage for verified runtime behavior.

## Retained-window lifecycle (verified)

GPUI 0.2.2 had a reproducible deadlock on the second show after a hide: the window-state mutex was held while Cocoa synchronously re-entered the key-status callback. This blocked the launcher's primary Option-Space toggle. It was reproduced in a minimal GPUI-only probe (no launcher code) before being fixed in the vendored crate. See `docs/gpui-patch.md` for the root cause and patch.

After the patch:

```sh
cargo run -p launcher-ui --example retained_window_smoke
```

- Performs 20 show/hide cycles with a 20-second watchdog that exits nonzero on a stall.
- Result: 5 consecutive runs, 20 cycles each (100 cycles total), exit code 0 every time.
- Window state transitions observed in a probe using the real `launcher-ui` path:
  - initial (created with `show: false`): inactive, size 680x148
  - after `show`: active (key window), same bounds
  - after `hide`: inactive, same bounds
- A separate probe drove the real `launcher-ui` path through 20 show/hide cycles per run, 3 runs (60 cycles total), each run exiting 0 with no watchdog fire. Every `show` reached `active=true` with a focused window element; every `hide` reached `active=false`.

This proves the retained pop-up window can be shown, hidden, and shown again repeatedly without the deadlock, and that the app-level `activate`/`hide` path toggles the window as intended. The probes could not identify the private search-input handle independently, so they verify that *some* window element held focus, not that the search input specifically did.

## Application startup (single sample)

```sh
target/debug/maccer --show
```

- Ran for 22 seconds with no errors in the log, then was terminated by PID.
- Observed RSS ~74 MB and ~0.5% CPU in that single debug-build sample.

This is a smoke sample only. It does not establish idle CPU over time, memory stability, or any performance target from the plan. A screenshot could not be captured on this machine (`screencapture` reported `could not create image from display`), so visual polish is unverified.

## Quit behavior (verified, corrects an earlier claim)

An earlier change claimed `window.on_window_should_close(..., false)` vetoed `NSApp terminate:` and therefore `cx.quit()` could not exit. A runtime probe disproved this:

- show -> hide -> `cx.quit()`: exit 0, 3/3 runs.
- show -> `cx.quit()` while visible: exit 0, 3/3 runs.

`windowShouldClose:` is registered on GPUI's delegate but `applicationShouldTerminate:` is not, so the close veto applies to window close (Cmd-W), not application termination. Because `cx.quit()` exits with status 0, startup failures use an explicit `std::process::exit(1)` instead (see `crates/launcher/src/main.rs`, `fail_startup`). Normal Cmd-Q still uses `cx.quit()`.

## fd file search, previews and transfers

```sh
cargo run -p provider-files --example search_smoke -- crates /absolute/root
cargo run -p provider-files --example search_smoke -- 'Documents pdf' "$HOME"
cargo test -p launcher-macos
cargo test -p launcher-ui
```

The application now uses fd, not the previous Spotlight/zoxide/fzf backend. The diagnostic builds a temporary SQLite index and checks root containment, descending scores and the 50-result limit. It opens no user files and records no usage.

Debug-build samples on this machine:
- Repository root: initial indexing 19 ms, 50-result cached queries 7 ms and 5 ms.
- Home root, about 178,000 entries: initial indexing 6.7–12.6 seconds. The initial unbounded query measured 866–875 ms. Caching normalized paths and applying an Elephant-style 1,000-candidate budget reduced the measured `Documents pdf` queries to 228 ms and 227 ms.

These are individual samples, not p95 benchmarks or proof of the roadmap's latency targets. Browsing considers 100 recent files; queries prefer literal path terms before fuzzy-only matches, within a 1,000-candidate budget per root. Watching uses debounced complete rescans so fd ignore rules remain authoritative. Large roots can take seconds to refresh and may create background load; configure narrower roots or disable watching when needed.

Tests exercise real fd discovery of files/folders, NUL/newline/Unicode paths, ignored-directory regexes, rename/delete refresh, automatic watcher updates, per-file preview revision changes, bounded candidate pools and retention of the previous snapshot after a scan failure. Actual macOS sips/Quick Look thumbnail generation produced PNGs for image and PDF fixtures. Native file and folder URLs round-trip through a private pasteboard, without altering the user's clipboard.

GPUI tests render 50 results, scroll beyond the six-row viewport and select row 50 through keyboard actions. They also verify stale-preview rejection, scrolling a long Unicode text preview, and one-shot drag initiation after a pointer threshold. The simulated preview test explicitly delivers the window resize callback, which the test platform does not generate automatically. These tests do not establish physical pointer delivery or cross-application drag behavior.

A fresh `maccer --config /tmp/maccer-fd-startup-smoke.toml --show` probe using the repository as the watched root stayed alive for five seconds with an empty log and approximately 70 MiB RSS, then was terminated by its PID. This verifies startup with the fd backend, not visual rendering or default-home memory/idle behavior. Physical dragging into Finder/other programs and Finder paste remain manual checks.

## Shell execution and output

`cargo test -p provider-shell` executes only controlled test scripts in temporary working directories. Tests verify that search creates no script side effects; actual activation captures stdout/stderr and nonzero exit status, caps retained output, and responds to cancellation. Native process tests separately cover inherited-pipe timeout and process-group cleanup.

`cargo test -p launcher-ui` verifies the rendered output screen's arrow-key scrolling and clipboard copy using GPUI's test platform. It also checks that Enter while viewing output cannot repeat the hidden command, and that query edits/Escape cancel the activation token. GPUI's test platform does not implement application hide, so dismissal cancellation is source-reviewed rather than covered by that test.

No user-configured scripts were executed. Physical keyboard delivery and clipboard behavior in a live output window remain unverified.

## Compact Walker-inspired layout

The default base width is now 480 points, with six 44-point result rows at most. A preview adds 280 points and reserves five rows. The dark-gray surface has 12-point rounded corners and a bright gray one-point border; selected rows have 6-point rounded corners, and selected rows are near-white with dark text. Titles use a 14-point font/18-point line box and types use 11/14, with no flex shrinking. A regression failed before the fix with a squeezed 17.5-point title line, then passed with both lines fully contained. Tests also cover selected actions beyond the first viewport and compact empty/error sizing.

`RUST_LOG=debug cargo run -p launcher --example preview_ui_smoke -- /tmp/maccer-preview-4k.png` passed five native width/center checks: preview-on bounds `(340,298,760,304)` and preview-off `(480,386,480,128)` on a 1440x900 display. Both centers stayed `(720,450)`. This verifies native geometry, not a screenshot comparison; actual appearance and font rendering still need user-session confirmation against the supplied Walker references.

## Image preview latency

The current image path uses ImageIO background decoding into render-ready BGRA, with no preview debounce and a synchronous 16-entry/32 MiB decoded-image UI cache. A native UI sample on the generated 4K PNG improved from 201 ms to 145 ms to the ready element tree; revisits now hit the decoded cache synchronously without dispatching a worker. See [current implementation, Vicinae sources, timings and limits](image-preview-performance.md).

The measurements below describe the earlier PNG-based implementation. It replaced `sips` subprocesses with in-process ImageIO, at most two simultaneous native decodes, and an 800-pixel thumbnail limit. Its 16-entry/16 MiB PNG cache remains for PDF thumbnails, but no longer sits on the native image path. Its 40 ms UI debounce has since been removed.

Comparison sources: [Elephant Files](https://github.com/abenz1267/elephant/blob/master/internal/providers/files/query.go) passes a path and preview type to its frontend. [Walker](https://github.com/abenz1267/walker/blob/master/src/preview/mod.rs) asynchronously reads images and decodes/scales them in-process with gdk-pixbuf. It retains only the last preview widget, not a disk thumbnail cache. Sources were inspected, not benchmarked.

```sh
cargo run -p launcher-macos --example preview_bench -- /path/to/image.png
cargo run -p launcher-macos --example preview_bench -- /path/to/image.png --cold
```

`--cold` previews temporary copies to bypass the thumbnail cache without changing the source. Debug-build samples on generated 3840x2160 fixtures:

- Previous PNG backend: 165 ms first generation; 125–150 ms subsequent generations.
- ImageIO PNG cache misses: 135 ms first generation; 88–89 ms subsequent generations.
- Previous JPEG `sips` command plus PNG readback: 249 ms first generation; 122–123 ms subsequent generations (measured directly, without the process runner's polling overhead).
- ImageIO JPEG cache misses: 80 ms first generation; 42 ms subsequent generations.
- PNG/JPEG thumbnail cache hits: below 0.1 ms.

These backend timings exclude the debounce, GPUI PNG decoding, GPU upload and presentation; they are not end-to-end latency or p95 measurements. The generated fixtures are not representative of every image or codec. Native tests cover PNG output, malformed images and decode-slot release; cache tests cover generation avoidance, cancellation, change invalidation, recency and memory limits. PDF generation retains the cancellable five-second `qlmanage` subprocess deadline. ImageIO cannot interrupt a decode already in progress; native decoding is concurrency-limited, checks cancellation between stages and rejects sources over 100 megapixels. Waiting requests check cancellation and have a two-second slot-acquisition limit.

### Earlier UI loading follow-up

Backend benchmarks do not cover the full loading indicator. `RUST_LOG=debug cargo run -p launcher --example preview_ui_smoke -- /path/to/image.png` runs the native selection/provider/UI path and logs request, worker start/completion, UI application and ready-frame render. On the generated 4K PNG, these stages measured 0/81/204/205/213 ms. The final stage records GPUI building the ready element tree, not GPU presentation or a screenshot.

The user's multi-second loading report remains unreproduced. Stage logs in the real launcher omit filenames and file contents. After two seconds an unfinished request displays a slow-preview notice instead of the generic loading label; it does not cancel valid slow work or shorten the PDF backend's five-second deadline. A late valid preview still displays and caches. Tests verify notice timing, late completion, and that completed/newer previews are not overwritten. This is diagnostic hardening, not evidence that the reported latency is fixed.

## Not verified

- OS-level keyboard and IME events delivered to the launcher window (unit tests cover the range logic, not Cocoa event dispatch).
- NSWorkspace launch/reveal/open against real applications (deliberately not exercised to avoid opening user apps).
- Physical global-hotkey delivery under a live login session, and conflict handling when another app already owns `alt-space`. Registration itself did succeed on this machine: running the real binary with `--show` reached the event loop with no fatal startup error, and a registration failure would have exited nonzero via `fail_startup` (`crates/launcher/src/main.rs`).
- Multi-monitor placement and focus behavior across displays and fullscreen apps.
- Idle CPU/memory over an extended session.
- Any latency target from the plan.
- Visual layout, preview appearance, scrolling, and caret rendering in a live window.
- Physical external file/folder drag-and-drop and Finder paste. The native file-URL pasteboard implementation is tested through a private pasteboard, not through a user-session paste operation.
