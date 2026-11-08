//! Read-only catalog browsing uses the ordinary service view/details contract.
use launcher_agent::client::{Client, RemoteError};
use launcher_agent::core::{HistoryStore, ProviderConfig};
use launcher_agent::protocol::{
    ContentBlock, DomainData, GetDetailsParams, InvokeActionParams, Route, ThemePalette,
};
use launcher_agent::service::{
    Server, Service, ServiceProvider, ThemeCatalog, ThemeServiceProvider, ThemerState,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

fn palettes() -> Vec<ThemePalette> {
    let mut dark = ThemerState::fixture().snapshot().palette.unwrap();
    dark.slug = "night-blue".into();
    dark.name = "Night Blue".into();
    let mut light = dark.clone();
    light.slug = "day-white".into();
    light.name = "Day White".into();
    light.variant = "light".into();
    light.palette.insert("background".into(), "#ffffff".into());
    vec![dark, light]
}

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl Running {
    fn new(enabled: bool, theme: Arc<ThemerState>) -> Self {
        Self::with_catalog(
            ThemeCatalog::from_palettes(palettes()).unwrap(),
            enabled,
            theme,
        )
    }
    fn with_catalog(catalog: Arc<ThemeCatalog>, enabled: bool, theme: Arc<ThemerState>) -> Self {
        Self::start(Arc::new(ThemeServiceProvider::new(catalog)), enabled, theme)
    }
    /// A catalog that can also apply. The helper path is never executed by these
    /// tests: they only read the prompt and the action list.
    fn with_apply_catalog(
        catalog: Arc<ThemeCatalog>,
        enabled: bool,
        theme: Arc<ThemerState>,
    ) -> Self {
        Self::start(
            Arc::new(ThemeServiceProvider::with_apply(
                catalog,
                launcher_agent::service::ThemeApply::new(std::path::PathBuf::from(
                    "/nonexistent/mac-themer",
                )),
            )),
            enabled,
            theme,
        )
    }
    fn start(provider: Arc<dyn ServiceProvider>, enabled: bool, theme: Arc<ThemerState>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let service = Service::with_state(
            vec![(
                provider,
                ProviderConfig {
                    enabled,
                    max_results: 50,
                    ..Default::default()
                },
            )],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            theme,
        )
        .unwrap();
        let dir = tempfile::Builder::new()
            .prefix("mc-cat-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || server.serve_until(&worker_stop));
        Self {
            dir,
            stop,
            worker: Some(worker),
        }
    }
    fn client(&self) -> Client {
        Client::connect(self.dir.path().join("rpc")).unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

#[test]
fn home_and_scope_search_real_catalog_with_lazy_path_free_details() {
    let running = Running::new(true, ThemerState::fixture());
    let mut client = running.client();
    let home = client.open_view(Route::Home, "", 20).unwrap();
    assert!(
        home.sections
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| i.id == "themes")
    );
    let global = client.query_view(&home.view_id, "nght blu").unwrap();
    let dark = global
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.id == "themes:night-blue")
        .unwrap();
    assert_eq!(dark.title, "Night Blue");
    assert!(dark.actions.is_empty());
    assert!(dark.details_available);
    assert!(
        matches!(&dark.data, Some(DomainData::ThemeSummary{slug, variant, ..}) if slug == "night-blue" && variant == "dark")
    );
    let scoped = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "",
            20,
        )
        .unwrap();
    assert_eq!(
        scoped.sections.iter().map(|s| s.items.len()).sum::<usize>(),
        2
    );
    let item = &scoped.sections[0].items[0];
    let details = client
        .get_details(&GetDetailsParams {
            view_id: scoped.view_id.clone(),
            revision: scoped.revision.clone(),
            provider_id: "themes".into(),
            item_id: item.id.clone(),
        })
        .unwrap();
    assert!(details.content.iter().any(|b| matches!(b, ContentBlock::ColorPalette{colors,..} if colors.iter().any(|c| c.value == "#ffffff"))));
    let wire = serde_json::to_string(&details).unwrap();
    assert!(!wire.contains("pack") && !wire.contains("/Users/") && !wire.contains("payload"));
}

#[test]
fn browsing_cannot_apply_a_theme_or_replace_active_state() {
    let theme = ThemerState::fixture();
    let before = theme.snapshot();
    let running = Running::new(true, theme.clone());
    let mut first = running.client();
    let mut second = running.client();
    let view = first
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "day",
            10,
        )
        .unwrap();
    assert_eq!(view.sections[0].items.len(), 1);
    assert!(view.sections[0].items[0].actions.is_empty());
    first
        .get_details(&GetDetailsParams {
            view_id: view.view_id.clone(),
            revision: view.revision.clone(),
            provider_id: "themes".into(),
            item_id: view.sections[0].items[0].id.clone(),
        })
        .unwrap();
    let error = first
        .invoke(&InvokeActionParams {
            operation_id: "must-not-apply".into(),
            view_id: view.view_id,
            revision: view.revision,
            action_ref: "apply".into(),
            confirmed: false,
            input: Default::default(),
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RemoteError>().unwrap().0.data.code,
        "STALE_ACTION"
    );
    assert_eq!(second.theme_state().unwrap(), before);
    assert_eq!(theme.snapshot(), before);
}

#[test]
fn catalog_failure_retains_entries_but_expires_old_detail_authority() {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("catalog.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({"schema":1,"themes":palettes()})).unwrap(),
    )
    .unwrap();
    let helper = dir.path().join("mac-themer");
    std::fs::write(&helper, "#!/bin/sh\n/bin/cat \"${0%/*}/catalog.json\"\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = ThemeCatalog::start(helper);
    let before = catalog.snapshot();
    assert!(before.error.is_none());
    let running = Running::with_catalog(catalog.clone(), true, ThemerState::fixture());
    let mut client = running.client();
    let view = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "night",
            10,
        )
        .unwrap();
    let params = GetDetailsParams {
        view_id: view.view_id.clone(),
        revision: view.revision.clone(),
        provider_id: "themes".into(),
        item_id: view.sections[0].items[0].id.clone(),
    };
    client.get_details(&params).unwrap();
    std::fs::write(file, b"not JSON").unwrap();
    let deadline = Instant::now() + Duration::from_secs(23);
    while catalog.snapshot().error.is_none() {
        assert!(Instant::now() < deadline, "catalog failed to reconcile");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(catalog.snapshot().palettes, before.palettes);
    assert_ne!(catalog.snapshot().revision, before.revision);
    let error = client.get_details(&params).unwrap_err();
    assert_eq!(
        error.downcast_ref::<RemoteError>().unwrap().0.data.code,
        "STALE_DETAILS"
    );
    let refreshed = client.query_view(&view.view_id, "night").unwrap();
    assert_eq!(refreshed.sections[0].items.len(), 1);
    assert!(refreshed.sections[0].items[0].actions.is_empty());
    client
        .get_details(&GetDetailsParams {
            revision: refreshed.revision,
            ..params
        })
        .unwrap();
}

#[test]
fn a_theme_publishes_the_wallpapers_it_ships_and_names_one_on_apply() {
    use launcher_agent::protocol::ThemeWallpaper;
    use std::collections::BTreeMap;

    let mut wallpapers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    wallpapers.insert(
        "night-blue".into(),
        vec!["1-totoro.webp".into(), "2-waves.webp".into()],
    );
    let catalog = ThemeCatalog::from_catalog(palettes(), wallpapers).unwrap();
    // Read-only browsing still publishes the association, so a picker can show it.
    let running = Running::with_catalog(catalog.clone(), true, ThemerState::fixture());
    let mut client = running.client();
    let scoped = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "night",
            20,
        )
        .unwrap();
    let item = &scoped.sections[0].items[0];
    assert_eq!(item.id, "themes:night-blue");
    assert_eq!(
        item.subtitle.as_deref(),
        Some("night-blue · dark · preview · 2 wallpapers")
    );
    match &item.data {
        Some(DomainData::ThemeSummary { wallpapers, .. }) => {
            assert_eq!(
                wallpapers,
                &vec![
                    ThemeWallpaper {
                        id: "1-totoro.webp".into(),
                        label: "Totoro".into()
                    },
                    ThemeWallpaper {
                        id: "2-waves.webp".into(),
                        label: "Waves".into()
                    },
                ]
            );
        }
        other => panic!("expected a theme summary, got {other:?}"),
    }
    // The other pack ships none and says so instead of inventing one.
    let all = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "day",
            20,
        )
        .unwrap();
    let plain = &all.sections[0].items[0];
    assert_eq!(plain.id, "themes:day-white");
    assert_eq!(
        plain.subtitle.as_deref(),
        Some("day-white · light · preview · no wallpaper")
    );
    assert!(matches!(
        &plain.data,
        Some(DomainData::ThemeSummary { wallpapers, .. }) if wallpapers.is_empty()
    ));

    // With mutation enabled, Apply names the wallpaper it will set.
    let applying = Running::with_apply_catalog(
        ThemeCatalog::from_catalog(
            palettes(),
            [(
                "night-blue".to_owned(),
                vec!["1-totoro.webp".to_owned(), "2-waves.webp".to_owned()],
            )]
            .into_iter()
            .collect(),
        )
        .unwrap(),
        true,
        ThemerState::fixture(),
    );
    let mut client = applying.client();
    let scoped = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "night",
            20,
        )
        .unwrap();
    let item = &scoped.sections[0].items[0];
    assert_eq!(item.actions.len(), 1);
    assert_eq!(item.actions[0].title, "Apply theme…");
    assert_eq!(
        item.actions[0].confirmation.as_deref(),
        Some(
            "Apply \"Night Blue\"? This writes theme files, updates app settings, reloads apps and sets the desktop wallpaper to \"Totoro\"."
        )
    );
}

#[test]
fn a_pack_without_wallpapers_says_the_wallpaper_will_not_change() {
    let applying = Running::with_apply_catalog(
        ThemeCatalog::from_palettes(palettes()).unwrap(),
        true,
        ThemerState::fixture(),
    );
    let mut client = applying.client();
    let view = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "night",
            20,
        )
        .unwrap();
    let item = &view.sections[0].items[0];
    assert_eq!(
        item.actions[0].confirmation.as_deref(),
        Some(
            "Apply \"Night Blue\"? This writes theme files, updates app settings and reloads apps. The wallpaper will not change."
        )
    );
}

#[test]
fn disabled_theme_provider_exposes_neither_results_nor_routes() {
    let running = Running::new(false, ThemerState::fixture());
    let mut client = running.client();
    let home = client.open_view(Route::Home, "night", 20).unwrap();
    assert!(home.sections.iter().all(|s| s.items.is_empty()));
    assert!(
        client
            .open_view(
                Route::Provider {
                    provider_id: "themes".into()
                },
                "",
                20
            )
            .is_err()
    );
}
