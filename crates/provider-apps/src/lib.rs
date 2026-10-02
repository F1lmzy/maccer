use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, IconDescriptor, Item, ItemId, Provider, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_macos::{Application, Platform};
use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use serde_json::json;
use std::sync::Arc;

const ID: &str = "apps";
const OPEN: &str = "open";
const REVEAL: &str = "reveal";

pub struct ApplicationProvider {
    applications: Vec<Application>,
    platform: Arc<dyn Platform>,
}

impl ApplicationProvider {
    pub fn new(applications: Vec<Application>, platform: Arc<dyn Platform>) -> Self {
        Self {
            applications,
            platform,
        }
    }

    fn item(app: &Application) -> Item {
        let stable_id = app
            .bundle_id
            .as_deref()
            .map(|id| format!("apps:{id}"))
            .unwrap_or_else(|| format!("apps:{}", app.path.display()));
        let mut keywords = vec![app.path.display().to_string()];
        if let Some(bundle) = &app.bundle_id {
            keywords.push(bundle.clone());
        }
        if let Some(executable) = &app.executable_name {
            keywords.push(executable.clone());
        }
        Item {
            id: ItemId(stable_id),
            provider: ProviderId(ID.into()),
            title: app.display_name.clone(),
            subtitle: Some("Application".into()),
            keywords,
            // .icns is metadata, not a GPUI image. Leave rendering to a later icon-loader slice.
            icon: app
                .icon_path
                .as_ref()
                .map(|_| IconDescriptor::Text("Application".into())),
            score: 0.0,
            payload: json!({"path": app.path}),
        }
    }

    fn path_for<'a>(&'a self, item: &Item) -> Result<&'a std::path::Path> {
        if item.provider != ProviderId(ID.into()) {
            bail!("item is not an application result")
        }
        let path = item
            .payload
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("application result has no path"))?;
        self.applications
            .iter()
            .find(|app| app.path == std::path::Path::new(path))
            .map(|app| app.path.as_path())
            .ok_or_else(|| anyhow::anyhow!("application is not in this provider's index"))
    }
}

impl Provider for ApplicationProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ID.into())
    }
    fn name(&self) -> &str {
        "Applications"
    }

    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() {
            return Ok(Vec::new());
        }
        let pattern = Pattern::parse(&query.text, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut scored = Vec::new();
        for app in &self.applications {
            if ctx.cancellation.is_cancelled() {
                return Ok(Vec::new());
            }
            let item = Self::item(app);
            let haystack = format!("{} {}", item.title, item.keywords.join(" "));
            let mut chars = Vec::new();
            if let Some(score) = pattern.score(
                nucleo_matcher::Utf32Str::new(&haystack, &mut chars),
                &mut matcher,
            ) {
                let mut item = item;
                item.score = f64::from(score);
                scored.push(item);
            }
        }
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        scored.truncate(ctx.limit);
        Ok(scored)
    }

    fn actions(&self, item: &Item) -> Vec<Action> {
        if self.path_for(item).is_err() {
            return Vec::new();
        }
        vec![
            Action {
                id: ActionId(OPEN.into()),
                title: "Open".into(),
            },
            Action {
                id: ActionId(REVEAL.into()),
                title: "Reveal in Finder".into(),
            },
        ]
    }

    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        let path = self.path_for(item)?;
        match action.id.0.as_str() {
            OPEN => self.platform.launch_application(path)?,
            REVEAL => self.platform.reveal_in_finder(path)?,
            _ => bail!("unsupported application action: {}", action.id.0),
        }
        Ok(ActionOutcome::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::CancellationToken;
    use std::{path::Path, sync::Mutex};
    #[derive(Default)]
    struct FakePlatform(Mutex<Vec<String>>);
    impl Platform for FakePlatform {
        fn launch_application(&self, p: &Path) -> Result<()> {
            self.0.lock().unwrap().push(format!("open:{}", p.display()));
            Ok(())
        }
        fn reveal_in_finder(&self, p: &Path) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("reveal:{}", p.display()));
            Ok(())
        }
        fn open_url(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn copy_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }
    fn app(name: &str, bundle: &str) -> Application {
        Application {
            path: format!("/Applications/{name}.app").into(),
            bundle_id: Some(bundle.into()),
            display_name: name.into(),
            executable_name: Some(name.to_lowercase()),
            icon_path: None,
        }
    }
    fn ctx(limit: usize) -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: CancellationToken::default(),
            limit,
        }
    }
    #[test]
    fn finds_by_name_bundle_and_executable_and_limits_after_ranking() {
        let p = ApplicationProvider::new(
            vec![app("Safari", "com.apple.safari"), app("Safe", "org.safe")],
            Arc::new(FakePlatform::default()),
        );
        let found = p
            .search(
                &SearchQuery {
                    raw: "saf".into(),
                    text: "saf".into(),
                },
                &ctx(1),
            )
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Safari");
        assert_eq!(
            p.search(
                &SearchQuery {
                    raw: "com.apple".into(),
                    text: "com.apple".into()
                },
                &ctx(5)
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            p.search(
                &SearchQuery {
                    raw: "safari".into(),
                    text: "safari".into()
                },
                &ctx(5)
            )
            .unwrap()[0]
                .id
                .0,
            "apps:com.apple.safari"
        );
    }
    #[test]
    fn observes_cancelled_search_before_scanning() {
        let p = ApplicationProvider::new(
            vec![app("Safari", "com.apple.safari")],
            Arc::new(FakePlatform::default()),
        );
        let token = CancellationToken::default();
        token.cancel();
        let c = SearchContext {
            generation: 2,
            cancellation: token,
            limit: 10,
        };
        assert!(p.search(&SearchQuery::default(), &c).unwrap().is_empty());
    }
    #[test]
    fn activates_only_supported_actions_for_indexed_items() {
        let platform = Arc::new(FakePlatform::default());
        let p = ApplicationProvider::new(vec![app("Safari", "safari")], platform.clone());
        let item = p
            .search(
                &SearchQuery {
                    raw: "Safari".into(),
                    text: "Safari".into(),
                },
                &ctx(1),
            )
            .unwrap()
            .remove(0);
        assert!(
            p.activate(
                &item,
                &Action {
                    id: ActionId("unknown".into()),
                    title: "?".into()
                }
            )
            .is_err()
        );
        p.activate(&item, &p.actions(&item)[0]).unwrap();
        assert_eq!(
            platform.0.lock().unwrap().as_slice(),
            ["open:/Applications/Safari.app"]
        );
    }
}
