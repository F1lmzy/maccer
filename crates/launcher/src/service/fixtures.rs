use crate::core::{
    Action, ActionId, ActionOutcome, IconDescriptor, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use crate::protocol::{ContentBlock, DomainData, Icon};
use crate::service::{ItemPresentation, Service, ServiceProvider};
use anyhow::{Result, bail};
use serde_json::json;
use std::sync::Arc;

struct FixtureProvider {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    kind: &'static str,
    explicit: bool,
}
impl Provider for FixtureProvider {
    fn id(&self) -> ProviderId {
        ProviderId(self.id.into())
    }
    fn name(&self) -> &str {
        self.name
    }
    fn requires_explicit_scope(&self) -> bool {
        self.explicit
    }
    fn records_usage(&self) -> bool {
        self.id != "clipboard"
    }
    fn search(&self, q: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 {
            return Ok(vec![]);
        }
        let title = match self.id {
            "apps" => "Safari",
            "calculator" => "2",
            "web" => "Search the web",
            "files" => "Documents",
            "clipboard" => "Fixture clipboard entry",
            "shell" => "Configured command",
            "themes" => "Catppuccin Mocha",
            "wallpapers" => "Catppuccin wallpaper",
            _ => self.name,
        };
        if !q.text.trim().is_empty()
            && !title.to_lowercase().contains(&q.text.to_lowercase())
            && !self.name.to_lowercase().contains(&q.text.to_lowercase())
        {
            return Ok(vec![]);
        }
        Ok(vec![Item {
            id: ItemId(format!("{}:fixture", self.id)),
            provider: self.id(),
            title: title.into(),
            subtitle: Some(format!("Inert {} fixture", self.name.to_lowercase())),
            keywords: vec![self.name.into(), self.kind.into()],
            icon: Some(IconDescriptor::Text(
                self.name.chars().next().unwrap_or('?').to_string(),
            )),
            score: 0.0,
            payload: json!({"fixturePrivatePayload":"never exposed"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("select".into()),
            title: "Select".into(),
        }]
    }
    fn activate(&self, _: &Item, action: &Action) -> Result<ActionOutcome> {
        if action.id.0 != "select" {
            bail!("unsupported fixture action")
        }
        Ok(ActionOutcome::KeepOpen(
            "Fixture action completed without a system side effect.".into(),
        ))
    }
}
impl ServiceProvider for FixtureProvider {
    fn description(&self) -> &str {
        self.description
    }
    fn kind(&self) -> &str {
        self.kind
    }
    fn details_available(&self, _: &Item) -> bool {
        true
    }
    fn details(&self, _item: &Item) -> Result<Vec<ContentBlock>> {
        Ok(match self.id {
            "apps" => vec![ContentBlock::Metadata {
                entries: vec![crate::protocol::MetadataEntry {
                    label: "Type".into(),
                    value: "Application".into(),
                }],
                fallback_text: "Safari application details".into(),
            }],
            "themes" => vec![ContentBlock::ColorPalette {
                colors: vec![
                    crate::protocol::ColorSwatch {
                        role: "background".into(),
                        value: "#1e1e2e".into(),
                    },
                    crate::protocol::ColorSwatch {
                        role: "foreground".into(),
                        value: "#cdd6f4".into(),
                    },
                    crate::protocol::ColorSwatch {
                        role: "accent".into(),
                        value: "#cba6f7".into(),
                    },
                ],
                fallback_text: "Dark theme with purple accent".into(),
            }],
            "calculator" => vec![ContentBlock::Value {
                value: "2".into(),
                label: Some("1 + 1".into()),
                fallback_text: "1 + 1 = 2".into(),
            }],
            "files" => vec![ContentBlock::Text {
                text: "A Unicode preview: notes 🦀".into(),
                truncated: false,
                fallback_text: "A Unicode preview: notes 🦀".into(),
            }],
            "web" => vec![ContentBlock::Unknown {
                kind: "futurePreview".into(),
                fallback_text: "Search the web for this result".into(),
            }],
            _ => vec![ContentBlock::Metadata {
                entries: vec![crate::protocol::MetadataEntry {
                    label: "Fixture".into(),
                    value: self.name.into(),
                }],
                fallback_text: format!("{} fixture details", self.name),
            }],
        })
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        let mut data = None;
        let mut content = vec![ContentBlock::Text {
            text: item.subtitle.clone().unwrap_or_default(),
            truncated: false,
            fallback_text: item.subtitle.clone().unwrap_or_default(),
        }];
        match self.id {
            "themes" => {
                data = Some(DomainData::ThemeSummary {
                    schema_version: 1,
                    slug: "catppuccin".into(),
                    variant: "dark".into(),
                });
                content.push(ContentBlock::ColorPalette {
                    colors: vec![
                        crate::protocol::ColorSwatch {
                            role: "background".into(),
                            value: "#1e1e2e".into(),
                        },
                        crate::protocol::ColorSwatch {
                            role: "foreground".into(),
                            value: "#cdd6f4".into(),
                        },
                        crate::protocol::ColorSwatch {
                            role: "accent".into(),
                            value: "#cba6f7".into(),
                        },
                    ],
                    fallback_text: "Dark theme with purple accent".into(),
                })
            }
            "wallpapers" => {
                data = Some(DomainData::WallpaperSummary {
                    schema_version: 1,
                    theme_slug: "catppuccin".into(),
                    wallpaper_id: "catppuccin:fixture.webp".into(),
                })
            }
            "calculator" => {
                data = Some(DomainData::Calculation {
                    schema_version: 1,
                    expression: "1 + 1".into(),
                    result: "2".into(),
                })
            }
            "apps" => {
                data = Some(DomainData::Application {
                    schema_version: 1,
                    application_id: "com.example.fixture".into(),
                })
            }
            _ => {}
        }
        ItemPresentation {
            kind: self.kind.into(),
            icon: Some(Icon::Glyph {
                name: self.kind.into(),
                fallback_text: self.name.chars().next().unwrap_or('?').to_string(),
            }),
            content,
            data,
        }
    }
}
pub(crate) fn service() -> Result<Arc<Service>> {
    let defs = [
        (
            "apps",
            "Apps",
            "Search and open applications.",
            "application",
            false,
        ),
        (
            "calculator",
            "Calculator",
            "Calculate expressions.",
            "calculation",
            false,
        ),
        ("web", "Web search", "Search the web.", "webSearch", false),
        ("files", "Files", "Find files.", "file", true),
        (
            "clipboard",
            "Clipboard",
            "Search copied items.",
            "clipboard",
            true,
        ),
        (
            "shell",
            "Commands",
            "Run configured commands.",
            "command",
            true,
        ),
        (
            "themes",
            "Themes",
            "Browse and apply themes.",
            "theme",
            false,
        ),
        (
            "wallpapers",
            "Wallpapers",
            "Browse wallpapers.",
            "wallpaper",
            false,
        ),
    ];
    let registrations = defs
        .into_iter()
        .map(|(id, name, description, kind, explicit)| {
            let p: Arc<dyn ServiceProvider> = Arc::new(FixtureProvider {
                id,
                name,
                description,
                kind,
                explicit,
            });
            let c = ProviderConfig {
                prefix: Some(format!("{id} ")),
                default_search: !explicit,
                max_results: 10,
                ..ProviderConfig::default()
            };
            (p, c)
        })
        .collect();
    Service::with_state(
        registrations,
        Arc::new(crate::core::HistoryStore::in_memory()?),
        None,
        crate::service::ThemerState::fixture(),
    )
}
