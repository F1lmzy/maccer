use anyhow::{Context, Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, IconDescriptor, Item, ItemId, Provider, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_macos::Platform;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use url::Url;

const ID: &str = "web";
const SEARCH: &str = "search";
const COPY: &str = "copy-url";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebEngine {
    pub id: String,
    pub name: String,
    pub url: String,
}

pub struct WebProvider {
    platform: Arc<dyn Platform>,
    engines: Vec<WebEngine>,
    default_engine: String,
}
impl WebProvider {
    pub fn default_engines() -> Vec<WebEngine> {
        vec![
            WebEngine {
                id: "google".into(),
                name: "Google".into(),
                url: "https://www.google.com/search?q={query}".into(),
            },
            WebEngine {
                id: "duckduckgo".into(),
                name: "DuckDuckGo".into(),
                url: "https://duckduckgo.com/?q={query}".into(),
            },
        ]
    }
    pub fn new(
        platform: Arc<dyn Platform>,
        engines: Vec<WebEngine>,
        default_engine: String,
    ) -> Result<Self> {
        if engines.is_empty() {
            bail!("at least one web engine is required");
        }
        let mut ids = std::collections::HashSet::new();
        for engine in &engines {
            if engine.id.trim().is_empty() || !ids.insert(engine.id.clone()) {
                bail!("web engine IDs must be non-empty and unique");
            }
            validate_template(&engine.url)?;
        }
        if !engines.iter().any(|e| e.id == default_engine) {
            bail!("default web engine '{default_engine}' is not configured");
        }
        Ok(Self {
            platform,
            engines,
            default_engine,
        })
    }
    fn build_url(engine: &WebEngine, query: &str) -> Result<String> {
        validate_template(&engine.url)?;
        let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
        let raw = engine.url.replace("{query}", &encoded);
        let url = Url::parse(&raw).context("invalid generated web search URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("web search URL must use HTTP or HTTPS");
        }
        Ok(url.into())
    }
}
fn validate_template(template: &str) -> Result<()> {
    if !template.contains("{query}") {
        bail!("web engine URL must contain {{query}}");
    }
    let query_or_fragment = template.find(['?', '#']).ok_or_else(|| {
        anyhow::anyhow!("web engine URL must place {{query}} in its query or fragment")
    })?;
    if template
        .match_indices("{query}")
        .any(|(index, _)| index < query_or_fragment)
    {
        bail!("web engine {{query}} placeholder cannot appear in the URL authority or path");
    }
    let base = template.replace("{query}", "query");
    let parsed = Url::parse(&base).context("web engine URL must be an absolute URL")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("web engine URL must use HTTP or HTTPS");
    }
    Ok(())
}
impl Provider for WebProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ID.into())
    }
    fn name(&self) -> &str {
        "Web Search"
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 || query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut engines = self.engines.iter().collect::<Vec<_>>();
        engines.sort_by_key(|e| if e.id == self.default_engine { 0 } else { 1 });
        engines
            .into_iter()
            .take(ctx.limit)
            .map(|engine| {
                let url = Self::build_url(engine, &query.text)?;
                Ok(Item {
                    id: ItemId(format!("web:{}", engine.id)),
                    provider: ProviderId(ID.into()),
                    title: format!("Search with {}", engine.name),
                    subtitle: Some("Web".into()),
                    keywords: vec![query.text.clone()],
                    icon: Some(IconDescriptor::Text("↗".into())),
                    score: 0.0,
                    payload: json!({"engine":engine.id,"url":url,"query":query.text}),
                })
            })
            .collect()
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if item.provider == ProviderId(ID.into())
            && item.payload.get("url").and_then(|v| v.as_str()).is_some()
        {
            vec![
                Action {
                    id: ActionId(SEARCH.into()),
                    title: "Search".into(),
                },
                Action {
                    id: ActionId(COPY.into()),
                    title: "Copy Search URL".into(),
                },
            ]
        } else {
            Vec::new()
        }
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if item.provider != ProviderId(ID.into()) {
            bail!("item is not a web result");
        }
        let url = item
            .payload
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("web result has no URL"))?;
        let parsed = Url::parse(url).context("invalid web result URL")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("only absolute HTTP and HTTPS URLs can be used");
        }
        match action.id.0.as_str() {
            SEARCH => self.platform.open_url(url)?,
            COPY => self.platform.copy_text(url)?,
            _ => bail!("unsupported web action: {}", action.id.0),
        }
        Ok(ActionOutcome::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::Mutex};
    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);
    impl Platform for Fake {
        fn launch_application(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn reveal_in_finder(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn open_url(&self, s: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("open:{s}"));
            Ok(())
        }
        fn copy_text(&self, s: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("copy:{s}"));
            Ok(())
        }
    }
    fn ctx(limit: usize) -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: Default::default(),
            limit,
        }
    }
    fn query(s: &str) -> SearchQuery {
        SearchQuery {
            raw: s.into(),
            text: s.into(),
        }
    }
    fn provider(f: Arc<Fake>) -> WebProvider {
        WebProvider::new(f, WebProvider::default_engines(), "google".into()).unwrap()
    }
    #[test]
    fn encodes_unicode_and_url_delimiters_in_search_text() {
        let p = provider(Arc::new(Fake::default()));
        let got = p.search(&query("rust & gpui/你好"), &ctx(1)).unwrap();
        let url = got[0].payload["url"].as_str().unwrap();
        assert!(url.contains("rust+%26+gpui%2F%E4%BD%A0%E5%A5%BD"));
        assert_eq!(got[0].id.0, "web:google");
    }
    #[test]
    fn validates_templates_scheme_placeholder_default_and_unique_ids() {
        let f = Arc::new(Fake::default());
        let make = |url: &str| {
            vec![WebEngine {
                id: "x".into(),
                name: "X".into(),
                url: url.into(),
            }]
        };
        assert!(WebProvider::new(f.clone(), make("file:///tmp/?q={query}"), "x".into()).is_err());
        assert!(
            WebProvider::new(
                f.clone(),
                make("https://{query}.example/?q={query}"),
                "x".into()
            )
            .is_err()
        );
        assert!(WebProvider::new(f.clone(), make("https://example.com/"), "x".into()).is_err());
        assert!(
            WebProvider::new(
                f.clone(),
                make("https://example.com/?q={query}"),
                "missing".into()
            )
            .is_err()
        );
    }
    #[test]
    fn default_engine_first_and_actions_are_validated() {
        let f = Arc::new(Fake::default());
        let p = provider(f.clone());
        let mut items = p.search(&query("hello"), &ctx(4)).unwrap();
        assert_eq!(items[0].payload["engine"], "google");
        let item = items.remove(0);
        p.activate(&item, &p.actions(&item)[0]).unwrap();
        assert!(f.0.lock().unwrap()[0].starts_with("open:https://www.google.com/search?q=hello"));
    }
    #[test]
    fn rejects_untrusted_non_http_action_url() {
        let f = Arc::new(Fake::default());
        let p = provider(f);
        let mut item = p.search(&query("hello"), &ctx(1)).unwrap().remove(0);
        item.payload["url"] = "file:///etc/passwd".into();
        assert!(
            p.activate(&item, &p.actions(&item).into_iter().next().unwrap())
                .is_err()
        );
    }
}
