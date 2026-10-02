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

## Not verified

- OS-level keyboard and IME events delivered to the launcher window (unit tests cover the range logic, not Cocoa event dispatch).
- NSWorkspace launch/reveal/open against real applications (deliberately not exercised to avoid opening user apps).
- Physical global-hotkey delivery under a live login session, and conflict handling when another app already owns `alt-space`. Registration itself did succeed on this machine: running the real binary with `--show` reached the event loop with no fatal startup error, and a registration failure would have exited nonzero via `fail_startup` (`crates/launcher/src/main.rs`).
- Multi-monitor placement and focus behavior across displays and fullscreen apps.
- Idle CPU/memory over an extended session.
- Any latency target from the plan.
- Visual layout, scrolling, and caret rendering in a live window.
