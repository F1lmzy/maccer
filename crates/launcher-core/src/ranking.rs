use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{Item, UsageStats};

pub(crate) fn rank(
    items: &mut [Item],
    query: &str,
    priorities: &std::collections::HashMap<String, i32>,
    usage: &std::collections::HashMap<(String, String), UsageStats>,
) {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    for item in items.iter_mut() {
        let text = format!("{} {}", item.title, item.keywords.join(" "));
        let mut chars = Vec::new();
        let score = pattern
            .score(
                nucleo_matcher::Utf32Str::new(&text, &mut chars),
                &mut matcher,
            )
            .unwrap_or(0) as f64;
        let stats = usage.get(&(item.provider.0.clone(), item.id.0.clone()));
        let uses = stats.map_or(0, |stats| stats.count);
        item.score += score
            + f64::from(*priorities.get(&item.provider.0).unwrap_or(&0))
            + (uses as f64).ln_1p()
            + stats
                .and_then(|stats| stats.last_used)
                .map_or(0.0, |timestamp| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let age_days = now.saturating_sub(timestamp).max(0) as f64 / 86_400.0;
                    1.0 / (1.0 + age_days / 30.0)
                });
    }
    items.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.provider.0.cmp(&b.provider.0))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
}
