use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    CancellationToken, HistoryStore, Item, ProviderId, ProviderRegistry, QueryMode, SearchContext,
    SearchQuery,
};

#[derive(Clone, Debug)]
pub struct ProviderFailure {
    pub provider: ProviderId,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct SearchUpdate {
    pub generation: u64,
    pub items: Vec<Item>,
    pub errors: Vec<ProviderFailure>,
    pub complete: bool,
}

pub struct SearchSession {
    pub generation: u64,
    pub receiver: async_channel::Receiver<SearchUpdate>,
    pub cancellation: CancellationToken,
}

pub struct SearchCoordinator {
    pub registry: Arc<ProviderRegistry>,
    history: Arc<HistoryStore>,
    limit: usize,
    generation: AtomicU64,
    active: std::sync::Mutex<Option<CancellationToken>>,
}

impl SearchCoordinator {
    pub fn new(registry: Arc<ProviderRegistry>, history: Arc<HistoryStore>, limit: usize) -> Self {
        Self {
            registry,
            history,
            limit,
            generation: AtomicU64::new(0),
            active: std::sync::Mutex::new(None),
        }
    }
    pub fn start(&self, raw: String) -> SearchSession {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let cancellation = CancellationToken::default();
        if let Ok(mut active) = self.active.lock()
            && let Some(previous) = active.replace(cancellation.clone())
        {
            previous.cancel();
        }
        let (sender, receiver) = async_channel::bounded(32);
        let parsed = self.registry.parse(&raw);
        let providers: Vec<_> = match &parsed.mode {
            QueryMode::Mixed => self
                .registry
                .providers
                .values()
                .filter(|p| p.config.enabled && p.config.default_search)
                .cloned()
                .collect(),
            QueryMode::Provider(id) => self
                .registry
                .providers
                .get(&id.0)
                .filter(|p| p.config.enabled)
                .cloned()
                .into_iter()
                .collect(),
            QueryMode::ProviderPicker => Vec::new(),
        };
        if matches!(parsed.mode, QueryMode::ProviderPicker) {
            let items = self
                .registry
                .descriptors()
                .into_iter()
                .filter(|p| p.config.enabled && p.config.prefix.is_some())
                .map(|p| Item {
                    id: crate::ItemId(p.id.0.clone()),
                    provider: ProviderId("picker".into()),
                    title: p.name,
                    subtitle: p.prefix.clone(),
                    keywords: vec![],
                    icon: None,
                    score: f64::from(p.config.priority),
                    payload: serde_json::json!({"provider":p.id.0,"prefix":p.prefix}),
                })
                .collect();
            let _ = sender.try_send(SearchUpdate {
                generation,
                items,
                errors: vec![],
                complete: true,
            });
            return SearchSession {
                generation,
                receiver,
                cancellation,
            };
        }
        let registry_priorities: HashMap<String, i32> = providers
            .iter()
            .map(|p| (p.provider.id().0, p.config.priority))
            .collect();
        if providers.is_empty() {
            let _ = sender.try_send(SearchUpdate {
                generation,
                items: Vec::new(),
                errors: Vec::new(),
                complete: true,
            });
            return SearchSession {
                generation,
                receiver,
                cancellation,
            };
        }
        let worker_cancel = cancellation.clone();
        let history = self.history.clone();
        let limit = self.limit;
        let query = SearchQuery {
            raw,
            text: parsed.search_text,
        };
        std::thread::spawn(move || {
            let (updates_tx, updates_rx) = async_channel::unbounded();
            let total = providers.len();
            for registered in providers {
                let tx = updates_tx.clone();
                let query = query.clone();
                let token = worker_cancel.clone();
                let request_generation = generation;
                std::thread::spawn(move || {
                    if token.is_cancelled() {
                        return;
                    }
                    let context = SearchContext {
                        generation: request_generation,
                        cancellation: token.clone(),
                        limit: registered.config.max_results,
                    };
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        registered.provider.search(&query, &context)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("provider panicked")));
                    if !token.is_cancelled() {
                        let _ =
                            tx.send_blocking((registered.provider.id(), registered.config, result));
                    }
                });
            }
            drop(updates_tx);
            let mut finished = 0;
            let mut raw_items: Vec<Item> = Vec::new();
            let mut errors = Vec::new();
            while finished < total && !worker_cancel.is_cancelled() {
                if sender.is_closed() {
                    worker_cancel.cancel();
                    break;
                }
                match updates_rx.try_recv() {
                    Ok((provider, config, result)) => {
                        finished += 1;
                        match result {
                            Ok(mut found) => {
                                found.truncate(config.max_results);
                                raw_items.retain(|old| old.provider != provider);
                                raw_items.extend(found);
                            }
                            Err(error) => errors.push(ProviderFailure {
                                provider,
                                message: format!("{error:#}"),
                            }),
                        }
                        // Populate provider priorities for every result; later batches remain globally ranked.
                        let usage = history.snapshot_for(&raw_items).unwrap_or_default();
                        let mut ranked = raw_items.clone();
                        crate::ranking::rank(
                            &mut ranked,
                            &query.text,
                            &registry_priorities,
                            &usage,
                        );
                        let mut seen = std::collections::HashSet::new();
                        ranked.retain(|item| {
                            seen.insert((item.provider.0.clone(), item.id.0.clone()))
                        });
                        ranked.truncate(limit);
                        let _ = sender.send_blocking(SearchUpdate {
                            generation,
                            items: ranked,
                            errors: errors.clone(),
                            complete: finished == total,
                        });
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                }
            }
        });
        SearchSession {
            generation,
            receiver,
            cancellation,
        }
    }
}
