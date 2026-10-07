use anyhow::{Context, Result, bail};
use launcher_core::ProviderConfig;
use launcher_macos::{NativePlatform, validate_hotkey};
use provider_shell::{CustomCommand, ShellProvider};
use provider_web::{WebEngine, WebProvider};
use serde::{Deserialize, Deserializer};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub launcher: LauncherConfig,
    pub providers: ProvidersConfig,
    pub web: WebConfig,
    pub files: FileSearchConfig,
    pub shell: ShellConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub commands: Vec<CustomCommand>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LauncherConfig {
    pub hotkey: String,
    pub width: f32,
    pub max_results: usize,
}

/// Per-provider configuration after partial TOML overrides are merged over
/// each provider's own defaults.
#[derive(Clone, Debug)]
pub struct ProvidersConfig {
    pub apps: ProviderConfig,
    pub calculator: ProviderConfig,
    pub web: ProviderConfig,
    pub files: ProviderConfig,
    pub shell: ProviderConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub default_engine: String,
    pub engines: Vec<WebEngine>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FileSearchConfig {
    #[serde(alias = "search_dirs")]
    pub roots: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub ignored_dirs: Vec<String>,
    pub ignore_previews: Vec<String>,
    pub watch: bool,
    /// Accepted for old configs; fd indexing no longer uses these tools.
    pub use_zoxide: bool,
    pub use_fzf: bool,
}
impl Default for FileSearchConfig {
    fn default() -> Self {
        Self {
            roots: vec!["~".into()],
            exclude_paths: vec![],
            ignored_dirs: vec![],
            ignore_previews: vec![],
            watch: true,
            use_zoxide: false,
            use_fzf: false,
        }
    }
}
impl FileSearchConfig {
    fn expand(paths: &[String]) -> Result<Vec<PathBuf>> {
        let home = dirs::home_dir().context("finding home directory for file search")?;
        paths
            .iter()
            .map(|text| {
                let path = if text == "~" {
                    home.clone()
                } else if let Some(relative) = text.strip_prefix("~/") {
                    home.join(relative.trim_start_matches('/'))
                } else {
                    PathBuf::from(text)
                };
                let traverses_parent = path
                    .components()
                    .any(|component| component == std::path::Component::ParentDir);
                if !path.is_absolute() || text.chars().any(char::is_control) || traverses_parent {
                    bail!("file search paths must be absolute or start with ~/, without '..' or control characters");
                }
                Ok(path)
            })
            .collect()
    }
    pub fn fd_config(&self) -> Result<launcher_macos::fd_index::FdConfig> {
        Ok(launcher_macos::fd_index::FdConfig {
            roots: self.expanded_roots()?,
            excluded: self.expanded_excluded_paths()?,
            ignored_dirs: self.ignored_dirs.clone(),
            watch: self.watch,
        })
    }
    pub fn expanded_ignored_previews(&self) -> Result<Vec<PathBuf>> {
        Self::expand(&self.ignore_previews)
    }
    pub fn expanded_roots(&self) -> Result<Vec<PathBuf>> {
        Self::expand(&self.roots)
    }
    pub fn expanded_excluded_paths(&self) -> Result<Vec<PathBuf>> {
        Self::expand(&self.exclude_paths)
    }
}

/// Optional fields for a provider table. An absent field keeps the
/// per-provider default rather than resetting to `ProviderConfig::default()`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ProviderOverrides {
    enabled: Option<bool>,
    prefix: Option<String>,
    priority: Option<i32>,
    default_search: Option<bool>,
    max_results: Option<usize>,
}

impl ProviderOverrides {
    fn apply(self, mut base: ProviderConfig) -> ProviderConfig {
        if let Some(enabled) = self.enabled {
            base.enabled = enabled;
        }
        if let Some(prefix) = self.prefix {
            base.prefix = Some(prefix);
        }
        if let Some(priority) = self.priority {
            base.priority = priority;
        }
        if let Some(default_search) = self.default_search {
            base.default_search = default_search;
        }
        if let Some(max_results) = self.max_results {
            base.max_results = max_results;
        }
        base
    }
}

impl<'de> Deserialize<'de> for ProvidersConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            apps: ProviderOverrides,
            calculator: ProviderOverrides,
            web: ProviderOverrides,
            files: ProviderOverrides,
            shell: ProviderOverrides,
        }
        let raw = Raw::deserialize(deserializer)?;
        let defaults = ProvidersConfig::default();
        Ok(ProvidersConfig {
            apps: raw.apps.apply(defaults.apps),
            calculator: raw.calculator.apply(defaults.calculator),
            web: raw.web.apply(defaults.web),
            files: raw.files.apply(defaults.files),
            shell: raw.shell.apply(defaults.shell),
        })
    }
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            hotkey: "alt-space".into(),
            width: 480.0,
            max_results: 50,
        }
    }
}
impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            apps: ProviderConfig {
                enabled: true,
                prefix: Some("/apps".into()),
                priority: 100,
                default_search: true,
                max_results: 10,
            },
            calculator: ProviderConfig {
                enabled: true,
                prefix: Some("=".into()),
                priority: 90,
                default_search: false,
                max_results: 1,
            },
            files: ProviderConfig {
                enabled: true,
                prefix: Some("/".into()),
                priority: 80,
                default_search: false,
                max_results: 50,
            },
            shell: ProviderConfig {
                enabled: true,
                prefix: Some(">".into()),
                priority: 70,
                default_search: false,
                max_results: 10,
            },
            web: ProviderConfig {
                enabled: true,
                prefix: Some("?".into()),
                priority: -100,
                default_search: true,
                max_results: 2,
            },
        }
    }
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            default_engine: "google".into(),
            engines: WebProvider::default_engines(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        validate_hotkey(&self.launcher.hotkey)?;
        if !(1..=8).contains(&self.files.roots.len()) {
            bail!("files.roots must contain 1 to 8 search directories");
        }
        if self.files.exclude_paths.len() > 64 {
            bail!("files.exclude_paths supports at most 64 directories");
        }
        self.files.expanded_roots()?;
        self.files.expanded_excluded_paths()?;
        self.files.expanded_ignored_previews()?;
        if self.files.ignored_dirs.len() > 64
            || self.files.ignored_dirs.iter().any(|p| p.len() > 512)
        {
            bail!("files.ignored_dirs supports at most 64 regex patterns of 512 bytes");
        }
        if self.files.ignore_previews.len() > 64 {
            bail!("files.ignore_previews supports at most 64 paths");
        }
        self.files.fd_config()?.validate()?;
        if !self.launcher.width.is_finite() || !(400.0..=1200.0).contains(&self.launcher.width) {
            bail!("launcher.width must be between 400 and 1200 points");
        }
        if !(1..=50).contains(&self.launcher.max_results) {
            bail!("launcher.max_results must be between 1 and 50");
        }
        let providers = [
            ("apps", &self.providers.apps),
            ("calculator", &self.providers.calculator),
            ("web", &self.providers.web),
            ("files", &self.providers.files),
            ("shell", &self.providers.shell),
        ];
        for (name, provider) in providers.iter().copied() {
            if !(1..=100).contains(&provider.max_results) {
                bail!("providers.{name}.max_results must be between 1 and 100");
            }
            if let Some(prefix) = provider.prefix.as_deref() {
                if prefix.trim().is_empty() {
                    bail!("providers.{name}.prefix cannot be empty or whitespace");
                }
                if prefix.starts_with(';') {
                    bail!(
                        "providers.{name}.prefix cannot start with ';', which is reserved for the provider picker"
                    );
                }
            }
        }
        let prefixes: Vec<_> = providers
            .iter()
            .filter_map(|(_, provider)| provider.prefix.as_deref())
            .collect();
        for (index, prefix) in prefixes.iter().enumerate() {
            if prefixes[index + 1..].contains(prefix) {
                bail!("provider prefix '{prefix}' is duplicated");
            }
        }
        ShellProvider::new(
            Arc::new(NativePlatform),
            self.shell.commands.clone(),
            dirs::home_dir().context("finding shell working directory")?,
        )
        .map_err(|error| anyhow::anyhow!("invalid shell configuration: {error:#}"))?;
        // Reuse the provider's own constructor so `--check` rejects the same
        // engine problems (missing placeholder, non-HTTP URL, unknown default
        // engine, duplicate IDs) that would fail startup. It performs no I/O.
        WebProvider::new(
            Arc::new(NativePlatform),
            self.web.engines.clone(),
            self.web.default_engine.clone(),
        )
        .map_err(|error| anyhow::anyhow!("invalid web configuration: {error:#}"))?;
        Ok(())
    }
}

pub fn config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let home = dirs::home_dir().context("finding user home directory")?;
    Ok(home.join(".config/maccer/config.toml"))
}
pub fn history_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("finding user data directory")?;
    Ok(base.join("maccer/history.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rejects_invalid_custom_commands_before_starting_the_app() {
        let config: Config =
            toml::from_str("[[shell.commands]]\nname = 'empty script'\ncommand = ''\n").unwrap();
        assert!(config.validate().is_err());
        let duplicate: Config = toml::from_str("[[shell.commands]]\nname = 'same'\ncommand = 'true'\n[[shell.commands]]\nname = 'same'\ncommand = 'false'\n").unwrap();
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn shell_partial_overrides_keep_explicit_defaults_and_named_commands() {
        let config: Config = toml::from_str("[providers.shell]\npriority = 60\n[[shell.commands]]\nname = 'Show working directory'\ncommand = 'pwd'\nkeywords = ['cwd']\n").unwrap();
        assert_eq!(config.providers.shell.prefix.as_deref(), Some(">"));
        assert!(!config.providers.shell.default_search);
        assert_eq!(config.providers.shell.max_results, 10);
        assert_eq!(config.shell.commands[0].keywords, ["cwd"]);
        config.validate().unwrap();
    }

    #[test]
    fn files_default_to_prefix_only_search() {
        for config in [
            Config::default(),
            toml::from_str("[providers.files]\npriority = 85\n").unwrap(),
            toml::from_str(include_str!("../../../config/config.example.toml")).unwrap(),
        ] {
            assert_eq!(config.providers.files.prefix.as_deref(), Some("/"));
            assert!(!config.providers.files.default_search);
        }
    }

    #[test]
    fn default_result_budget_is_larger_than_the_compact_viewport() {
        let config = Config::default();
        assert_eq!(config.launcher.max_results, 50);
        assert_eq!(config.providers.files.max_results, 50);
        let partial: Config = toml::from_str("[providers.files]\npriority = 85\n").unwrap();
        assert_eq!(partial.providers.files.max_results, 50);
        config.validate().unwrap();
    }

    #[test]
    fn repeated_slashes_in_a_tilde_root_do_not_escape_home() {
        assert_eq!(
            FileSearchConfig::expand(&["~//Documents".into()]).unwrap(),
            vec![dirs::home_dir().unwrap().join("Documents")]
        );
    }

    #[test]
    fn elephant_search_dirs_and_ignored_dirs_are_validated() {
        let config: Config = toml::from_str("[files]\nsearch_dirs = ['~/Documents']\nignored_dirs = ['/private/']\nignore_previews = ['~/Documents/Private']\nwatch = false\n").unwrap();
        assert_eq!(config.files.roots, ["~/Documents"]);
        assert!(!config.files.watch);
        config.validate().unwrap();
        let invalid: Config = toml::from_str("[files]\nignored_dirs = ['[']\n").unwrap();
        assert!(invalid.validate().is_err());
    }
    #[test]
    fn optional_file_tools_and_roots_merge_with_defaults() {
        let config: Config = toml::from_str("[files]\nuse_fzf = false\n").unwrap();
        assert_eq!(config.files.roots, vec!["~"]);
        assert!(!config.files.use_fzf);
        assert!(!config.files.use_zoxide);
        assert!(config.files.watch);
        config.validate().unwrap();
        for roots in [vec![], vec!["relative".into()], vec!["~/../other".into()]] {
            let mut config = Config::default();
            config.files.roots = roots;
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn compact_width_matches_example_and_partial_configs() {
        assert_eq!(Config::default().launcher.width, 480.);
        let partial: Config = toml::from_str("[providers.apps]\nenabled = true").unwrap();
        assert_eq!(partial.launcher.width, 480.);
        let example: Config =
            toml::from_str(include_str!("../../../config/config.example.toml")).unwrap();
        assert_eq!(example.launcher.width, 480.);
    }

    #[test]
    fn partial_config_inherits_defaults() {
        let config: Config = toml::from_str("[launcher]\nwidth = 720\n").unwrap();
        assert_eq!(config.launcher.width, 720.0);
        assert_eq!(config.launcher.hotkey, "alt-space");
        assert_eq!(config.providers.apps.prefix.as_deref(), Some("/apps"));
        assert_eq!(config.providers.web.max_results, 2);
        assert_eq!(config.web.engines, WebProvider::default_engines());
        config.validate().unwrap();
    }

    #[test]
    fn provider_overrides_keep_per_provider_defaults() {
        let config: Config = toml::from_str(
            r#"
            [providers.apps]
            enabled = false

            [providers.calculator]
            priority = 123

            [providers.web]
            prefix = "!"
            "#,
        )
        .unwrap();

        assert!(!config.providers.apps.enabled);
        assert_eq!(config.providers.apps.prefix.as_deref(), Some("/apps"));
        assert_eq!(config.providers.apps.priority, 100);
        assert!(config.providers.apps.default_search);
        assert_eq!(config.providers.apps.max_results, 10);

        assert!(config.providers.calculator.enabled);
        assert_eq!(config.providers.calculator.priority, 123);
        assert_eq!(config.providers.calculator.prefix.as_deref(), Some("="));
        assert!(!config.providers.calculator.default_search);
        assert_eq!(config.providers.calculator.max_results, 1);

        assert!(config.providers.web.enabled);
        assert_eq!(config.providers.web.prefix.as_deref(), Some("!"));
        assert_eq!(config.providers.web.priority, -100);
        assert_eq!(config.providers.web.max_results, 2);
        assert!(config.providers.web.default_search);

        config.validate().unwrap();
    }

    #[test]
    fn web_engines_default_and_override() {
        let config: Config = toml::from_str("[web]\ndefault_engine = \"google\"\n").unwrap();
        assert_eq!(config.web.engines.len(), 2);
        config.validate().unwrap();

        let config: Config = toml::from_str(
            r#"
            [web]
            default_engine = "custom"

            [[web.engines]]
            id = "custom"
            name = "Custom"
            url = "https://example.com/search?q={query}"
            "#,
        )
        .unwrap();
        assert_eq!(config.web.engines.len(), 1);
        assert_eq!(config.web.engines[0].id, "custom");
        config.validate().unwrap();
    }

    #[test]
    fn config_load_matches_toml_parse() {
        let contents = "[providers.calculator]\npriority = 123\n";
        let parsed: Config = toml::from_str(contents).unwrap();

        let path = std::env::temp_dir().join(format!("maccer-config-{}.toml", std::process::id()));
        fs::write(&path, contents).unwrap();
        let loaded = Config::load(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(loaded.providers.calculator.priority, 123);
        assert_eq!(
            loaded.providers.calculator.priority,
            parsed.providers.calculator.priority
        );
        assert_eq!(
            loaded.providers.calculator.prefix,
            parsed.providers.calculator.prefix
        );
        assert_eq!(loaded.web.engines, parsed.web.engines);
    }

    #[test]
    fn rejects_invalid_dimensions_and_duplicate_prefixes() {
        let mut config = Config::default();
        config.launcher.width = f32::NAN;
        assert!(config.validate().is_err());
        let mut config = Config::default();
        config.providers.web.prefix = config.providers.apps.prefix.clone();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicated")
        );
    }

    #[test]
    fn rejects_empty_and_reserved_prefixes() {
        let mut config = Config::default();
        config.providers.web.prefix = Some("   ".into());
        assert!(config.validate().unwrap_err().to_string().contains("empty"));

        let mut config = Config::default();
        config.providers.web.prefix = Some(";custom".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );

        let mut config = Config::default();
        config.providers.web.prefix = Some(";".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }

    #[test]
    fn rejects_unknown_default_engine() {
        let mut config = Config::default();
        config.web.default_engine = "missing".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("default web engine"), "{error}");
    }

    #[test]
    fn rejects_invalid_web_engine_url() {
        let mut config = Config::default();
        config.web.engines = vec![WebEngine {
            id: "x".into(),
            name: "X".into(),
            url: "file:///tmp/?q={query}".into(),
        }];
        config.web.default_engine = "x".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_hotkey() {
        let mut config = Config::default();
        config.launcher.hotkey = "bogus".into();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }

    #[test]
    fn config_override_is_not_written_or_relocated() {
        let path = PathBuf::from("/tmp/custom-maccer.toml");
        assert_eq!(config_path(Some(&path)).unwrap(), path);
    }
}
