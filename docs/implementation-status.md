# Implementation status

## Working tree stage

The workspace contains a runnable macOS launcher binary (`cargo run -p launcher`) and its provider, core, UI, and platform crates. This is the initial usable slice of the plan in `gpui-macos-launcher-plan.md`: applications, calculator, and web search with a generic provider/action foundation. It is not a complete implementation of that roadmap. The plan file remains the source of the larger design.

## Implemented

- Rust 2024 workspace with GPUI pinned to 0.2.2 and `runtime_shaders` enabled. This avoids a build-time dependency on Apple's `metal` shader compiler.
- Core provider registry, prefix parser, generation-tagged concurrent search, fuzzy ranking, stable selection, and SQLite activation history.
- Ranking uses nucleo fuzzy matching, provider priority, per-item activation counts, and a bounded recency bonus. No adaptive learning beyond that.
- Application bundle discovery and NSWorkspace launch/reveal/open URL operations centralized in `launcher-macos`.
- Applications, calculator, and web-search providers behind the same generic `Provider`/`Item`/`Action` contracts.
- GPUI launcher shell with a retained pop-up window, keyboard navigation and action menu, provider picker, focus-loss dismissal, virtualized result list with selection scrolling, and window resizing.
- Text input with cursor movement, selection, clipboard cut/copy/paste, Unicode/grapheme-aware boundaries, and IME composition, adapted from GPUI's `EntityInputHandler` example.
- TOML config with partial nested overrides and defaults. Config defaults to `~/.config/maccer/config.toml`; SQLite usage history defaults to `~/Library/Application Support/maccer/history.sqlite3` on macOS.
- `--check` diagnostics and `--show` development flag.
- Local GPUI patch for a retained-window reactivation deadlock (see `docs/gpui-patch.md`).

## Deferred

Spotlight file provider, clipboard history, shell/custom commands, emoji/symbols provider, open-windows provider, application icon decoding/rendering, previews, settings GUI, login item, menu bar item, multi-monitor placement, theme reload, plugin protocol, accessibility review, code signing/notarization, and automatic updates.

## Verification boundary

Portable unit tests cover parsing, ranking, stable selection, history, provider calculations/URL construction, application-discovery fixtures, config validation, UI state transitions, and text-range conversion.

Native macOS verification performed: repeated retained-window show/hide cycles (`cargo run -p launcher-ui --example retained_window_smoke`, 5 runs x 20 cycles, exit 0) and a single `--show` startup sample with no errors. See `docs/runtime-verification.md`.

Not verified: OS-level keyboard/IME events in the launcher, actual NSWorkspace launch/reveal against real applications, global hotkey registration/conflict behavior under a live user session, multi-monitor placement, idle CPU over time, and all performance goals in the plan. No performance target has been measured. Run `cargo test --workspace` and `cargo build -p launcher`, then manually test the global shortcut, app launch, Escape/focus behavior, and calculator/web actions before treating the MVP as verified.
