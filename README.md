# maccer

A keyboard-first macOS launcher with an independent background service and removable GPUI or Pill/QML frontends. Shared-service mode supplies applications, web search, text clipboard history, themes (including installing and removing packs from a GitHub repository) and a wallpaper picker. Embedded GPUI also supports files, calculator expressions and explicitly selected shell commands.

## Run embedded GPUI

This mode requires no `[service.frontend]` selection. For a backend-owned shortcut and Pill or GPUI frontend, follow the [shared-service guide](docs/shared-service.md).

Requires macOS, Rust 1.85 or newer (edition 2024), a working Apple SDK, and `fd` for the files provider. Install `fd` with `brew install fd`, or set `[providers.files].enabled = false`. GPUI is pinned to 0.2.2 with runtime shader compilation enabled, so full Xcode is not required merely to compile on machines with Command Line Tools. First build can take a few minutes.

```sh
cargo run -p launcher
```

The launcher starts hidden and stays resident. Press **Option-Space** to toggle it. Use arrows (or Ctrl-N/Ctrl-P) to move, Return to use the default action, Cmd-Return or Cmd-K for the action menu, Tab for the secondary action, Cmd-P or `;` for the provider picker, and Escape to go back, clear the query, or hide the window. Prefixes are configurable; defaults are `/apps`, `/` for files, `=`, `?`, `:` for clipboard history, and `>` for shell commands. If Karabiner remaps your left Option key, use right Option-Space.

The **maccer** menu bar item has **Open Launcher**, **Launch at Login**, and **Quit** actions, even while the launcher is hidden. Open Launcher always shows the window; Option-Space still toggles it. **Cmd-Q** also quits while the launcher is focused.

### Install as a resident app

Launch at Login requires macOS 13 or newer and a signed app bundle. Build one with:

```sh
./scripts/bundle-macos.sh
mkdir -p ~/Applications
cp -R target/release/maccer.app ~/Applications/
open ~/Applications/maccer.app
```

Quit any terminal-launched copy first. Enable **Launch at Login** from the installed app's menu. It is off by default; macOS saves the registration through `SMAppService.mainAppService`, not the TOML config. A checkmark means enabled, a dash means awaiting approval in **System Settings > General > Login Items**. Click either to disable. The app offers to open System Settings when approval is needed. The menu refreshes from macOS each time it opens, so changes in System Settings are reflected.

Terminal builds have the same Open Launcher and Quit actions, but Launch at Login is disabled. The script uses ad-hoc signing for local builds; set `MACCER_SIGNING_IDENTITY` to a signing identity for distribution. Signing/notarization for distribution and the actual login-time launch remain manual verification steps. macOS registration errors appear in an alert rather than setting a false local preference. Keep the app at its installed location; disable Launch at Login before replacing it, then re-enable it afterward.

For a faster development bundle, use `./scripts/bundle-macos.sh debug`, which creates `target/debug/maccer.app`.

### Development flags

```sh
# Open the launcher immediately instead of starting hidden.
cargo run -p launcher -- --show

# Validate configuration and scan application bundles without opening a
# window or registering a hotkey.
cargo run -p launcher -- --check
cargo run -p launcher -- --config ./config/config.example.toml --check
```

`--config PATH` selects a TOML file other than the default.

## Shared service

The backend owns providers, capture, theme state and the global shortcut. Only the selected service-mode native frontend can start; a shared lifetime lock prevents duplicate GPUI/Pill processes. Closing a frontend leaves the backend running. Headless clients remain independent.

See the [setup and operations guide](docs/shared-service.md) for building both frontends, selecting Pill or GPUI, starting the backend, clipboard privacy, confirmed theme actions, verification and known limits. The [architecture contract](docs/provider-service-spec.md) describes IPC, snapshots, action authorization and lifecycle. The protocol is experimental; files, calculator, shell, direct Paste and independent installation have not migrated.

## Configuration

Configuration is read from `~/.config/maccer/config.toml`. Start from [`config/config.example.toml`](config/config.example.toml). Partial files inherit defaults, so a file with only `[launcher]` keeps the default providers. Provider tables merge field by field: `[providers.calculator] priority = 123` keeps the calculator prefix, `default_search`, and result limit. Web search engines default to Google and DuckDuckGo; define `[[web.engines]]` tables to replace that list, and set `[web].default_engine` to one of the configured ids. A web result opens in a native panel inside the launcher; `[web].inline_preview = false` hands it to the default browser instead, `prewarm` starts WebKit while a web result is selected, and `idle_release_seconds` releases a hidden page. See [web preview cost](docs/web-preview-performance.md).

Usage history is stored at `~/Library/Application Support/maccer/history.sqlite3`. maccer records an activation only after the action succeeds; each record includes the query text, the provider and item identifiers, the action, and a timestamp. Ranking uses per-item activation counts plus a bounded recency bonus. The database never leaves the machine; delete the file to clear history. Set `RUST_LOG=debug` for diagnostics. Startup failures display a native alert with the error chain and exit nonzero after dismissal. Hotkey conflicts display an alert but leave the app running, so Open Launcher and Quit remain available in the menu bar. Change `[launcher].hotkey` and restart to restore the shortcut. `--check` remains terminal-only and never opens an alert.

### Application search and icons

Application metadata and fuzzy-match strings are indexed once at startup. Each query matches in memory and clones only the ranked results; it does no filesystem access or native icon extraction.

Results carry `IconDescriptor::ApplicationBundle` paths. The GUI displays placeholders immediately, then resolves Finder icons through four bounded background workers with visible-row priority and per-bundle single-flight. Render-ready straight-alpha BGRA icons are at most 64x64. Embedded GPUI caches them for its process lifetime; the service and adapters use bounded caches. Icon loading never delays search completion.

See [application-search measurements and upstream comparisons](docs/application-search.md). To measure discovery, cold/warm matching, and coordinator result delivery without opening a window:

```sh
cargo run -p launcher --example app_search_bench -- safari
cargo run -p launcher --example app_search_bench -- a
```

### File search

The files provider follows [Elephant Files](https://github.com/abenz1267/elephant/tree/master/internal/providers/files), using `fd` to discover files and folders in the background. It searches full paths, so project/directory names work as well as filenames. Files appear only with the `/` prefix; ordinary searches and the empty launcher view do not include files, even if an older configuration enables `default_search` for files. Type `/` alone to browse recent files. `/ ~/Documents rpt pdf` narrows a query to an existing indexed directory. Configured roots default to your home directory.

`fd` follows its `.gitignore`, `.ignore` and `.fdignore` rules and skips hidden entries. Configure `[files].search_dirs`, regex `ignored_dirs`, or literal `exclude_paths` to control the index. `roots` remains an alias for `search_dirs`. There are no hardcoded personal-folder boosts. zoxide/fzf settings from older configs are accepted but no longer used. Full-path fuzzy matching, change times and launcher activation history determine ranking. Like Elephant, browsing considers up to 100 recent files and searches use a 1,000-candidate budget per root, preferring literal path terms before fuzzy-only matches.

Live filesystem watching defaults on. Changes trigger a debounced rescan and publish a complete refreshed snapshot without blocking cached searches; an open results list refreshes automatically. A full home-directory refresh can take seconds, so use narrower roots or disable watching if background scans are too costly. Set `watch = false` to index only at startup, then use the Refresh File Index action. The rebuildable path/metadata cache is `~/Library/Caches/maccer/files.sqlite3`, separate from usage history. Internal maccer database folders are excluded to prevent update loops. Index scans have a 60-second subprocess deadline and a 64 MiB output cap; narrow your roots if this limit is reached.

Selected files show inline UTF-8 text, image or first-page PDF previews. Cmd-Shift-P toggles the preview panel. Text is limited to 64 KiB. Images up to 64 MiB and 100 megapixels use in-process ImageIO on background workers, producing thumbnails up to 800 pixels per side; PDFs use Quick Look thumbnails. The UI reuses a 16-entry/32 MiB decoded-image cache synchronously, invalidated by the file index's change stamps. Native images avoid the PNG encode/decode round trip; PDFs retain a separate 16-entry/16 MiB PNG thumbnail cache. Cold loads start without a debounce, and the loading label appears only after 150 ms. See [image preview measurements and Vicinae comparison](docs/image-preview-performance.md). No preview contents are cached on disk. Binary/unsupported files show a message and retain the Quick Look action. Configure `ignore_previews` with absolute or `~/` directory paths to prevent content previews in sensitive folders. Previews run off the UI thread and cancel on selection/query changes.

Open files/folders with Return or a double click. The action menu includes Open Containing Folder, Reveal in Finder, Quick Look, Copy Path, Copy File and Refresh File Index. Copy File writes a native file URL that Finder and other apps can paste. Drag a result row into another program to start a native copy-only file drag. LocalSend is not included.

The compact, Walker-inspired GUI uses a dark-gray background, rounded corners, a highlighted border and a white selection row. It defaults to 480 points wide with six visible rows; the optional preview adds 280 points. The full window stays centered as its size changes. Override `[launcher].width` to change the base width.

The default keeps 50 results with six visible rows. Scroll with the trackpad/mouse wheel or use arrow keys to reach later results. Existing configurations setting either `[launcher].max_results` or `[providers.files].max_results` to 8 still cap results at eight; raise both to 50.

### Clipboard history

Type `:` to browse recent text copies, or `: words` to search them. Return copies the selected entry back to the clipboard and closes the launcher. Cmd-K opens Copy, Paste, Pin/Unpin, Delete, and Clear History actions. Tab invokes Paste in embedded mode. Cmd-Shift-P shows the text preview.

In shared-service mode, capture belongs to the background service and survives closing GPUI or Pill. History disappears when that service exits, not when a frontend closes. Copy, Pin/Unpin, Delete and Clear History work in both frontends. Direct Paste is unavailable until previous-app handoff is verified, so use Copy and Cmd-V in the target app. Service details and frontend previews have bounded text lengths; Copy still uses the stored entry. New captures appear on the next query or when you reopen history.

History stays in memory and disappears when maccer exits, including pins. Capture starts after launch and polls every 300 ms; rapid copies between polls may be missed. The text already on the clipboard at startup is not captured. Duplicate copies move the existing entry to the front. Pins prevent automatic eviction, but Delete and Clear History remove them.

The default limit is 200 entries, configurable with `[clipboard].max_entries` from 1 to 1,000. Text is limited to 64 KiB per entry and 4 MiB total. If pinned entries leave no room, new copies are ignored. History never participates in ordinary app search and clipboard queries/activations are excluded from SQLite usage history. Set `[providers.clipboard].enabled = false` to disable capture.

In embedded mode, Paste returns to the application active before the launcher opened and requires **System Settings > Privacy & Security > Accessibility** permission for maccer. Copy needs no extra permission. If the target cannot be confirmed, no paste keystroke is sent. After a delayed-paste failure, the error appears when the launcher is reopened; the text remains available for manual Cmd-V.

Files, images, and pasteboard content marked concealed, transient, or autogenerated are skipped. **Unmarked passwords and other sensitive text can still enter history.** In-memory storage avoids intentional disk persistence; it is not a password detector or a guarantee against OS memory swapping.

### Shell and custom commands

Type `> printf 'hello world'`, then Return to execute. Normal mixed search never invokes the shell provider, even if its configuration enables `default_search`. Typing or searching a command does not run it. To define named commands, add `[[shell.commands]]` entries with `name`, `command` and optional `keywords`, as shown in the example configuration. Type `>` alone to list them.

Commands run through `/bin/sh -c` in your home directory, not an interactive login shell. Your shell aliases and startup scripts are not loaded. Execution has a 30-second deadline and retains at most 64 KiB of stdout and 64 KiB of stderr. Nonzero exit status appears with the output. Use arrow keys or the wheel to scroll output, Cmd-C to copy it, and Escape to return to results. Return on the output screen does not repeat execution.

Escape while an action is running cancels it. Query changes and launcher dismissal also cancel cooperative actions. Shell cancellation/timeout kills its process group; it cannot undo side effects already performed. These commands run with your user permissions and are not sandboxed. Only configure and execute commands you trust.

Shell command text and activations are excluded from SQLite history. The launcher keeps a bounded output cache in memory for its Copy Last Output action; it disappears on restart. Set `[providers.shell].enabled = false` to disable shell execution.

## Current scope

Included:

- Resident floating launcher window, global hotkey, menu bar controls, and native launch-at-login support.
- Application discovery, native icons, open, and reveal in Finder (via NSWorkspace).
- Fuzzy application/file search, calculator, and configurable web search providers.
- fd-indexed file/folder discovery, live updates, text/image/PDF previews, native file/path copying and external dragging.
- Explicit shell execution, named custom commands, bounded output and cancellation.
- In-memory text clipboard history with search, previews, copy/paste, pins, deletion, and clearing.
- Generic action menu and provider picker.
- SQLite usage history with counts and bounded recency ranking.
- Full text input: cursor movement, selection, clipboard cut/copy/paste, Unicode/grapheme handling, and IME composition, adapted from GPUI's `EntityInputHandler` example.

Not included yet:

- Image/file clipboard history and emoji/symbols.
- Settings GUI.
- Multi-monitor placement and embedded GPUI theme reload.
- Plugin protocol, accessibility review, code signing/notarization, automatic updates.

## Development

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p launcher
```

The whole launcher is one package, `crates/launcher`. Its former `launcher-*` and `provider-*` crates are now modules under `src` (`core`, `protocol`, `client`, `config`, `macos`, `agent`, `service`, `ui`, `providers`). The package builds the `maccer`, `maccer-service` and `maccer-client` binaries plus the `liblauncher_agent.dylib` C ABI consumed by Pill; the header is at `crates/launcher/include/maccer_agent.h`.

`gpui` is patched locally to fix a retained-window reactivation deadlock. See [`docs/gpui-patch.md`](docs/gpui-patch.md). The native regression check is:

```sh
cargo run -p launcher --example retained_window_smoke
```

It performs 20 show/hide cycles and exits nonzero if the main thread stalls. See the [shared-service verification limits](docs/shared-service.md#verification-and-remaining-limits) before treating scoped checks as a clean workspace-wide gate. Physical hotkey/remapper behavior, real IME input, native desktop theme Apply, login-time launch and notarization still require manual acceptance.
