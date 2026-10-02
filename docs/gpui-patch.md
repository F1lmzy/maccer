# GPUI 0.2.2 local patch

`gpui` is pinned to crates.io version `=0.2.2` in the workspace. The `[patch.crates-io]` entry redirects that exact package to `vendor/gpui`, a local copy of the published crate with one macOS reentrancy fix. The crate retains its upstream Apache-2.0 license and source attribution in `vendor/gpui/LICENSE-APACHE` and `vendor/gpui/README.md`.

## Why the patch exists

In `src/platform/mac/window.rs`, `window_did_change_key_status` handles a spurious `windowDidBecomeKey` event by calling `resignKeyWindow`. Cocoa synchronously emits `windowDidResignKey`, re-entering the same callback. The original code called Cocoa while holding GPUI's non-reentrant window-state mutex, so the re-entry blocked forever. The handler now copies the native window handle, drops the mutex, and only then invokes `resignKeyWindow`.

This is narrowly scoped to that callback. The `Arc<Mutex<MacWindowState>>` is retained for the callback's lifetime, and AppKit owns the native window; the local Objective-C object handle remains valid for the synchronous call. The re-entered callback can safely acquire the mutex.

## Provenance and maintenance

The vendored source was copied from the `gpui 0.2.2` crates.io package resolved by Cargo. The cached `.crate` archive SHA-256 is `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`. The upstream repository is <https://github.com/zed-industries/zed>, license Apache-2.0. The patch intentionally retains the published crate's normalized `Cargo.toml`, `Cargo.toml.orig`, build script, source, resources, `docs/`, `examples/`, and `tests/` rather than relying on a machine-local Cargo registry edit. Every target path declared in the normalized manifest resolves to a real file, so the vendored crate is a complete copy of the published package. Only `src/platform/mac/window.rs` differs from upstream. This keeps builds reproducible and preserves the exact GPUI version/API used by the launcher.

When upgrading GPUI, first check the corresponding upstream `window_did_change_key_status` implementation and add/run `cargo run -p launcher-ui --example retained_window_smoke` on macOS. Remove the local override and vendored copy only after the upstream version contains the lock-release-before-AppKit-call fix and repeated hide/show cycles pass. Do not silently bump GPUI: it is pre-1.0 and may change APIs.

## Verification

`crates/launcher-ui/examples/retained_window_smoke.rs` exercises repeated retained pop-up activation/hide cycles without registering a hotkey or invoking providers. Run it on macOS with:

```sh
cargo run -p launcher-ui --example retained_window_smoke
```

The example drives 20 `show`/`hide` cycles, asserts the window reports `active=true` after each show and `active=false` after each hide, checks both nested GPUI update results, and checks the final `cx.quit()` result. A watchdog runs on a separate OS thread, so it still fires (exit 2) if the main thread deadlocks inside a Cocoa notification. It is a native runtime regression check; unit tests alone cannot trigger Cocoa's synchronous window notifications.

## Verified results

On this machine (macOS, arm64, GPUI `0.2.2` + this patch):

- Pure-GPUI minimal probe (no launcher code), unpatched upstream: 3/5 runs deadlocked on the second show; patched: 5 runs x 20 cycles, exit 0 each.
- Real `launcher-ui` `Launcher::show`/`hide` path via a `/tmp` probe: 3 runs x 20 cycles, exit 0 each; every show reported a focused window element and `active=true`, every hide reported `active=false`.
- `cargo run -p launcher-ui --example retained_window_smoke`: 1 run x 20 cycles, exit 0.

These checks prove retained-window reactivation, focus presence, and hide/show state on this machine. They do not prove behavior on other macOS versions, physical hotkey delivery, keyboard/IME events, or visual appearance.
