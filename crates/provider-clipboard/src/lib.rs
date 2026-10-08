//! In-memory, text-only clipboard history.
//!
//! The provider never touches the system pasteboard itself. The parent polls
//! the OS on the main thread, applies its own sensitivity policy, and hands
//! sanitized text to [`ClipboardProvider::capture`]. Stored text lives only in
//! RAM for the process lifetime: it is never written to disk and never
//! recorded in usage history.

use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, CancellationToken, Item, ItemId, Preview, Provider,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_macos::Platform;
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

/// Largest history the provider will ever keep, regardless of configuration.
pub const MAX_ENTRIES_LIMIT: usize = 1000;
/// Default history size used by the launcher configuration.
pub const DEFAULT_MAX_ENTRIES: usize = 200;
/// Largest single clipboard entry accepted, in bytes.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// Largest aggregate history size, in bytes.
pub const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
/// Longest single-line title excerpt, in Unicode scalar values.
pub const MAX_TITLE_CHARS: usize = 120;

const ID: &str = "clipboard";
const COPY: &str = "copy";
const PASTE: &str = "paste";
const PIN: &str = "pin";
const UNPIN: &str = "unpin";
const DELETE: &str = "delete";
const CLEAR: &str = "clear";

#[derive(Clone)]
struct Entry {
    id: u64,
    text: Arc<str>,
    pinned: bool,
}

#[derive(Default)]
struct Store {
    baseline: Option<i64>,
    next_id: u64,
    total_bytes: usize,
    /// Newest entries first; the back of the deque is the oldest.
    entries: VecDeque<Entry>,
}

impl Store {
    fn insert(&mut self, text: &str, max_entries: usize) -> bool {
        // Exact duplicates move to the front and keep their id and pin state.
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.text.as_ref() == text)
        {
            if let Some(entry) = self.entries.remove(index) {
                self.entries.push_front(entry);
            }
            return true;
        }
        // Check eviction capacity before mutating. If pins leave too little
        // room, rejecting a new entry must not silently delete older entries.
        let pinned_bytes: usize = self
            .entries
            .iter()
            .filter(|entry| entry.pinned)
            .map(|entry| entry.text.len())
            .sum();
        if self.entries.iter().filter(|entry| entry.pinned).count() >= max_entries
            || pinned_bytes + text.len() > MAX_TOTAL_BYTES
        {
            return false;
        }
        while self.entries.len() >= max_entries {
            if !self.evict_oldest_unpinned() {
                return false;
            }
        }
        while self.total_bytes + text.len() > MAX_TOTAL_BYTES {
            if !self.evict_oldest_unpinned() {
                return false;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.total_bytes += text.len();
        self.entries.push_front(Entry {
            id,
            text: Arc::from(text),
            pinned: false,
        });
        true
    }

    fn evict_oldest_unpinned(&mut self) -> bool {
        let Some(index) = self.entries.iter().rposition(|entry| !entry.pinned) else {
            return false;
        };
        if let Some(entry) = self.entries.remove(index) {
            self.total_bytes = self.total_bytes.saturating_sub(entry.text.len());
            return true;
        }
        false
    }
}

pub struct ClipboardProvider {
    platform: Arc<dyn Platform>,
    max_entries: usize,
    store: Mutex<Store>,
    revision: AtomicU64,
}

impl ClipboardProvider {
    pub fn new(platform: Arc<dyn Platform>, max_entries: usize) -> Self {
        Self {
            platform,
            max_entries: max_entries.clamp(1, MAX_ENTRIES_LIMIT),
            store: Mutex::new(Store::default()),
            revision: AtomicU64::new(0),
        }
    }

    /// Records a pasteboard change observed by the caller. The first call only
    /// establishes the baseline, so the clipboard contents that predate launch
    /// are never recorded. Returns whether the history changed.
    pub fn capture(&self, change_count: i64, text: Option<&str>) -> bool {
        let mut store = self.lock();
        match store.baseline {
            None => {
                store.baseline = Some(change_count);
                return false;
            }
            Some(previous) if previous == change_count => return false,
            Some(_) => store.baseline = Some(change_count),
        }
        let Some(text) = text else {
            return false;
        };
        if text.trim().is_empty() || text.len() > MAX_ENTRY_BYTES {
            return false;
        }
        let captured = store.insert(text, self.max_entries);
        drop(store);
        if captured {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        captured
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lookup<'a>(store: &'a Store, item: &Item) -> Option<&'a Entry> {
        if item.provider.0 != ID {
            return None;
        }
        let id = item.payload.get("entry")?.as_u64()?;
        store.entries.iter().find(|entry| entry.id == id)
    }

    fn item(entry: &Entry) -> Item {
        Item {
            id: ItemId(format!("{ID}:{}", entry.id)),
            provider: ProviderId(ID.into()),
            title: excerpt(&entry.text),
            subtitle: Some(
                if entry.pinned {
                    "Pinned"
                } else {
                    "Text clipboard"
                }
                .into(),
            ),
            // The item carries only the numeric entry id, never the text.
            keywords: Vec::new(),
            icon: None,
            score: 0.0,
            payload: json!({"entry": entry.id}),
        }
    }
}

/// First non-empty line, trimmed and bounded to [`MAX_TITLE_CHARS`].
fn excerpt(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    line.chars().take(MAX_TITLE_CHARS).collect()
}

impl Provider for ClipboardProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ID.into())
    }
    fn name(&self) -> &str {
        "Clipboard"
    }
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn records_usage(&self) -> bool {
        false
    }
    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
    fn supports_preview(&self, item: &Item) -> bool {
        let store = self.lock();
        Self::lookup(&store, item).is_some()
    }
    fn preview_revision(&self, _item: &Item) -> u64 {
        self.revision()
    }
    fn preview(&self, item: &Item, cancellation: &CancellationToken) -> Result<Option<Preview>> {
        if cancellation.is_cancelled() {
            bail!("preview cancelled");
        }
        let store = self.lock();
        let Some(entry) = Self::lookup(&store, item) else {
            return Ok(None);
        };
        Ok(Some(Preview::Text {
            text: entry.text.to_string(),
            truncated: false,
        }))
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 {
            return Ok(Vec::new());
        }
        // Clone a bounded snapshot and release the lock before matching.
        let snapshot: Vec<Entry> = {
            let store = self.lock();
            store.entries.iter().cloned().collect()
        };
        let needle = query.text.trim();
        let pattern = if needle.is_empty() {
            None
        } else {
            Some(Pattern::new(
                needle,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
            ))
        };
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut scored: Vec<(usize, u32, &Entry)> = Vec::new();
        for (order, entry) in snapshot.iter().enumerate() {
            if ctx.cancellation.is_cancelled() {
                return Ok(Vec::new());
            }
            let score = match &pattern {
                None => 0,
                Some(pattern) => {
                    let mut buffer = Vec::new();
                    match pattern.score(Utf32Str::new(&entry.text, &mut buffer), &mut matcher) {
                        Some(score) => score,
                        None => continue,
                    }
                }
            };
            scored.push((order, score, entry));
        }
        scored.sort_by(|a, b| {
            b.2.pinned
                .cmp(&a.2.pinned)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        Ok(scored
            .into_iter()
            .take(ctx.limit)
            .map(|(order, score, entry)| {
                let mut item = Self::item(entry);
                // The coordinator re-sorts all results. Carry pin/recency
                // preference in the score rather than relying on vector order.
                item.score = if entry.pinned { 1_000_000_000.0 } else { 0.0 }
                    + f64::from(score)
                    + (snapshot.len() - order) as f64 / (snapshot.len() + 1) as f64;
                item
            })
            .collect())
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        let store = self.lock();
        let Some(entry) = Self::lookup(&store, item) else {
            return Vec::new();
        };
        let pin = if entry.pinned {
            Action {
                id: ActionId(UNPIN.into()),
                title: "Unpin".into(),
            }
        } else {
            Action {
                id: ActionId(PIN.into()),
                title: "Pin".into(),
            }
        };
        vec![
            Action {
                id: ActionId(COPY.into()),
                title: "Copy".into(),
            },
            Action {
                id: ActionId(PASTE.into()),
                title: "Paste".into(),
            },
            pin,
            Action {
                id: ActionId(DELETE.into()),
                title: "Delete".into(),
            },
            Action {
                id: ActionId(CLEAR.into()),
                title: "Clear History".into(),
            },
        ]
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if item.provider.0 != ID {
            bail!("item is not a clipboard result");
        }
        let id = item
            .payload
            .get("entry")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow::anyhow!("clipboard item is missing its entry"))?;
        let action_id = action.id.0.as_str();
        // Copy leaves the store untouched, so release the lock before the call out.
        if action_id == COPY {
            let text = {
                let store = self.lock();
                store
                    .entries
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| entry.text.to_string())
                    .ok_or_else(|| anyhow::anyhow!("clipboard entry is no longer available"))?
            };
            self.platform.copy_text(&text)?;
            return Ok(ActionOutcome::Close);
        }
        let mut store = self.lock();
        let index = store
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| anyhow::anyhow!("clipboard entry is no longer available"))?;
        match action_id {
            PASTE => {
                return Ok(ActionOutcome::PasteText(
                    store.entries[index].text.to_string(),
                ));
            }
            PIN => store.entries[index].pinned = true,
            UNPIN => store.entries[index].pinned = false,
            DELETE => {
                if let Some(removed) = store.entries.remove(index) {
                    store.total_bytes = store.total_bytes.saturating_sub(removed.text.len());
                }
            }
            CLEAR => {
                store.entries.clear();
                store.total_bytes = 0;
            }
            other => bail!("unsupported clipboard action: {other}"),
        }
        drop(store);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(ActionOutcome::RefreshSearch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::{
        CancellationToken, HistoryStore, ProviderConfig, ProviderRegistry, SearchCoordinator,
    };
    use std::{path::Path, sync::Mutex};

    #[derive(Default)]
    struct Fake {
        copied: Mutex<Vec<String>>,
    }
    impl Platform for Fake {
        fn launch_application(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn reveal_in_finder(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn open_url(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn copy_text(&self, text: &str) -> Result<()> {
            self.copied.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    fn provider(max: usize) -> (ClipboardProvider, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        (ClipboardProvider::new(fake.clone(), max), fake)
    }
    fn baseline(p: &ClipboardProvider) {
        assert!(!p.capture(1000, None));
    }
    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            raw: text.into(),
            text: text.into(),
        }
    }
    fn ctx(limit: usize) -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: CancellationToken::default(),
            limit,
        }
    }
    fn action(p: &ClipboardProvider, item: &Item, id: &str) -> Action {
        p.actions(item)
            .into_iter()
            .find(|a| a.id.0 == id)
            .unwrap_or_else(|| panic!("missing action {id} for {}", item.id.0))
    }
    fn entry(text: &str) -> Item {
        Item {
            id: ItemId(format!("clipboard:{}", text.len())),
            provider: ProviderId(ID.into()),
            title: text.into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({"entry": 1}),
        }
    }

    #[test]
    fn rejected_insert_does_not_evict_entries_or_change_revision() {
        let (p, _) = provider(1000);
        baseline(&p);
        for n in 0..64 {
            let len = if n == 63 {
                MAX_ENTRY_BYTES - 1
            } else {
                MAX_ENTRY_BYTES
            };
            let text = format!("{n:02}{}", "x".repeat(len - 2));
            assert!(p.capture(n, Some(&text)));
            let item = p
                .search(&query(""), &ctx(1000))
                .unwrap()
                .into_iter()
                .find(|item| item.title.starts_with(&format!("{n:02}")))
                .unwrap();
            p.activate(&item, &action(&p, &item, PIN)).unwrap();
        }
        assert!(p.capture(65, Some("z")));
        let revision = p.revision();
        assert!(!p.capture(66, Some("ab")));
        assert_eq!(p.lock().entries.len(), 65);
        assert_eq!(p.revision(), revision);
    }

    #[test]
    fn coordinator_preserves_pinned_and_newest_order_for_empty_scope() {
        let (p, _) = provider(200);
        baseline(&p);
        p.capture(1, Some("omega pinned"));
        let first = p.search(&query(""), &ctx(50)).unwrap().remove(0);
        p.activate(&first, &action(&p, &first, PIN)).unwrap();
        p.capture(2, Some("beta old"));
        p.capture(3, Some("zulu newest"));
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(p),
                ProviderConfig {
                    prefix: Some(":".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let coordinator = SearchCoordinator::new(
            Arc::new(registry),
            Arc::new(HistoryStore::in_memory().unwrap()),
            50,
        );
        let session = coordinator.start(":".into());
        loop {
            let update = session.receiver.recv_blocking().unwrap();
            if update.complete {
                assert_eq!(
                    update
                        .items
                        .iter()
                        .map(|item| item.title.as_str())
                        .collect::<Vec<_>>(),
                    vec!["omega pinned", "zulu newest", "beta old"]
                );
                break;
            }
        }
    }

    #[test]
    fn first_capture_only_records_baseline_and_skips_preexisting_clipboard() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        assert!(!p.capture(7, Some("preexisting secret")));
        assert!(p.search(&query(""), &ctx(50)).unwrap().is_empty());
    }

    #[test]
    fn duplicate_change_counts_are_ignored() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        assert!(p.capture(2, Some("hello")));
        assert!(!p.capture(2, Some("ignored")));
        let items = p.search(&query(""), &ctx(50)).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "hello");
    }

    #[test]
    fn whitespace_only_text_is_ignored_but_real_data_keeps_surrounding_whitespace() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        assert!(!p.capture(2, Some("   \n\t ")));
        assert!(p.capture(3, Some("  keep me  ")));
        let items = p.search(&query("keep"), &ctx(50)).unwrap();
        assert_eq!(items.len(), 1);
        let Some(Preview::Text { text, .. }) =
            p.preview(&items[0], &CancellationToken::default()).unwrap()
        else {
            panic!("expected text preview");
        };
        assert_eq!(text, "  keep me  ");
    }

    #[test]
    fn oversized_entries_are_rejected_at_the_byte_boundary() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        assert!(!p.capture(2, Some(&"a".repeat(MAX_ENTRY_BYTES + 1))));
        assert!(p.capture(3, Some(&"b".repeat(MAX_ENTRY_BYTES))));
        assert_eq!(p.search(&query(""), &ctx(50)).unwrap().len(), 1);
    }

    #[test]
    fn unicode_duplicate_moves_to_newest_and_keeps_id_and_pin() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        assert!(p.capture(2, Some("café ☕")));
        let first = p.search(&query(""), &ctx(50)).unwrap().remove(0);
        let id = first.id.0.clone();
        let pin = action(&p, &first, "pin");
        assert!(matches!(
            p.activate(&first, &pin).unwrap(),
            ActionOutcome::RefreshSearch
        ));
        assert!(p.capture(3, Some("other")));
        assert!(p.capture(4, Some("café ☕")));
        let items = p.search(&query(""), &ctx(50)).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.0, id, "deduplicated entry must be newest");
        assert_eq!(items[0].subtitle.as_deref(), Some("Pinned"));
        assert_eq!(items.iter().filter(|i| i.id.0 == id).count(), 1);
    }

    #[test]
    fn history_is_bounded_by_max_entries_and_pins_prevent_eviction() {
        let (p, _) = provider(2);
        baseline(&p);
        assert!(p.capture(2, Some("one")));
        assert!(p.capture(3, Some("two")));
        let one = p.search(&query("one"), &ctx(10)).unwrap().remove(0);
        let pin = action(&p, &one, "pin");
        p.activate(&one, &pin).unwrap();
        assert!(p.capture(4, Some("three")));
        let titles: Vec<_> = p
            .search(&query(""), &ctx(10))
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert!(titles.contains(&"one".to_string()));
        assert!(titles.contains(&"three".to_string()));
        assert!(!titles.contains(&"two".to_string()));
    }

    #[test]
    fn all_pinned_history_rejects_new_entries() {
        let (p, _) = provider(1);
        baseline(&p);
        assert!(p.capture(2, Some("only")));
        let only = p.search(&query(""), &ctx(10)).unwrap().remove(0);
        let pin = action(&p, &only, "pin");
        p.activate(&only, &pin).unwrap();
        assert!(!p.capture(3, Some("nope")));
        let items = p.search(&query(""), &ctx(10)).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "only");
    }

    #[test]
    fn aggregate_byte_bound_evicts_oldest_unpinned_entry() {
        let (p, _) = provider(MAX_ENTRIES_LIMIT);
        baseline(&p);
        let chunk = "x".repeat(MAX_ENTRY_BYTES - 6);
        let mut change = 2;
        for i in 0..64 {
            let text = format!("{:0>6}{chunk}", i);
            assert!(p.capture(change, Some(&text)));
            change += 1;
        }
        // Exactly MAX_TOTAL_BYTES is retained.
        assert_eq!(p.search(&query(""), &ctx(200)).unwrap().len(), 64);
        let text = format!("{:0>6}{chunk}", 64);
        assert!(p.capture(change, Some(&text)));
        let items = p.search(&query(""), &ctx(200)).unwrap();
        assert_eq!(items.len(), 64, "aggregate bound caps history at 4 MiB");
        assert!(
            !items.iter().any(|i| i.title.starts_with("000000")),
            "oldest entry should have been evicted"
        );
    }

    #[test]
    fn search_matches_mid_text_and_promotes_pinned() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        assert!(p.capture(2, Some("deploy production cluster")));
        assert!(p.capture(3, Some("grocery list")));
        let grocery = p.search(&query("grocery"), &ctx(10)).unwrap().remove(0);
        let pin = action(&p, &grocery, "pin");
        p.activate(&grocery, &pin).unwrap();
        let items = p.search(&query("list"), &ctx(10)).unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0].subtitle.as_deref(), Some("Pinned"));
        let items = p.search(&query("production"), &ctx(10)).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "deploy production cluster");
    }

    #[test]
    fn search_obeys_cancellation_and_limit_and_lists_newest_first_when_empty() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        for (offset, value) in ["one", "two", "three", "four"].iter().enumerate() {
            assert!(p.capture(2 + offset as i64, Some(value)));
        }
        let titles: Vec<_> = p
            .search(&query(""), &ctx(10))
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["four", "three", "two", "one"]);
        assert_eq!(p.search(&query("o"), &ctx(2)).unwrap().len(), 2);
        let cancelled = ctx(10);
        cancelled.cancellation.cancel();
        assert!(p.search(&query("o"), &cancelled).unwrap().is_empty());
    }

    #[test]
    fn history_is_process_local_and_not_restored_by_a_new_provider() {
        let fake = Arc::new(Fake::default());
        let first = ClipboardProvider::new(fake.clone(), DEFAULT_MAX_ENTRIES);
        baseline(&first);
        assert!(first.capture(2, Some("transient")));
        assert_eq!(first.search(&query(""), &ctx(10)).unwrap().len(), 1);
        // A second provider sharing the same platform sees nothing: there is no
        // on-disk or cross-instance store.
        let second = ClipboardProvider::new(fake, DEFAULT_MAX_ENTRIES);
        assert!(second.search(&query(""), &ctx(10)).unwrap().is_empty());
    }

    #[test]
    fn forged_or_foreign_items_are_rejected_by_actions_preview_and_activate() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("real"));
        let mut forged = entry("forged");
        forged.payload = json!({"entry": 9999});
        assert!(p.actions(&forged).is_empty());
        assert!(
            p.preview(&forged, &CancellationToken::default())
                .unwrap()
                .is_none()
        );
        assert!(
            p.activate(
                &forged,
                &Action {
                    id: ActionId("copy".into()),
                    title: "Copy".into()
                }
            )
            .is_err()
        );
        let mut non_numeric = entry("forged");
        non_numeric.payload = json!({"entry": "1", "text": "secret"});
        assert!(p.actions(&non_numeric).is_empty());
        let mut foreign = entry("foreign");
        foreign.provider = ProviderId("other".into());
        assert!(p.actions(&foreign).is_empty());
    }

    #[test]
    fn copy_is_the_default_action_and_paste_returns_text_outcome() {
        let (p, fake) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("copy me"));
        let item = p.search(&query(""), &ctx(10)).unwrap().remove(0);
        let actions = p.actions(&item);
        assert_eq!(actions[0].id.0, "copy");
        assert!(matches!(
            p.activate(&item, &actions[0]).unwrap(),
            ActionOutcome::Close
        ));
        assert_eq!(fake.copied.lock().unwrap().as_slice(), ["copy me"]);
        let paste = action(&p, &item, "paste");
        assert!(matches!(
            p.activate(&item, &paste).unwrap(),
            ActionOutcome::PasteText(text) if text == "copy me"
        ));
    }

    #[test]
    fn delete_and_clear_return_refresh_search_and_remove_entries() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("one"));
        p.capture(3, Some("two"));
        let two = p.search(&query("two"), &ctx(10)).unwrap().remove(0);
        let delete = action(&p, &two, "delete");
        assert!(matches!(
            p.activate(&two, &delete).unwrap(),
            ActionOutcome::RefreshSearch
        ));
        let items = p.search(&query(""), &ctx(10)).unwrap();
        assert_eq!(items.len(), 1);
        let one = items.into_iter().next().unwrap();
        let clear = action(&p, &one, "clear");
        assert!(matches!(
            p.activate(&one, &clear).unwrap(),
            ActionOutcome::RefreshSearch
        ));
        assert!(p.search(&query(""), &ctx(10)).unwrap().is_empty());
    }

    #[test]
    fn pin_toggle_switches_action_label() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("toggle"));
        let item = p.search(&query(""), &ctx(10)).unwrap().remove(0);
        let pin = action(&p, &item, "pin");
        p.activate(&item, &pin).unwrap();
        assert!(p.actions(&item).iter().any(|a| a.id.0 == "unpin"));
        let unpin = action(&p, &item, "unpin");
        p.activate(&item, &unpin).unwrap();
        assert!(p.actions(&item).iter().any(|a| a.id.0 == "pin"));
    }

    #[test]
    fn item_payload_never_contains_plaintext() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("top secret token"));
        let item = p.search(&query(""), &ctx(10)).unwrap().remove(0);
        let serialized = item.payload.to_string();
        assert!(!serialized.contains("top secret token"));
        assert!(item.payload.get("entry").and_then(|v| v.as_u64()).is_some());
        assert!(item.payload.get("text").is_none());
    }

    #[test]
    fn title_is_a_single_line_excerpt_bounded_to_120_chars() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("first line\nsecond line"));
        p.capture(3, Some(&"z".repeat(500)));
        let line = p.search(&query("first"), &ctx(10)).unwrap().remove(0);
        assert_eq!(line.title, "first line");
        let long = p.search(&query("z"), &ctx(10)).unwrap().remove(0);
        assert_eq!(long.title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(long.title, "z".repeat(MAX_TITLE_CHARS));
    }

    #[test]
    fn new_clamps_configured_history_size() {
        let (p, _) = provider(0);
        baseline(&p);
        assert!(p.capture(2, Some("kept")));
        assert_eq!(p.search(&query(""), &ctx(10)).unwrap().len(), 1);
    }

    #[test]
    fn provider_is_explicit_scope_and_never_records_usage() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        assert!(p.requires_explicit_scope());
        assert!(!p.records_usage());
    }

    #[test]
    fn coordinator_excludes_clipboard_from_mixed_search_but_includes_scoped() {
        let (p, _) = provider(DEFAULT_MAX_ENTRIES);
        baseline(&p);
        p.capture(2, Some("scoped value"));
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(p),
                ProviderConfig {
                    enabled: true,
                    prefix: Some(":".into()),
                    priority: 10,
                    default_search: true,
                    max_results: 10,
                },
            )
            .unwrap();
        let coordinator = SearchCoordinator::new(
            Arc::new(registry),
            Arc::new(HistoryStore::in_memory().unwrap()),
            50,
        );
        let mixed = coordinator
            .start("scoped value".into())
            .receiver
            .recv_blocking()
            .unwrap();
        assert!(mixed.complete);
        assert!(
            mixed.items.is_empty(),
            "mixed search must not leak clipboard"
        );
        let scoped = coordinator
            .start(": scoped value".into())
            .receiver
            .recv_blocking()
            .unwrap();
        assert_eq!(scoped.items.len(), 1);
        assert_eq!(scoped.items[0].provider.0, ID);
    }
}
