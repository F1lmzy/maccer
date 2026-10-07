//! Runs the native selection -> provider -> UI preview pipeline on one image.
//! RUST_LOG=debug cargo run -p launcher --example preview_ui_smoke -- /path/to/image.png
//! Reads the supplied file; performs no file actions or history persistence.
use anyhow::Result;
use gpui::{Application, Timer};
use launcher_core::{
    Action, ActionOutcome, CancellationToken, HistoryStore, Item, ItemId, Preview, Provider,
    ProviderConfig, ProviderId, ProviderRegistry, SearchContext, SearchCoordinator, SearchQuery,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct ImageProvider {
    path: PathBuf,
    completed: Arc<AtomicBool>,
}
impl Provider for ImageProvider {
    fn id(&self) -> ProviderId {
        ProviderId("preview-smoke".into())
    }
    fn name(&self) -> &str {
        "Image preview smoke"
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("image".into()),
            provider: self.id(),
            title: "Image preview smoke".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.,
            payload: serde_json::Value::Null,
        }])
    }
    fn supports_preview(&self, _: &Item) -> bool {
        true
    }
    fn preview(&self, _: &Item, token: &CancellationToken) -> Result<Option<Preview>> {
        let result = launcher_macos::file_preview::preview(&self.path, token);
        self.completed.store(true, Ordering::Release);
        result.map(Some)
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        anyhow::bail!("no smoke actions")
    }
}
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let path = PathBuf::from(std::env::args_os().nth(1).expect("provide an image path"));
    assert!(path.is_file());
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        eprintln!("preview smoke timed out");
        std::process::exit(2);
    });
    Application::new().run(move |cx| {
        let completed = Arc::new(AtomicBool::new(false));
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(ImageProvider {
                    path,
                    completed: completed.clone(),
                }),
                ProviderConfig::default(),
            )
            .unwrap();
        let coordinator = Arc::new(SearchCoordinator::new(
            Arc::new(registry),
            history.clone(),
            8,
        ));
        let window = launcher_ui::open_launcher(
            coordinator,
            history,
            launcher_ui::LauncherOptions {
                width: 480.,
                max_results: 8,
            },
            cx,
        )
        .unwrap();
        window
            .update(cx, |launcher, window, cx| launcher.show(window, cx))
            .unwrap();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_secs(3)).await;
            assert!(
                completed.load(Ordering::Acquire),
                "preview worker never completed"
            );
            for (index, expected_width) in [760., 480., 760., 480., 760.].into_iter().enumerate() {
                if index > 0 {
                    cx.update(|cx| {
                        gpui::AnyWindowHandle::from(window).update(cx, |_, window, cx| {
                            window.dispatch_keystroke(
                                gpui::Keystroke::parse("cmd-shift-p").unwrap(),
                                cx,
                            );
                        })
                    })
                    .unwrap()
                    .unwrap();
                    Timer::after(Duration::from_millis(200)).await;
                }
                cx.update(|cx| {
                    window.update(cx, |_, window, cx| {
                        let bounds = window.bounds();
                        let center = window.display(cx).unwrap().bounds().center();
                        assert!((bounds.center().x - center.x).abs() <= gpui::px(1.));
                        assert!((bounds.center().y - center.y).abs() <= gpui::px(1.));
                        assert!(
                            (bounds.size.width - gpui::px(expected_width)).abs() <= gpui::px(1.)
                        );
                        eprintln!("native centered bounds: {bounds:?}");
                    })
                })
                .unwrap()
                .unwrap();
            }
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
