# Spotlight app-loading investigation

## Target and scope

Inspected the installed `/System/Library/CoreServices/Spotlight.app/Contents/MacOS/Spotlight` with REA and Hopper on macOS 26.4, build 25E246. SHA-256: `fe77a46c91bde72132ce4fe9330779dd68b0becc18fd2155ca28e64be021e567`.

REA/Hopper labels the selected architecture arm64. REA's Mach-O header inspection identifies the shipped ARM slice as arm64e. Addresses below refer to Hopper's selected slice. This was static analysis, not observation of a live search. Hopper does not expose complete function body ranges and some Swift indirect dispatch remains unresolved.

Full retained evidence was exported to `/tmp/maccer-spotlight-rea-evidence-final.json`. That local temporary file is not committed and should be copied to durable storage if needed.

## Observations

| Finding | Location | REA evidence ID |
| --- | --- | --- |
| App browsing obtains `SSFilterResult.applications` and passes it to another routine. | `SearchViewController goToAppsSearchWithResetQuery:`, `0x1000bfa64` | `ev_827bd55bfbfa5bf3e63237edae39f318c893b0b77f7aa137b63ec3c43041b760` |
| A SearchQueryController routine gets `clientSession`, calls `queryTaskWithContext:`, sets the task queue and delegate, calls `start`, and stores the task. | `sub_1000aacb8`, query-task call at `0x1000ab0a4` | `ev_dfac352b7f92a460108b6345e136cd412ed6aa55c0bd01ccdc0239e7e31459d0` |
| The Objective-C query-response entry point calls a routine that schedules processing asynchronously on its serial queue. | `processQueryResponse:`, `0x1000ac3bc`, and `sub_1000ab2b0` | `ev_00aafc99bcc13bcd8f5d2078c082551ff9a4283fd7021b77fdd033a069a573b4`, `ev_f15f1e4fc21da5146c8b1982676c1259d7563464752c9fe3b4468f65af0467fd` |
| A result classification routine compares content type with `com.apple.application-bundle`, also checking folder type and a numeric result type. This is classification, not an enumeration predicate. | `sub_10001c4b8` | `ev_8a456bbb6f939630ea8eafac75e94c48e1cc06f19dedd108e23caa3ec7f02f67` |
| A subitems double-click handler uses NSWorkspace. Its application branch opens a selected file with the represented app; its other branch uses `openURL:`. | `SPResultWithSubitemsHelper doubleClick:`, `0x100008168` | `ev_b81d009e93dc229d6e85a0fce48e150af2580180bab6c0412b9ad968a5434876` |

Spotlight imports `SpotlightUIShared` query/controller interfaces, including `QueryController.start`, cancellation, query representations, and query-response types. Evidence: `ev_96fd56331d3aca4606bd702d4f2dfeb359a0982aa7a9f4c9977d5cf302453c2d`.

## Interpretation and limits

The observed frontend delegates query work to a client session and handles responses asynchronously. It does not establish how the service enumerates installed apps. It also does not establish that every search takes the same path. The executable contains an `isNewQueryArchitectureEnabled` branch, evidenced by `ev_96e904de63de656fd0f890cc1603a742838fd75b2bcc0e35c5184d60a989f499`.

Not recovered: backend app enumeration, ranking, freshness notifications, icon caching, the primary app-result launch action, or entitlement requirements for calling the private service from Maccer. The subitems handler above is not proof of the primary app-launch path. No assertion of matching Spotlight latency or ranking is justified by this investigation.

## Maccer comparison

- `crates/launcher-macos/src/applications.rs` scans standard application roots and explicitly includes Finder. It reads bundle plists and deduplicates paths and bundle IDs.
- `crates/provider-apps/src/lib.rs` builds an immutable searchable index, fuzzy-matches in memory, and returns lazy app-icon descriptors. It validates activation paths against that index.
- `crates/launcher-macos/src/platform.rs` already opens app bundle URLs through public `NSWorkspace.openURL` on the main thread.
- `crates/launcher-macos/src/spotlight.rs` already uses bounded, cancellable `mdfind` processes for file queries, but app discovery does not use that backend.

## Proposed implementation, awaiting choice

Recommended supported-API route:

1. Discover application bundles from the Spotlight metadata index, validate returned absolute paths and app metadata, and merge with the existing scan as a fallback for unindexed apps. Preserve Finder and deterministic deduplication. This is an equivalent discovery source, not a recovered implementation of Apple's private service.
2. Keep fuzzy matching and lazy icon loading off the discovery path. Do not spawn `mdfind` for every app-search keystroke. Define catalog refresh behavior separately if live install/uninstall updates are required.
3. Test malformed and stale results, paths containing spaces or newlines, duplicate bundles, missing or disabled indexes, and fallback behavior. Run the existing provider tests and app-search benchmark.

Alternative: continue tracing Apple's private query service through its framework implementation and check whether a third-party process can use it. Availability and entitlements are unknown, so this route should not become the default without an explicit choice.

No Rust behavior was changed during this investigation.
