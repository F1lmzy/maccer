//! Read-only fd-index diagnostic. Does not open files or record usage.
//! cargo run -p provider-files --example search_smoke -- crates /absolute/root
use launcher_core::{CancellationToken, Provider, SearchContext, SearchQuery};
use launcher_macos::{
    NativePlatform,
    fd_index::{FdConfig, FdIndex},
    spotlight::FileSearch,
};
use provider_files::FileProvider;
use std::{path::PathBuf, sync::Arc, time::Instant};

fn main() -> anyhow::Result<()> {
    let text = std::env::args().nth(1).unwrap_or_default();
    let root = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let cache = tempfile::tempdir()?;
    let index = FdIndex::open(
        FdConfig {
            roots: vec![root.clone()],
            excluded: vec![],
            ignored_dirs: vec![],
            watch: false,
        },
        &cache.path().join("files.db"),
    )?;
    let token = CancellationToken::default();
    let started = Instant::now();
    index.refresh(&token)?;
    println!(
        "fd_index_ms={} indexed_entries={}",
        started.elapsed().as_millis(),
        index.entry_count()
    );
    let started = Instant::now();
    let candidates = index.search(&text, Some(&root), &token, usize::MAX)?;
    println!(
        "candidate_count={} discovery_ms={}",
        candidates.len(),
        started.elapsed().as_millis()
    );
    let provider = FileProvider::new(index, Arc::new(NativePlatform), root.clone());
    for _ in 0..2 {
        let started = Instant::now();
        let results = provider.search(
            &SearchQuery {
                raw: text.clone(),
                text: text.clone(),
            },
            &SearchContext {
                generation: 1,
                cancellation: token.clone(),
                limit: 50,
            },
        )?;
        assert!(results.len() <= 50);
        assert!(
            results
                .windows(2)
                .all(|pair| pair[0].score >= pair[1].score)
        );
        assert!(
            results.iter().all(
                |item| PathBuf::from(item.payload["path"].as_str().unwrap()).starts_with(&root)
            )
        );
        println!(
            "results={} cached_query_ms={}",
            results.len(),
            started.elapsed().as_millis()
        );
    }
    Ok(())
}
