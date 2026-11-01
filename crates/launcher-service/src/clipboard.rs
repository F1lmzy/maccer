//! Explicit-scope, RAM-only clipboard adapter. Paste requires a future verified
//! previous-application handoff and is deliberately not authorized here.
use crate::{ItemPresentation, ServiceProvider};
use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionOutcome, CancellationToken, Item, Preview, Provider, ProviderId, SearchContext,
    SearchQuery,
};
use launcher_protocol::{ContentBlock, Icon};
use provider_clipboard::ClipboardProvider;
use std::sync::Arc;

/// Main-thread backend capture only; frontends never own a second timer.
/// `new` does not inspect the pasteboard. The first tick establishes a baseline.
pub struct ClipboardMonitor {
    provider: Arc<ClipboardProvider>,
    previous: Option<i64>,
    next_poll: std::time::Instant,
}
impl ClipboardMonitor {
    pub fn new(provider: Arc<ClipboardProvider>) -> Self {
        Self {
            provider,
            previous: None,
            next_poll: std::time::Instant::now(),
        }
    }
    pub fn tick(&mut self) {
        self.poll_with(
            std::time::Instant::now(),
            launcher_macos::clipboard::snapshot_if_changed,
        );
    }
    fn poll_with(
        &mut self,
        now: std::time::Instant,
        read: impl FnOnce(Option<i64>) -> Result<Option<launcher_macos::clipboard::ClipboardSnapshot>>,
    ) {
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + std::time::Duration::from_millis(300);
        // Errors and rejected text never get logged or written to disk.
        if let Ok(Some(snapshot)) = read(self.previous) {
            let baseline = self.previous.is_none();
            self.previous = Some(snapshot.change_count);
            self.provider.capture(
                snapshot.change_count,
                if baseline {
                    None
                } else {
                    snapshot.text.as_deref()
                },
            );
        }
    }
}

pub struct ClipboardServiceProvider {
    inner: Arc<ClipboardProvider>,
}
impl ClipboardServiceProvider {
    pub fn new(inner: Arc<ClipboardProvider>) -> Self {
        Self { inner }
    }
}
impl Provider for ClipboardServiceProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn revision(&self) -> u64 {
        self.inner.revision()
    }
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn records_usage(&self) -> bool {
        false
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        self.inner.search(query, ctx)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        self.inner
            .actions(item)
            .into_iter()
            .filter(|action| {
                matches!(
                    action.id.0.as_str(),
                    "copy" | "pin" | "unpin" | "delete" | "clear"
                )
            })
            .collect()
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if !self
            .actions(item)
            .iter()
            .any(|allowed| allowed.id == action.id)
        {
            bail!("clipboard action is unavailable");
        }
        self.inner.activate(item, action)
    }
}
impl ServiceProvider for ClipboardServiceProvider {
    fn description(&self) -> &str {
        "Text clipboard history, captured in memory by the backend"
    }
    fn kind(&self) -> &str {
        "clipboard"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "clipboard".into(),
            icon: Some(Icon::Glyph {
                name: "clipboard".into(),
                fallback_text: "▤".into(),
            }),
            content: vec![],
            data: None,
        }
    }
    fn details_available(&self, item: &Item) -> bool {
        self.inner.supports_preview(item)
    }
    fn details(&self, item: &Item) -> Result<Vec<ContentBlock>> {
        let Some(Preview::Text { text, .. }) =
            self.inner.preview(item, &CancellationToken::default())?
        else {
            return Ok(vec![]);
        };
        // Bound encoded size, including worst-case JSON escaping of control
        // characters. Keep ample room under the 64 KiB details envelope.
        let mut encoded_bytes = 0;
        let mut end = 0;
        for (offset, ch) in text.char_indices() {
            let cost = match ch {
                '"' | '\\' => 2,
                '\u{0000}'..='\u{001f}' => 6,
                _ => ch.len_utf8(),
            };
            if encoded_bytes + cost > 48 * 1024 {
                break;
            }
            encoded_bytes += cost;
            end = offset + ch.len_utf8();
        }
        Ok(vec![ContentBlock::Text {
            text: text[..end].into(),
            truncated: end < text.len(),
            fallback_text: "Clipboard text preview".into(),
        }])
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use launcher_macos::clipboard::ClipboardSnapshot;
    use std::time::{Duration, Instant};

    #[test]
    fn capture_baseline_throttle_and_filtered_changes_are_backend_owned() {
        struct Inert;
        impl launcher_macos::Platform for Inert {
            fn launch_application(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn reveal_in_finder(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn open_url(&self, _: &str) -> Result<()> {
                unreachable!()
            }
            fn copy_text(&self, _: &str) -> Result<()> {
                unreachable!()
            }
        }
        let provider = Arc::new(ClipboardProvider::new(Arc::new(Inert), 20));
        let mut monitor = ClipboardMonitor::new(provider.clone());
        let now = Instant::now();
        monitor.poll_with(now, |previous| {
            assert_eq!(previous, None);
            Ok(Some(ClipboardSnapshot {
                change_count: 10,
                text: Some("before launch".into()),
            }))
        });
        assert_eq!(provider.revision(), 0);
        monitor.poll_with(now + Duration::from_millis(100), |_| {
            panic!("poll must be throttled")
        });
        monitor.poll_with(now + Duration::from_millis(300), |previous| {
            assert_eq!(previous, Some(10));
            Ok(Some(ClipboardSnapshot {
                change_count: 11,
                text: Some("new owned text".into()),
            }))
        });
        assert_eq!(provider.revision(), 1);
        monitor.poll_with(now + Duration::from_millis(600), |_| {
            Ok(Some(ClipboardSnapshot {
                change_count: 12,
                text: None,
            }))
        });
        assert_eq!(provider.revision(), 1, "filtered data cannot enter history");
        monitor.poll_with(now + Duration::from_millis(900), |_| {
            Err(anyhow::anyhow!("owned read failure"))
        });
        monitor.poll_with(now + Duration::from_millis(1200), |previous| {
            assert_eq!(previous, Some(12));
            Ok(Some(ClipboardSnapshot {
                change_count: 13,
                text: Some("recovered".into()),
            }))
        });
        assert_eq!(provider.revision(), 2);
    }
}
