mod history;
mod provider;
mod query;
mod ranking;
mod search;
mod selection;
#[cfg(test)]
mod tests;

pub use history::{HistoryStore, UsageEvent, UsageStats};
pub use provider::{
    Action, ActionId, ActionOutcome, CancellationToken, IconDescriptor, Item, ItemId, Preview,
    Provider, ProviderConfig, ProviderDescriptor, ProviderId, ProviderRegistry, SearchContext,
    SearchQuery,
};
pub use query::{ParsedQuery, QueryMode};
pub use search::{ProviderFailure, SearchCoordinator, SearchSession, SearchUpdate};
pub use selection::Selection;
