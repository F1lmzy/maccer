# Vicinae-style application catalog

## Scope

Adopt the directory-discovery, debounced-catalog-refresh and prebuilt-search-record architecture described in `vicinae-app-investigation.md`. Implement independently in Rust without copying Vicinae's GPL code. Keep Nucleo matching, Maccer's exact-name priority and existing history ranking. Keep lazy icons and current NSWorkspace launch integration; this change does not claim identical FZF scores or launch semantics.

## Acceptance criteria

- Scan standard macOS roots plus explicitly configured app roots, with earlier configured roots winning duplicate bundle IDs. Scan one subfolder level and deliberate `Contents/Applications` companions, not arbitrary internal helpers.
- Resolve Finder-localized names on macOS and keep a differing raw bundle name searchable. Preserve path, bundle-ID and executable matching.
- Refresh prebuilt provider records on a background worker after 500 ms of filesystem quiet. Publish snapshots atomically with provider revisions; queries never scan the filesystem or decode icons. Shutdown must stop and join the worker. Watch failures must be reported.
- Removed or replaced catalog entries cannot be activated from stale results. Unchanged scans must not increment revisions or restart UI search.
- Existing config and static-provider examples remain compatible. New `[apps]` config accepts extra `paths` and `watch`, with watching enabled by default.

## Implementation and verification

1. `launcher-macos/src/applications.rs`: bounded scan, root precedence, localized aliases and fixture tests.
2. `launcher-macos/src/application_watcher.rs` and `provider-apps/src/lib.rs`: bounded debounced worker and immutable index publication. Fixture tests for add/remove/rename, stale activation, unchanged revision and shutdown.
3. `launcher/src/config.rs`, `launcher/src/main.rs`, config example: configure extra roots and enable live provider. UI already polls provider revisions.
4. Run `cargo test -p launcher-macos --lib`, `cargo test -p provider-apps`, `cargo test -p launcher --bin maccer`, `cargo check -p launcher`, affected-crate clippy and format checks, plus the installed-app smoke/benchmark examples.

Always preserve unrelated uncommitted work and existing vendor files. No new dependencies, private Apple services, UI redesign or license change.

## Implemented behavior

- Standard roots now include CoreServices/Applications and Finder. Scans visit root entries and one subfolder level, plus immediate companions under Contents/Applications. Earlier roots win duplicates before presentation sorting.
- On macOS, NSFileManager supplies Finder-localized names. Raw plist names are retained as search aliases when different. This does not reproduce every NSBundle localized-plist fallback used upstream.
- notify watches existing roots recursively, which also detects bundle metadata changes. Missing roots use a nonrecursive nearest-existing-ancestor watch. This is broader than Vicinae's root-only watch policy.
- A bounded event channel coalesces changes. The worker waits for 500 ms of quiet, capped at two seconds per burst. Failed watches retry without requiring a filesystem event. Failed batched commits track uncertain paths and force repair.
- Searches hold an Arc to an immutable index snapshot. Refresh compares metadata before rebuilding and increments the provider revision only on changes. The existing UI polls revisions every 500 ms. Thus quiet-to-visible timing includes notification latency, debounce, scan/index time and the UI poll.
- Activation validates the current item ID and path. Removed, moved or replaced catalog entries reject stale actions. The existing launch method is unchanged. A subsequent icon-throughput improvement adds direct pixels and bounded visible-first loading; see [application search performance](application-search.md).
- Discovery has a 30,000-entry budget. Exhaustion returns an error rather than publishing a partial catalog. Initial failure stops startup visibly; background failure retains the last successful snapshot. Missing/unreadable roots retain the prior scanner's skip behavior.

Configuration example:

```toml
[apps]
paths = ["/opt/apps", "~/Applications/Setapp"]
watch = true
```

Extra paths precede built-in roots. Paths must be absolute or start with ~/ and cannot contain parent traversal or control characters. At most 64 extra roots are accepted. Setting watch=false uses one startup scan. Changes to this configuration require restarting Maccer.

## Verification results

On the development macOS host:

- `cargo test -p launcher-macos --lib`: 84 passed.
- `cargo test -p provider-apps`: 10 unit and 2 live-catalog integration tests passed.
- `cargo test -p launcher --bin maccer`: 26 passed.
- Affected-crate `cargo clippy --all-targets -- -D warnings`, formatting checks and `git diff --check` passed.
- `maccer --check` discovered 112 installed apps. The mixed-search smoke test ranked Safari first.
- Debug app-search benchmark, static snapshots: discovery 172–184 ms, index construction 0.44–0.47 ms. Safari cold search was 0.52 ms, warm p95 0.38 ms. Broad `a` cold search was 0.38 ms, warm p95 0.27 ms. Coordinator completion p95 was about 3.5 ms for both queries. These measurements are not UI paint or icon timings, and not a benchmark against Vicinae.

Regression coverage includes shallow scans, companions, root precedence, aliases, budget exhaustion, initial snapshots, add/remove/rename/metadata updates, unchanged revisions, stale activation, watch-mode changes, initial watch failures, failed/partially applied watch commits, event-free timer recovery and worker shutdown. An independent final review approved the corrected implementation.

The actual GUI/hotkey startup path was compiled, not visually exercised in this task. Live provider tests use the real macOS notification backend with generous deadlines. Existing nom 1.2.4 emits a dependency future-incompatibility warning, unrelated to this change.
