use crate::{ProviderId, ProviderRegistry};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryMode {
    Mixed,
    Provider(ProviderId),
    ProviderPicker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedQuery {
    pub raw: String,
    pub search_text: String,
    pub mode: QueryMode,
}

impl ProviderRegistry {
    /// Parses configured prefixes. The longest matching prefix wins; word prefixes
    /// require whitespace or end-of-input after the prefix.
    pub fn parse(&self, input: &str) -> ParsedQuery {
        let raw = input.to_owned();
        if input.trim() == ";" {
            return ParsedQuery {
                raw,
                search_text: String::new(),
                mode: QueryMode::ProviderPicker,
            };
        }
        let mut candidates: Vec<_> = self
            .providers
            .values()
            .filter(|p| p.config.enabled)
            .filter_map(|p| p.config.prefix.as_ref().map(|prefix| (prefix.as_str(), p)))
            .collect();
        candidates.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        for (prefix, provider) in candidates {
            let Some(rest) = input.strip_prefix(prefix) else {
                continue;
            };
            if prefix.chars().last().is_some_and(char::is_alphanumeric)
                && !rest.is_empty()
                && !rest.starts_with(char::is_whitespace)
            {
                continue;
            }
            return ParsedQuery {
                raw,
                search_text: rest.trim_start().to_owned(),
                mode: QueryMode::Provider(provider.provider.id()),
            };
        }
        ParsedQuery {
            raw,
            search_text: input.to_owned(),
            mode: QueryMode::Mixed,
        }
    }
}
