# Vicinae app loading and search

## Evidence boundary

The installed `/Applications/Vicinae.app` reports version 0.29.1, build 2593. REA identified `/Applications/Vicinae.app/Contents/MacOS/Vicinae` as arm64 Mach-O with SHA-256 `ec592f97091e3af4432e12559066cd312a4c827e0fe157d5b2a9f517c3948c62`.

Target-free REA CLI Mach-O inspection succeeded. Its evidence is `ev_7b74b031b44257bb96890a0c7817c2fb3eae82b3e6f09c95b2c669f9528508b5`, saved locally in `/tmp/maccer-vicinae-macho.json`. It reports imports for NSBundle, NSWorkspace, NSWorkspaceOpenConfiguration, QFileSystemWatcher, and C++ recursive-directory iteration. Imports corroborate available mechanisms, not their call order. Import/export coverage is explicitly non-exhaustive.

Hopper procedure analysis timed out and then returned `unreadable_output`, including on its permitted retry. Doctor reports Hopper available, but that does not establish a working analysis bridge. Closing the session returned `cleanup_incomplete` with unconfirmed Hopper document shutdown and retained private directory `/tmp/rea-61LREJ`. No files were removed or user Hopper process terminated. No native pseudocode or runtime timing was recovered for Vicinae.

The behavioral findings below come from upstream source pinned to release tag v0.29.1, commit `7f1d1d96ea7ca9b08130962c91d0469df24a1362`. Matching release versions do not prove byte-for-byte correspondence between that source and the installed executable.

## App discovery

[mac-app-database.mm](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/services/app-service/macos/mac-app-database.mm)

`MacAppDatabase` calls `scan()` in its constructor. It scans these default roots:

- `/Applications`
- `/System/Applications`
- `/System/Library/CoreServices/Applications`
- `$HOME/Applications`, when HOME exists

Configured extra roots precede defaults, so their app IDs win conflicts. It uses `std::filesystem::recursive_directory_iterator`, follows directory symlinks and skips permission-denied entries. It visits root entries and one directory level below them, disabling deeper ordinary traversal. When it finds an `.app`, it records that bundle and stops ordinary recursion into its contents.

It makes a deliberate exception for immediate `.app` entries in each bundle's `Contents/Applications`. It also explicitly adds `/System/Library/CoreServices/Finder.app`, including Finder's embedded applications through the same helper. This is not unrestricted traversal of internal helpers.

`scan()` clears its app vector and ID map, builds `MacApplication` records, and skips duplicate IDs. This app-discovery path does not call Spotlight metadata queries or enumerate all apps through Launch Services. NSWorkspace is used elsewhere for opening apps and finding file/URL handlers.

## Metadata and identity

[mac-app.mm](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/services/app-service/macos/mac-app.mm)

`MacApplication::fromBundle` uses `NSBundle`. Identity is the bundle identifier when present. Without one, it requires an executable bundle file and a successfully canonicalized path, then uses `macos:path:<canonical-path>`.

Display-name order is Finder's `NSFileManager.displayNameAtPath`, localized display name, raw display name, localized bundle name, raw bundle name, then filename without `.app`. Finder-name processing removes the app extension and soft hyphens. A differing raw bundle name is retained as an unlocalized search title. The record also holds bundle path and executable name.

## Refresh and index construction

[app-service.cpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/services/app-service/app-service.cpp)

`AppService` installs QFileSystemWatcher directory watches for existing search roots. Directory changes and database preference changes restart a single-shot 500 ms timer. Its timeout calls `scanSync()`, which updates watches, scans the database and emits `appsChanged`.

Only search roots are registered here. This does not prove detection of every metadata change deep inside a bundle or every nested-folder change.

[app-root-provider.cpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/root-search/apps/app-root-provider.cpp)

`AppRootProvider` forwards `appsChanged` as `itemsChanged`. Its `loadItems()` wraps displayable applications as `AppRootItem` records.

[root-item-manager.cpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/services/root-item-manager/root-item-manager.cpp)

`RootItemManager::loadProvider` connects provider item changes to `updateIndex()`. Index construction prebuilds title, unlocalized title, subtitle, keywords and metadata references, and restores visit counts and last-visited times. Keystroke searches iterate these records rather than scanning application directories or decoding icons.

## Matching and ranking

[fzf.hpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/lib/fuzzy/include/fuzzy/fzf.hpp)

The matcher is an in-process C++ implementation of FZF V2, not a spawned `fzf` command. A query is prepared once, split into words and expanded into supported transliteration variants. A thread-local matcher reuses its working vectors. Matching scores subsequences with word-boundary, camel-case and consecutive-character bonuses, and gap penalties. Non-ASCII strings pass through folding.

The root item manager supplies these field weights:

| Field | Weight |
| --- | ---: |
| Display title | 1.0 |
| Unlocalized title | 1.0 |
| User alias | 1.0 |
| Subtitle | 0.5 |
| Keywords | 0.6 |

Each query word can match a different field. Ranking uses normalized weighted scores, while filtering uses the weakest word's unweighted coherent-match quality. Items below quality 60 are rejected. Accepted nonempty queries get up to 6 additional points from frequency and recency. Empty queries score `94 + 6 * frecency`.

[fuzzy-searchable.hpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/lib/fuzzy/include/fuzzy/fuzzy-searchable.hpp)

Frecency combines logarithmic visit count with an exponential recency term and clamps to a 0–1 range. The code calls its decay parameter `RECENCY_HALF_LIFE_DAYS`, but the formula is `exp(-days / 30)`, so 30 days is an exponential decay time constant rather than a mathematical half-life.

Search filters disabled items, provider scope and favorites according to options, then stable-sorts descending score. With `prioritizeAliased`, aliases starting with the raw query rank first, with shorter matching aliases preferred.

Important macOS detail: [MacApplication](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/services/app-service/macos/mac-app.hpp) does not override the abstract application's empty keyword list. Its root subtitle is also empty. Ordinary macOS app matching therefore uses display name, unlocalized name and user alias, not automatically bundle ID, path or executable. Maccer currently searches those extra fields.

## Icons and launch

`MacApplication::iconUrl()` canonicalizes and caches the bundle path and returns `ImageURL::macBundle`. It does not render icon bytes during fuzzy search.

[image-stream.cpp](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/ui/image/image-stream.cpp)

ImageStream checks a rendered-image cache first. Its MacBundle case dispatches rendering through QtConcurrent and the decoding pool. Rendered results are cached by URL, dimensions and rendering options. The image cache has a 64 MiB cost budget; the latest-frame cache holds 256 entries, and a separate fetched-byte cache has a 32 MiB budget.

[mac-file-icon-loader.mm](https://github.com/vicinaehq/vicinae/blob/7f1d1d96ea7ca9b08130962c91d0469df24a1362/src/server/src/ui/image/mac-file-icon-loader.mm)

The worker calls `NSWorkspace.iconForFile`, then draws the NSImage directly into a requested-size RGBA bitmap and copies it into QImage. This avoids a TIFF export followed by image decoding.

`MacAppDatabase::launch` checks that the bundle path exists, creates an NSWorkspaceOpenConfiguration and calls `openApplicationAtURL:configuration:completionHandler:`. With arguments, it converts them to file/other URLs and uses `openURLs:withApplicationAtURL:configuration:completionHandler:`. Completion handlers are nil, so the immediate boolean return is not confirmation of successful asynchronous launch.

## Implications for Maccer

Maccer already follows the central design: startup directory discovery, prebuilt in-memory fuzzy records and lazy background icon loading. Adopting Spotlight is not required to reproduce Vicinae's app-loading architecture.

Differences worth considering separately are localized Finder names and unlocalized aliases, bounded scan depth, the additional CoreServices application root and deliberate embedded-app discovery, configurable root precedence, debounced catalog refresh, field-specific matching with a quality threshold, and direct requested-size native icon rendering. These are source-derived recommendations, not performance measurements or an instruction to copy Vicinae's GPL-3.0 implementation into this project.

No Rust code or existing project files were changed by this investigation.
