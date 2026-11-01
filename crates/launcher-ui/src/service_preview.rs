//! Bounded, frontend-neutral service preview decoding for the GPUI renderer.
use serde_json::Value;

pub(crate) const MAX_PREVIEW_BLOCKS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ServicePreview {
    pub provider: String,
    pub id: String,
    pub blocks: Vec<ServicePreviewBlock>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ServicePreviewBlock {
    Text {
        text: String,
        fallback: String,
    },
    Value {
        value: String,
        label: Option<String>,
        fallback: String,
    },
    Metadata {
        entries: Vec<(String, String)>,
        fallback: String,
    },
    Palette {
        colors: Vec<String>,
        fallback: String,
    },
    Image {
        source: Option<String>,
        alt: String,
        fallback: String,
    },
    Unknown {
        fallback: String,
    },
}

impl ServicePreview {
    pub fn from_event(provider: String, id: String, content: &[Value]) -> Self {
        Self {
            provider,
            id,
            blocks: content
                .iter()
                .take(MAX_PREVIEW_BLOCKS)
                .map(ServicePreviewBlock::from)
                .collect(),
        }
    }
}

impl ServicePreview {
    pub fn elements(
        &self,
        images: &std::collections::HashMap<std::path::PathBuf, std::sync::Arc<gpui::RenderImage>>,
    ) -> Vec<gpui::AnyElement> {
        use gpui::{div, prelude::*, px};
        self.blocks
            .iter()
            .map(|block| {
                let base = div().mb_2();
                match block {
                    ServicePreviewBlock::Text { text, .. } => {
                        base.child(text.clone()).into_any_element()
                    }
                    ServicePreviewBlock::Value { value, label, .. } => base
                        .child(label.clone().unwrap_or_default())
                        .child(div().text_lg().child(value.clone()))
                        .into_any_element(),
                    ServicePreviewBlock::Metadata { entries, fallback } => {
                        if entries.is_empty() {
                            base.child(fallback.clone()).into_any_element()
                        } else {
                            base.children(
                                entries
                                    .iter()
                                    .map(|(label, value)| div().child(format!("{label}: {value}"))),
                            )
                            .into_any_element()
                        }
                    }
                    ServicePreviewBlock::Palette { colors, fallback } => base
                        .child(fallback.clone())
                        .child(div().flex().flex_wrap().gap_1().children(colors.iter().map(
                            |color| {
                                let rgb =
                                    u32::from_str_radix(&color.trim_start_matches('#')[..6], 16)
                                        .unwrap_or(0);
                                div().w(px(24.)).h(px(24.)).bg(gpui::rgb(rgb))
                            },
                        )))
                        .into_any_element(),
                    ServicePreviewBlock::Image {
                        source,
                        alt,
                        fallback,
                    } => {
                        if let Some(image) = source
                            .as_ref()
                            .and_then(|source| images.get(&std::path::PathBuf::from(source)))
                        {
                            base.child(
                                gpui::img(image.clone())
                                    .w(px(64.))
                                    .h(px(64.))
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                            .child(alt.clone())
                            .into_any_element()
                        } else {
                            base.child(fallback.clone()).into_any_element()
                        }
                    }
                    ServicePreviewBlock::Unknown { fallback } => {
                        base.child(fallback.clone()).into_any_element()
                    }
                }
            })
            .collect()
    }
}

impl From<&Value> for ServicePreviewBlock {
    fn from(block: &Value) -> Self {
        let fallback = block
            .get("fallbackText")
            .and_then(Value::as_str)
            .unwrap_or("Preview unavailable")
            .chars()
            .take(4096)
            .collect::<String>();
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => Self::Text {
                text: bounded(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or(&fallback),
                ),
                fallback,
            },
            "value" => Self::Value {
                value: bounded(
                    block
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or(&fallback),
                ),
                label: block.get("label").and_then(Value::as_str).map(bounded),
                fallback,
            },
            "metadata" => {
                let entries = block
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(32)
                    .filter_map(|entry| {
                        Some((
                            bounded(entry.get("label")?.as_str()?),
                            bounded(entry.get("value")?.as_str()?),
                        ))
                    })
                    .collect();
                Self::Metadata { entries, fallback }
            }
            "colorPalette" => {
                let colors = block
                    .get("colors")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(32)
                    .filter_map(|color| {
                        let hex = color.get("value").and_then(Value::as_str)?;
                        valid_hex(hex).then(|| hex.to_ascii_lowercase())
                    })
                    .collect();
                Self::Palette { colors, fallback }
            }
            "image" => Self::Image {
                source: block
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("image://maccer-icons/"))
                    .map(str::to_owned),
                alt: block
                    .get("alt")
                    .and_then(Value::as_str)
                    .map(bounded)
                    .unwrap_or_else(|| fallback.clone()),
                fallback,
            },
            _ => Self::Unknown { fallback },
        }
    }
}

fn bounded(value: &str) -> String {
    value.chars().take(16_384).collect()
}

fn valid_hex(value: &str) -> bool {
    value
        .strip_prefix('#')
        .is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_unicode_palette_metadata_images_and_fallbacks_safely() {
        let preview = ServicePreview::from_event(
            "themes".into(),
            "one".into(),
            &[
                json!({"type":"text","text":"Crème 🦀","fallbackText":"text fallback"}),
                json!({"type":"colorPalette","colors":[{"value":"#aBcDeF"},{"value":"url(unsafe)"}],"fallbackText":"palette fallback"}),
                json!({"type":"metadata","entries":[{"label":"名称","value":"雪"}],"fallbackText":"metadata fallback"}),
                json!({"type":"image","alt":"Icon","source":"image://maccer-icons/token","fallbackText":"image fallback"}),
                json!({"type":"future","fallbackText":"future fallback"}),
            ],
        );
        assert!(
            matches!(&preview.blocks[0], ServicePreviewBlock::Text { text, .. } if text == "Crème 🦀")
        );
        assert!(
            matches!(&preview.blocks[1], ServicePreviewBlock::Palette { colors, .. } if colors == &["#abcdef"])
        );
        assert!(
            matches!(&preview.blocks[2], ServicePreviewBlock::Metadata { entries, .. } if entries == &[("名称".into(), "雪".into())])
        );
        assert!(
            matches!(&preview.blocks[3], ServicePreviewBlock::Image { source: Some(source), .. } if source.ends_with("token"))
        );
        assert!(
            matches!(&preview.blocks[4], ServicePreviewBlock::Unknown { fallback } if fallback == "future fallback")
        );
    }

    #[test]
    fn bounds_items_and_rejects_frontend_image_urls_and_unsafe_colors() {
        let many = vec![json!({"type":"text","text":"x".repeat(20_000),"fallbackText":"f"}); 40];
        let preview = ServicePreview::from_event("p".into(), "i".into(), &many);
        assert_eq!(preview.blocks.len(), MAX_PREVIEW_BLOCKS);
        assert!(
            matches!(&preview.blocks[0], ServicePreviewBlock::Text { text, .. } if text.chars().count() == 16_384)
        );
        assert!(matches!(
            ServicePreviewBlock::from(
                &json!({"type":"image","source":"file:///etc/passwd","fallbackText":"safe"})
            ),
            ServicePreviewBlock::Image { source: None, .. }
        ));
        assert!(!valid_hex("#fff"));
        assert!(!valid_hex("url(red)"));
        assert!(!valid_hex("#fff00080"));
        assert!(!valid_hex("fff000"));
        assert!(valid_hex("#fff000"));
    }
}
