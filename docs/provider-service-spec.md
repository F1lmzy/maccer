# Maccer provider service architecture

The experimental shared backend is implemented for Applications, Web Search, text Clipboard and Themes. GPUI and Pill/QML are removable native frontends. Use the [setup and operations guide](shared-service.md) for commands, current verification and remaining limits. Files, calculator and shell remain embedded-only; independent installation and retirement of embedded ownership are unfinished.

## Ownership and navigation

One per-user backend owns the provider registry, application catalogs, usage history, clipboard capture, theme state/catalog caches, confirmed theme execution and configured global shortcut. It runs a Cocoa event loop without linking GPUI or Qt. Frontends own windows, rendering, input and interactions requiring a window or mouse event. Exiting a frontend never stops the backend.

Home is a searchable backend view. Empty home queries list enabled providers. Typed queries search those menu entries and eligible provider results. Selecting a provider navigates to its scope, including empty-query browsing when supported. Sensitive provider contents never participate in mixed home search. Routes, not frontend-invented prefix strings, represent navigation.

Each connection has an independent session, search coordinator, view revisions and action-reference table. Search cancellation is session-local. Provider caches and mutations are shared. Query changes and frontend disconnects cannot cancel accepted side effects.

## Public contract

The `protocol` module defines Rust DTOs, the [JSON Schema](../crates/launcher/schema/protocol.schema.json) and fixtures. Public DTOs contain no GPUI/Qt types, frontend image URLs, executable commands or provider-private payloads. Providers project safe display data through service presentation adapters; internal `Item.payload` never crosses IPC.

Public objects include provider descriptors, routes, snapshots, result items, content blocks, action descriptors, asset references and structured errors. Titles remain readable. Content blocks provide fallback text for unknown types. Unknown domain records retain their discriminator/schema version but discard unrecognized fields. This is a display contract, not a lossless proxy format. Layout hints are preferences, not frontend components.

| Protocol minor | Added operations/capability |
| --- | --- |
| 0 | `hello`, `providers.list`, `views.open/query/close`, `actions.invoke` |
| 1 | `assets.get`, negotiated `iconAssetsBgra8` |
| 2 | `views.details`, negotiated `itemDetails` |
| 3 | `state.theme`, negotiated `themeState`, independent of view generations |
| 4 | Optional action confirmation prompt, explicit `confirmed` consent and `actionConfirmation` |
| 5 | `frontend.poll`, negotiated `frontendControl` |
| 6 | `themeWallpapers` capability; each theme's wallpapers in `themeSummary`, applied by the existing confirmed Apply |
| 7 | Wallpapers provider, `previewAssetsBgra8` capability and the `previewBgra8` asset representation, `filmstrip` section layout hint |
| 7 | Optional `webLink` domain data carrying a provider-validated URL for a frontend-rendered web preview |
| 8 | `themes.install` and `themes.uninstall`, negotiated `themeInstall` |
| 9 | Provider-published action `input` field, collected by the frontend and returned on `actions.invoke` |
| 10 | `viewSnapshot.scopeActions`: provider-published commands for the whole scope, invoked with their own `actionRef` |

Capabilities identify what is implemented. Clients must not assume optional operations exist. Unknown optional fields are additive. Ordinary actions omit false consent fields for compatibility with older strict decoders. Streaming, generic state subscriptions and durable operation lookup/cancellation are not implemented.

## Transport and security

IPC uses a local Unix socket and four-byte big-endian length-framed UTF-8 JSON-RPC 2.0. Frames are bounded to one MiB before allocation. Requests require validated string IDs and typed parameters. Client batches and notifications are unsupported. Queries, result counts, content blocks, strings, clients and deadlines are bounded.

The socket parent must already be private and owner-only. The server checks effective peer UID, uses a 0600 socket and refuses existing paths rather than deleting another instance's socket. It never chmods arbitrary supplied directories. Shutdown unlinks only its own socket inode. Same-user applications are trusted clients; Unix permissions do not isolate applications running as that user.

Do not log clipboard contents, sensitive queries or provider payloads. User-selected executable/helper paths stay backend-private. Launches use fixed arguments without a shell; arbitrary RPC command strings cannot start processes.

## Snapshot and mutation authority

View revisions identify authoritative snapshots. Only server-retained actions may execute. Stale, forged or cross-session action/detail/asset references fail. Provider resources and private revision pins are rechecked before mutation. Frontends revoke action/detail authority and staged confirmations immediately when a query changes, even while retaining old visuals until replacement results arrive.

Operation IDs deduplicate matching invocations within a session's bounded history, including retained failures. Reusing an ID with different arguments fails. This is not durable history, cross-session deduplication or cross-daemon locking. Accepted work can finish after frontend disconnect. Clients must never automatically replay an uncertain operation after reconnect or restart.

Confirmed actions carry a service-issued prompt. Frontends stage snapshot-bound consent separately from accepted operations and invalidate it on query/navigation/selection changes. Only explicit confirmation sends `confirmed: true`. The service rejects omitted consent and fails closed if provider confirmation projection panics or attempts to downgrade previously issued requirements.

Clipboard management's internal `RefreshSearch` maps to a same-query `SetQuery` outcome, preserving scope without replaying a mutation. Direct Paste remains rejected until a verified previous-application target handoff exists.

## Assets and details

Negotiated icon retrieval returns correlated JSON metadata followed by a separate binary frame. Icons are straight-alpha, top-to-bottom, tightly packed BGRA8, at most 64x64 and 16,384 bytes. A session may retrieve only assets advertised in its current view. Search results never embed full image bytes. QML alone turns opaque resource IDs into `image://` URLs.

A second size class carries wallpaper thumbnails: `previewBgra8` is straight-alpha BGRA8 at most 384 on each edge, within a 640 KiB frame, and is authorized by the `previewAssetsBgra8` capability. Each asset reference advertises exactly one representation, so a caller cannot ask the 64px icon slot for 512px pixels or the reverse; the service rejects a mismatch with `UNSUPPORTED_REPRESENTATION`. Both size classes use the same lazy store, which resolves pixels on first request and caches them per (path, revision).

Section `layoutHint` values are `list` (default) and `filmstrip`. The hint describes the published snapshot only; a client that does not understand it must render a list.

Lazy details support bounded text, values, metadata, palettes and small cached images. Unknown blocks render their fallback text. Expired assets permit one explicit read-only view refresh with a fresh request ID, never an action retry. Large service previews and additional image formats remain unfinished. Local icon caches are bounded and may evict tokens.

## Native presentation and process lifecycle

Optional `[service.frontend]` or paired `--frontend`/`--frontend-path` flags select GPUI or Pill. Without selection the backend registers no global shortcut. A selected shortcut shows/refocuses the frontend, never toggles it closed. GPUI launch uses `--service-socket SOCKET --show`; Pill launch uses `/usr/bin/open -a APP --env MACCER_SERVICE_SOCKET=SOCKET --args --show-launcher`.

A separate connection renews a three-second presentation lease and acknowledges opaque, coalesced Show serials. It is independent of search, details and slow actions. Only the current owner may acknowledge serials it was offered. Adapter acknowledgments follow event-queue delivery and survive lease expiry; a full queue retains the event. Disconnect releases the lease. Unacknowledged Show requests may be delivered to a later frontend instance, but cannot authorize or replay mutations.

Both service-mode native entry points acquire the same private per-socket lifetime file lock and check selected-kind registration before constructing windows/providers. Thus an unselected kind cannot start even when the selected frontend is closed, and a second process of either kind is refused. GPUI also refuses embedded startup when a shared frontend is configured. Headless and adapter clients remain unrestricted. Pill's separately configured embedded mode remains available; quit it before launching through the service.

Startup admission is exposed as `launcher_agent::frontend_startup::claim` and the additive C guard API `maccer_agent_claim_frontend` / `maccer_agent_frontend_claim_free`. Normal Rust/C async connection APIs remain nonblocking. Native applications retain their guard through exit. A brief startup probe releases its control lease before the normal polling worker takes over.

The backend deduplicates launches through a startup grace, tracks/reaps direct children and never kills frontends on exit. The process lock survives presentation lease loss and daemon restarts. Clients do not reconnect automatically; restart frontends after a backend restart. The service's AppKit event pump dispatches Carbon hotkeys and main-queue work with a bounded 100 ms cadence. Login/service installation is separate from the legacy GPUI bundle's `SMAppService.mainAppService` registration and is not implemented.

## Shared clipboard and theme state

Clipboard capture runs on the backend's main-thread timer independently of frontend connections. It establishes a no-content startup baseline, applies native sensitivity/type filters and stores bounded text only in RAM. Clipboard search is explicit-scope-only and never records usage. The [operations guide](shared-service.md#clipboard-privacy) documents bounds and privacy limits.

mac-themer remains authoritative for palette derivation and desktop effects. The backend watches the state directory, reads bounded `current.json`, periodically reconciles and retains last-good colors. Theme state updates are independent of search revisions. Each frontend maps the same validated semantic palette to its own color types. Preview selection never installs an active palette.

Catalog loading uses `catalog --json`, bounded to 128 path-free schema-1 resolved palettes, with a three-second process deadline and two-MiB output limit. The same document carries a per-pack `wallpapers` map of filenames; identities are validated (no separator, no control byte, 128-byte cap), capped at 64 per pack and must name a published pack. Failures retain last-good entries but invalidate obsolete detail/action authority. Read-only constructors authorize no Apply action; live construction supplies a shared single-flight executor.

Apply invokes `apply SLUG --expected-palette JSON [--background ID] --json` using the same helper/root, an exact slug, a private catalog-revision pin and fresh expected-palette validation. The wallpaper identity comes from the service's own snapshot and is rechecked before it becomes an argument. It has a 30-second deadline, 64 KiB output cap and no automatic retry. The reply must name the exact wallpaper that was requested, otherwise it is a partial failure; a `wallpaperFailed` warning leaves the palette applied and reports the picture separately. Partial effects are reported rather than claimed as rollback. Applied state reconciles independently of the action result.

Theme acquisition needs a value no retained action can carry: a repository URL, or one of the installed packs. An action descriptor may therefore publish an `input` field — `{id, kind, title, label, placeholder?, submitLabel, options?}` — where `kind` is `text` or `select`, the provider writes every word, and `options` is the provider's own list. The frontend draws that dialog, collects one value, and sends it back as `input` (a bounded map of field id to value) on `actions.invoke`; a dialog with no field is an ordinary action. The service validates the bounds and the provider revalidates the value, exactly as it does for a wallpaper identity. An option that needs no consent carries none; one that does carries the existing `confirmation` prompt, and the collected value is staged with the consent so it cannot be sent before the prompt has been shown.

A command that belongs to a whole scope rather than to one row is published as a scope action. `viewSnapshot.scopeActions` carries `{providerId, id, actionId, actionRef, title, confirmation?, input?}`: the same field and confirmation a row action carries, its own `actionRef`, and a subject identity (`id`) instead of a row. It is invoked through `actions.invoke` exactly like a row action and revalidated the same way. Only an explicit provider route publishes scope actions — a mixed home search has no single page whose commands they would be.

The Themes scope publishes its two acquisition commands this way: "Install a theme from GitHub…" with a `text` field named `url`, and "Uninstall a theme…" with a `select` field named `slug` whose choices are the catalog's own packs, plus a confirmation prompt. They never appear as rows, so a strip of palettes stays a strip of palettes; the frontend lists them in its action panel beside the selected row's own actions, and a frontend that has no row selected (an empty collection) can still open that panel. The provider runs the same executor and the same catalog reload as the two RPC methods below. A read-only service publishes neither command.

Acquisition is also available without a scope row: `themes.install` takes a whole `https://github.com/<owner>/<repo>` URL plus optional `all` (a collection repository) and `force` (replace a pack the producer did not install itself); `themes.uninstall` takes one exact catalog slug. The service revalidates both, refuses anything but one GitHub repository over https before a helper runs, and passes them as fixed arguments to `theme-install URL [--ALL] [--FORCE] --json` and `theme-uninstall SLUG --json`.

Install has a 120-second deadline, removal 30 seconds, and both share a 64 KiB output cap and one single-flight lock. A reply must be schema 1 with the exact status, a bounded list of valid, distinct slugs and names, exactly one pack unless `all` was asked for, and — for a removal — the exact slug that was requested; anything else is a failure. The result is a summary (slug, name, wallpaper count, whether a pack was replaced), never a path, and the producer's stderr is never forwarded. A failed install may have fetched a repository, so its message says a theme may be partly installed instead of claiming nothing happened. The catalog is reloaded before the reply, so the pack is searchable immediately rather than up to one refresh interval later. Installing is not an action: it has no operation table, no confirmation prompt and no automatic retry, and the frontend is responsible for confirming the removal it offers.

Apply is not transactional. Concurrent theme-pack replacement after validation remains a producer write-boundary risk; external tools and other daemon instances do not share an execution lock. A pack is replaced by rename, so Apply either reads the pack it validated or fails its own palette recheck. A helper may outlive a daemon crash, and its timeout requires a living supervisor. Picking a wallpaper other than the theme's first, per-display wallpaper operations and native desktop Apply acceptance are unfinished.

## Wallpapers

A wallpaper is published as one result per image the *active* theme ships, with the pack slug, the wallpaper identity and a `wallpaperSummary` domain record; the active theme is read from the same `current.json` reader that publishes theme state, and the provider revision mixes in its slug so a theme switch re-searches the strip. Without an active theme every pack is listed. Results are explicit-scope only and stay in catalog order: the shared ranker sorts by score with a positional tiebreak, so the positional term is what keeps the strip's order stable while a typed query narrows it.

The service resolves `<themes>/<slug>/backgrounds/<id>` itself, using the same override chain as mac-themer's `paths.py`, and refuses any identity whose resolved file leaves that directory. The private path is used only to produce a preview asset; it never enters the protocol. Wallpaper identities are the ones the catalog published, so a caller still cannot name a path.

Set invokes `wallpaper SLUG --id ID --json` through the shared single-flight executor with a 30-second deadline and 64 KiB output cap. It writes no theme file and no application setting, so it carries no confirmation prompt; the reply must name exactly the requested theme and wallpaper, and a `wallpaperFailed` warning is reported as a desktop-picture failure rather than a rejected identity. There is no rollback of a picture that did change.

## Testing boundaries

Fixture service mode uses in-memory providers/state and never activates apps, registers real hotkeys, monitors the native clipboard, reads personal theme state or changes the desktop. It demonstrates eight planned menu categories, not eight migrated live providers. Provider mutation acceptance uses owned helpers, copied packs, sandbox state and fake desktop effects.

Tests cover navigation, independent sessions, disabled providers, unknown-content fallbacks, private-payload exclusion, framing, peer security, bounded connections, stale/forged authority, once-only actions, confirmation failures, clipboard privacy, last-good theme recovery, native admission and presentation during blocked actions. DTO/fixture round trips are tested; a full validator-backed Schema conformance gate remains unfinished. Current commands and known full-suite limitations are in the [operations guide](shared-service.md#verification-and-remaining-limits).
