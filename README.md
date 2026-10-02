# maccer

A small, keyboard-first macOS launcher written in Rust and GPUI. The current build searches installed applications, evaluates calculator expressions, and offers configured web search engines. This is the first usable slice of the larger plan in `gpui-macos-launcher-plan.md`, not a complete implementation of that roadmap.

## Run

Requires macOS, Rust 1.85 or newer (edition 2024), and a working Apple SDK. GPUI is pinned to 0.2.2 with runtime shader compilation enabled, so full Xcode is not required merely to compile on machines with Command Line Tools. First build can take a few minutes.

```sh
cargo run -p launcher
```

The launcher starts hidden and stays resident. Press **Option-Space** to toggle it. Use arrows (or Ctrl-N/Ctrl-P) to move, Return to use the default action, Cmd-Return or Cmd-K for the action menu, Tab for the secondary action, Cmd-P or `;` for the provider picker, and Escape to go back, clear the query, or hide the window. Prefixes are configurable; defaults are `/apps`, `=`, and `?`.

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

## Current scope

Included:

- Resident floating launcher window and global hotkey.
- Application discovery, open, and reveal in Finder (via NSWorkspace).
- Fuzzy search, calculator, and configurable web search providers.
- Generic action menu and provider picker.
- SQLite usage history with counts and bounded recency ranking.
- Full text input: cursor movement, selection, clipboard cut/copy/paste, Unicode/grapheme handling, and IME composition, adapted from GPUI's `EntityInputHandler` example.

Not included yet:

- Spotlight file search, clipboard history, shell/custom commands, emoji/symbols.
- Application icon rendering.
- Login item, menu bar item, settings GUI.
- Multi-monitor placement, theme reload, previews.
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
