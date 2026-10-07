use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, CancellationToken, IconDescriptor, Item, ItemId, Provider,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_macos::{Platform, process::run_bounded};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
pub struct CustomCommand {
    pub name: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub command: String,
}

pub struct ShellProvider {
    platform: Arc<dyn Platform>,
    commands: Vec<CustomCommand>,
    home: PathBuf,
    outputs: Mutex<HashMap<String, String>>,
}
impl ShellProvider {
    pub fn new(
        platform: Arc<dyn Platform>,
        commands: Vec<CustomCommand>,
        home: PathBuf,
    ) -> Result<Self> {
        if commands.len() > 128 || !home.is_absolute() || !home.is_dir() {
            bail!(
                "shell commands require an existing absolute working directory and at most 128 configured commands"
            );
        }
        let mut names = std::collections::HashSet::new();
        for command in &commands {
            if command.name.trim().is_empty()
                || command.name.len() > 128
                || command.name.chars().any(char::is_control)
                || command.command.trim().is_empty()
                || command.command.len() > 8192
                || command.command.contains('\0')
                || command.keywords.len() > 32
                || command
                    .keywords
                    .iter()
                    .any(|keyword| keyword.len() > 128 || keyword.chars().any(char::is_control))
                || !names.insert(&command.name)
            {
                bail!(
                    "custom commands require unique names of 1–128 bytes, scripts of 1–8192 bytes without NUL, and at most 32 keywords of 128 bytes without control characters"
                );
            }
        }
        Ok(Self {
            platform,
            commands,
            home,
            outputs: Mutex::new(HashMap::new()),
        })
    }
}
impl Provider for ShellProvider {
    fn id(&self) -> ProviderId {
        ProviderId("shell".into())
    }
    fn name(&self) -> &str {
        "Shell / Commands"
    }
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn records_usage(&self) -> bool {
        false
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        let text = query.text.trim();
        if text.len() > 8192 || text.contains('\0') {
            bail!("command is too long or contains NUL");
        }
        let needle = text.to_lowercase();
        let mut items: Vec<_> = self
            .commands
            .iter()
            .filter(|command| {
                let haystack =
                    format!("{} {}", command.name, command.keywords.join(" ")).to_lowercase();
                needle
                    .split_whitespace()
                    .all(|word| haystack.contains(word))
            })
            .map(|command| Item {
                id: ItemId(format!("command:{}", command.name)),
                provider: self.id(),
                title: command.name.clone(),
                subtitle: Some("Configured shell command".into()),
                keywords: command.keywords.clone(),
                icon: Some(IconDescriptor::Text("›".into())),
                score: 5.0,
                payload: json!({"configured": command.name}),
            })
            .collect();
        if !text.is_empty() {
            items.push(Item {
                id: ItemId(format!("shell:{text}")),
                provider: self.id(),
                title: format!("Run: {text}"),
                subtitle: Some("Explicit shell command · 30s timeout".into()),
                keywords: vec![text.into()],
                icon: Some(IconDescriptor::Text("›".into())),
                score: 0.0,
                payload: json!({"command": text}),
            });
        }
        items.truncate(ctx.limit);
        Ok(items)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if item.provider != self.id() {
            return vec![];
        }
        [
            ("execute", "Execute and Show Output"),
            ("copy-command", "Copy Command"),
            ("copy-output", "Copy Last Output"),
        ]
        .into_iter()
        .map(|(id, title)| Action {
            id: ActionId(id.into()),
            title: title.into(),
        })
        .collect()
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        self.activate_with_cancellation(item, action, &CancellationToken::default())
    }
    fn activate_with_cancellation(
        &self,
        item: &Item,
        action: &Action,
        cancellation: &CancellationToken,
    ) -> Result<ActionOutcome> {
        if cancellation.is_cancelled() {
            bail!("action cancelled");
        }
        if item.provider != self.id() {
            bail!("item is not a shell result");
        }
        let command = if let Some(name) = item.payload["configured"].as_str() {
            let configured = self
                .commands
                .iter()
                .find(|c| c.name == name && item.id.0 == format!("command:{name}"))
                .ok_or_else(|| anyhow::anyhow!("unknown configured command"))?;
            configured.command.as_str()
        } else {
            let command = item.payload["command"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing command"))?;
            if item.id.0 != format!("shell:{command}")
                || command.trim().is_empty()
                || command.len() > 8192
                || command.contains('\0')
            {
                bail!("invalid command identity");
            }
            command
        };
        match action.id.0.as_str() {
            "copy-command" => {
                self.platform.copy_text(command)?;
                Ok(ActionOutcome::Close)
            }
            "copy-output" => {
                let outputs = self
                    .outputs
                    .lock()
                    .map_err(|_| anyhow::anyhow!("command output lock poisoned"))?;
                let text = outputs
                    .get(&item.id.0)
                    .ok_or_else(|| anyhow::anyhow!("run this command before copying its output"))?
                    .clone();
                drop(outputs);
                self.platform.copy_text(&text)?;
                Ok(ActionOutcome::Close)
            }
            "execute" => {
                let mut process = Command::new("/bin/sh");
                process.args(["-c", command]).current_dir(&self.home);
                let result = run_bounded(
                    &mut process,
                    cancellation,
                    Duration::from_secs(30),
                    64 * 1024,
                )?;
                let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
                if !result.stderr.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&String::from_utf8_lossy(&result.stderr));
                }
                if result.stdout.len() == 64 * 1024 || result.stderr.len() == 64 * 1024 {
                    text.push_str("\nOutput retention limit reached (64 KiB per stream).");
                }
                if !result.status.success() {
                    text.push_str(&format!("\nProcess exited with {}", result.status));
                }
                if text.is_empty() {
                    text = "Command completed with no output.".into();
                }
                let mut outputs = self
                    .outputs
                    .lock()
                    .map_err(|_| anyhow::anyhow!("command output lock poisoned"))?;
                if outputs.len() >= 32 {
                    outputs.clear();
                }
                outputs.insert(item.id.0.clone(), text.clone());
                Ok(ActionOutcome::Output {
                    title: "Command output".into(),
                    text,
                })
            }
            _ => bail!("unsupported shell action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    struct Fake;
    impl Platform for Fake {
        fn launch_application(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn reveal_in_finder(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn open_url(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn copy_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }
    fn provider() -> ShellProvider {
        ShellProvider::new(
            Arc::new(Fake),
            vec![CustomCommand {
                name: "Dotfiles".into(),
                keywords: vec!["config".into()],
                command: "printf configured".into(),
            }],
            std::env::temp_dir(),
        )
        .unwrap()
    }
    fn context() -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: CancellationToken::default(),
            limit: 8,
        }
    }
    #[test]
    fn searching_named_commands_cannot_create_script_side_effects() {
        let fixture = tempfile::tempdir().unwrap();
        let marker = fixture.path().join("must-not-exist");
        let script = format!("printf marker > '{}'", marker.display());
        let p = ShellProvider::new(
            Arc::new(Fake),
            vec![CustomCommand {
                name: "Create marker".into(),
                keywords: vec![],
                command: script.clone(),
            }],
            fixture.path().to_path_buf(),
        )
        .unwrap();
        for text in ["".to_string(), "Create marker".into(), script] {
            p.search(
                &SearchQuery {
                    raw: format!("> {text}"),
                    text,
                },
                &context(),
            )
            .unwrap();
            assert!(!marker.exists());
        }
    }

    #[test]
    fn captures_stderr_and_exit_status_without_losing_stdout() {
        let p = provider();
        let item = p
            .search(
                &SearchQuery {
                    raw: String::new(),
                    text: "printf ok; printf problem >&2; exit 7".into(),
                },
                &context(),
            )
            .unwrap()
            .pop()
            .unwrap();
        let ActionOutcome::Output { text, .. } = p
            .activate(
                &item,
                &Action {
                    id: ActionId("execute".into()),
                    title: String::new(),
                },
            )
            .unwrap()
        else {
            panic!("expected output");
        };
        assert!(text.starts_with("ok\nproblem"));
        assert!(text.contains("Process exited with"));
        assert!(text.contains('7'));
    }

    #[test]
    fn retained_command_output_is_bounded_and_reports_the_limit() {
        let p = provider();
        let item = p
            .search(
                &SearchQuery {
                    raw: String::new(),
                    text: "printf '%070000d' 0".into(),
                },
                &context(),
            )
            .unwrap()
            .pop()
            .unwrap();
        let ActionOutcome::Output { text, .. } = p
            .activate(
                &item,
                &Action {
                    id: ActionId("execute".into()),
                    title: String::new(),
                },
            )
            .unwrap()
        else {
            panic!("expected output");
        };
        assert!(text.starts_with(&"0".repeat(64 * 1024)));
        assert!(text.len() < 66 * 1024);
        assert!(text.contains("Output retention limit reached"));
    }

    #[test]
    fn search_is_inert_and_execution_only_happens_on_activation() {
        let p = provider();
        let items = p
            .search(
                &SearchQuery {
                    raw: "> printf 'hello world'".into(),
                    text: "printf 'hello world'".into(),
                },
                &context(),
            )
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(p.requires_explicit_scope());
        assert!(!p.records_usage());
        let result = p
            .activate(
                &items[0],
                &Action {
                    id: ActionId("execute".into()),
                    title: "Execute".into(),
                },
            )
            .unwrap();
        assert_eq!(
            result,
            ActionOutcome::Output {
                title: "Command output".into(),
                text: "hello world".into()
            }
        );
    }
    #[test]
    fn custom_commands_match_keywords_and_have_distinct_identity() {
        let p = provider();
        let items = p
            .search(
                &SearchQuery {
                    raw: "> config".into(),
                    text: "config".into(),
                },
                &context(),
            )
            .unwrap();
        assert_eq!(items[0].title, "Dotfiles");
        assert_eq!(items[0].id.0, "command:Dotfiles");
        let result = p
            .activate(
                &items[0],
                &Action {
                    id: ActionId("execute".into()),
                    title: "Execute".into(),
                },
            )
            .unwrap();
        assert_eq!(
            result,
            ActionOutcome::Output {
                title: "Command output".into(),
                text: "configured".into()
            }
        );
    }
    #[test]
    fn running_commands_cooperate_with_activation_cancellation() {
        let p = provider();
        let item = p
            .search(
                &SearchQuery {
                    raw: "> sleep 2".into(),
                    text: "sleep 2".into(),
                },
                &context(),
            )
            .unwrap()
            .pop()
            .unwrap();
        let token = CancellationToken::default();
        let cancel = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let result = p.activate_with_cancellation(
            &item,
            &Action {
                id: ActionId("execute".into()),
                title: "Execute".into(),
            },
            &token,
        );
        canceller.join().unwrap();
        assert!(
            result.is_err(),
            "cancelled commands must not report successful output"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn configured_commands_have_bounded_names_and_scripts() {
        for (name, command) in [
            ("x".repeat(129), "true".into()),
            ("bad\nname".into(), "true".into()),
            ("long command".into(), "x".repeat(8193)),
        ] {
            assert!(
                ShellProvider::new(
                    Arc::new(Fake),
                    vec![CustomCommand {
                        name,
                        keywords: vec![],
                        command
                    }],
                    std::env::temp_dir()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn invalid_custom_commands_are_rejected() {
        assert!(
            ShellProvider::new(
                Arc::new(Fake),
                vec![CustomCommand {
                    name: "".into(),
                    keywords: vec![],
                    command: "true".into()
                }],
                PathBuf::from("/tmp")
            )
            .is_err()
        );
    }
}
