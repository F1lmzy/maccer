mod config;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use config::{Config, config_path, history_path};
use gpui::Application;
use launcher_core::{HistoryStore, ProviderRegistry, SearchCoordinator};
use launcher_macos::{
    GlobalHotkey, NativePlatform, configure_accessory_app, discover_applications,
    is_activation_event,
};
use provider_apps::ApplicationProvider;
use provider_calculator::CalculatorProvider;
use provider_web::WebProvider;
use std::{path::PathBuf, sync::Arc};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "maccer",
    version,
    about = "A keyboard-first macOS application launcher"
)]
struct Args {
    /// Read configuration from this path instead of ~/.config/maccer/config.toml
    #[arg(long)]
    config: Option<PathBuf>,
    /// Validate config and scan applications without opening a window or registering a hotkey
    #[arg(long)]
    check: bool,
    /// Open the launcher immediately on startup instead of starting hidden
    #[arg(long)]
    show: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let args = Args::parse();
    let config_file = config_path(args.config.as_deref())?;
    if args.config.is_some() && !config_file.is_file() {
        bail!(
            "specified config file does not exist: {}",
            config_file.display()
        );
    }
    let config = if config_file.exists() {
        Config::load(&config_file)?
    } else {
        let defaults = Config::default();
        defaults.validate()?;
        defaults
    };
    let applications = discover_applications().context("scanning installed applications")?;
    if args.check {
        println!(
            "configuration: {}",
            if config_file.exists() {
                config_file.display().to_string()
            } else {
                format!("{} (defaults; file not found)", config_file.display())
            }
        );
        println!("applications indexed: {}", applications.len());
        println!("hotkey: {}", config.launcher.hotkey);
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        bail!(
            "maccer requires macOS; use --check only to inspect configuration and application scanning"
        );
    }

    let platform = Arc::new(NativePlatform);
    let mut registry = ProviderRegistry::new();
    registry.register(
        Arc::new(ApplicationProvider::new(applications, platform.clone())),
        config.providers.apps.clone(),
    )?;
    registry.register(
        Arc::new(CalculatorProvider::new(platform.clone())),
        config.providers.calculator.clone(),
    )?;
    registry.register(
        Arc::new(WebProvider::new(
            platform,
            config.web.engines.clone(),
            config.web.default_engine.clone(),
        )?),
        config.providers.web.clone(),
    )?;
    registry.register(
        Arc::new(provider_files::FileProvider::new(
            Arc::new(launcher_macos::spotlight::Spotlight),
            Arc::new(NativePlatform),
            dirs::home_dir().context("finding home directory for file scope")?,
        )),
        config.providers.files.clone(),
    )?;
    let registry = Arc::new(registry);
    let history_file = history_path()?;
    if let Some(parent) = history_file.parent() {
        std::fs::create_dir_all(parent).context("creating history directory")?;
    }
    let history = Arc::new(HistoryStore::open(&history_file).context("opening usage history")?);
    let coordinator = Arc::new(SearchCoordinator::new(
        registry,
        history.clone(),
        config.launcher.max_results,
    ));

    let show_on_start = args.show;

    Application::new().run(move |cx| {
        if let Err(error) = configure_accessory_app() {
            tracing::warn!(%error, "could not set accessory activation policy");
        }
        let hotkey = match GlobalHotkey::new(&config.launcher.hotkey) {
            Ok(hotkey) => hotkey,
            Err(error) => fail_startup(error.context("registering global hotkey")),
        };
        let window = match launcher_ui::open_launcher(
            coordinator,
            history,
            launcher_ui::LauncherOptions {
                width: config.launcher.width,
                max_results: config.launcher.max_results,
            },
            cx,
        ) {
            Ok(window) => window,
            Err(error) => fail_startup(error.context("opening launcher window")),
        };
        if show_on_start
            && let Err(error) = window.update(cx, |launcher, window, cx| launcher.show(window, cx))
        {
            fail_startup(error.context("showing launcher window"));
        }
        let hotkey_id = hotkey.id();
        let (events_tx, events_rx) = async_channel::unbounded::<()>();
        std::thread::spawn(move || {
            loop {
                match global_hotkey::GlobalHotKeyEvent::receiver().recv() {
                    Ok(event) if is_activation_event(&event, hotkey_id) => {
                        if events_tx.send_blocking(()).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "global hotkey event channel closed");
                        break;
                    }
                }
            }
        });
        cx.spawn(async move |cx| {
            let _hotkey = hotkey;
            while events_rx.recv().await.is_ok() {
                // `cx.update` and `WindowHandle::update` each return a `Result`, so the
                // nested call yields `Result<Result<_, _>>`. Flatten it, otherwise an inner
                // window error is silently swallowed and the loop keeps the hotkey alive.
                if let Err(error) = cx
                    .update(|cx| {
                        window.update(cx, |launcher, window, cx| launcher.toggle(window, cx))
                    })
                    .and_then(|result| result)
                {
                    tracing::debug!(%error, "launcher window is no longer available");
                    break;
                }
            }
        })
        .detach();
    });

    Ok(())
}

/// Report a fatal startup error and terminate with a nonzero status.
///
/// `cx.quit()` is not used here. On macOS it terminates through
/// `[NSApp terminate:]` with exit status 0, and `Application::run` does not
/// return on that path, so a startup failure would be indistinguishable from a
/// clean shutdown. Exiting directly also guarantees no invisible resident
/// process is left behind.
fn fail_startup(error: anyhow::Error) -> ! {
    let error = error.context("launcher startup failed");
    tracing::error!(%error, "launcher startup failed");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_and_check_flags_parse() {
        let args = Args::try_parse_from(["maccer", "--show"]).unwrap();
        assert!(args.show);
        assert!(!args.check);

        let args = Args::try_parse_from(["maccer", "--check"]).unwrap();
        assert!(args.check);
        assert!(!args.show);

        assert!(Args::try_parse_from(["maccer", "--nonsense"]).is_err());
    }
}
