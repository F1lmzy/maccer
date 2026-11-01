//! Theme browsing and opt-in confirmed application over mac-themer's catalog.
use crate::{ItemPresentation, ServiceProvider, ThemeApply, ThemeCatalog, menu};
use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, Item, ItemId, Provider, ProviderId, SearchContext, SearchQuery,
};
use launcher_protocol::{ColorSwatch, ContentBlock, DomainData, Icon, MetadataEntry, ThemePalette};
use std::sync::Arc;

pub struct ThemeServiceProvider {
    catalog: Arc<ThemeCatalog>,
    apply: Option<Arc<ThemeApply>>,
}
impl ThemeServiceProvider {
    /// Browsing only. Never starts a mutation helper.
    pub fn new(catalog: Arc<ThemeCatalog>) -> Self {
        Self {
            catalog,
            apply: None,
        }
    }

    /// Explicit mutation support. Share this executor across all service clients.
    pub fn with_apply(catalog: Arc<ThemeCatalog>, apply: Arc<ThemeApply>) -> Self {
        Self {
            catalog,
            apply: Some(apply),
        }
    }

    fn palette_for(&self, item: &Item) -> Option<ThemePalette> {
        if item.provider != self.id() {
            return None;
        }
        let slug = item.id.0.strip_prefix("themes:")?;
        let snapshot = self.catalog.snapshot();
        // The session checks revision before invoking, but the cache can change
        // immediately afterward. Never substitute a newer palette for the one
        // that produced this private, server-retained search item.
        if item
            .payload
            .get("catalogRevision")
            .and_then(serde_json::Value::as_u64)
            != Some(snapshot.revision)
        {
            return None;
        }
        snapshot.palettes.iter().find(|p| p.slug == slug).cloned()
    }
}
impl Provider for ThemeServiceProvider {
    fn id(&self) -> ProviderId {
        ProviderId("themes".into())
    }
    fn name(&self) -> &str {
        "Themes"
    }
    fn revision(&self) -> u64 {
        self.catalog.snapshot().revision
    }
    fn status(&self) -> Option<String> {
        self.catalog.snapshot().error
    }
    fn records_usage(&self) -> bool {
        false
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 {
            return Ok(vec![]);
        }
        let snapshot = self.catalog.snapshot();
        let mut matches: Vec<_> = snapshot
            .palettes
            .iter()
            .filter_map(|palette| {
                let score = menu::score(
                    &query.text,
                    &format!("{} {} {}", palette.name, palette.slug, palette.variant),
                )?;
                Some((score, palette))
            })
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.slug.cmp(&b.1.slug)));
        Ok(matches
            .into_iter()
            .take(ctx.limit)
            .map(|(score, palette)| Item {
                id: ItemId(format!("themes:{}", palette.slug)),
                provider: self.id(),
                title: palette.name.clone(),
                subtitle: Some(format!(
                    "{} · {} · read-only preview",
                    palette.slug, palette.variant
                )),
                keywords: vec![palette.slug.clone(), palette.variant.clone()],
                icon: None,
                score: -(score as f64),
                payload: serde_json::json!({"catalogRevision":snapshot.revision}),
            })
            .collect())
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if self.apply.is_some()
            && self.catalog.snapshot().error.is_none()
            && self.palette_for(item).is_some()
        {
            vec![Action {
                id: ActionId("apply".into()),
                title: "Apply theme…".into(),
            }]
        } else {
            vec![]
        }
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if action.id.0 != "apply" || self.catalog.snapshot().error.is_some() {
            bail!("theme Apply is unavailable; search again before confirming")
        }
        let executor = self.apply.as_ref().ok_or_else(|| {
            anyhow::anyhow!("theme catalog is read-only; applying themes is not enabled")
        })?;
        let palette = self
            .palette_for(item)
            .ok_or_else(|| anyhow::anyhow!("theme is no longer in the catalog"))?;
        executor.apply(&palette).map(ActionOutcome::KeepOpen)
    }
}
impl ServiceProvider for ThemeServiceProvider {
    fn description(&self) -> &str {
        if self.apply.is_some() {
            "Browse palettes and explicitly confirm Apply. Wallpaper is unchanged."
        } else {
            "Browse theme palettes. Read-only; applying themes is not enabled."
        }
    }
    fn kind(&self) -> &str {
        "theme"
    }
    fn action_confirmation(&self, item: &Item, action: &Action) -> Option<String> {
        if self.apply.is_none() || action.id.0 != "apply" {
            return None;
        }
        self.palette_for(item).map(|palette| format!("Apply \"{}\"? This writes theme files, updates app settings and reloads apps. The wallpaper will not change.", palette.name))
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        let palette = self.palette_for(item);
        ItemPresentation {
            kind: "theme".into(),
            icon: Some(Icon::Glyph {
                name: "palette".into(),
                fallback_text: "◐".into(),
            }),
            content: palette.as_ref().map(summary).into_iter().collect(),
            data: palette.map(|p| DomainData::ThemeSummary {
                schema_version: 1,
                slug: p.slug,
                variant: p.variant,
            }),
        }
    }
    fn details_available(&self, item: &Item) -> bool {
        self.palette_for(item).is_some()
    }
    fn details(&self, item: &Item) -> Result<Vec<ContentBlock>> {
        let palette = self
            .palette_for(item)
            .ok_or_else(|| anyhow::anyhow!("theme is no longer in the catalog"))?;
        let mut blocks = vec![
            summary(&palette),
            ContentBlock::Metadata {
                entries: vec![
                    MetadataEntry {
                        label: "Name".into(),
                        value: palette.name.clone(),
                    },
                    MetadataEntry {
                        label: "Slug".into(),
                        value: palette.slug.clone(),
                    },
                    MetadataEntry {
                        label: "Variant".into(),
                        value: palette.variant.clone(),
                    },
                    MetadataEntry {
                        label: "Alpha byte".into(),
                        value: palette.alpha_byte.to_string(),
                    },
                    MetadataEntry {
                        label: "Mode".into(),
                        value: if self.apply.is_some() {
                            "Preview only. Apply requires confirmation. Wallpaper stays unchanged."
                        } else {
                            "Read-only preview. Applying themes is not enabled."
                        }
                        .into(),
                    },
                ],
                fallback_text: format!(
                    "{} ({}) · read-only palette",
                    palette.name, palette.variant
                ),
            },
        ];
        // Both adapters bound each palette block to 16 swatches. Split rather
        // than silently dropping the remaining resolved roles.
        for (group, colors) in [
            ("Palette", &palette.palette),
            ("Tokens", &palette.tokens),
            ("Border", &palette.border),
            ("Bar", &palette.bar),
        ] {
            let colors: Vec<_> = colors
                .iter()
                .map(|(role, value)| ColorSwatch {
                    role: format!("{group}: {role}"),
                    value: value.clone(),
                })
                .collect();
            for chunk in colors.chunks(16) {
                blocks.push(ContentBlock::ColorPalette {
                    colors: chunk.to_vec(),
                    fallback_text: format!("{} {group} colors", palette.name),
                });
            }
        }
        Ok(blocks)
    }
}
fn summary(palette: &ThemePalette) -> ContentBlock {
    ContentBlock::ColorPalette {
        colors: ["background", "foreground", "accent"]
            .into_iter()
            .filter_map(|role| {
                palette.palette.get(role).map(|value| ColorSwatch {
                    role: role.into(),
                    value: value.clone(),
                })
            })
            .collect(),
        fallback_text: format!(
            "{} {} palette. Preview only, not applied.",
            palette.name, palette.variant
        ),
    }
}
