# Maccer provider service architecture

The experimental shared backend is implemented for Applications, Web Search, text Clipboard and Themes. GPUI and Pill/QML are removable native frontends. Use the [setup and operations guide](shared-service.md) for commands, current verification and remaining limits. Files, calculator and shell remain embedded-only; independent installation and retirement of embedded ownership are unfinished.

## Ownership and navigation

One per-user backend owns the provider registry, application catalogs, usage history, clipboard capture, theme state/catalog caches, confirmed theme execution and configured global shortcut. It runs a Cocoa event loop without linking GPUI or Qt. Frontends own windows, rendering, input and interactions requiring a window or mouse event. Exiting a frontend never stops the backend.

Home is a searchable backend view. Empty home queries list enabled providers. Typed queries search those menu entries and eligible provider results. Selecting a provider navigates to its scope, including empty-query browsing when supported. Sensitive provider contents never participate in mixed home search. Routes, not frontend-invented prefix strings, represent navigation.

Each connection has an independent session, search coordinator, view revisions and action-reference table. Search cancellation is session-local. Provider caches and mutations are shared. Query changes and frontend disconnects cannot cancel accepted side effects.

## Public contract

`launcher-protocol` defines Rust DTOs, the [JSON Schema](../crates/launcher-protocol/schema/protocol.schema.json) and fixtures. Public DTOs contain no GPUI/Qt types, frontend image URLs, executable commands or provider-private payloads. Providers project safe display data through service presentation adapters; internal `Item.payload` never crosses IPC.

Public objects include provider descriptors, routes, snapshots, result items, content blocks, action descriptors, asset references and structured errors. Titles remain readable. Content blocks provide fallback text for unknown types. Unknown domain records retain their discriminator/schema version but discard unrecognized fields. This is a display contract, not a lossless proxy format. Layout hints are preferences, not frontend components.

| Protocol minor | Added operations/capability |
| --- | --- |
| 0 | `hello`, `providers.list`, `views.open/query/close`, `actions.invoke` |
| 1 | `assets.get`, negotiated `iconAssetsBgra8` |
| 2 | `views.details`, negotiated `itemDetails` |
| 3 | `state.theme`, negotiated `themeState`, independent of view generations |
| 4 | Optional action confirmation prompt, explicit `confirmed` consent and `actionConfirmation` |
| 5 | `frontend.poll`, negotiated `frontendControl` |

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

Catalog loading uses `catalog --json`, bounded to 128 path-free schema-1 resolved palettes, with a three-second process deadline and two-MiB output limit. Failures retain last-good entries but invalidate obsolete detail/action authority. Read-only constructors authorize no Apply action; live construction supplies a shared single-flight executor.

Apply invokes `apply SLUG --expected-palette JSON --json` using the same helper/root, an exact slug, a private catalog-revision pin and fresh expected-palette validation. It has a 30-second deadline, 64 KiB output cap and no automatic retry. It leaves wallpaper unchanged and reports possible partial effects rather than claiming rollback. Applied state reconciles independently of the action result.

Apply is not transactional. Concurrent theme-pack replacement after validation remains a producer write-boundary risk; external tools and other daemon instances do not share an execution lock. A helper may outlive a daemon crash, and its timeout requires a living supervisor. Wallpaper catalogs, per-display wallpaper operations and native desktop Apply acceptance are unfinished.

## Testing boundaries

Fixture service mode uses in-memory providers/state and never activates apps, registers real hotkeys, monitors the native clipboard, reads personal theme state or changes the desktop. It demonstrates eight planned menu categories, not eight migrated live providers. Provider mutation acceptance uses owned helpers, copied packs, sandbox state and fake desktop effects.

Tests cover navigation, independent sessions, disabled providers, unknown-content fallbacks, private-payload exclusion, framing, peer security, bounded connections, stale/forged authority, once-only actions, confirmation failures, clipboard privacy, last-good theme recovery, native admission and presentation during blocked actions. DTO/fixture round trips are tested; a full validator-backed Schema conformance gate remains unfinished. Current commands and known full-suite limitations are in the [operations guide](shared-service.md#verification-and-remaining-limits).
