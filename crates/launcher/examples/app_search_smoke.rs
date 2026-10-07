//! Inspect mixed application/web ranking without opening the UI or activating results.
//! cargo run -p launcher --example app_search_smoke -- aerospace
use std::sync::Arc;

use anyhow::Result;
use launcher_core::{HistoryStore, ProviderConfig, ProviderRegistry, SearchCoordinator};
use launcher_macos::{NativePlatform, discover_applications};
use provider_apps::ApplicationProvider;
use provider_web::WebProvider;

fn main() -> Result<()> {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "aerospace".into());
    let platform = Arc::new(NativePlatform);
    let mut registry = ProviderRegistry::new();
    registry.register(
        Arc::new(ApplicationProvider::new(
            discover_applications()?,
            platform.clone(),
        )),
        ProviderConfig {
            priority: 100,
            ..Default::default()
        },
    )?;
    registry.register(
        Arc::new(WebProvider::new(
            platform,
            WebProvider::default_engines(),
            "google".into(),
        )?),
        ProviderConfig {
            priority: 10,
            ..Default::default()
        },
    )?;
    let coordinator =
        SearchCoordinator::new(Arc::new(registry), Arc::new(HistoryStore::in_memory()?), 50);
    let session = coordinator.start(query);
    loop {
        let update = session.receiver.recv_blocking()?;
        if update.complete {
            for (index, item) in update.items.iter().take(5).enumerate() {
                println!("{}. {} [{}]", index + 1, item.title, item.provider.0);
            }
            anyhow::ensure!(
                update.errors.is_empty(),
                "provider errors: {:?}",
                update.errors
            );
            break;
        }
    }
    Ok(())
}
