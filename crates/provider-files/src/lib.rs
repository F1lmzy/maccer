use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, HistoryStore, IconDescriptor, Item, ItemId, Provider,
    ProviderId, SearchContext, SearchQuery,
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
    roots: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
    history: Option<Arc<HistoryStore>>,
    ignored_previews: Vec<PathBuf>,
}
impl FileProvider {
    pub fn new(search: Arc<dyn FileSearch>, platform: Arc<dyn Platform>, home: PathBuf) -> Self {
        Self {
            search,
            platform,
            roots: vec![home.clone()],
            home,
            excluded: vec![],
            history: None,
            ignored_previews: vec![],
        }
    }
    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }
    pub fn with_excluded_paths(mut self, excluded: Vec<PathBuf>) -> Self {
        self.excluded = excluded;
        self
    }
    pub fn with_history(mut self, history: Arc<HistoryStore>) -> Self {
        self.history = Some(history);
        self
    }
    pub fn with_ignored_previews(mut self, paths: Vec<PathBuf>) -> Self {
        self.ignored_previews = paths
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or(p))
            .collect();
        self
    }
    fn file_path<'a>(&self, item: &'a Item) -> Result<&'a Path> {
        let path = Path::new(
            item.payload["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("file result has no path"))?,
        );
        if item.provider != self.id()
            || !path.is_absolute()
            || item.id.0 != format!("file:{}", path.display())
        {
            bail!("invalid file identity");
        }
        Ok(path)
    }
    fn scoped_query(&self, text: &str) -> (Option<PathBuf>, String) {
        if let Some((root, query)) = text.trim().split_once(char::is_whitespace) {
            let path = if let Some(relative) = root.strip_prefix("~/") {
                Some(self.home.join(relative.trim_start_matches('/')))
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
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn revision(&self) -> u64 {
        self.search.revision()
    }
    fn status(&self) -> Option<String> {
        self.search.status()
    }
    fn supports_preview(&self, item: &Item) -> bool {
        self.file_path(item).is_ok()
    }
    fn preview_revision(&self, item: &Item) -> u64 {
        // Rendering must never stat files on the UI thread. The index publishes
        // per-file change stamps along with each background snapshot.
        self.file_path(item)
            .map_or(0, |path| self.search.preview_revision(path))
    }
    fn supports_drag(&self, item: &Item) -> bool {
        self.file_path(item).is_ok()
    }
    fn preview(
        &self,
        item: &Item,
        token: &launcher_core::CancellationToken,
    ) -> Result<Option<launcher_core::Preview>> {
        let path = self.file_path(item)?;
        if self
            .ignored_previews
            .iter()
            .any(|ignored| path.starts_with(ignored))
        {
            return Ok(Some(launcher_core::Preview::Info(
                "Preview disabled for this directory".into(),
            )));
        }
        launcher_macos::file_preview::preview(path, token).map(Some)
    }
    fn begin_drag(&self, item: &Item) -> Result<()> {
        self.platform.begin_file_drag(self.file_path(item)?)
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        let indexed = self.search.is_indexed();
        if (!indexed && query.text.trim().is_empty()) || ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        let (mut root, mut text) = self.scoped_query(&query.text);
        if indexed {
            // Only interpret an existing directory as an explicit scope. A
            // full-path query containing spaces must remain a full-path query.
            if root.as_ref().is_some_and(|p| !p.is_dir()) {
                root = None;
                text = query.text.trim().into();
            }
            if root.is_none()
                && let Some(relative) = text.strip_prefix("~/")
            {
                text = self
                    .home
                    .join(relative.trim_start_matches('/'))
                    .display()
                    .to_string();
            }
        }
        if !indexed && text.is_empty() {
            return Ok(vec![]);
        }
        use nucleo_matcher::{
            Config, Matcher, Utf32Str,
            pattern::{AtomKind, CaseMatching, Normalization, Pattern},
        };
        let explicit_scope = root.is_some();
        let roots = root.map(|p| vec![p]).unwrap_or_else(|| self.roots.clone());
        let mut paths = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Ask for a wider pool: an index's early results can be mostly build
        // artifacts. Filter noise before spending time on per-file metadata.
        for root in &roots {
            if ctx.cancellation.is_cancelled() {
                return Ok(vec![]);
            }
            let found = self.search.search(
                &text,
                Some(root),
                &ctx.cancellation,
                if indexed { usize::MAX } else { 10_000 },
            )?;
            for path in found {
                if (indexed || useful_path(&path, root, &self.home, explicit_scope, &self.excluded))
                    && !self
                        .excluded
                        .iter()
                        .any(|excluded| path.starts_with(excluded))
                    && seen.insert(path.clone())
                {
                    paths.push(path);
                }
                if !indexed && paths.len() >= 2048 {
                    break;
                }
            }
            if !indexed && paths.len() >= 2048 {
                break;
            }
        }
        let directories = if indexed {
            vec![]
        } else {
            self.search.preferred_directories(&ctx.cancellation)
        };
        let max_directory_score = directories
            .iter()
            .map(|d| d.score.ln_1p())
            .fold(1.0, f64::max);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::new(
            &text,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut candidates = Vec::new();
        for (position, path) in paths.into_iter().enumerate() {
            if ctx.cancellation.is_cancelled() {
                return Ok(vec![]);
            }
            let title = path
                .file_name()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut buffer = Vec::new();
            let Some(matched) = pattern.score(
                Utf32Str::new(
                    if indexed {
                        path.to_str().unwrap_or(&title)
                    } else {
                        &title
                    },
                    &mut buffer,
                ),
                &mut matcher,
            ) else {
                continue;
            };
            let metadata = self.search.metadata(&path);
            let directory = directories
                .iter()
                .take(256)
                .filter(|d| {
                    d.path != self.home && !roots.contains(&d.path) && path.starts_with(&d.path)
                })
                .max_by_key(|d| d.path.components().count());
            let directory_bonus =
                directory.map_or(0.0, |d| 24.0 * d.score.ln_1p() / max_directory_score);
            let personal_bonus = if !indexed
                && ["Documents", "Desktop", "Downloads"]
                    .iter()
                    .any(|dir| path.starts_with(self.home.join(dir)))
            {
                4.0
            } else {
                0.0
            };
            let browse_bonus = if indexed && text.is_empty() {
                40.0 / (1.0 + position as f64 / 50.0)
            } else {
                0.0
            };
            let score = browse_bonus
                + f64::from(matched) * 0.2
                + directory_bonus
                + personal_bonus
                + 20.0 * recency(metadata.last_used, now)
                + (metadata.use_count as f64).ln_1p().min(4.0) * 5.0
                + (if indexed && text.is_empty() {
                    80.0
                } else {
                    4.0
                }) * recency(metadata.modified, now);
            let directory = self.search.is_directory(&path);
            candidates.push(Item {
                id: ItemId(format!("file:{}", path.display())),
                provider: self.id(),
                title: if directory {
                    format!("{title}/")
                } else {
                    title
                },
                subtitle: path.parent().map(|p| p.display().to_string()),
                keywords: if indexed {
                    vec![path.display().to_string()]
                } else {
                    vec![]
                },
                icon: Some(IconDescriptor::Text(
                    if directory { "▸" } else { "▤" }.into(),
                )),
                score,
                payload: json!({"path": path}),
            });
        }
        // Apply launcher history while the full candidate pool is still present.
        // Ranking only the first eight/50 would permanently hide a frequently opened file.
        if let Some(history) = &self.history {
            let usage = history.snapshot_for(&candidates)?;
            for item in &mut candidates {
                if let Some(stats) = usage.get(&(item.provider.0.clone(), item.id.0.clone())) {
                    item.score += (stats.count as f64).ln_1p().min(5.0) * 12.0
                        + 12.0 * recency(stats.last_used, now);
                }
            }
        }
        sort_candidates(&mut candidates);
        let paths: Vec<_> = candidates
            .iter()
            .filter_map(|item| item.payload["path"].as_str().map(PathBuf::from))
            .collect();
        if let Some(matched) = self.search.fuzzy_matches(&text, &paths, &ctx.cancellation) {
            let positions: std::collections::HashMap<_, _> = matched
                .into_iter()
                .enumerate()
                .map(|(index, path)| (path, index))
                .collect();
            candidates.retain_mut(|item| {
                let path = PathBuf::from(item.payload["path"].as_str().unwrap_or_default());
                if let Some(index) = positions.get(&path) {
                    item.score += 24.0 / (1.0 + *index as f64 / 20.0);
                    true
                } else {
                    false
                }
            });
            sort_candidates(&mut candidates);
        }
        if ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        candidates.truncate(ctx.limit);
        Ok(candidates)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if item.provider != self.id() {
            return vec![];
        }
        let mut actions: Vec<_> = [
            ("open", "Open"),
            ("reveal", "Reveal in Finder"),
            ("open-directory", "Open Containing Folder"),
            ("quick-look", "Quick Look"),
            ("copy-path", "Copy Path"),
            ("copy-file", "Copy File"),
        ]
        .into_iter()
        .map(|(id, title)| Action {
            id: ActionId(id.into()),
            title: title.into(),
        })
        .collect();
        if self.search.is_indexed() {
            actions.push(Action {
                id: ActionId("refresh-index".into()),
                title: "Refresh File Index".into(),
            });
        }
        actions
    }
    fn activate_with_cancellation(
        &self,
        item: &Item,
        action: &Action,
        token: &launcher_core::CancellationToken,
    ) -> Result<ActionOutcome> {
        if token.is_cancelled() {
            bail!("action cancelled");
        }
        if action.id.0 == "refresh-index" {
            self.file_path(item)?;
            self.search.refresh_index(token)?;
            return Ok(ActionOutcome::KeepOpen("File index refreshed".into()));
        }
        self.activate(item, action)
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        let path = self.file_path(item)?;
        if action.id.0 == "refresh-index" {
            return self.activate_with_cancellation(
                item,
                action,
                &launcher_core::CancellationToken::default(),
            );
        }
        match action.id.0.as_str() {
            "open" => self.platform.open_path(path)?,
            "reveal" => self.platform.reveal_in_finder(path)?,
            "open-directory" => self.platform.open_path(path.parent().unwrap_or(path))?,
            "copy-file" => self.platform.copy_file(path)?,
            "quick-look" => self.platform.quick_look(path)?,
            "copy-path" => self.platform.copy_text(&path.to_string_lossy())?,
            _ => bail!("unsupported file action"),
        }
        Ok(ActionOutcome::Close)
    }
}

fn sort_candidates(items: &mut [Item]) {
    items.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
}

fn recency(timestamp: Option<i64>, now: i64) -> f64 {
    timestamp.map_or(0.0, |t| {
        let days = now.saturating_sub(t).max(0) as f64 / 86_400.0;
        1.0 / (1.0 + days / 30.0)
    })
}

fn useful_path(
    path: &Path,
    root: &Path,
    home: &Path,
    explicit: bool,
    excluded: &[PathBuf],
) -> bool {
    use std::path::Component;
    if !path.is_absolute()
        || !path.starts_with(root)
        || excluded.iter().any(|p| path.starts_with(p))
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return false;
    }
    if explicit {
        return true;
    }
    let library = home.join("Library");
    if path.starts_with(&library)
        && !path.starts_with(library.join("Mobile Documents"))
        && !path.starts_with(library.join("CloudStorage"))
    {
        return false;
    }
    !path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|component| {
            let name = component.as_os_str().to_string_lossy();
            name.starts_with('.')
                || name.ends_with(".app")
                || matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "build"
                        | "dist"
                        | "vendor"
                        | "Caches"
                        | "__pycache__"
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::CancellationToken;
    struct Files;

    #[derive(Default)]
    struct Transfers(std::sync::Mutex<Vec<String>>);
    impl Platform for Transfers {
        fn launch_application(&self, path: &Path) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("open:{}", path.display()));
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
        fn copy_file(&self, path: &Path) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("copy:{}", path.display()));
            Ok(())
        }
        fn begin_file_drag(&self, path: &Path) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("drag:{}", path.display()));
            Ok(())
        }
    }
    #[test]
    fn file_copy_drag_and_containing_folder_use_the_validated_item_path() {
        let transfers = Arc::new(Transfers::default());
        let p = FileProvider::new(
            Arc::new(IndexedFiles),
            transfers.clone(),
            "/Users/test".into(),
        );
        let mut item = p.search(&query("invoice"), &context()).unwrap().remove(0);
        for id in ["copy-file", "open-directory"] {
            p.activate(
                &item,
                &Action {
                    id: ActionId(id.into()),
                    title: id.into(),
                },
            )
            .unwrap();
        }
        p.begin_drag(&item).unwrap();
        assert_eq!(
            *transfers.0.lock().unwrap(),
            [
                "copy:/Users/test/Projects/client/invoice.pdf",
                "open:/Users/test/Projects/client",
                "drag:/Users/test/Projects/client/invoice.pdf"
            ]
        );
        item.id = ItemId("file:/unrelated".into());
        assert!(p.begin_drag(&item).is_err());
    }
    struct IndexedFiles;
    impl FileSearch for IndexedFiles {
        fn is_indexed(&self) -> bool {
            true
        }
        fn search(
            &self,
            _: &str,
            _: Option<&Path>,
            _: &CancellationToken,
            _: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(vec![
                "/Users/test/Projects/client/invoice.pdf".into(),
                "/Users/test/Projects/client".into(),
            ])
        }
    }
    #[test]
    fn files_only_participate_in_prefixed_search_even_with_default_search_enabled() {
        use launcher_core::{HistoryStore, ProviderConfig, ProviderRegistry, SearchCoordinator};

        let provider =
            FileProvider::new(Arc::new(IndexedFiles), Arc::new(Fake), "/Users/test".into());
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(provider),
                ProviderConfig {
                    prefix: Some("/".into()),
                    default_search: true,
                    ..ProviderConfig::default()
                },
            )
            .unwrap();
        let coordinator = SearchCoordinator::new(
            Arc::new(registry),
            Arc::new(HistoryStore::in_memory().unwrap()),
            50,
        );
        for (query, expected_count) in [("", 0), ("client", 0), ("/", 2), ("/ client", 2)] {
            let session = coordinator.start(query.into());
            loop {
                let update = session.receiver.recv_blocking().unwrap();
                assert!(update.errors.is_empty());
                if update.complete {
                    assert_eq!(update.items.len(), expected_count, "query: {query:?}");
                    break;
                }
            }
        }
    }

    #[test]
    fn indexed_files_support_path_matching_and_recent_browsing() {
        let provider =
            FileProvider::new(Arc::new(IndexedFiles), Arc::new(Fake), "/Users/test".into());
        let mut ctx = context();
        ctx.limit = 50;
        let results = provider.search(&query("client"), &ctx).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(provider.search(&query(""), &ctx).unwrap().len(), 2);
        assert!(
            provider
                .actions(&results[0])
                .iter()
                .any(|a| a.id.0 == "copy-file")
        );
        assert!(
            provider
                .actions(&results[0])
                .iter()
                .any(|a| a.id.0 == "open-directory")
        );
    }
    impl FileSearch for Files {
        fn search(
            &self,
            _: &str,
            _: Option<&Path>,
            _: &CancellationToken,
            _: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(vec![
                PathBuf::from("/Users/test/Documents/report.pdf"),
                PathBuf::from("/Users/test/Documents/report.txt"),
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
            assert_eq!(text, "/Users/test/Documents/report.pdf");
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
    struct CandidateFiles(Vec<PathBuf>);
    impl FileSearch for CandidateFiles {
        fn search(
            &self,
            _: &str,
            _: Option<&Path>,
            _: &CancellationToken,
            limit: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(self.0.iter().take(limit).cloned().collect())
        }
    }
    fn candidates(paths: &[&str]) -> FileProvider {
        FileProvider::new(
            Arc::new(CandidateFiles(paths.iter().map(PathBuf::from).collect())),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        )
    }
    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            raw: format!("/ {text}"),
            text: text.into(),
        }
    }

    #[test]
    fn default_search_filters_system_cache_build_and_hidden_paths_before_limiting() {
        let p = candidates(&[
            "/System/Library/report.plist",
            "/Users/test/Library/Caches/report.dat",
            "/Users/test/project/target/report.d",
            "/Users/test/project/node_modules/report.js",
            "/Users/test/.config/report.toml",
            "/Users/test/Apps/Something.app/Contents/report.json",
            "/Users/test/Documents/report.pdf",
        ]);
        let items = p.search(&query("report"), &context()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.0, "file:/Users/test/Documents/report.pdf");
    }

    #[test]
    fn file_matching_is_fuzzy_and_best_match_is_chosen_before_provider_limit() {
        let p = candidates(&[
            "/Users/test/Documents/unrelated.txt",
            "/Users/test/Documents/monthly-report.pdf",
        ]);
        let items = p.search(&query("mntrpt"), &context()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "monthly-report.pdf");
        assert!(
            items[0].keywords.is_empty(),
            "query must not be echoed as a matching keyword"
        );
    }

    #[test]
    fn enough_results_survive_for_scrolling_beyond_eight_rows() {
        let p = FileProvider::new(
            Arc::new(CandidateFiles(
                (0..30)
                    .map(|i| PathBuf::from(format!("/Users/test/Documents/report-{i:02}.pdf")))
                    .collect(),
            )),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        );
        let mut context = context();
        context.limit = 50;
        assert_eq!(p.search(&query("report"), &context).unwrap().len(), 30);
    }

    #[test]
    fn launcher_usage_is_applied_before_a_top_one_limit() {
        let paths: Vec<_> = (0..80)
            .map(|i| PathBuf::from(format!("/Users/test/Documents/{i:02}/report.pdf")))
            .collect();
        let target = format!("file:{}", paths[79].display());
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        history
            .record(&launcher_core::UsageEvent {
                query: "report".into(),
                provider: ProviderId("files".into()),
                item: ItemId(target.clone()),
                action: ActionId("open".into()),
                timestamp: now(),
            })
            .unwrap();
        let p = FileProvider::new(
            Arc::new(CandidateFiles(paths)),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        )
        .with_history(history);
        assert_eq!(
            p.search(&query("report"), &context()).unwrap()[0].id.0,
            target
        );
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
    struct UsageFiles {
        last_used: bool,
        zoxide: bool,
    }
    impl FileSearch for UsageFiles {
        fn search(
            &self,
            _: &str,
            _: Option<&Path>,
            _: &CancellationToken,
            _: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(vec![
                PathBuf::from("/Users/test/Projects/archive/report.pdf"),
                PathBuf::from("/Users/test/Projects/current/report.pdf"),
            ])
        }
        fn metadata(&self, path: &Path) -> launcher_macos::spotlight::FileMetadata {
            let recent = path.starts_with("/Users/test/Projects/current");
            launcher_macos::spotlight::FileMetadata {
                last_used: (self.last_used && recent).then(now),
                use_count: if self.last_used && recent { 10 } else { 0 },
                modified: (!recent).then(now),
            }
        }
        fn preferred_directories(
            &self,
            _: &CancellationToken,
        ) -> Vec<launcher_macos::spotlight::DirectoryUsage> {
            if self.zoxide {
                vec![launcher_macos::spotlight::DirectoryUsage {
                    path: PathBuf::from("/Users/test/Projects/current"),
                    score: 900.0,
                }]
            } else {
                vec![]
            }
        }
    }
    #[test]
    fn last_opened_files_outrank_recently_built_but_unused_files() {
        let p = FileProvider::new(
            Arc::new(UsageFiles {
                last_used: true,
                zoxide: false,
            }),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        );
        assert_eq!(
            p.search(&query("report"), &context()).unwrap()[0].id.0,
            "file:/Users/test/Projects/current/report.pdf"
        );
    }
    #[test]
    fn zoxide_directory_history_favours_the_users_current_project() {
        let p = FileProvider::new(
            Arc::new(UsageFiles {
                last_used: false,
                zoxide: true,
            }),
            Arc::new(Fake),
            PathBuf::from("/Users/test"),
        );
        assert_eq!(
            p.search(&query("report"), &context()).unwrap()[0].id.0,
            "file:/Users/test/Projects/current/report.pdf"
        );
    }
    #[test]
    fn cloud_documents_are_kept_while_library_internals_are_removed() {
        let p = candidates(&[
            "/Users/test/Library/Containers/app/report.dat",
            "/Users/test/Library/Mobile Documents/Cloud/report.pdf",
        ]);
        let items = p.search(&query("report"), &context()).unwrap();
        assert_eq!(
            items[0].id.0,
            "file:/Users/test/Library/Mobile Documents/Cloud/report.pdf"
        );
    }
    #[test]
    fn explicit_directory_scope_can_find_files_outside_home() {
        let p = candidates(&["/System/Library/report.plist"]);
        assert_eq!(
            p.search(&query("/System/Library report"), &context())
                .unwrap()[0]
                .id
                .0,
            "file:/System/Library/report.plist"
        );
    }
    #[test]
    fn configured_exclusions_still_apply_to_explicit_scopes() {
        let p = candidates(&["/Users/test/Documents/private/report.pdf"])
            .with_excluded_paths(vec![PathBuf::from("/Users/test/Documents/private")]);
        assert!(
            p.search(&query("/Users/test/Documents report"), &context())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn query_operators_are_literal_filename_characters() {
        let p = candidates(&[
            "/Users/test/Documents/budget.pdf",
            "/Users/test/Documents/'budget.pdf",
        ]);
        let items = p.search(&query("'budget"), &context()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "'budget.pdf");
    }

    #[test]
    fn repeated_slashes_in_a_tilde_scope_stay_under_home() {
        let p = provider();
        assert_eq!(
            p.scoped_query("~//Documents report"),
            (
                Some(PathBuf::from("/Users/test/Documents")),
                "report".into()
            )
        );
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
        assert_eq!(items[0].id.0, "file:/Users/test/Documents/report.pdf");
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
