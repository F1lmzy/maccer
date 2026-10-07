# maccer

A small, keyboard-first macOS launcher written in Rust and GPUI. The current build searches installed applications and personal files, evaluates calculator expressions, offers configured web search engines, and runs explicitly selected shell commands. This is the first usable slice of the larger plan in `gpui-macos-launcher-plan.md`, not a complete implementation of that roadmap.

## Run

Requires macOS, Rust 1.85 or newer (edition 2024), a working Apple SDK, and `fd` for the files provider. Install `fd` with `brew install fd`, or set `[providers.files].enabled = false`. GPUI is pinned to 0.2.2 with runtime shader compilation enabled, so full Xcode is not required merely to compile on machines with Command Line Tools. First build can take a few minutes.

```sh
cargo run -p launcher
```

The launcher starts hidden and stays resident. Press **Option-Space** to toggle it. Use arrows (or Ctrl-N/Ctrl-P) to move, Return to use the default action, Cmd-Return or Cmd-K for the action menu, Tab for the secondary action, Cmd-P or `;` for the provider picker, and Escape to go back, clear the query, or hide the window. Prefixes are configurable; defaults are `/apps`, `/` for files, `=`, `?`, and `>` for shell commands. If Karabiner remaps your left Option key, use right Option-Space.

While the launcher window is focused, **Cmd-Q** quits. There is no menu bar item yet, so there is no global quit affordance; if the window is hidden, use `kill <pid>` or re-open it with Option-Space first.

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

## Configuration

Configuration is read from `~/.config/maccer/config.toml`. Start from [`config/config.example.toml`](config/config.example.toml). Partial files inherit defaults, so a file with only `[launcher]` keeps the default providers. Provider tables merge field by field: `[providers.calculator] priority = 123` keeps the calculator prefix, `default_search`, and result limit. Web search engines default to Google and DuckDuckGo; define `[[web.engines]]` tables to replace that list, and set `[web].default_engine` to one of the configured ids.

Usage history is stored at `~/Library/Application Support/maccer/history.sqlite3`. maccer records an activation only after the action succeeds; each record includes the query text, the provider and item identifiers, the action, and a timestamp. Ranking uses per-item activation counts plus a bounded recency bonus. The database never leaves the machine; delete the file to clear history. Set `RUST_LOG=debug` for diagnostics. Hotkey conflicts are reported to stderr; change `[launcher].hotkey` and rerun.

### Application search and icons

Application metadata and fuzzy-match strings are indexed once at startup. Each query matches in memory and clones only the ranked results; it does no filesystem access or native icon extraction.

Results carry `IconDescriptor::ApplicationBundle` paths. The GUI displays results immediately with placeholders, then resolves Finder icons on a single background worker, choosing the next icon from the current ranked results after each load. Images and failed lookups are cached until restart. PNGs are at most 64 pixels per side and 256 KiB; asset-catalog icons and `.icns` resources are supported. Icon loading never delays search completion.

See [application-search measurements and upstream comparisons](docs/application-search.md). To measure discovery, cold/warm matching, and coordinator result delivery without opening a window:

```sh
cargo run -p launcher --example app_search_bench -- safari
cargo run -p launcher --example app_search_bench -- a
```

### File search

The files provider follows [Elephant Files](https://github.com/abenz1267/elephant/tree/master/internal/providers/files), using `fd` to discover files and folders in the background. It searches full paths, so project/directory names work as well as filenames. Files appear only with the `/` prefix; ordinary searches and the empty launcher view do not include files, even if an older configuration enables `default_search` for files. Type `/` alone to browse recent files. `/ ~/Documents rpt pdf` narrows a query to an existing indexed directory. Configured roots default to your home directory.

`fd` follows its `.gitignore`, `.ignore` and `.fdignore` rules and skips hidden entries. Configure `[files].search_dirs`, regex `ignored_dirs`, or literal `exclude_paths` to control the index. `roots` remains an alias for `search_dirs`. There are no hardcoded personal-folder boosts. zoxide/fzf settings from older configs are accepted but no longer used. Full-path fuzzy matching, change times and launcher activation history determine ranking. Like Elephant, browsing considers up to 100 recent files and searches use a 1,000-candidate budget per root, preferring literal path terms before fuzzy-only matches.

Live filesystem watching defaults on. Changes trigger a debounced rescan and publish a complete refreshed snapshot without blocking cached searches; an open results list refreshes automatically. A full home-directory refresh can take seconds, so use narrower roots or disable watching if background scans are too costly. Set `watch = false` to index only at startup, then use the Refresh File Index action. The rebuildable path/metadata cache is `~/Library/Caches/maccer/files.sqlite3`, separate from usage history. Internal maccer database folders are excluded to prevent update loops. Index scans have a 60-second subprocess deadline and a 64 MiB output cap; narrow your roots if this limit is reached.

Selected files show inline UTF-8 text, image or first-page PDF previews. Cmd-Shift-P toggles the preview panel. Text is limited to 64 KiB. Images up to 64 MiB and 100 megapixels use in-process ImageIO on background workers, producing thumbnails up to 800 pixels per side; PDFs use Quick Look thumbnails. Both reuse a 16-entry/16 MiB in-memory PNG cache, invalidated when the file changes. No preview contents are cached on disk. Binary/unsupported files show a message and retain the Quick Look action. Configure `ignore_previews` with absolute or `~/` directory paths to prevent content previews in sensitive folders. Previews run off the UI thread and cancel on selection/query changes.

Open files/folders with Return or a double click. The action menu includes Open Containing Folder, Reveal in Finder, Quick Look, Copy Path, Copy File and Refresh File Index. Copy File writes a native file URL that Finder and other apps can paste. Drag a result row into another program to start a native copy-only file drag. LocalSend is not included.

The compact, Walker-inspired GUI uses a dark-gray background, rounded corners, a highlighted border and a white selection row. It defaults to 480 points wide with six visible rows; the optional preview adds 280 points. The full window stays centered as its size changes. Override `[launcher].width` to change the base width.

The default keeps 50 results with six visible rows. Scroll with the trackpad/mouse wheel or use arrow keys to reach later results. Existing configurations setting either `[launcher].max_results` or `[providers.files].max_results` to 8 still cap results at eight; raise both to 50.

### Shell and custom commands

Type `> printf 'hello world'`, then Return to execute. Normal mixed search never invokes the shell provider, even if its configuration enables `default_search`. Typing or searching a command does not run it. To define named commands, add `[[shell.commands]]` entries with `name`, `command` and optional `keywords`, as shown in the example configuration. Type `>` alone to list them.

Commands run through `/bin/sh -c` in your home directory, not an interactive login shell. Your shell aliases and startup scripts are not loaded. Execution has a 30-second deadline and retains at most 64 KiB of stdout and 64 KiB of stderr. Nonzero exit status appears with the output. Use arrow keys or the wheel to scroll output, Cmd-C to copy it, and Escape to return to results. Return on the output screen does not repeat execution.

Escape while an action is running cancels it. Query changes and launcher dismissal also cancel cooperative actions. Shell cancellation/timeout kills its process group; it cannot undo side effects already performed. These commands run with your user permissions and are not sandboxed. Only configure and execute commands you trust.

Shell command text and activations are excluded from SQLite history. The launcher keeps a bounded output cache in memory for its Copy Last Output action; it disappears on restart. Set `[providers.shell].enabled = false` to disable shell execution.

## Current scope

Included:

- Resident floating launcher window and global hotkey.
- Application discovery, native icons, open, and reveal in Finder (via NSWorkspace).
- Fuzzy application/file search, calculator, and configurable web search providers.
- fd-indexed file/folder discovery, live updates, text/image/PDF previews, native file/path copying and external dragging.
- Explicit shell execution, named custom commands, bounded output and cancellation.
- Generic action menu and provider picker.
- SQLite usage history with counts and bounded recency ranking.
- Full text input: cursor movement, selection, clipboard cut/copy/paste, Unicode/grapheme handling, and IME composition, adapted from GPUI's `EntityInputHandler` example.

Not included yet:

- Clipboard history and emoji/symbols.
- Login item, menu bar item, settings GUI.
- Multi-monitor placement and theme reload.
- Plugin protocol, accessibility review, code signing/notarization, automatic updates.

## Development

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p launcher
```

`gpui` is patched locally to fix a retained-window reactivation deadlock. See [`docs/gpui-patch.md`](docs/gpui-patch.md). The native regression check is:

```sh
cargo run -p launcher-ui --example retained_window_smoke
```

It performs 20 show/hide cycles and exits nonzero if the main thread stalls. See [`docs/runtime-verification.md`](docs/runtime-verification.md) for what has and has not been verified on real macOS.
