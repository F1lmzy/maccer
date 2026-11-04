# Shared service setup and operation

Maccer runs one independent background service with either GPUI or Pill/QML as its selected native frontend. Closing or removing a frontend does not stop the backend. This path is opt-in; embedded mode remains available for providers that have not migrated.

## Supported features

| Feature | Shared service |
| --- | --- |
| Applications | Discovery, fuzzy search, Open/Reveal, native icons and metadata previews |
| Web search | Configured engines, Google by default, encoded URLs and Copy Search URL |
| Clipboard | RAM-only text capture, scoped search, previews, Copy, Pin/Unpin, Delete and Clear History |
| Themes | Shared active palette, catalog search, palette previews and explicitly confirmed Apply |
| Files, calculator, shell | Embedded mode only |
| Direct Paste, wallpaper changes, plugins | Not available through the service |

Home is a searchable backend view. An empty query lists enabled providers; selecting one enters its scope. Clipboard contents never appear in mixed home search. Both frontends retain displayed results, icons and preview geometry while a replacement query loads, but immediately invalidate old actions, details and staged confirmations.

## Build

Use macOS with a working Rust toolchain and Apple SDK:

```sh
cargo build -p launcher
# Optional signed GPUI bundle:
./scripts/bundle-macos.sh release
```

Build [Pill](https://github.com/F1lmzy/mac-qml-bar) separately. Its `MACCER_SOURCE` build variable must point to this checkout so the bundled agent library and header match the backend:

```sh
cd /path/to/mac-qml-bar
MACCER_SOURCE=/path/to/maccer ./scripts/build.sh
```

Theme integration requires a compatible mac-themer installation providing `catalog --json` and guarded `apply SLUG --expected-palette JSON --json`. It is maintained separately and is not bundled here. `MACTHEMER_BIN` names the directory containing `mac-themer`, not the executable itself. Without that override, helper resolution uses `MACTHEMER_HOME` or the user's home plus `.local/bin`. Set `MACTHEMER_STATE` on the backend to override the published palette directory.

## Select a frontend and start the backend

Quit old GPUI and Pill instances and stop the old backend before switching. For Pill, place this in `~/.config/maccer/config.toml`:

```toml
[launcher]
hotkey = "alt-space"

[service.frontend]
kind = "pill"
path = "~/pill-macos-spike/build/app/PillSpike.app"
```

For GPUI, set `kind = "gpui"` and use an executable path, such as `~/Documents/maccer/target/release/maccer.app/Contents/MacOS/maccer` or an absolute development `target/debug/maccer` path. Pill requires an existing `.app` directory.

From the Maccer checkout:

```sh
# The socket parent must be owned by you and inaccessible to other users.
mkdir -m 700 -p "$HOME/Library/Caches/maccer-service"

MACTHEMER_BIN="$HOME/Documents/mac-themer/bin" \
./target/debug/maccer-service --apps \
  --socket "$HOME/Library/Caches/maccer-service/rpc"
```

Keep the service in its own terminal or supervise it independently. It does not install a login item or launch agent. A socket already present at startup is an error; do not remove it while its backend is running. The backend removes its own socket on graceful shutdown.

Press Option-Space. The backend opens or refocuses the selected frontend and launches it if absent. Repeated presses never hide an open launcher. Paired `--frontend pill --frontend-path ABSOLUTE_APP_PATH` flags override the config; use `gpui` and an executable for GPUI. Fixture mode rejects these launch flags. No selection means no backend-owned global shortcut.

Both service-mode native entry points acquire a private per-socket `.frontend.lock` for their process lifetime and check the selected kind before constructing windows or providers. A second native process of either kind is refused. The selected-kind check also refuses GPUI while Pill is selected but closed. GPUI refuses accidental embedded startup when its config selects a shared frontend. Headless probes and ordinary IPC clients can still connect simultaneously.

The process lock is separate from the renewable presentation lease, so lease expiry and backend restarts cannot allow duplicate native frontends. No frontend is killed automatically. Restart frontends after restarting the backend; clients do not reconnect or replay mutations automatically.

Pill's environment-unset embedded mode remains available. Quit an embedded Pill before using the shared-service shortcut: LaunchServices cannot replace an existing process's environment. These startup guarantees cover the supplied service-mode native applications, not arbitrary third-party clients or separately configured embedded instances.

## Explicit frontend launch and diagnostics

When no frontend is selected, or for development, launch a frontend against an existing socket:

```sh
./target/debug/maccer --service-socket "$HOME/Library/Caches/maccer-service/rpc" --show

# After quitting any existing Pill instance:
/usr/bin/open -a "$HOME/pill-macos-spike/build/app/PillSpike.app" \
  --env "MACCER_SERVICE_SOCKET=$HOME/Library/Caches/maccer-service/rpc" \
  --args --show-launcher
```

The selected-kind and single-process checks still apply. These commands never start a backend or fall back to embedded providers. A refused native startup logs a diagnostic and exits without a launcher window. GPUI `--check` bypasses native admission and is safe while Pill is running:

```sh
./target/debug/maccer --service-socket "$HOME/Library/Caches/maccer-service/rpc" --check
./target/debug/maccer-client --socket "$HOME/Library/Caches/maccer-service/rpc" providers
```

Pass provider config and helper environment to the backend, not a service frontend. A configured shortcut conflict prevents backend startup. If a keyboard remapper changes Left Option into a chord, test Right Option-Space and inspect the remapping separately.

## Clipboard privacy

The backend polls sanitized pasteboard snapshots every 300 ms, even with no frontend connected. Its first poll records only the change-count baseline, not prelaunch contents. Files, nontext, oversized and marked concealed/transient/autogenerated content are skipped. Unmarked passwords can still enter history.

History is memory-only and excluded from SQLite usage recording. Defaults are 200 entries, configurable from 1 to 1,000, with 64 KiB per entry and 4 MiB total. Pins prevent automatic eviction, not Delete or Clear History. Restarting the backend clears everything, including pins; closing a frontend does not. New captures appear on the next query or when reopening `:` or the Clipboard menu. Disable `[providers.clipboard].enabled` to stop capture.

Direct Paste is deliberately absent until previous-application handoff is verified. Use Copy, then Cmd-V in the target application. Automated provider tests use owned memory data and fake desktop effects without reading or overwriting the user's clipboard or opening a browser.

## Themes and action safety

The backend watches mac-themer's published state, validates bounded palettes and retains last-good colors on invalid replacements. Catalog reads have a three-second deadline, two-MiB output cap and 128-theme limit. Selection previews do not change active colors.

Apply requires an explicit snapshot-bound confirmation in either frontend and service-enforced consent. Accepted operations run once independently of query changes or frontend disconnect. A shared executor permits one Apply at a time, with a 30-second helper deadline and 64 KiB output cap. The expected palette and catalog revision are revalidated. Wallpaper remains unchanged, and helper errors warn about possible partial effects.

Do not retry an uncertain operation after disconnect or restart. Deduplication is session-local, not durable. Apply is not transactional, concurrent external tools are not locked out, and concurrent theme-pack replacement after validation remains a producer write-boundary risk. The helper can outlive a daemon crash; its timeout requires a living supervisor. Native desktop Apply acceptance remains manual.

## Verification and remaining limits

The test suites cover socket permissions and ownership, framing and bounds, independent sessions, stale/cross-session authority, retained mutation results, confirmation, shared providers, rich previews, palette updates and native admission. See the [architecture contract](provider-service-spec.md) and `crates/launcher/schema/protocol.schema.json` for wire details.

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test -p launcher -- --test-threads=1
cargo test -p launcher --lib frontend_instance
cargo test -p launcher --test service_hotkey
cargo clippy -p launcher --all-targets --no-deps -- -D warnings
```

The launcher is one package. The former `launcher-*` and `provider-*` crates are modules under `crates/launcher/src`, so the whole launcher builds and tests as a unit. The C ABI header is `crates/launcher/include/maccer_agent.h`, and `cargo build -p launcher --lib` still produces `liblauncher_agent.dylib`; Pill's `MACCER_SOURCE` scripts must use those two paths.

Pill's `scripts/test.sh` includes QML-to-IPC owned-data web/clipboard acceptance, native geometry, Swift integration and relocated-bundle checks. Its latest full run passed 22 launcher C++ and 80 QML tests with no skips. Rust startup tests cover both kinds, C ownership and GPUI process refusal. Injected Option-Space has opened/refocused Pill without GPUI, and closing/reopening Pill preserved the backend instance and four providers. This is not physical keyboard/IME or native desktop mutation acceptance.

The consolidated workspace passes `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, 469 Rust tests with `--test-threads=1`, and the two native harness checks (`resident_menu` and `service_hotkey`). Timing-sensitive watcher tests should still be rerun when changing debounce or filesystem behavior. GPUI's retained-window patch has its own [native regression instructions](gpui-patch.md).

Independent installation, login lifecycle, complete JSON Schema validator conformance, streaming, larger service previews, remaining provider migration and retirement of embedded ownership are unfinished.
