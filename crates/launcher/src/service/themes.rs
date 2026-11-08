//! Theme browsing, opt-in confirmed application, and pack acquisition over
//! mac-themer's catalog.
//!
//! The scope publishes the packs it can apply, and, for the whole page, two
//! commands it can run: install a pack from a repository, and remove one. The
//! commands are scope actions rather than rows, so a strip of palettes stays a
//! strip of palettes and the commands appear in the frontend's action panel
//! beside Apply. Both are actions with a provider-published input field, so the
//! frontend draws the dialog and sends the value back without knowing what
//! either command does. All of the work — validating the URL or slug, running
//! the helper, reloading the catalog — stays here.
use crate::core::{
    Action, ActionId, ActionOutcome, Item, ItemId, Provider, ProviderId, SearchContext, SearchQuery,
};
use crate::protocol::{
    ACTION_INPUT_SELECT, ACTION_INPUT_TEXT, ActionInput, ActionInputOption, ColorSwatch,
    ContentBlock, DomainData, Icon, MetadataEntry, ThemeCard, ThemePalette, ThemeWallpaper,
};
use crate::service::{
    ItemPresentation, ServiceProvider, ThemeApply, ThemeCatalog, ThemeInstall, ThemerState, menu,
};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Item ids of the two commands the scope publishes. A pack is
/// `themes:<slug>`, so a command is the one id that is not a palette.
const INSTALL_COMMAND: &str = "themes:install";
const UNINSTALL_COMMAND: &str = "themes:uninstall";
/// Field ids the commands expect back. Published in their input spec.
const INSTALL_FIELD: &str = "url";
const UNINSTALL_FIELD: &str = "slug";
/// Most choices one uninstall dialog offers. The catalog cap is smaller.
const MAX_UNINSTALL_CHOICES: usize = 128;

pub struct ThemeServiceProvider {
    catalog: Arc<ThemeCatalog>,
    apply: Option<Arc<ThemeApply>>,
    /// Install and removal executor. Without one the two commands are not
    /// published at all, so a read-only service never advertises them.
    install: Option<Arc<ThemeInstall>>,
    /// Read-only active theme, so a palette card can say which one is live.
    theme: Option<Arc<ThemerState>>,
}

/// The two commands the scope publishes as options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Install,
    Uninstall,
}
impl Command {
    fn item_id(self) -> &'static str {
        match self {
            Command::Install => INSTALL_COMMAND,
            Command::Uninstall => UNINSTALL_COMMAND,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Command::Install => "Install a theme from GitHub…",
            Command::Uninstall => "Uninstall a theme…",
        }
    }
    fn subtitle(self) -> &'static str {
        match self {
            Command::Install => "Paste a repository link",
            Command::Uninstall => "Choose an installed pack to remove",
        }
    }
    fn keywords(self) -> &'static str {
        match self {
            Command::Install => "install add theme github repository download pack",
            Command::Uninstall => "uninstall remove delete theme pack",
        }
    }
    fn glyph(self) -> &'static str {
        match self {
            Command::Install => "⤓",
            Command::Uninstall => "⌫",
        }
    }
    fn action_id(self) -> &'static str {
        match self {
            Command::Install => "install",
            Command::Uninstall => "uninstall",
        }
    }
    /// The words the action panel shows. A command is never a row, so this is
    /// the only place its label is read from.
    fn action_title(self) -> &'static str {
        self.title()
    }
    fn from_item(item: &Item) -> Option<Self> {
        match item.id.0.as_str() {
            INSTALL_COMMAND => Some(Command::Install),
            UNINSTALL_COMMAND => Some(Command::Uninstall),
            _ => None,
        }
    }
}
/// One retained search item resolved against the catalog snapshot that produced
/// it: the palette to apply and the wallpapers that pack ships.
struct ThemeEntry {
    palette: ThemePalette,
    wallpapers: Vec<ThemeWallpaper>,
}
impl ThemeServiceProvider {
    /// Browsing only. Never starts a mutation helper.
    pub fn new(catalog: Arc<ThemeCatalog>) -> Self {
        Self {
            catalog,
            apply: None,
            install: None,
            theme: None,
        }
    }

    /// Explicit mutation support. Share this executor across all service clients.
    pub fn with_apply(catalog: Arc<ThemeCatalog>, apply: Arc<ThemeApply>) -> Self {
        Self {
            catalog,
            apply: Some(apply),
            install: None,
            theme: None,
        }
    }

    /// Share the installer/remover with the service endpoints. Only a service
    /// that attached one publishes the two commands.
    pub fn with_install(mut self, install: Arc<ThemeInstall>) -> Self {
        self.install = Some(install);
        self
    }

    /// Share the reader of the theme the desktop is wearing. Without it a card
    /// is published with `applied: false`, never with stale state.
    pub fn with_theme(mut self, theme: Arc<ThemerState>) -> Self {
        self.theme = Some(theme);
        self
    }

    /// The colours a strip draws as one card: the theme's own surface, its
    /// text, its accent and the six accents shown as dots.
    fn card(&self, palette: &ThemePalette) -> ThemeCard {
        let role = |key: &str| palette.palette.get(key).cloned().unwrap_or_default();
        let applied = self.theme.as_ref().is_some_and(|theme| {
            theme
                .snapshot()
                .palette
                .is_some_and(|active| active.slug == palette.slug)
        });
        ThemeCard {
            background: role("background"),
            foreground: role("foreground"),
            accent: role("accent"),
            swatches: (1..=6)
                .map(|index| role(&format!("color{index}")))
                .filter(|value| !value.is_empty())
                .collect(),
            applied,
        }
    }

    fn entry_for(&self, item: &Item) -> Option<ThemeEntry> {
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
        let palette = snapshot.palettes.iter().find(|p| p.slug == slug).cloned()?;
        Some(ThemeEntry {
            palette,
            // A producer identity that cannot round-trip never reaches a
            // frontend or an argument list; the catalog rejects it upstream.
            wallpapers: snapshot
                .wallpapers_for(slug)
                .iter()
                .map(String::as_str)
                .filter_map(ThemeWallpaper::new)
                .collect(),
        })
    }

    /// The command an item names, after the same catalog-revision pin that
    /// authorizes a pack row: a command published by an older snapshot is not
    /// run against a catalog that has moved since.
    fn command_for(&self, item: &Item) -> Option<Command> {
        if item.provider != self.id() {
            return None;
        }
        let command = Command::from_item(item)?;
        if self.install.is_none() || self.catalog.snapshot().error.is_some() {
            return None;
        }
        if item
            .payload
            .get("catalogRevision")
            .and_then(serde_json::Value::as_u64)
            != Some(self.catalog.snapshot().revision)
        {
            return None;
        }
        if command == Command::Uninstall && self.catalog.snapshot().palettes.is_empty() {
            return None;
        }
        Some(command)
    }

    /// One command identity. It carries the catalog revision that authorizes it
    /// and no palette, so it can never be mistaken for a theme.
    fn command_item(&self, command: Command, revision: u64) -> Item {
        Item {
            id: ItemId(command.item_id().into()),
            provider: self.id(),
            title: command.title().into(),
            subtitle: Some(command.subtitle().into()),
            keywords: command.keywords().split(' ').map(str::to_owned).collect(),
            icon: Some(crate::core::IconDescriptor::Text(command.glyph().into())),
            score: 0.,
            payload: serde_json::json!({"catalogRevision":revision}),
        }
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
        let mut items: Vec<Item> = matches
            .into_iter()
            .map(|(score, palette)| Item {
                id: ItemId(format!("themes:{}", palette.slug)),
                provider: self.id(),
                title: palette.name.clone(),
                subtitle: Some(format!(
                    "{} · {} · preview · {}",
                    palette.slug,
                    palette.variant,
                    wallpaper_note(snapshot.wallpapers_for(&palette.slug).len())
                )),
                keywords: vec![palette.slug.clone(), palette.variant.clone()],
                icon: None,
                score: -(score as f64),
                payload: serde_json::json!({"catalogRevision":snapshot.revision}),
            })
            .collect();
        items.truncate(ctx.limit);
        Ok(items)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if let Some(command) = self.command_for(item) {
            return vec![Action {
                id: ActionId(command.action_id().into()),
                title: command.action_title().into(),
            }];
        }
        if self.apply.is_some()
            && self.catalog.snapshot().error.is_none()
            && self.entry_for(item).is_some()
        {
            vec![Action {
                id: ActionId("apply".into()),
                title: "Apply theme…".into(),
            }]
        } else {
            vec![]
        }
    }
    fn activate_with_input(
        &self,
        item: &Item,
        action: &Action,
        input: &BTreeMap<String, String>,
    ) -> Result<ActionOutcome> {
        let Some(command) = self.command_for(item) else {
            // A pack row takes no input: the palette itself authorizes Apply.
            if input.is_empty() {
                return self.activate(item, action);
            }
            bail!("this action does not take input");
        };
        if action.id.0 != command.action_id() {
            bail!("theme command is unavailable; search again before running it");
        }
        let executor = self
            .install
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("theme installation is not enabled on this service"))?;
        match command {
            Command::Install => {
                let url = input
                    .get(INSTALL_FIELD)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Enter a GitHub repository URL."))?;
                if !url.starts_with("https://github.com/") {
                    bail!("Enter a GitHub repository URL such as https://github.com/owner/theme.");
                }
                executor.install(url, false, false)?;
            }
            Command::Uninstall => {
                let slug = input
                    .get(UNINSTALL_FIELD)
                    .map(String::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Choose a theme to uninstall."))?;
                executor.uninstall(slug)?;
            }
        }
        // The executor reloads the catalog before returning, so this drops the
        // old authority pin and delivers the collection as it is now.
        Ok(ActionOutcome::RefreshSearch)
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if let Some(command) = self.command_for(item) {
            if action.id.0 != command.action_id() {
                bail!("theme command is unavailable; search again before running it");
            }
            // Reachable only from a client that skipped the provider's field.
            bail!("this action needs a value from its dialog");
        }
        if action.id.0 != "apply" || self.catalog.snapshot().error.is_some() {
            bail!("theme Apply is unavailable; search again before confirming")
        }
        let executor = self.apply.as_ref().ok_or_else(|| {
            anyhow::anyhow!("theme catalog is read-only; applying themes is not enabled")
        })?;
        let entry = self
            .entry_for(item)
            .ok_or_else(|| anyhow::anyhow!("theme is no longer in the catalog"))?;
        executor
            .apply(&entry.palette, entry.wallpapers.first())
            .map(ActionOutcome::KeepOpen)
    }
}
impl ServiceProvider for ThemeServiceProvider {
    fn scope_actions(&self) -> Vec<Item> {
        let snapshot = self.catalog.snapshot();
        if self.install.is_none() || snapshot.error.is_some() {
            return Vec::new();
        }
        [Command::Install, Command::Uninstall]
            .into_iter()
            .filter(|command| *command != Command::Uninstall || !snapshot.palettes.is_empty())
            .map(|command| self.command_item(command, snapshot.revision))
            .collect()
    }
    fn description(&self) -> &str {
        if self.apply.is_some() {
            "Browse palettes and explicitly confirm Apply. Apply also sets that theme's wallpaper."
        } else {
            "Browse theme palettes. Read-only; applying themes is not enabled."
        }
    }
    fn kind(&self) -> &str {
        "theme"
    }
    fn action_input(&self, item: &Item, action: &Action) -> Option<ActionInput> {
        let command = self.command_for(item)?;
        if action.id.0 != command.action_id() {
            return None;
        }
        match command {
            Command::Install => Some(ActionInput {
                id: INSTALL_FIELD.into(),
                kind: ACTION_INPUT_TEXT.into(),
                title: "Install a theme from GitHub".into(),
                label: "Repository URL".into(),
                placeholder: Some("https://github.com/<owner>/<repo>".into()),
                submit_label: "Install".into(),
                options: vec![],
            }),
            Command::Uninstall => {
                // The choices are the catalog's own packs, so the picker can
                // never name something that is not installed right now.
                let snapshot = self.catalog.snapshot();
                let options: Vec<ActionInputOption> = snapshot
                    .palettes
                    .iter()
                    .take(MAX_UNINSTALL_CHOICES)
                    .map(|palette| ActionInputOption {
                        value: palette.slug.clone(),
                        label: format!("{} ({})", palette.name, palette.slug),
                    })
                    .collect();
                if options.is_empty() {
                    return None;
                }
                Some(ActionInput {
                    id: UNINSTALL_FIELD.into(),
                    kind: ACTION_INPUT_SELECT.into(),
                    title: "Uninstall a theme".into(),
                    label: "Choose the pack to remove".into(),
                    placeholder: None,
                    submit_label: "Continue".into(),
                    options,
                })
            }
        }
    }
    fn action_confirmation(&self, item: &Item, action: &Action) -> Option<String> {
        if let Some(command) = self.command_for(item) {
            if action.id.0 != command.action_id() {
                return None;
            }
            return match command {
                Command::Install => None,
                Command::Uninstall => Some(
                    "Are you sure you want to uninstall this theme? Its pack and wallpapers are \
removed from disk and this cannot be undone."
                        .into(),
                ),
            };
        }
        if self.apply.is_none() || action.id.0 != "apply" {
            return None;
        }
        let entry = self.entry_for(item)?;
        Some(match entry.wallpapers.first() {
            Some(wallpaper) => format!(
                "Apply \"{}\"? This writes theme files, updates app settings, reloads apps and sets the desktop wallpaper to \"{}\".",
                entry.palette.name, wallpaper.label
            ),
            None => format!(
                "Apply \"{}\"? This writes theme files, updates app settings and reloads apps. The wallpaper will not change.",
                entry.palette.name
            ),
        })
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        let entry = self.entry_for(item);
        ItemPresentation {
            kind: "theme".into(),
            icon: Some(Icon::Glyph {
                name: "palette".into(),
                fallback_text: "◐".into(),
            }),
            content: entry
                .as_ref()
                .map(|entry| summary(&entry.palette))
                .into_iter()
                .collect(),
            data: entry.map(|entry| {
                let card = self.card(&entry.palette);
                DomainData::ThemeSummary {
                    schema_version: 1,
                    slug: entry.palette.slug,
                    variant: entry.palette.variant,
                    wallpapers: entry.wallpapers,
                    card: Some(card),
                }
            }),
        }
    }
    fn details_available(&self, item: &Item) -> bool {
        self.entry_for(item).is_some()
    }
    fn details(&self, item: &Item) -> Result<Vec<ContentBlock>> {
        let entry = self
            .entry_for(item)
            .ok_or_else(|| anyhow::anyhow!("theme is no longer in the catalog"))?;
        let palette = &entry.palette;
        let mut blocks = vec![
            summary(palette),
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
                        label: "Wallpapers".into(),
                        value: wallpaper_detail(&entry.wallpapers),
                    },
                    MetadataEntry {
                        label: "Mode".into(),
                        value: if self.apply.is_some() {
                            "Preview only. Apply requires confirmation and sets the theme's first wallpaper."
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

/// What Apply would do with the wallpaper, said plainly in the result row.
fn wallpaper_note(count: usize) -> String {
    match count {
        0 => "no wallpaper".to_owned(),
        1 => "1 wallpaper".to_owned(),
        count => format!("{count} wallpapers"),
    }
}

/// One metadata row naming the pack's wallpapers, bounded so a pack with many
/// images cannot turn a details panel into a wall of text.
fn wallpaper_detail(wallpapers: &[ThemeWallpaper]) -> String {
    const MAX_BYTES: usize = 128;
    let Some(first) = wallpapers.first() else {
        return "none".to_owned();
    };
    if wallpapers.len() == 1 {
        return first.label.clone();
    }
    let mut text = format!("{}: {}", wallpapers.len(), first.label);
    for wallpaper in &wallpapers[1..] {
        let addition = format!(", {}", wallpaper.label);
        if text.len() + addition.len() > MAX_BYTES {
            text.push_str(", …");
            break;
        }
        text.push_str(&addition);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SearchContext;
    use crate::service::ThemeCatalog;
    use std::path::PathBuf;

    fn catalog(slugs: &[&str]) -> Arc<ThemeCatalog> {
        let palettes = slugs
            .iter()
            .map(|slug| {
                let mut palette = ThemerState::fixture().snapshot().palette.unwrap();
                palette.slug = (*slug).into();
                palette.name = slug.to_uppercase();
                palette
            })
            .collect();
        ThemeCatalog::from_palettes(palettes).unwrap()
    }

    fn scope(catalog: Arc<ThemeCatalog>, install: bool) -> ThemeServiceProvider {
        let with_apply = ThemeServiceProvider::with_apply(
            catalog,
            ThemeApply::new(PathBuf::from("/nonexistent/mac-themer")),
        );
        if install {
            with_apply.with_install(ThemeInstall::new(PathBuf::from("/nonexistent/mac-themer")))
        } else {
            with_apply
        }
    }

    fn rows(provider: &ThemeServiceProvider, query: &str) -> Vec<Item> {
        provider
            .search(
                &SearchQuery {
                    raw: query.into(),
                    text: query.into(),
                },
                &SearchContext {
                    generation: 1,
                    cancellation: Default::default(),
                    limit: 50,
                },
            )
            .unwrap()
    }

    fn action(id: &str) -> Action {
        Action {
            id: ActionId(id.into()),
            title: id.into(),
        }
    }

    fn commands(provider: &ThemeServiceProvider) -> Vec<Item> {
        provider.scope_actions()
    }

    #[test]
    fn the_commands_belong_to_the_scope_and_never_to_its_results() {
        let provider = scope(catalog(&["night"]), true);
        // The strip is palettes only: a command is not a result, so neither a
        // browsed scope nor a query that names one puts it in the strip.
        let ids: Vec<String> = rows(&provider, "").into_iter().map(|i| i.id.0).collect();
        assert_eq!(ids, ["themes:night"]);
        for query in ["", "uninstall", "install theme github"] {
            assert!(
                rows(&provider, query)
                    .iter()
                    .all(|item| item.id.0 == "themes:night"),
                "query {query:?} surfaced a command as a result"
            );
        }
        // The page publishes them instead, in the order they are offered.
        let ids: Vec<String> = commands(&provider).into_iter().map(|i| i.id.0).collect();
        assert_eq!(ids, [INSTALL_COMMAND, UNINSTALL_COMMAND]);
        // And the panel wording is the action's own.
        let install = &commands(&provider)[0];
        assert_eq!(
            provider.actions(install)[0].title,
            "Install a theme from GitHub…"
        );
        assert_eq!(
            provider.actions(&commands(&provider)[1])[0].title,
            "Uninstall a theme…"
        );

        // A scope with no installer is read-only and publishes neither command.
        let read_only = scope(catalog(&["night"]), false);
        assert!(commands(&read_only).is_empty());
        let ids: Vec<String> = rows(&read_only, "").into_iter().map(|i| i.id.0).collect();
        assert_eq!(ids, ["themes:night"]);
    }

    #[test]
    fn with_nothing_installed_only_installing_is_offered() {
        let provider = scope(catalog(&[]), true);
        // With no packs the strip is empty, and Install is the one thing the
        // page can still do.
        assert!(rows(&provider, "").is_empty());
        let ids: Vec<String> = commands(&provider).into_iter().map(|i| i.id.0).collect();
        assert_eq!(ids, [INSTALL_COMMAND]);
    }

    #[test]
    fn an_option_from_an_older_catalog_snapshot_is_not_runnable() {
        let provider = scope(catalog(&["night"]), true);
        let install = commands(&provider)
            .into_iter()
            .find(|item| item.id.0 == INSTALL_COMMAND)
            .unwrap();
        assert_eq!(provider.actions(&install)[0].id.0, "install");
        assert!(
            provider
                .action_input(&install, &action("install"))
                .is_some()
        );

        // The row is pinned to the catalog revision that published it, so a
        // command published before the collection changed cannot run against
        // the collection as it is now.
        let mut stale = install.clone();
        stale.payload = serde_json::json!({
            "catalogRevision": provider.revision().wrapping_add(1)
        });
        assert!(provider.actions(&stale).is_empty());
        assert!(provider.action_input(&stale, &action("install")).is_none());
        assert!(
            provider
                .activate_with_input(&stale, &action("install"), &BTreeMap::new())
                .is_err()
        );
        // And an item that is not a command cannot borrow its action either.
        let pack = rows(&provider, "")
            .into_iter()
            .find(|item| item.id.0 == "themes:night")
            .unwrap();
        assert!(provider.action_input(&pack, &action("install")).is_none());
    }

    #[test]
    fn the_uninstall_choices_are_the_packs_the_catalog_has_now() {
        let provider = scope(catalog(&["night", "day"]), true);
        let uninstall = commands(&provider)
            .into_iter()
            .find(|item| item.id.0 == UNINSTALL_COMMAND)
            .unwrap();
        let field = provider
            .action_input(&uninstall, &action("uninstall"))
            .unwrap();
        assert_eq!(field.kind, ACTION_INPUT_SELECT);
        assert_eq!(
            field
                .options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            ["day", "night"]
        );
        // The prompt names what the action does, not a theme: the choice is
        // made in the dialog the same action opened.
        let prompt = provider
            .action_confirmation(&uninstall, &action("uninstall"))
            .unwrap();
        assert!(prompt.starts_with("Are you sure you want to uninstall this theme?"));
    }
}
