//! Configured web engines behind server-retained action references.
use crate::core::{Action, ActionOutcome, Item, Provider, ProviderId, SearchContext, SearchQuery};
use crate::protocol::{DomainData, Icon};
use crate::providers::web::WebProvider;
use crate::service::{ItemPresentation, ServiceProvider};
use anyhow::Result;

pub struct WebServiceProvider {
    inner: WebProvider,
}
impl WebServiceProvider {
    pub fn new(inner: WebProvider) -> Self {
        Self { inner }
    }
}
impl Provider for WebServiceProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        let mut items = self.inner.search(query, ctx)?;
        // Core merging sorts equal scores by title. Preserve the configured
        // default-engine order rather than alphabetically preferring another engine.
        for (index, item) in items.iter_mut().enumerate() {
            item.score = -(index as f64);
        }
        Ok(items)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        self.inner.actions(item)
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        self.inner.activate(item, action)
    }
}
impl ServiceProvider for WebServiceProvider {
    fn description(&self) -> &str {
        "Search the web with configured engines"
    }
    fn kind(&self) -> &str {
        "web"
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        // The service never forwards provider payloads, so the URL the GPUI
        // frontend may render in its native WKWebView travels as versioned,
        // display-only domain data.
        let data = item
            .payload
            .get("url")
            .and_then(|value| value.as_str())
            .and_then(|url| {
                let parsed = url::Url::parse(url).ok()?;
                (matches!(parsed.scheme(), "http" | "https")
                    && parsed.host_str().is_some()
                    && url.len() <= 4096)
                    .then(|| DomainData::WebLink {
                        schema_version: 1,
                        url: url.to_owned(),
                    })
            });
        ItemPresentation {
            kind: "web".into(),
            icon: Some(Icon::Glyph {
                name: "web".into(),
                fallback_text: "↗".into(),
            }),
            content: vec![],
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ItemId, Provider};
    use crate::macos::Platform;
    use std::{path::Path, sync::Arc};

    #[derive(Default)]
    struct Desktop;
    impl Platform for Desktop {
        fn launch_application(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn reveal_in_finder(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn open_url(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn copy_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn provider() -> WebServiceProvider {
        WebServiceProvider::new(
            WebProvider::new(
                Arc::new(Desktop),
                WebProvider::default_engines(),
                "google".into(),
            )
            .unwrap(),
        )
    }
    fn item(url: &str) -> Item {
        Item {
            id: ItemId("web:google".into()),
            provider: ProviderId("web".into()),
            title: "Search with Google".into(),
            subtitle: Some("Web".into()),
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: serde_json::json!({"url": url, "engine": "google"}),
        }
    }

    #[test]
    fn presentation_publishes_a_versioned_web_link_for_http_urls() {
        let presentation = provider().presentation(&item("https://www.google.com/search?q=rust"));
        assert_eq!(presentation.kind, "web");
        let Some(DomainData::WebLink {
            schema_version,
            url,
        }) = presentation.data
        else {
            panic!("expected webLink data");
        };
        assert_eq!(schema_version, 1);
        assert_eq!(url, "https://www.google.com/search?q=rust");
        assert!(presentation.content.is_empty());
    }

    #[test]
    fn presentation_drops_untrusted_or_oversized_urls() {
        let provider = provider();
        for url in ["file:///etc/passwd", "javascript:alert(1)", "not a url"] {
            assert!(provider.presentation(&item(url)).data.is_none(), "{url}");
        }
        let long = format!("https://example.com/{}", "a".repeat(5000));
        assert!(provider.presentation(&item(&long)).data.is_none());
    }

    #[test]
    fn service_search_keeps_the_engine_order_and_actions() {
        let provider = provider();
        let query = SearchQuery {
            raw: "rust".into(),
            text: "rust".into(),
        };
        let context = SearchContext {
            generation: 1,
            cancellation: Default::default(),
            limit: 4,
        };
        let items = provider.search(&query, &context).unwrap();
        assert_eq!(items[0].payload["engine"], "google");
        assert_eq!(provider.actions(&items[0]).len(), 2);
        assert_eq!(provider.kind(), "web");
        assert!(provider.presentation(&items[0]).data.is_some());
    }
}
