# macOS Application Launcher with Zed GPUI

## Project Goal

Build a **macOS-native application launcher in Rust using Zed's GPUI**, inspired by:

- [RustCast](https://github.com/MystikoLab/rustcast) for macOS integration
- [Walker](https://github.com/abenz1267/walker) for its provider-oriented launcher model
- Alfred and Raycast for the overall keyboard-first macOS experience

The core design should behave more like Walker than Raycast:

- one lightweight resident process
- providers as independent search sources
- provider-specific actions
- configurable prefixes
- fast mixed search
- generic result/action UI
- minimal coupling between UI and macOS-specific functionality

The first objective is:

> **The fastest keyboard-driven way to find something on macOS and perform an action on it.**

---

# 1. Product Model

The core interaction should look roughly like this:

```text
⌥ Space
┌──────────────────────────────────────────────┐
│ > safari                                     │
├──────────────────────────────────────────────┤
│ Safari                         Applications  │
│ Safari Technology Preview      Applications  │
│ Search web for "safari"              Web     │
└──────────────────────────────────────────────┘
```

Unlike Raycast, every feature should not become its own isolated application screen.

Instead, almost everything should be represented as a **provider** that returns results and actions.

Example prefixes:

```text
normal text    → search all default providers
;              → provider picker
/apps          → applications only
/              → files
>              → shell/commands
=              → calculator
:              → clipboard
.              → symbols/emoji
?              → web search
```

The exact prefixes should be configurable.

The main navigation model should be:

```text
query
  ↓
provider(s)
  ↓
results
  ↓
selected result
  ├── Enter       default action
  ├── Cmd+Enter   action menu
  ├── Tab         alternate action / drill down
  └── Esc         back → clear → close
```

This gives the application Walker's composability without requiring a separate backend daemon initially.

---

# 2. Overall Architecture

Start with **one resident process**.

Do not begin with a frontend process plus daemon unless there is a demonstrated need for that separation.

```text
┌─────────────────────────────────────┐
│             macOS Process           │
│                                     │
│   ┌──────────── GPUI ────────────┐  │
│   │ Launcher Window              │  │
│   │ Search Input                 │  │
│   │ Result List                  │  │
│   │ Action Menu                  │  │
│   └──────────────┬───────────────┘  │
│                  │                  │
│          Launcher Controller        │
│                  │                  │
│       ┌──────────▼──────────┐       │
│       │    Search Engine    │       │
│       └──────────┬──────────┘       │
│                  │                  │
│   ┌──────────────▼──────────────┐   │
│   │       Provider Registry      │   │
│   └──┬────┬────┬────┬────┬─────┘   │
│      │    │    │    │    │         │
│    Apps Files Calc Web Clipboard   │
│                                     │
│   macOS services / SQLite / config  │
└─────────────────────────────────────┘
```

This gives you Walker-style separation of concerns while keeping startup, deployment, and IPC simple.

If external plugins eventually need isolation, the same provider boundary can later move across IPC.

---

# 3. Repository Structure

Start with a Rust workspace even if the first version produces one application binary.

```text
launcher/
├── Cargo.toml
├── crates/
│   ├── launcher/
│   │   └── main.rs
│   │
│   ├── launcher-ui/
│   │   ├── launcher.rs
│   │   ├── search_input.rs
│   │   ├── result_list.rs
│   │   ├── result_row.rs
│   │   ├── action_menu.rs
│   │   └── theme.rs
│   │
│   ├── launcher-core/
│   │   ├── provider.rs
│   │   ├── result.rs
│   │   ├── action.rs
│   │   ├── registry.rs
│   │   ├── search.rs
│   │   ├── ranking.rs
│   │   └── history.rs
│   │
│   ├── launcher-macos/
│   │   ├── applications.rs
│   │   ├── app_icons.rs
│   │   ├── hotkey.rs
│   │   ├── spotlight.rs
│   │   ├── workspace.rs
│   │   ├── clipboard.rs
│   │   ├── permissions.rs
│   │   └── login_item.rs
│   │
│   ├── provider-apps/
│   ├── provider-files/
│   ├── provider-calculator/
│   ├── provider-shell/
│   ├── provider-web/
│   └── provider-clipboard/
│
├── assets/
└── config/
```

The most important separation is:

```text
launcher-ui
launcher-core
launcher-macos
```

`launcher-core` should know nothing about AppKit, Cocoa, Objective-C, NSWorkspace, or GPUI.

`launcher-macos` should expose normal Rust-facing APIs to the rest of the project.

---

# 4. Define the Provider API First

The provider API is the most important architectural boundary in the application.

A conceptual starting point:

```rust
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn name(&self) -> &str;
    fn prefix(&self) -> Option<&str>;

    fn search(
        &self,
        query: SearchQuery,
        ctx: SearchContext,
    ) -> SearchTask;

    fn actions(&self, item: &Item) -> Vec<Action>;

    fn activate(
        &self,
        item: &Item,
        action: &Action,
        ctx: ActionContext,
    ) -> Result<()>;
}
```

Generic results:

```rust
struct Item {
    id: ItemId,
    provider: ProviderId,

    title: String,
    subtitle: Option<String>,
    keywords: Vec<String>,

    icon: Option<Icon>,
    score: f64,

    payload: ItemPayload,
}
```

Do not make the UI understand domain objects such as:

```text
Application
ClipboardEntry
FileResult
BrowserTab
Emoji
Command
```

The UI should understand only concepts like:

```text
Item
Action
Icon
Preview
```

Providers own their specific payloads and behavior.

This makes future external plugins significantly easier.

---

# 5. Search Engine

Search orchestration should be independent from GPUI.

```text
input changes
     ↓
parse query
     ↓
determine active providers
     ↓
cancel previous request
     ↓
query providers concurrently
     ↓
provider-local scoring
     ↓
global ranking
     ↓
stream results into UI
```

For fuzzy matching, `nucleo-matcher` is a strong initial option and is already used by RustCast.

A ranking formula could eventually resemble:

```text
score =
    fuzzy_match
  + exact_prefix_bonus
  + usage_frequency
  + recency
  + favourite_bonus
  + provider_priority
```

Keep ranking abstract:

```rust
trait Ranker {
    fn score(
        &self,
        query: &str,
        item: &Item,
        usage: &UsageStats,
    ) -> Score;
}
```

That makes it possible to improve ranking later without rewriting the providers.

---

# 6. Query Parsing

Introduce a query parser before hitting providers.

For example:

```rust
struct ParsedQuery {
    raw: String,
    search_text: String,
    provider_filter: Option<ProviderId>,
    mode: QueryMode,
}
```

Potential modes:

```rust
enum QueryMode {
    Mixed,
    Provider(ProviderId),
    ProviderPicker,
}
```

Parsing examples:

```text
safari
→ Mixed("safari")

/ report
→ Provider(files, "report")

= 100 * 1.15
→ Provider(calculator, "100 * 1.15")

;
→ ProviderPicker
```

Avoid having each provider manually inspect the raw input prefix.

The parser should own that responsibility.

---

# 7. GPUI Layer

GPUI should handle presentation, keyboard input, state updates, and rendering.

The primary GPUI entity could resemble:

```rust
struct Launcher {
    query: String,
    state: LauncherState,

    results: Vec<Item>,
    selected: usize,

    search_generation: u64,

    registry: Arc<ProviderRegistry>,
}
```

Application states:

```rust
enum LauncherState {
    Search,
    ProviderPicker,
    Actions(ItemId),
    Submenu(MenuId),
}
```

## Initial UI Components

Only build these at first:

```text
LauncherWindow
 ├── SearchInput
 ├── ProviderIndicator
 ├── ResultList
 │    └── ResultRow
 └── Footer
```

Do not start by building:

- settings screens
- preview panes
- extension management
- AI interfaces
- complex menus
- full onboarding

The launcher surface itself should be excellent first.

---

# 8. Keyboard Model

Keyboard behavior is core product functionality.

Start with:

```text
⌥ Space        toggle launcher
↓ / Ctrl-N     next result
↑ / Ctrl-P     previous result
Enter          default action
Cmd-Enter      action menu
Tab            secondary action / drill down
Esc            back / clear / hide
Cmd-K          action menu, optionally
Cmd-P          provider picker, optionally
```

Model these as GPUI actions rather than hard-coding raw key handling throughout components.

---

# 9. macOS Window Behavior

The launcher should behave like a launcher, not like a normal application window.

Desired behavior:

```text
⌥ Space
      ↓
window appears
      ↓
window appears on appropriate display
      ↓
search input is immediately focused
      ↓
window stays above ordinary windows
      ↓
Escape or focus loss
      ↓
window hides
      ↓
process remains running
```

Keep the GPUI window allocated while hidden rather than repeatedly creating and destroying it.

Prefer GPUI-native APIs for:

- floating windows
- focus
- activation
- borderless presentation
- keyboard dispatch

Only drop down into AppKit or `objc2` when GPUI does not expose the required behavior.

---

# 10. Global Hotkey

Global launcher invocation is required early.

Target behavior:

```text
Option + Space
```

Responsibilities:

```text
register global shortcut
detect conflicts
toggle launcher visibility
activate application
focus search input
restore prior application after closing if needed
```

Keep the hotkey implementation inside:

```text
launcher-macos/hotkey.rs
```

The UI should receive only something like:

```rust
LauncherEvent::ToggleRequested
```

---

# 11. Applications Provider — MVP #1

The applications provider should be the first real search provider.

Index at least:

```text
/Applications
/System/Applications
/System/Applications/Utilities
~/Applications
```

Potential data model:

```rust
struct Application {
    path: PathBuf,
    bundle_id: Option<String>,
    display_name: String,
    executable_name: Option<String>,
    icon: Option<AppIcon>,
}
```

Search fields:

```text
display name
bundle name
bundle identifier
executable name
aliases
```

Index applications once at startup.

Refresh when appropriate instead of rescanning on every query.

Initial actions:

```text
Open
```

Later:

```text
Open
Open New Window
Reveal in Finder
Show Package Contents
Quit
Force Quit
```

Launching should go through a centralized macOS abstraction rather than scattered shell calls.

---

# 12. Application Icons

Icons should be cached.

Avoid decoding application icons synchronously during every search.

Suggested flow:

```text
application indexed
    ↓
store icon source/path/reference
    ↓
result becomes visible
    ↓
load icon lazily
    ↓
cache decoded texture/image
```

Consider caches at two layers:

```text
app icon metadata cache
renderable GPUI image/texture cache
```

Result rows should not block while icons load.

---

# 13. File Provider — MVP #2

Do not recursively crawl the entire filesystem yourself.

Use Spotlight.

For the first version, shelling out to:

```text
mdfind
```

is acceptable and dramatically easier.

Later, replace the implementation with:

```text
NSMetadataQuery
```

while keeping the provider interface unchanged.

Example syntax:

```text
/ report
/ ~/dev launcher
```

Potential file actions:

```text
Open
Reveal in Finder
Quick Look
Copy Path
Copy File
Open With…
```

---

# 14. Provider Set for v1

A reasonable provider roadmap:

| Provider | Prefix | Priority |
|---|---:|---:|
| Applications | default | P0 |
| Calculator | `=` | P0 |
| Web search | `?` | P0 |
| Files | `/` | P1 |
| Shell commands | `>` | P1 |
| Clipboard | `:` | P1 |
| Provider picker | `;` | P1 |
| Emoji/symbols | `.` | P2 |
| Open windows | configurable | P2 |
| Custom menus | configurable | P2 |

The important part is that each of these is represented using the same generic provider/result/action system.

---

# 15. Provider Registry

Centralize providers in a registry.

Conceptually:

```rust
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn Provider>>,
}
```

Useful operations:

```rust
impl ProviderRegistry {
    pub fn register(&mut self, provider: Arc<dyn Provider>);
    pub fn get(&self, id: ProviderId) -> Option<Arc<dyn Provider>>;
    pub fn default_providers(&self) -> Vec<Arc<dyn Provider>>;
    pub fn providers_for_query(&self, query: &ParsedQuery) -> Vec<Arc<dyn Provider>>;
}
```

Provider configuration should determine:

```text
enabled
prefix
priority
default-search participation
maximum results
```

---

# 16. Actions Are as Important as Providers

The launcher should not treat `Enter` as its only extension point.

Use a generic action model:

```rust
struct Action {
    id: ActionId,
    title: String,
    icon: Option<Icon>,
    shortcut: Option<KeyBinding>,
    kind: ActionKind,
}
```

Example application actions:

```text
Safari
   ↳ Open
   ↳ Reveal in Finder
   ↳ Quit
```

Example file actions:

```text
report.pdf
   ↳ Open
   ↳ Quick Look
   ↳ Reveal
   ↳ Copy Path
```

Example clipboard actions:

```text
"hello world"
   ↳ Paste
   ↳ Copy
   ↳ Delete
```

This is what turns the application from a finder into a launcher platform.

---

# 17. Action Menu

The action menu should use the same general result navigation model as search.

Example:

```text
Safari
────────────────────
Open                  ↵
Reveal in Finder
Quit
Force Quit
```

Potential state transition:

```text
Search
  ↓ Cmd+Enter
Actions(ItemId)
  ↓ Enter
Execute Action
  ↓
Hide Launcher
```

Avoid implementing a completely separate menu framework unless necessary.

---

# 18. Calculator Provider

Calculator is an ideal second provider because it tests whether the provider abstraction is actually generic.

Examples:

```text
= 2 + 2
= 100 * 1.15
= sqrt(144)
```

Potential result:

```text
12
Calculator
```

Actions:

```text
Copy Result
Paste Result
```

If adding calculator requires significant changes to the launcher UI, your provider abstraction is too coupled.

---

# 19. Web Search Provider

Example:

```text
? gpui rust
```

or as a fallback result:

```text
Search the web for "gpui rust"
```

Configuration:

```toml
[providers.web]
enabled = true
prefix = "?"
default_engine = "google"

[[providers.web.engines]]
name = "Google"
url = "https://www.google.com/search?q={query}"

[[providers.web.engines]]
name = "DuckDuckGo"
url = "https://duckduckgo.com/?q={query}"
```

Actions:

```text
Search Default Engine
Search with Google
Search with DuckDuckGo
Copy Search URL
```

---

# 20. Shell Provider

Shell execution should be explicit.

Example:

```text
> git status
```

Possible behavior:

```text
execute command
show command output
copy output
open command in Terminal
```

Do not run arbitrary shell commands from normal mixed search text.

Require the shell provider prefix or explicit provider selection.

Keep shell execution out of the UI crate.

---

# 21. Clipboard Provider

Clipboard history is a useful Walker/Raycast-style provider.

Data model:

```rust
struct ClipboardEntry {
    id: ClipboardEntryId,
    content_type: ClipboardContentType,
    text: Option<String>,
    created_at: DateTime,
}
```

Actions:

```text
Paste
Copy
Delete
Pin
```

Storage can use SQLite.

Sensitive clipboard handling should eventually include configuration such as:

```text
history enabled
maximum history size
excluded applications
ignored content types
automatic expiration
```

Do not attempt sophisticated password-manager detection in the initial release.

---

# 22. Provider Picker

Walker-style provider switching is worth making a first-class feature.

Example:

```text
;
```

produces:

```text
Applications
Files
Calculator
Web
Shell
Clipboard
Emoji
```

Selecting one changes the current query scope.

Potential flow:

```text
;
↓
Files
↓ Enter
/
```

The picker itself can be represented using normal `Item` objects.

---

# 23. Configuration

Use TOML initially.

Example:

```toml
[launcher]
hotkey = "alt-space"
width = 680
max_results = 8

[search]
debounce_ms = 20

[providers.apps]
enabled = true
priority = 100

[providers.files]
enabled = true
prefix = "/"
priority = 80

[providers.shell]
enabled = true
prefix = ">"

[providers.clipboard]
enabled = true
prefix = ":"

[theme]
radius = 14
blur = true
```

Store configuration under something like:

```text
~/.config/<launcher-name>/config.toml
```

Support filesystem watching so configuration can eventually reload without restarting the application.

Avoid building a settings GUI until the configuration model stabilizes.

---

# 24. Persistence

Use SQLite for persistent launcher data.

Potential tables:

```text
usage_events
favourites
clipboard_entries
aliases
query_history
```

Possible schema concepts:

```text
usage_events
  item_id
  provider_id
  query
  action_id
  timestamp

favourites
  item_id
  provider_id
  created_at

aliases
  alias
  provider_id
  target_id
```

Do not persist every provider's entire index until profiling demonstrates that doing so is useful.

---

# 25. Adaptive Ranking

Usage ranking can improve results substantially.

Potential factors:

```text
fuzzy match score
exact prefix match
word boundary match
launch frequency
launch recency
query-specific historical selection
favourite state
provider priority
```

A later ranking function might resemble:

```text
final_score =
    fuzzy_score
  + prefix_bonus
  + favourite_bonus
  + log(usage_count)
  + recency_weight
  + provider_weight
```

Keep usage ranking transparent and deterministic initially.

Avoid adding ML until there is enough real usage data to justify it.

---

# 26. Search Concurrency

Search providers should run concurrently.

```text
GPUI main thread
      │
      ├── launcher state
      └── rendering
             │
             ▼
       SearchCoordinator
             │
       ┌─────┼─────────┐
       ▼     ▼         ▼
      Apps  Files   Clipboard
             │
        background work
```

Every query gets a generation identifier:

```rust
struct SearchRequest {
    generation: u64,
    query: ParsedQuery,
}
```

When new input arrives:

```text
generation 45 → stale
generation 46 → active
```

Any result from generation 45 should be discarded.

This prevents races such as:

```text
"saf"
 results arrive late

"safa"
 results arrive

"safari"
 results arrive

old "saf" results overwrite the newest state
```

---

# 27. Cancellation

Slow providers should support cancellation where practical.

At minimum:

```text
new query
↓
mark old request stale
↓
ignore old results
```

Better later:

```text
new query
↓
cancel token
↓
provider stops unnecessary work
```

File search and remote providers especially benefit from proper cancellation.

---

# 28. Result Streaming

Providers should be allowed to emit results independently.

For example:

```text
query: "not"

3 ms     Apps → Notes
6 ms     Apps → Notion
18 ms    Files → notes.md
31 ms    Web → Search web for "not"
```

The UI should not wait for every provider before displaying anything.

However, avoid severe result jumping.

Strategies:

```text
brief batching window
stable sort
preserve selected item identity
provider result limits
```

---

# 29. Selection Stability

Never track selection only by numeric row index.

Prefer:

```rust
selected_item: Option<ItemId>
```

When results change:

```text
if selected ItemId still exists
    keep selection on it
else
    select first result
```

This greatly reduces annoying UI movement during streaming search.

---

# 30. Performance Goals

Treat performance as a product requirement.

Suggested targets:

```text
Hotkey → visible window:       < 30 ms perceived
Keystroke → app results:       < 16 ms
Keystroke → mixed results:     < 40 ms
App index query:               < 5 ms
Idle CPU:                      ~0%
Idle memory target:            < 100 MB
```

These are goals rather than hard guarantees.

Architectural implications:

- resident process
- hidden window retained
- in-memory application index
- lazy icon decoding
- cancellation/stale-generation handling
- asynchronous slow providers
- no filesystem work from render callbacks
- no SQLite work from render callbacks

---

# 31. Debouncing

Do not over-debounce local providers.

For in-memory application search:

```text
0–10 ms debounce
```

may be sufficient.

For expensive providers:

```text
files
network
browser integrations
```

use a slightly larger provider-specific delay if needed.

The SearchCoordinator can distinguish:

```rust
ProviderLatencyClass::Immediate
ProviderLatencyClass::LocalAsync
ProviderLatencyClass::Expensive
```

---

# 32. macOS Abstraction Layer

Keep native code centralized.

For example:

```rust
pub trait MacPlatform {
    fn launch_application(&self, app: &Application) -> Result<()>;
    fn reveal_in_finder(&self, path: &Path) -> Result<()>;
    fn quick_look(&self, path: &Path) -> Result<()>;
    fn open_url(&self, url: &Url) -> Result<()>;
}
```

The actual implementation can use:

```text
objc2
objc2-app-kit
objc2-foundation
NSWorkspace
NSPasteboard
NSMetadataQuery
```

The rest of the application should not care.

---

# 33. Launch at Login

Eventually support:

```text
launch at login
```

Keep this in:

```text
launcher-macos/login_item.rs
```

Do not make it part of core startup logic.

The initial development version can simply be started manually.

---

# 34. Menu Bar Item

A small menu bar item can provide:

```text
Show Launcher
Preferences
Reload Configuration
Quit
```

This is useful because a launcher often has no normal Dock window.

It should be optional if you want a minimal daemon-like experience.

---

# 35. Dock Behavior

Consider running as an accessory-style application so the launcher does not behave like a normal Dock application.

Desired experience:

```text
no ordinary persistent main window
optional/no Dock icon
menu bar item optional
global shortcut is primary entry point
```

This will likely require AppKit-level application activation policy configuration.

Keep that behavior in the platform crate.

---

# 36. Focus Behavior

Focus handling deserves explicit testing.

Scenarios:

```text
launcher invoked while Finder active
launcher invoked while fullscreen app active
launcher invoked on secondary display
launcher dismissed with Escape
launcher loses focus from clicking elsewhere
launcher opens app
launcher opens URL
```

Expected behavior should be deterministic.

Focus bugs can make a launcher feel slow even when the actual search is fast.

---

# 37. Multi-Monitor Behavior

Decide how the launcher chooses a display.

Recommended default:

```text
display containing mouse pointer
```

Alternative:

```text
display containing focused/frontmost application
```

Make it configurable later.

Window placement options could include:

```text
top center
screen center
custom vertical offset
```

---

# 38. Window Geometry

Example configuration:

```toml
[window]
width = 680
max_height = 560
position = "top-center"
vertical_offset = 160
```

The window should resize smoothly based on result count while respecting a maximum height.

---

# 39. Theme Model

Keep theme variables centralized.

Possible theme structure:

```rust
struct Theme {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    selected_background: Hsla,
    border: Hsla,
    radius: Pixels,
    row_height: Pixels,
}
```

Do not hard-code styles across components.

Later support config-driven themes.

---

# 40. Result Row Design

Each row should support:

```text
icon
title
subtitle
provider
optional shortcut hint
```

Example:

```text
[icon] Safari
       Apple · Application                 ↵
```

Or:

```text
[icon] report.pdf
       ~/Documents                         File
```

Avoid overcrowding.

The primary result title should dominate visually.

---

# 41. Footer

The footer can show relevant actions for the selected item.

Example:

```text
↵ Open       ⌘↵ Actions       Esc Close
```

This can vary by state.

Provider picker:

```text
↵ Select Provider       Esc Back
```

Action menu:

```text
↵ Run Action            Esc Back
```

---

# 42. Previews

Previews are useful but should be postponed.

Potential preview types:

```text
file preview
image preview
clipboard content
calculator explanation
application metadata
command output
```

The generic item model can reserve:

```rust
preview: Option<PreviewDescriptor>
```

without implementing previews immediately.

---

# 43. Emoji and Symbols Provider

Later provider:

```text
. smile
. arrow
. lambda
```

Results:

```text
😀
→
λ
```

Actions:

```text
Paste
Copy
```

A static index is sufficient.

No external service is needed.

---

# 44. Open Windows Provider

A later macOS-specific provider could index application windows.

Possible query:

```text
windows notion
```

or a custom prefix.

Actions:

```text
Focus Window
Close Window
```

This will likely require Accessibility APIs.

Treat it as a P2 feature because permissions and native integration increase complexity.

---

# 45. Accessibility Permissions

Some future functionality will require macOS permissions.

Examples:

```text
window control
simulated paste
global event monitoring
certain automation features
```

Do not request permissions until the user actually enables a feature that needs them.

Provide one platform-level permission manager:

```text
launcher-macos/permissions.rs
```

---

# 46. Plugin Strategy

Do not implement third-party plugins in the first release.

Design for them now, but postpone the runtime.

Avoid loading arbitrary Rust dynamic libraries directly into the main launcher process.

Problems include:

```text
ABI compatibility
launcher crashes from plugin bugs
dependency conflicts
version coupling
security
```

A process-based protocol is safer.

---

# 47. Future Plugin Protocol

Two reasonable models:

## Option A — Unix Socket

```text
launcher
   ↕
Unix socket
   ↕
plugin/provider process
```

## Option B — JSON-RPC over stdin/stdout

```text
launcher
   ↕
stdin/stdout
   ↕
plugin process
```

Potential protocol:

```json
{
  "type": "search",
  "request_id": 42,
  "query": "safari"
}
```

Response:

```json
{
  "type": "results",
  "request_id": 42,
  "items": [
    {
      "id": "foo",
      "title": "Example",
      "subtitle": "Plugin result"
    }
  ]
}
```

Activation:

```json
{
  "type": "activate",
  "item_id": "foo",
  "action_id": "open"
}
```

The internal Provider trait should be designed so it maps naturally to this future protocol.

---

# 48. Security Boundary

External providers should eventually have explicit capabilities.

Potential permissions:

```text
filesystem
network
clipboard
shell execution
browser access
calendar
contacts
```

Do not give every plugin unrestricted access by default.

This can wait until the plugin runtime exists, but the concept should shape the design.

---

# 49. Custom Commands

Before building full plugins, support user-defined commands.

Example configuration:

```toml
[[commands]]
name = "Open dotfiles"
keywords = ["dotfiles", "config"]
command = "code ~/.config"

[[commands]]
name = "Restart Dock"
keywords = ["dock", "restart"]
command = "killall Dock"
```

These can be surfaced through the shell/custom command provider.

This gives users substantial extensibility without requiring a plugin ecosystem.

---

# 50. Aliases

Aliases should allow user customization.

Example:

```toml
[[aliases]]
alias = "ff"
provider = "apps"
target = "Firefox"
```

Searching:

```text
ff
```

should strongly rank Firefox.

Aliases can be stored either in TOML or SQLite.

---

# 51. Favourites

Allow users to pin frequently used results.

Potential action:

```text
Add to Favourites
Remove from Favourites
```

Ranking effect:

```text
favourite_bonus
```

Avoid forcing favourites into a separate screen initially.

---

# 52. History

Store activation history rather than every keystroke.

Useful event:

```rust
struct UsageEvent {
    query: String,
    provider: ProviderId,
    item: ItemId,
    action: ActionId,
    timestamp: DateTime,
}
```

This gives enough information for adaptive ranking without excessive data collection.

---

# 53. Testing Strategy

Separate testing into layers.

## launcher-core

Unit tests:

```text
query parsing
ranking
provider selection
selection stability
result merging
history scoring
```

## provider tests

```text
application matching
calculator expressions
web URL formatting
command parsing
```

## macOS integration

Integration tests where practical:

```text
app discovery
workspace launch APIs
Spotlight query construction
clipboard operations
```

## GPUI

Focus on higher-level behavioral tests:

```text
typing updates query
arrow keys change selection
Enter activates
Escape transitions correctly
```

---

# 54. Benchmarking

Create benchmarks early.

Measure:

```text
10 apps
100 apps
1,000 indexed items
10,000 indexed items
mixed provider merge
fuzzy query latency
ranking latency
icon-cache hit/miss
```

Use `criterion` for non-UI benchmarks if appropriate.

Do not rely only on subjective launcher feel.

---

# 55. Logging

Use structured logging.

Likely stack:

```text
tracing
tracing-subscriber
```

Useful spans:

```text
launcher.show
query.parse
search.total
provider.apps.search
provider.files.search
ranking.merge
action.execute
```

Avoid logging clipboard contents or sensitive user data.

---

# 56. Error Handling

Providers should fail independently.

If file search fails:

```text
Applications still work
Calculator still works
Web still works
```

Do not make one provider error take down the whole search request.

Conceptually:

```rust
enum ProviderResponse {
    Results(Vec<Item>),
    Error(ProviderError),
}
```

Provider errors can be optionally surfaced in development mode.

---

# 57. Development Milestones

## Milestone 0 — GPUI Spike

Prove:

```text
borderless/floating launcher window
hotkey → show
Esc → hide
text input
keyboard navigation
virtual result list
```

Nothing else.

---

## Milestone 1 — App Launcher

Implement:

```text
application scanner
application icons
nucleo fuzzy search
application launching
selection
usage ranking
```

At this stage the launcher should already be useful enough to replace Spotlight for opening applications.

---

## Milestone 2 — Provider Core

Extract:

```text
Provider
ProviderRegistry
Item
Action
SearchCoordinator
```

Convert application search into:

```text
ApplicationProvider
```

Then add:

```text
CalculatorProvider
```

If calculator can be added with almost no UI changes, the architecture is working.

---

## Milestone 3 — Walker-Style Behavior

Implement:

```text
provider prefixes
provider selector
mixed search
actions menu
nested/submenu state
provider configuration
```

At this point the application's interaction model should begin to feel closer to Walker.

---

## Milestone 4 — macOS Power Features

Add:

```text
Spotlight file search
clipboard history
Finder actions
global hotkey configuration
launch at login
menu bar item
active-screen placement
```

---

## Milestone 5 — Extensibility

Add:

```text
custom shell commands
aliases
web engines
custom menus
provider configuration
plugin protocol
```

---

## Milestone 6 — Polish

Then add:

```text
themes
blur
animations
preview pane
settings UI
accessibility improvements
automatic updates
onboarding
code signing
notarization
```

---

# 58. Features to Leave Out of v1

Do not attempt these initially:

```text
AI/chat
calendar
notes
window management
browser tab search
password-manager integration
extension marketplace
cloud sync
Raycast-compatible extension API
JavaScript runtime
full settings GUI
```

These may eventually be useful, but they substantially increase scope.

The provider foundation should come first.

---

# 59. Suggested First Commit Series

A practical first implementation sequence:

```text
01  bootstrap GPUI workspace
02  create borderless LauncherWindow
03  build SearchInput
04  build virtualized ResultList
05  add Up/Down/Ctrl-N/Ctrl-P/Enter/Esc actions
06  add global hotkey
07  add macOS ApplicationScanner
08  add AppIcon loader/cache
09  integrate nucleo matching
10  implement NSWorkspace application launch
11  keep process/window resident
12  add usage history SQLite
13  extract Provider trait
14  convert ApplicationProvider
15  add CalculatorProvider
16  add prefix parser
17  add ProviderPicker
18  add generic ActionMenu
```

After commit 18, the project should have the architectural skeleton needed for the rest of the launcher.

---

# 60. Recommended Dependency Direction

Keep dependencies flowing in one direction.

```text
launcher
   │
   ├── launcher-ui
   │      │
   │      └── launcher-core
   │
   ├── launcher-macos
   │      │
   │      └── launcher-core
   │
   └── providers
          │
          ├── launcher-core
          └── launcher-macos where required
```

Avoid:

```text
launcher-core → GPUI
launcher-core → AppKit
launcher-core → provider implementations
```

The core crate should remain portable Rust.

---

# 61. Recommended Core Types

A possible starting model:

```rust
pub struct ProviderId(pub &'static str);

pub struct ItemId(pub String);

pub struct ActionId(pub String);

pub struct SearchQuery {
    pub raw: String,
    pub text: String,
}

pub struct Item {
    pub id: ItemId,
    pub provider: ProviderId,
    pub title: String,
    pub subtitle: Option<String>,
    pub keywords: Vec<String>,
    pub icon: Option<IconDescriptor>,
    pub score: f64,
    pub payload: ItemPayload,
}

pub struct Action {
    pub id: ActionId,
    pub title: String,
    pub shortcut: Option<KeyBinding>,
}

pub struct SearchBatch {
    pub generation: u64,
    pub provider: ProviderId,
    pub items: Vec<Item>,
}
```

The exact types will evolve, but establishing stable domain boundaries early is worthwhile.

---

# 62. SearchCoordinator Responsibility

The SearchCoordinator should own:

```text
query generation
query parsing
provider selection
provider dispatch
cancellation
result collection
ranking
deduplication
result limits
UI notifications
```

Conceptually:

```rust
pub struct SearchCoordinator {
    registry: Arc<ProviderRegistry>,
    ranker: Arc<dyn Ranker>,
    generation: AtomicU64,
}
```

The GPUI layer should call something like:

```rust
coordinator.search(query, on_update);
```

rather than invoking individual providers.

---

# 63. Provider Result Limits

Providers should have limits.

Example:

```text
applications: 10
files: 8
clipboard: 5
web: 2
calculator: 1
```

The combined UI might display:

```text
maximum 10 results
```

The SearchCoordinator can over-fetch slightly and perform global ranking.

Without limits, large providers such as files can dominate the launcher.

---

# 64. Deduplication

Mixed search may produce equivalent results.

Examples:

```text
application provider → Safari
recent items provider → Safari
favourites provider → Safari
```

Prefer metadata modifiers rather than duplicate rows.

Conceptually:

```text
Safari
★ Favourite · Recently Used
```

Deduplication can use a stable canonical identity.

For applications:

```text
bundle identifier
```

For files:

```text
canonical path
```

---

# 65. Result Identity

Stable result identity is important for:

```text
ranking
history
favourites
selection stability
deduplication
actions
```

Examples:

```text
apps:com.apple.Safari
file:/Users/alice/Documents/report.pdf
web:google
clipboard:42
```

Avoid random UUIDs for persistent identities when a stable domain identifier exists.

---

# 66. Future Preview Architecture

If previews are added later:

```rust
enum PreviewDescriptor {
    Text(String),
    File(PathBuf),
    Image(PathBuf),
    Markdown(String),
    Custom(PreviewId),
}
```

Keep previews lazy.

The launcher should never block normal search while a preview is loading.

---

# 67. App Startup

Startup sequence:

```text
initialize logging
↓
load config
↓
open SQLite
↓
construct macOS platform services
↓
build provider registry
↓
start fast local indexing
↓
create GPUI app
↓
create hidden launcher window
↓
register global hotkey
↓
enter event loop
```

Application indexing can happen concurrently if necessary.

The launcher should become usable as early as possible.

---

# 68. Shutdown

Clean shutdown should:

```text
flush pending usage events
close SQLite cleanly
unregister global hotkey if necessary
stop provider tasks
close plugin processes later
```

Do not force-quit background tasks unnecessarily if graceful termination is straightforward.

---

# 69. Configuration Reload

Later:

```text
config.toml changed
↓
parse new config
↓
validate
↓
apply safe runtime changes
↓
reconfigure providers
↓
re-register hotkey if changed
↓
refresh launcher theme
```

Invalid configuration should not crash the application.

Prefer retaining the last valid config and logging/reporting the error.

---

# 70. Naming the Internal Layers

A clean conceptual model:

```text
Launcher
    presentation/state

SearchCoordinator
    orchestration

ProviderRegistry
    available capabilities

Provider
    search source

Item
    generic result

Action
    operation on item

Platform
    operating-system integration
```

If every new feature can be described using these concepts, the architecture is staying healthy.

---

# 71. Definition of MVP

A true MVP should be able to:

```text
launch from global hotkey
search installed applications instantly
open application
navigate entirely by keyboard
hide instantly with Escape
remain resident without consuming significant CPU
remember common application choices
```

That alone is a usable launcher.

Everything after this extends the same provider system.

---

# 72. Definition of v1

A stronger first public release could contain:

```text
Applications
Files
Calculator
Web Search
Clipboard
Shell Commands
Provider Picker
Generic Action Menu
Usage Ranking
Config File
Global Hotkey
Launch at Login
Menu Bar Controls
```

That provides a compelling Walker-like macOS launcher without trying to compete with every Raycast feature at once.

---

# 73. Architectural Principle

The core design decision should be:

> **GPUI handles presentation and application state.  
> `launcher-core` handles providers, queries, results, ranking, and actions.  
> `launcher-macos` owns Cocoa, Spotlight, clipboard, workspace, hotkey, and other native integrations.**

This gives the project:

- RustCast-style native macOS capability
- Walker-style provider architecture
- GPUI-native fast rendering and keyboard interaction
- a clean path toward external providers/plugins
- minimal coupling between UI and OS services

---

# 74. References

## RustCast

Repository:

https://github.com/MystikoLab/rustcast

Useful inspiration:

- macOS application discovery
- application icons
- global hotkeys
- clipboard history
- Spotlight-backed file search
- custom commands
- SQLite persistence
- fuzzy matching
- floating launcher window
- launch-at-login behavior

---

## Walker

Repository:

https://github.com/abenz1267/walker

Useful inspiration:

- provider-oriented architecture
- prefixes
- provider switching
- keyboard-first interaction
- mixed search
- generic provider behavior

---

## Elephant

Repository:

https://github.com/abenz1267/elephant

Useful inspiration for a future external-provider architecture:

- provider/service separation
- Unix-socket communication
- protocol-based activation/search

The first version of this macOS launcher does **not** need to reproduce Elephant's daemon architecture.

---

## Zed GPUI

Zed repository:

https://github.com/zed-industries/zed

Relevant GPUI areas include:

```text
crates/gpui
crates/gpui/examples
```

GPUI should be pinned to a known-good revision because it remains pre-1.0 and can change over time.

---

## Apple Spotlight

For the first implementation:

```text
mdfind
```

can provide a simple Spotlight-backed file search.

A future native implementation can use:

```text
NSMetadataQuery
```

through the macOS platform layer.

---

# Final Direction

Do not start by recreating Raycast.

Start by building an extremely fast launcher with:

```text
Applications
+
Provider Architecture
+
Generic Actions
+
Keyboard Navigation
+
Excellent macOS Window Behavior
```

Then add providers one at a time.

The first architectural milestone should be reached when adding a new provider requires almost no GPUI changes.

At that point, the foundation is correct.
