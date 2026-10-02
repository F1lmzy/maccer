use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, IconDescriptor, Item, ItemId, Provider, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_macos::{Platform, spotlight::FileSearch};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct FileProvider {
    search: Arc<dyn FileSearch>,
    platform: Arc<dyn Platform>,
    home: PathBuf,
}
impl FileProvider {
    pub fn new(search: Arc<dyn FileSearch>, platform: Arc<dyn Platform>, home: PathBuf) -> Self {
        Self {
            search,
            platform,
            home,
        }
    }
    fn scoped_query(&self, text: &str) -> (Option<PathBuf>, String) {
        if let Some((root, query)) = text.trim().split_once(char::is_whitespace) {
            let path = if let Some(relative) = root.strip_prefix("~/") {
                Some(self.home.join(relative))
            } else if root.starts_with('/') {
                Some(PathBuf::from(root))
            } else {
                None
            };
            if path.is_some() {
                return (path, query.trim().into());
            }
        }
        (None, text.trim().into())
    }
}
impl Provider for FileProvider {
    fn id(&self) -> ProviderId {
        ProviderId("files".into())
    }
    fn name(&self) -> &str {
        "Files"
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if query.text.trim().is_empty() || ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        let (root, text) = self.scoped_query(&query.text);
        if text.is_empty() {
            return Ok(vec![]);
        }
        let paths = self
            .search
            .search(&text, root.as_deref(), &ctx.cancellation, ctx.limit)?;
        Ok(paths
            .into_iter()
            .filter(|p| p.is_absolute())
            .take(ctx.limit)
            .map(|path| Item {
                id: ItemId(format!("file:{}", path.display())),
                provider: self.id(),
                title: path
                    .file_name()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                subtitle: path.parent().map(|p| p.display().to_string()),
                keywords: vec![text.clone()],
                icon: Some(IconDescriptor::Text("▤".into())),
                score: 0.0,
                payload: json!({"path": path}),
            })
            .collect())
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if item.provider != self.id() {
            return vec![];
        }
        [
            ("open", "Open"),
            ("reveal", "Reveal in Finder"),
            ("quick-look", "Quick Look"),
            ("copy-path", "Copy Path"),
        ]
        .into_iter()
        .map(|(id, title)| Action {
            id: ActionId(id.into()),
            title: title.into(),
        })
        .collect()
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if item.provider != self.id() {
            bail!("item is not a file result");
        }
        let path = Path::new(
            item.payload["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("file result has no path"))?,
        );
        if !path.is_absolute() || item.id.0 != format!("file:{}", path.display()) {
            bail!("invalid file identity");
        }
        match action.id.0.as_str() {
            "open" => self.platform.open_path(path)?,
            "reveal" => self.platform.reveal_in_finder(path)?,
            "quick-look" => self.platform.quick_look(path)?,
            "copy-path" => self.platform.copy_text(&path.to_string_lossy())?,
            _ => bail!("unsupported file action"),
        }
        Ok(ActionOutcome::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::CancellationToken;
    struct Files;
    impl FileSearch for Files {
        fn search(
            &self,
            _: &str,
            _: Option<&Path>,
            _: &CancellationToken,
            _: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(vec![
                PathBuf::from("/tmp/report.pdf"),
                PathBuf::from("/tmp/report.txt"),
            ])
        }
    }
    struct Fake;
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
            assert_eq!(text, "/tmp/report.pdf");
            Ok(())
        }
    }
    fn provider() -> FileProvider {
        FileProvider::new(
            Arc::new(Files),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        )
    }
    fn context() -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: CancellationToken::default(),
            limit: 1,
        }
    }
    #[test]
    fn respects_limits_and_has_stable_file_identity() {
        let provider = provider();
        let items = provider
            .search(
                &SearchQuery {
                    raw: "/ report".into(),
                    text: "report".into(),
                },
                &context(),
            )
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.0, "file:/tmp/report.pdf");
        assert_eq!(items[0].title, "report.pdf");
        assert!(
            provider
                .actions(&items[0])
                .iter()
                .any(|a| a.id.0 == "copy-path")
        );
        assert!(
            provider
                .activate(
                    &items[0],
                    &Action {
                        id: ActionId("copy-path".into()),
                        title: "Copy Path".into()
                    }
                )
                .is_ok()
        );
    }
    #[test]
    fn recognizes_directory_scope_and_expands_home_only_at_start() {
        assert_eq!(
            provider().scoped_query("~/dev launcher"),
            (Some(PathBuf::from("/Users/test/dev")), "launcher".into())
        );
        assert_eq!(
            provider().scoped_query("/tmp report"),
            (Some(PathBuf::from("/tmp")), "report".into())
        );
        assert_eq!(
            provider().scoped_query("normal words"),
            (None, "normal words".into())
        );
    }
    #[test]
    fn empty_and_cancelled_searches_do_not_invoke_spotlight() {
        assert!(
            provider()
                .search(&SearchQuery::default(), &context())
                .unwrap()
                .is_empty()
        );
        let context = context();
        context.cancellation.cancel();
        assert!(
            provider()
                .search(
                    &SearchQuery {
                        raw: "/ report".into(),
                        text: "report".into()
                    },
                    &context
                )
                .unwrap()
                .is_empty()
        );
    }
}
