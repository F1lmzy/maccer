# Implementation status

## Working tree stage

The workspace contains a runnable macOS launcher binary (`cargo run -p launcher`) and its provider, core, UI, and platform crates. This is the initial usable slice of the plan in `gpui-macos-launcher-plan.md`: applications, personal files, calculator, and web search with a generic provider/action foundation. It is not a complete implementation of that roadmap. The plan file remains the source of the larger design.

## Implemented

- Rust 2024 workspace with GPUI pinned to 0.2.2 and `runtime_shaders` enabled. This avoids a build-time dependency on Apple's `metal` shader compiler.
- Core provider registry, prefix parser, generation-tagged concurrent search, fuzzy ranking, stable selection, and SQLite activation history.
- Ranking uses nucleo fuzzy matching, provider priority, per-item activation counts, and a bounded recency bonus. No adaptive learning beyond that.
- Application bundle discovery and NSWorkspace launch/reveal/open URL operations centralized in `launcher-macos`.
- Applications, files, calculator, and web-search providers behind the same generic `Provider`/`Item`/`Action` contracts.
- fd-based background file/folder indexing with a rebuildable SQLite cache and cached full-path fuzzy queries. Regex `ignored_dirs`, configurable roots/exclusions, debounced live filesystem updates, recent-file browsing, and launcher usage ranking before truncation. The application no longer uses zoxide/fzf or per-query Spotlight discovery.
- Lazy, cancellable UTF-8 text/image/first-page PDF previews with a toggleable scrollable side panel. Text and thumbnail generation are bounded, with preview exclusions for sensitive directories.
- File/folder Open, Open Containing Folder, Reveal in Finder, Quick Look, Copy Path, native file-URL Copy File, and Refresh File Index actions. Result rows initiate copy-only AppKit drag sessions to other programs. LocalSend is intentionally excluded.
- Default file and overall result budgets are 50 with a six-row virtualized viewport. Open queries refresh on provider revision changes while preserving selected identity.
- Explicit-only shell provider (`>`), named TOML custom commands, bounded process execution and output, copy-command/output actions, and an output screen with keyboard/wheel scrolling and clipboard copying. Shell commands opt out of persisted usage.
- Generic cooperative action cancellation, wired through query edits, dismissal and Escape. Shell actions use it to kill their process group; ordinary native actions keep their existing behavior.
- GPUI launcher shell with a retained pop-up window, keyboard navigation and action menu, provider picker, focus-loss dismissal, virtualized result list with selection scrolling, and centered window resizing. Compact monochrome styling with a dark-gray background and rounded corners, a highlighted border, high-contrast selection, and explicit title/type line heights to prevent row clipping.
- Text input with cursor movement, selection, clipboard cut/copy/paste, Unicode/grapheme-aware boundaries, and IME composition, adapted from GPUI's `EntityInputHandler` example.
- TOML config with partial nested overrides and defaults. Config defaults to `~/.config/maccer/config.toml`; SQLite usage history defaults to `~/Library/Application Support/maccer/history.sqlite3` on macOS.
- `--check` diagnostics and `--show` development flag.
- Local GPUI patch for a retained-window reactivation deadlock (see `docs/gpui-patch.md`).

## Deferred

Clipboard history, emoji/symbols provider, open-windows provider, application icon decoding/rendering, settings GUI, login item, menu bar item, multi-monitor placement, theme reload, plugin protocol, accessibility review, code signing/notarization, and automatic updates.

## Verification boundary

Portable unit tests cover parsing, ranking, stable selection, history, provider calculations/URL construction, application-discovery fixtures, config validation, UI state transitions, and text-range conversion.

File tests cover real fd discovery of files/folders, full-path matching, NUL/newline/Unicode names, regex exclusions, rename/delete refresh, live watcher updates, preview bounds, and actual macOS PNG/PDF thumbnail generation. Native file URLs round-trip through a private macOS pasteboard without altering the user's clipboard. GPUI tests verify stale-preview rejection, preview scrolling, drag initiation threshold/one-shot behavior, and wheel/keyboard scrolling beyond six rows. Large candidate-history queries are chunked to avoid SQLite bind limits. Legacy Spotlight/zoxide/fzf unit tests remain for the unused backend utilities. Actual shell subprocess tests cover inert search, custom commands, stderr/nonzero exit status, retained-output bounds and cancellation. GPUI tests cover output scrolling/copying, Escape/query cancellation and preventing Enter from repeating a command while viewing output.

Native macOS verification performed: repeated retained-window show/hide cycles (`cargo run -p launcher-ui --example retained_window_smoke`, 5 runs x 20 cycles, exit 0) and a single `--show` startup sample with no errors. See `docs/runtime-verification.md`.

Not verified: OS-level keyboard/IME events in the launcher, actual NSWorkspace launch/reveal against real applications, global hotkey registration/conflict behavior under a live user session, multi-monitor placement, idle CPU over time, and all performance goals in the plan. Older Spotlight timing samples describe the previous backend, not the fd index. Physical drag-and-drop into Finder/other programs and visual preview layout on a real display still need user-session verification. Cached fd-query diagnostics are provided by `cargo run -p provider-files --example search_smoke -- crates /absolute/root`. Run `cargo test --workspace` and `cargo build -p launcher`, then manually test the global shortcut, app launch, Escape/focus behavior, and calculator/web actions before treating the MVP as verified.
