use anyhow::{Context, Result, bail};
use launcher_core::ProviderConfig;
use launcher_macos::{NativePlatform, validate_hotkey};
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
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub default_engine: String,
    pub engines: Vec<WebEngine>,
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
        }
        let raw = Raw::deserialize(deserializer)?;
        let defaults = ProvidersConfig::default();
        Ok(ProvidersConfig {
            apps: raw.apps.apply(defaults.apps),
            calculator: raw.calculator.apply(defaults.calculator),
            web: raw.web.apply(defaults.web),
        })
    }
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            hotkey: "alt-space".into(),
            width: 680.0,
            max_results: 8,
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
