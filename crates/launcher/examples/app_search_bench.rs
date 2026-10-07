//! cargo run --release -p launcher --example app_search_bench -- safari
use anyhow::Result;
use launcher_core::{
    CancellationToken, HistoryStore, Provider, ProviderConfig, ProviderRegistry, SearchContext,
    SearchCoordinator, SearchQuery,
};
use launcher_macos::{NativePlatform, discover_applications};
use provider_apps::ApplicationProvider;
use std::{sync::Arc, time::Instant};

fn main() -> Result<()> {
    let text = std::env::args().nth(1).unwrap_or_else(|| "safari".into());
    let started = Instant::now();
    let apps = discover_applications()?;
    println!("discovery: {} apps in {:?}", apps.len(), started.elapsed());
    let started = Instant::now();
    let provider = ApplicationProvider::new(apps, Arc::new(NativePlatform));
    println!("index construction: {:?}", started.elapsed());
    let ctx = SearchContext {
        generation: 1,
        cancellation: CancellationToken::default(),
        limit: 50,
    };
    let query = SearchQuery {
        raw: text.clone(),
        text,
    };
    let started = Instant::now();
    let results = provider.search(&query, &ctx)?;
    println!(
        "cold search: {:?}, {} results",
        started.elapsed(),
        results.len()
    );
    let mut timings = Vec::new();
    for _ in 0..100 {
        let started = Instant::now();
        std::hint::black_box(provider.search(&query, &ctx)?);
        timings.push(started.elapsed());
    }
    timings.sort();
    println!(
        "warm search: p50 {:?}, p95 {:?}, max {:?}",
        timings[50], timings[95], timings[99]
    );
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(provider), ProviderConfig::default())?;
    let coordinator =
        SearchCoordinator::new(Arc::new(registry), Arc::new(HistoryStore::in_memory()?), 50);
    let mut timings = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let session = coordinator.start(query.text.clone());
        loop {
            let update = session.receiver.recv_blocking()?;
            anyhow::ensure!(
                update.errors.is_empty(),
                "provider errors: {:?}",
                update.errors
            );
            if update.complete {
                break;
            }
        }
        timings.push(started.elapsed());
    }
    timings.sort();
    println!(
        "coordinator to complete results: p50 {:?}, p95 {:?}",
        timings[10], timings[19]
    );
    Ok(())
}
