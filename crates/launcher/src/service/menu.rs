use crate::core::{IconDescriptor, Item, ItemId, ProviderId};
use crate::protocol::{ContentBlock, DomainData, Icon};

#[derive(Clone)]
pub struct ItemPresentation {
    pub kind: String,
    pub icon: Option<Icon>,
    pub content: Vec<ContentBlock>,
    pub data: Option<DomainData>,
}

/// Returns a lower-is-better edit-gap score for a fuzzy menu match.
pub(crate) fn score(query: &str, candidate: &str) -> Option<u32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let mut total = 0u32;
    for word in query.split_whitespace() {
        if let Some(at) = candidate.find(word) {
            total = total.saturating_add(at as u32);
            continue;
        }
        let mut chars = candidate.chars().enumerate();
        let mut last = None;
        let mut gaps = 0;
        for wanted in word.chars() {
            let (index, _) = chars.by_ref().find(|(_, got)| *got == wanted)?;
            if let Some(previous) = last {
                gaps += (index - previous - 1) as u32;
            }
            last = Some(index);
        }
        total = total.saturating_add(100 + gaps);
    }
    Some(total)
}

/// True when a query names a provider directly, so its menu row should stay
/// above search results. Short prefixes are ignored: one or two letters match
/// too many menus to count as deliberate navigation.
pub(crate) fn direct_match(query: &str, name: &str, kind: &str, id: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return false;
    }
    let prefix_ok = query.chars().count() >= 3;
    [name, kind, id].iter().any(|candidate| {
        let candidate = candidate.to_lowercase();
        candidate == query || (prefix_ok && candidate.starts_with(&query))
    })
}

pub(crate) fn menu_item(id: &str, name: &str, description: &str) -> Item {
    Item {
        id: ItemId(id.into()),
        provider: ProviderId("menu".into()),
        title: name.into(),
        subtitle: Some(description.into()),
        keywords: vec![id.into()],
        icon: Some(IconDescriptor::Text("›".into())),
        score: 0.0,
        payload: serde_json::Value::Null,
    }
}
pub(crate) fn menu_presentation(kind: &str) -> ItemPresentation {
    ItemPresentation {
        kind: "provider".into(),
        icon: Some(Icon::Glyph {
            name: kind.into(),
            fallback_text: "•".into(),
        }),
        content: vec![],
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fuzzy_menu_matches_and_scores_closer_matches_first() {
        assert!(score("thmes", "Themes").is_some());
        assert!(score("thmes", "Apps").is_none());
        assert!(score("apps", "Apps").unwrap() < score("apps", "Applications").unwrap());
    }

    #[test]
    fn fuzzy_menu_matches_do_not_count_as_direct_names() {
        assert!(
            score(
                "weather",
                "Web Search Search the web with configured engines web web"
            )
            .is_some()
        );
        assert!(!direct_match("weather", "Web Search", "web", "web"));
    }

    #[test]
    fn direct_matches_accept_exact_names_ids_kinds_and_prefixes() {
        assert!(direct_match("Themes", "Themes", "theme", "themes"));
        assert!(direct_match("theme", "Themes", "theme", "themes"));
        assert!(direct_match("web", "Web Search", "web", "web"));
        assert!(direct_match("clip", "Clipboard", "clipboard", "clipboard"));
        assert!(direct_match("apps", "Applications", "application", "apps"));
        assert!(!direct_match("thmes", "Themes", "theme", "themes"));
        assert!(!direct_match("th", "Themes", "theme", "themes"));
        assert!(!direct_match("", "Themes", "theme", "themes"));
    }
}
