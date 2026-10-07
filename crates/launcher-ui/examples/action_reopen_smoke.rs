//! Exercises the real launcher through action completion and repeated reopening.
//! Unlike retained_window_smoke, this includes search, rendering and activation.
use std::{sync::Arc, time::Duration};

use anyhow::Result;
use gpui::{Application, Keystroke, Timer};
use launcher_core::{
    Action, ActionId, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig,
    ProviderId, ProviderRegistry, SearchContext, SearchCoordinator, SearchQuery,
};

struct CloseProvider;
impl Provider for CloseProvider {
    fn id(&self) -> ProviderId {
        ProviderId("smoke".into())
    }
    fn name(&self) -> &str {
        "Smoke"
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("close".into()),
            provider: self.id(),
            title: "Close launcher".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.,
            payload: serde_json::Value::Null,
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("close".into()),
            title: "Close".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}

fn main() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("action/reopen smoke timed out");
        std::process::exit(2);
    });
    Application::new().run(|cx| {
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(CloseProvider), ProviderConfig::default())
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
                width: 680.,
                max_results: 8,
            },
            cx,
        )
        .unwrap();
        cx.spawn(async move |cx| {
            for cycle in 1..=5 {
                cx.update(|cx| {
                    window.update(cx, |launcher, window, cx| launcher.toggle(window, cx))
                })
                .unwrap()
                .unwrap();
                Timer::after(Duration::from_millis(300)).await;
                let active = cx
                    .update(|cx| window.update(cx, |_, window, _| window.is_window_active()))
                    .unwrap()
                    .unwrap();
                assert!(active, "cycle {cycle}: launcher did not reopen");
                eprintln!("cycle {cycle}: reopened");
                cx.update(|cx| {
                    gpui::AnyWindowHandle::from(window).update(cx, |_, window, cx| {
                        window.dispatch_keystroke(Keystroke::parse("enter").unwrap(), cx);
                    })
                })
                .unwrap()
                .unwrap();
                Timer::after(Duration::from_millis(300)).await;
                let active = cx
                    .update(|cx| window.update(cx, |_, window, _| window.is_window_active()))
                    .unwrap()
                    .unwrap();
                assert!(!active, "cycle {cycle}: action did not hide launcher");
                eprintln!("cycle {cycle}: action closed launcher");
            }
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
