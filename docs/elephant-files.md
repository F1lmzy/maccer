# Files provider: Elephant-style behavior

## Objective
Replace per-query Spotlight/zoxide heuristics with an `fd`-discovered index of files and folders. The user requested text/image/PDF previews, opening files/folders, native drag-and-drop to other apps, copying files/paths, and `ignored_dirs`. LocalSend is excluded.

Reference inspected: Elephant `internal/providers/files` at commit `a35b3b54ae8bfddaa3bae82d1409cf2d6dfebf3b`.

## Design and boundaries
- `launcher-macos`: direct, NUL-delimited `fd` invocation; background indexing and filesystem change notifications; bounded preview reads/thumbnail generation; AppKit file pasteboard and drag sessions.
- `provider-files`: full-path fuzzy matching, recent-file browsing on an empty query, stable path identities, generic preview/drag contracts and file actions.
- `launcher-core`: platform-independent preview and drag capabilities, provider revisions.
- `launcher-ui`: asynchronous selected-item preview panel with stale-result cancellation and native drag initiation via the provider. It must not import macOS services.
- Configuration retains `roots`/`exclude_paths` compatibility and adds Elephant-style `search_dirs`/regex `ignored_dirs`, `watch`, and preview exclusions. `fd` follows its ignore rules; no extra hardcoded personal-folder ranking is applied to this backend. zoxide/fzf are not required or used by it.
- The index is a private, rebuildable SQLite cache, not usage history. Publish complete snapshots atomically; keep the last good snapshot on a scan failure. Refresh on debounced filesystem changes when watching is enabled. These are complete fd rescans, not incremental per-file SQL updates, so large roots can take seconds to refresh.
- Cache normalized paths at indexing time. Like Elephant, empty browsing uses up to 100 recent files and search uses up to 1,000 candidates per root, preferring literal path terms before fuzzy-only candidates. Background change stamps include Unix change time so renames and metadata changes invalidate the selected preview without any filesystem reads in UI rendering.
- Native dragging advertises copy only. It must never move or delete the source file. Previews never execute file content or fetch remote URLs.

## Implementation order
1. Test and integrate `fd` indexing, full-path/folder search, empty-query browse, ignored directories, and refresh.
2. Add generic preview contracts and bounded text/image/PDF previews.
3. Add native file copying/drag sessions and the GPUI preview panel.
4. Verify the workspace, exercise real `fd` and thumbnails, and document remaining physical-interaction checks.

## Commands and tests
```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p launcher
target/debug/maccer --config config/config.example.toml --check
```

Regression tests live next to each Rust implementation, use temporary filesystem fixtures and real `fd` when installed, and retain the existing UI scrolling/cancellation tests. New cases cover files and folders, full-path queries, browsing, regex exclusions, rename/delete refresh, bounded/binary text previews, PDF/image thumbnails, stale preview rejection, and clipboard/drag identity validation. Use Rust 2024, `Result` for boundary errors, and `cargo fmt` formatting. Preserve unrelated shell work, the retained-window GPUI patch, and the original roadmap.
