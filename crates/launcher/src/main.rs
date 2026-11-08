use launcher_agent::config;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use config::{Config, config_path, history_path};
use gpui::Application;
use launcher_agent::core::{HistoryStore, Provider, ProviderRegistry, SearchCoordinator};
use launcher_agent::macos::{
    GlobalHotkey, NativePlatform, configure_accessory_app, discover_in, is_activation_event,
    resident::{ResidentEvent, ResidentMenu, show_error_alert},
};
use launcher_agent::providers::apps::ApplicationProvider;
use launcher_agent::providers::calculator::CalculatorProvider;
use launcher_agent::providers::web::WebProvider;
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
    /// Connect to an already-running shared service instead of starting embedded providers
    #[arg(long, conflicts_with = "config")]
    service_socket: Option<PathBuf>,
    /// Validate embedded config, or probe IPC in service mode, without opening a window
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
    let report_visibly = !args.check && cfg!(target_os = "macos");
    run(args).inspect_err(|error| {
        if report_visibly {
            report_error("maccer could not start", error);
        }
    })
}

fn run(args: Args) -> Result<()> {
    tracing::debug!(show_on_start = args.show, "launcher starting");
    let socket_env = std::env::var_os("MACCER_SERVICE_SOCKET");
    if let Some(socket) = selected_service_socket(&args, socket_env.as_deref())? {
        return run_service_frontend(socket, args.show, args.check);
    }
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
    if !args.check && config.service.frontend.is_some() {
        tracing::warn!(
            "a shared-service frontend is configured; use the backend shortcut instead of starting an embedded GPUI frontend"
        );
        return Ok(());
    }
    if config.files.use_zoxide || config.files.use_fzf {
        tracing::warn!("files.use_zoxide/use_fzf are ignored; the files provider now uses fd");
    }
    let app_roots = config.apps.search_paths()?;
    if args.check {
        let applications = discover_in(&app_roots).context("scanning installed applications")?;
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
        if config.providers.files.enabled {
            println!(
                "files backend: fd ({})",
                launcher_agent::macos::fd_index::FdIndex::executable()?.display()
            );
        }
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        bail!(
            "maccer requires macOS; use --check only to inspect configuration and application scanning"
        );
    }

    let history_file = history_path()?;
    if let Some(parent) = history_file.parent() {
        std::fs::create_dir_all(parent).context("creating history directory")?;
    }
    let history = Arc::new(HistoryStore::open(&history_file).context("opening usage history")?);
    let platform = Arc::new(NativePlatform);
    let mut registry = ProviderRegistry::new();
    // A watched provider installs watches before its first scan and republishes
    // prebuilt records after debounced changes. A disabled provider is never
    // searched, so it neither scans nor watches.
    let apps: Arc<dyn Provider> = if !config.providers.apps.enabled {
        Arc::new(ApplicationProvider::new(Vec::new(), platform.clone()))
    } else if config.apps.watch {
        Arc::new(
            ApplicationProvider::watched(app_roots.clone(), platform.clone())
                .context("starting application catalog watcher")?,
        )
    } else {
        let applications = discover_in(&app_roots).context("scanning installed applications")?;
        Arc::new(ApplicationProvider::new(applications, platform.clone()))
    };
    registry.register(apps, config.providers.apps.clone())?;
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
    if config.providers.files.enabled {
        let cache = dirs::cache_dir()
            .context("finding files cache directory")?
            .join("maccer/files.sqlite3");
        let mut files_config = config.files.fd_config()?;
        // Internal SQLite/WAL writes must not feed back into live indexing.
        if let Some(parent) = history_file.parent() {
            files_config.excluded.push(parent.to_path_buf());
        }
        let index = launcher_agent::macos::fd_index::FdIndex::open(files_config, &cache)?;
        index.start();
        registry.register(
            Arc::new(
                launcher_agent::providers::files::FileProvider::new(
                    index,
                    Arc::new(NativePlatform),
                    dirs::home_dir().context("finding home directory for file scope")?,
                )
                .with_roots(config.files.expanded_roots()?)
                .with_ignored_previews(config.files.expanded_ignored_previews()?)
                .with_history(history.clone()),
            ),
            config.providers.files.clone(),
        )?;
    }
    registry.register(
        Arc::new(launcher_agent::providers::shell::ShellProvider::new(
            Arc::new(NativePlatform),
            config.shell.commands.clone(),
            dirs::home_dir().context("finding shell working directory")?,
        )?),
        config.providers.shell.clone(),
    )?;
    let clipboard = if config.providers.clipboard.enabled {
        let provider = Arc::new(
            launcher_agent::providers::clipboard::ClipboardProvider::new(
                Arc::new(NativePlatform),
                config.clipboard.max_entries,
            ),
        );
        registry.register(provider.clone(), config.providers.clipboard.clone())?;
        Some(provider)
    } else {
        None
    };
    let registry = Arc::new(registry);
    let coordinator = Arc::new(SearchCoordinator::new(
        registry,
        history.clone(),
        config.launcher.max_results,
    ));

    let show_on_start = args.show;

    Application::new().run(move |cx| {
        if let Some(clipboard) = clipboard {
            // Initialize the change-count baseline before the first timer tick.
            // Pre-existing clipboard text is deliberately not added to history.
            let mut previous = None;
            if let Ok(Some(snapshot)) = launcher_agent::macos::clipboard::snapshot_if_changed(None) {
                previous = Some(snapshot.change_count);
                clipboard.capture(snapshot.change_count, None);
            }
            cx.spawn(async move |cx| {
                loop {
                    gpui::Timer::after(std::time::Duration::from_millis(300)).await;
                    if cx
                        .update(|_| {
                            // NSPasteboard access stays on the application thread, independent
                            // of provider search workers. Read only when changeCount advances.
                            if let Ok(Some(snapshot)) =
                                launcher_agent::macos::clipboard::snapshot_if_changed(previous)
                            {
                                previous = Some(snapshot.change_count);
                                clipboard.capture(snapshot.change_count, snapshot.text.as_deref());
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        if let Err(error) = configure_accessory_app() {
            fail_startup(cx, error.context("setting accessory activation policy"));
            return;
        }
        let (events_tx, events_rx) = async_channel::unbounded::<ResidentEvent>();
        let menu = match ResidentMenu::new(events_tx.clone()) {
            Ok(menu) => menu,
            Err(error) => {
                fail_startup(cx, error.context("creating menu bar controls"));
                return;
            }
        };
        let window = match launcher_agent::ui::open_launcher(
            coordinator,
            history,
            launcher_agent::ui::LauncherOptions {
                width: config.launcher.width,
                max_results: config.launcher.max_results,
                web: launcher_agent::ui::WebPreviewOptions {
                    inline: config.web.inline_preview,
                    prewarm: config.web.prewarm,
                    idle_release: std::time::Duration::from_secs(
                        config.web.idle_release_seconds,
                    ),
                },
            },
            cx,
        ) {
            Ok(window) => window,
            Err(error) => {
                fail_startup(cx, error.context("opening launcher window"));
                return;
            }
        };
        if show_on_start
            && let Err(error) = window.update(cx, |launcher, window, cx| launcher.show(window, cx))
        {
            fail_startup(cx, error.context("showing launcher window"));
            return;
        }
        let hotkey = match GlobalHotkey::new(&config.launcher.hotkey) {
            Ok(hotkey) => Some(hotkey),
            Err(error) => {
                let error = error.context("Use Open Launcher in the menu bar. Change [launcher].hotkey in your config and restart maccer.");
                cx.spawn(async move |_| {
                    // Yield until Application::run releases its mutable app borrow.
                    gpui::Timer::after(std::time::Duration::ZERO).await;
                    report_error("maccer shortcut unavailable", &error);
                }).detach();
                None
            }
        };
        if let Some(hotkey) = &hotkey {
            let hotkey_id = hotkey.id();
            tracing::debug!(shortcut = %config.launcher.hotkey, hotkey_id, "launcher shortcut registered");
            std::thread::spawn(move || {
                loop {
                    match global_hotkey::GlobalHotKeyEvent::receiver().recv() {
                        Ok(event) if is_activation_event(&event, hotkey_id) => {
                            if events_tx.send_blocking(ResidentEvent::ToggleLauncher).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::error!(%error, "global hotkey event channel closed");
                            let _ = events_tx.send_blocking(ResidentEvent::HotkeyFailed);
                            break;
                        }
                    }
                }
            });
        }
        tracing::debug!("launcher ready; menu bar controls available");
        cx.spawn(async move |cx| {
            // These native owners must outlive the hidden window.
            let _hotkey = hotkey;
            let menu = menu;
            while let Ok(event) = events_rx.recv().await {
                let result = match event {
                    ResidentEvent::OpenLauncher | ResidentEvent::ToggleLauncher => cx
                        .update(|cx| {
                            window.update(cx, |launcher, window, cx| {
                                if event == ResidentEvent::OpenLauncher {
                                    launcher.show(window, cx);
                                } else {
                                    launcher.toggle(window, cx);
                                }
                            })
                        })
                        .and_then(|result| result),
                    // Modal Cocoa loops must never run inside cx.update, which borrows App.
                    ResidentEvent::ToggleLaunchAtLogin => menu.toggle_launch_at_login(),
                    ResidentEvent::Quit => {
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                    ResidentEvent::HotkeyFailed => {
                        show_error_alert(
                            "maccer shortcut stopped working",
                            "Use Open Launcher in the menu bar, then restart maccer to restore the shortcut.",
                        );
                        continue;
                    }
                };
                if let Err(error) = result {
                    report_error("maccer action failed", &error);
                }
            }
        })
        .detach();
    });

    Ok(())
}

/// Choose IPC before reading any embedded configuration or initializing providers.
fn selected_service_socket(
    args: &Args,
    environment: Option<&std::ffi::OsStr>,
) -> Result<Option<PathBuf>> {
    let socket = args.service_socket.clone().or_else(|| {
        environment
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    if let Some(path) = &socket {
        if args.config.is_some() {
            bail!(
                "--config belongs to the backend; pass it to maccer-service, not the IPC frontend"
            );
        }
        let text = path.to_str().context("service socket path must be UTF-8")?;
        if !path.is_absolute() || text.len() > 4096 || text.contains('\0') {
            bail!("service socket path must be absolute, NUL-free and at most 4096 UTF-8 bytes");
        }
    }
    Ok(socket)
}

fn run_service_frontend(socket: PathBuf, show_on_start: bool, check: bool) -> Result<()> {
    if check {
        // This read-only CLI probe runs before GPUI, so blocking IPC cannot block its UI.
        let mut client = launcher_agent::client::Client::connect(&socket)
            .context("connecting to the existing service; no backend was started")?;
        let providers = client
            .providers()
            .context("reading service provider descriptors")?;
        println!("service instance: {}", client.hello().instance_id);
        println!("providers available: {}", providers.len());
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        bail!("the GPUI frontend requires macOS; use --check to probe the service");
    }
    let _instance = match launcher_agent::agent::frontend_startup::claim(&socket, "gpui") {
        Ok(instance) => instance,
        Err(error) => {
            // Refusal must not leave a resident UI or a modal startup alert
            // beside the selected frontend. Diagnostics stay in the log.
            tracing::warn!("GPUI frontend startup refused: {error:#}");
            return Ok(());
        }
    };
    Application::new().run(move |cx| {
        if let Err(error) = configure_accessory_app() {
            fail_startup(cx, error.context("setting frontend activation policy"));
            return;
        }
        let (events_tx, events_rx) = async_channel::unbounded::<ResidentEvent>();
        let menu = match ResidentMenu::new_frontend(events_tx) {
            Ok(menu) => menu,
            Err(error) => {
                fail_startup(cx, error.context("creating frontend menu controls"));
                return;
            }
        };
        let window = match launcher_agent::ui::open_service_launcher(
            &socket,
            launcher_agent::ui::LauncherOptions {
                width: 480.,
                max_results: 50,
                ..Default::default()
            },
            cx,
        ) {
            Ok(window) => window,
            Err(error) => {
                fail_startup(cx, error.context("opening service frontend"));
                return;
            }
        };
        if show_on_start
            && let Err(error) = window.update(cx, |launcher, window, cx| launcher.show(window, cx))
        {
            fail_startup(cx, error.context("showing service frontend"));
            return;
        }
        // No clipboard timer, native icon resolver, provider registry, history store,
        // global shortcut or service lifecycle owner is created by this branch.
        cx.spawn(async move |cx| {
            let menu = menu;
            while let Ok(event) = events_rx.recv().await {
                let result = match event {
                    ResidentEvent::OpenLauncher | ResidentEvent::ToggleLauncher => cx
                        .update(|cx| {
                            window.update(cx, |launcher, window, cx| {
                                if event == ResidentEvent::OpenLauncher {
                                    launcher.show(window, cx);
                                } else {
                                    launcher.toggle(window, cx);
                                }
                            })
                        })
                        .and_then(|result| result),
                    ResidentEvent::ToggleLaunchAtLogin => menu.toggle_launch_at_login(),
                    ResidentEvent::Quit => {
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                    ResidentEvent::HotkeyFailed => continue,
                };
                if let Err(error) = result {
                    report_error("maccer frontend action failed", &error);
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
fn fail_startup(cx: &mut gpui::App, error: anyhow::Error) {
    let error = error.context("launcher startup failed");
    cx.spawn(async move |_| {
        // runModal spins Cocoa's event loop. Wait until startup releases App's
        // RefCell borrow so clipboard/window callbacks can safely re-enter GPUI.
        gpui::Timer::after(std::time::Duration::ZERO).await;
        report_error("maccer could not start", &error);
        std::process::exit(1);
    })
    .detach();
}

fn report_error(title: &str, error: &anyhow::Error) {
    let detail = format!("{error:#}");
    tracing::error!(%detail, "{title}");
    show_error_alert(title, &detail);
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

    #[test]
    fn service_mode_selection_is_explicit_and_cli_overrides_environment() {
        let args = Args::try_parse_from(["maccer", "--service-socket", "/tmp/owned/rpc", "--show"])
            .unwrap();
        assert_eq!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new("/tmp/env/rpc"))).unwrap(),
            Some(PathBuf::from("/tmp/owned/rpc")),
        );
        let args = Args::try_parse_from(["maccer", "--show"]).unwrap();
        assert_eq!(selected_service_socket(&args, None).unwrap(), None);
        assert_eq!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new(""))).unwrap(),
            None
        );
        assert_eq!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new("/tmp/env/rpc"))).unwrap(),
            Some(PathBuf::from("/tmp/env/rpc")),
        );
    }

    #[test]
    fn service_mode_rejects_frontend_provider_config_and_invalid_paths() {
        assert!(
            Args::try_parse_from([
                "maccer",
                "--service-socket",
                "/tmp/owned/rpc",
                "--config",
                "/does/not/exist",
            ])
            .is_err()
        );
        let args = Args::try_parse_from(["maccer", "--config", "/does/not/exist"]).unwrap();
        assert!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new("/tmp/owned/rpc"))).is_err()
        );
        let args = Args::try_parse_from(["maccer", "--check"]).unwrap();
        assert!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new("relative/rpc"))).is_err()
        );
        assert!(
            selected_service_socket(&args, Some(std::ffi::OsStr::new("/tmp/bad\0rpc"))).is_err()
        );
        assert!(
            selected_service_socket(
                &args,
                Some(std::ffi::OsStr::new(&format!("/{}", "x".repeat(4096))))
            )
            .is_err()
        );
    }
}
