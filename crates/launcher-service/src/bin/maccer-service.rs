use anyhow::{Context, Result, bail};
use launcher_service::{Server, Service};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut socket = None;
    let mut fixture = false;
    let mut apps = false;
    let mut config = None;
    let mut history = None;
    let mut frontend_kind = None;
    let mut frontend_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture = true,
            "--apps" => apps = true,
            "--socket" => take_path(&mut socket, &mut args, "--socket")?,
            "--config" => take_path(&mut config, &mut args, "--config")?,
            "--history" => take_path(&mut history, &mut args, "--history")?,
            "--frontend" => {
                if frontend_kind
                    .replace(args.next().context("--frontend requires gpui or pill")?)
                    .is_some()
                {
                    bail!("--frontend may only be specified once");
                }
            }
            "--frontend-path" => take_path(&mut frontend_path, &mut args, "--frontend-path")?,
            "--help" | "-h" => {
                println!(
                    "usage: maccer-service (--fixture | --apps) --socket ABSOLUTE_PATH [--config PATH] [--history PATH] [--frontend gpui|pill --frontend-path PATH]\n\nThe socket parent must be an existing private directory. Fixture mode has no native or desktop side effects.\n--apps enables configured applications, themes, web search, text clipboard capture and theme state. --config/--history require --apps.\nOptional --frontend selects the frontend opened by the backend-owned configured global hotkey; --frontend-path is its executable or .app path. Alternatively configure [service.frontend] in config. No frontend or hotkey is enabled unless selected."
                );
                return Ok(());
            }
            _ => bail!("unknown option {arg:?}; use --help"),
        }
    }
    if fixture && apps {
        bail!("cannot combine --fixture and --apps");
    }
    if !fixture && !apps {
        bail!("choose --fixture or --apps explicitly");
    }
    if fixture && (config.is_some() || history.is_some()) {
        bail!("--config and --history are only available with --apps");
    }
    if frontend_kind.is_some() != frontend_path.is_some() {
        bail!("--frontend and --frontend-path must be provided together");
    }
    if fixture && (frontend_kind.is_some() || frontend_path.is_some()) {
        bail!("--frontend and --frontend-path are only available with --apps");
    }
    if !apps && (frontend_kind.is_some() || frontend_path.is_some()) {
        bail!("--frontend and --frontend-path require --apps");
    }
    let socket = socket.context("--socket is required")?;
    let (service, hotkey, frontend, mut clipboard) = if fixture {
        (Service::fixture()?, None, None, None)
    } else {
        live_apps(config, history, frontend_kind, frontend_path, &socket)?
    };
    let server = Server::bind(&socket, service)?;
    // Signal handlers only set an atomic; ordinary code handles sockets and cleanup.
    unsafe {
        if libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t) == libc::SIG_ERR
            || libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t) == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    println!(
        "Maccer {} service listening at {}",
        if fixture { "fixture" } else { "application" },
        socket.display()
    );
    if fixture {
        return server.serve_until(&STOP);
    }
    // NativePlatform dispatches activation onto the process main queue. Never block
    // that queue in the socket accept loop, and never initialize AppKit in fixture mode.
    let worker = std::thread::Builder::new()
        .name("maccer-service-socket".into())
        .spawn(move || {
            let result = server.serve_until(&STOP);
            STOP.store(true, Ordering::Release);
            result
        })?;
    if hotkey.is_some() {
        eprintln!("Backend shortcut enabled for the selected frontend");
    }
    let host = launcher_macos::run_service_loop_with(&STOP, move || {
        if let Some(clipboard) = &mut clipboard {
            clipboard.tick();
        }
        if let Some(frontend) = &frontend {
            frontend.maintain();
            if hotkey
                .as_ref()
                .is_some_and(|hotkey| hotkey.try_toggle_requested())
                && let Err(error) = frontend.present()
            {
                eprintln!("maccer could not present configured frontend: {error:#}");
            }
        }
    });
    STOP.store(true, Ordering::Release);
    let socket_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("socket server panicked"))?;
    host?;
    socket_result
}

fn take_path(
    slot: &mut Option<PathBuf>,
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<()> {
    if slot.is_some() {
        bail!("{flag} may only be specified once");
    }
    *slot = Some(PathBuf::from(
        args.next()
            .with_context(|| format!("{flag} requires a path"))?,
    ));
    Ok(())
}

type LiveRuntime = (
    Arc<Service>,
    Option<launcher_macos::GlobalHotkey>,
    Option<Arc<launcher_service::FrontendRouter>>,
    Option<launcher_service::ClipboardMonitor>,
);

#[cfg(target_os = "macos")]
fn live_apps(
    config: Option<PathBuf>,
    history: Option<PathBuf>,
    frontend_kind: Option<String>,
    frontend_path: Option<PathBuf>,
    socket: &std::path::Path,
) -> Result<LiveRuntime> {
    use launcher_config::{Config, FrontendConfig, config_path, history_path};
    use launcher_core::HistoryStore;
    use launcher_macos::{NativePlatform, discover_in};
    use launcher_service::{
        ApplicationServiceProvider, AssetStore, ClipboardMonitor, ClipboardServiceProvider,
        NativeIconResolver, ServiceProvider, ThemeApply, ThemeCatalog, ThemeServiceProvider,
        ThemerState, WebServiceProvider,
    };
    use provider_apps::ApplicationProvider;
    let explicit = config.is_some();
    let config_file = match config {
        Some(path) => path,
        None => config_path(None)?,
    };
    let config = if explicit || config_file.exists() {
        Config::load(&config_file)?
    } else {
        let config = Config::default();
        config.validate()?;
        config
    };
    let selected_frontend = match (frontend_kind, frontend_path) {
        (Some(kind), Some(path)) => Some(FrontendConfig {
            kind,
            path: path.to_string_lossy().into_owned(),
        }),
        (None, None) => config.service.frontend.clone(),
        _ => unreachable!("CLI pairing validated before startup"),
    };
    let (hotkey, frontend) = if let Some(settings) = selected_frontend {
        let kind = match settings.kind.as_str() {
            "gpui" => launcher_protocol::FrontendKind::Gpui,
            "pill" => launcher_protocol::FrontendKind::Pill,
            _ => bail!("service.frontend.kind must be 'gpui' or 'pill'"),
        };
        let path = settings.expanded_path()?;
        let frontend = launcher_service::FrontendRouter::new(kind, path, socket.to_owned())?;
        let hotkey = launcher_macos::GlobalHotkey::new(&config.launcher.hotkey).context(
            "registering backend global shortcut; check [launcher].hotkey and shortcut conflicts",
        )?;
        (Some(hotkey), Some(frontend))
    } else {
        (None, None)
    };
    let history_file = match history {
        Some(path) => path,
        None => history_path()?,
    };
    if !history_file.is_absolute() {
        bail!("usage history path must be absolute");
    }
    if let Some(parent) = history_file.parent() {
        std::fs::create_dir_all(parent).context("creating service history directory")?;
    }
    let history =
        Arc::new(HistoryStore::open(&history_file).context("opening service usage history")?);
    let platform = Arc::new(NativePlatform);
    let provider = if !config.providers.apps.enabled {
        ApplicationProvider::new(Vec::new(), platform.clone())
    } else {
        let roots = config.apps.search_paths()?;
        if config.apps.watch {
            ApplicationProvider::watched(roots, platform.clone())
                .context("starting application catalog watcher")?
        } else {
            ApplicationProvider::new(
                discover_in(&roots).context("scanning installed applications")?,
                platform.clone(),
            )
        }
    };
    let themes = if config.providers.themes.enabled {
        let helper = themer_helper()?;
        ThemeServiceProvider::with_apply(
            ThemeCatalog::start(helper.clone()),
            ThemeApply::new(helper),
        )
    } else {
        ThemeServiceProvider::new(ThemeCatalog::from_palettes(Vec::new())?)
    };
    let assets = AssetStore::new(Arc::new(NativeIconResolver));
    let mut registrations: Vec<(Arc<dyn ServiceProvider>, launcher_core::ProviderConfig)> = vec![
        (
            Arc::new(ApplicationServiceProvider::new(provider, assets.clone())),
            config.providers.apps,
        ),
        (Arc::new(themes), config.providers.themes),
    ];
    if config.providers.web.enabled {
        let web = provider_web::WebProvider::new(
            platform.clone(),
            config.web.engines,
            config.web.default_engine,
        )?;
        registrations.push((Arc::new(WebServiceProvider::new(web)), config.providers.web));
    }
    let clipboard = if config.providers.clipboard.enabled {
        let provider = Arc::new(provider_clipboard::ClipboardProvider::new(
            platform,
            config.clipboard.max_entries,
        ));
        registrations.push((
            Arc::new(ClipboardServiceProvider::new(provider.clone())),
            config.providers.clipboard,
        ));
        Some(ClipboardMonitor::new(provider))
    } else {
        None
    };
    let service = Service::with_state(
        registrations,
        history,
        Some(assets),
        ThemerState::watch(theme_directory()?),
    )?;
    if let Some(frontend) = &frontend {
        service.attach_frontend(frontend.clone())?;
    }
    Ok((service, hotkey, frontend, clipboard))
}

#[cfg(target_os = "macos")]
fn themer_helper() -> Result<PathBuf> {
    let user_home = dirs::home_dir().context("finding home for theme catalog helper")?;
    let bin = if let Some(value) = std::env::var_os("MACTHEMER_BIN").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        path.strip_prefix("~")
            .map(|relative| user_home.join(relative))
            .unwrap_or(path)
    } else {
        std::env::var_os("MACTHEMER_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(user_home)
            .join(".local/bin")
    };
    Ok(bin.join("mac-themer"))
}

/// mac-themer state defaults and current-user overrides from `paths.py`.
#[cfg(target_os = "macos")]
fn theme_directory() -> Result<PathBuf> {
    let user_home = dirs::home_dir().context("finding home for published theme state")?;
    Ok(resolve_theme_directory(
        std::env::var_os("MACTHEMER_STATE"),
        std::env::var_os("MACTHEMER_HOME"),
        user_home,
    ))
}

#[cfg(target_os = "macos")]
fn resolve_theme_directory(
    state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    user_home: PathBuf,
) -> PathBuf {
    if let Some(value) = state.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        // mac-themer's _env_path expands the current user's ~, independently
        // of its optional MACTHEMER_HOME override used for the default path.
        return path
            .strip_prefix("~")
            .map(|relative| user_home.join(relative))
            .unwrap_or(path);
    }
    home.filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(user_home)
        .join(".config")
        .join("macthemer")
}

#[cfg(all(test, target_os = "macos"))]
mod theme_path_tests {
    use super::*;
    #[test]
    fn state_directory_expands_tilde_independently_of_themer_home() {
        assert_eq!(
            resolve_theme_directory(
                Some("~/palette".into()),
                Some("/different".into()),
                "/owned/home".into()
            ),
            PathBuf::from("/owned/home/palette")
        );
        assert_eq!(
            resolve_theme_directory(Some("~".into()), None, "/owned/home".into()),
            PathBuf::from("/owned/home")
        );
    }
    #[test]
    fn empty_override_uses_themer_home_or_user_home() {
        assert_eq!(
            resolve_theme_directory(
                Some("".into()),
                Some("/owned/themer".into()),
                "/owned/home".into()
            ),
            PathBuf::from("/owned/themer/.config/macthemer")
        );
        assert_eq!(
            resolve_theme_directory(None, None, "/owned/home".into()),
            PathBuf::from("/owned/home/.config/macthemer")
        );
        assert_eq!(
            resolve_theme_directory(Some("/owned/state".into()), None, "/owned/home".into()),
            PathBuf::from("/owned/state")
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn live_apps(
    _: Option<PathBuf>,
    _: Option<PathBuf>,
    _: Option<String>,
    _: Option<PathBuf>,
    _: &std::path::Path,
) -> Result<LiveRuntime> {
    bail!("live application service requires macOS")
}
