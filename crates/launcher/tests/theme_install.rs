//! The `themes.install` / `themes.uninstall` endpoints over a socket.
//!
//! Installing a theme is a mutation outside the action table: a URL is not
//! something a retained action can carry, so it has its own method. These tests
//! pin that the method exists only when a service attached an installer, that
//! the helper is invoked with a fixed argument list, that the reply is the
//! producer's own report rather than a claim, and that every failure is a fixed
//! message with no helper output and no filesystem path in it.
use launcher_agent::client::{Client, RemoteError};
use launcher_agent::core::{HistoryStore, ProviderConfig};
use launcher_agent::protocol::{
    ThemeInstallParams, ThemeInstallResult, ThemePackSummary, ThemeUninstallParams,
    ThemeUninstallResult,
};
use launcher_agent::service::{
    Server, Service, ThemeCatalog, ThemeInstall, ThemeServiceProvider, ThemerState,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    /// A service with an installer whose helper runs `body`, plus the catalog it
    /// publishes into.
    fn start(body: &str) -> Self {
        Self::build(body, true)
    }

    /// A service with no installer attached: the methods must not exist.
    fn read_only() -> Self {
        Self::build("exit 1", false)
    }

    fn build(body: &str, attach: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-install-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let helper = dir.path().join("helper");
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${{0%/*}}/calls\"\n{body}\n");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = ThemeCatalog::from_palettes(Vec::new()).unwrap();
        let provider = Arc::new(ThemeServiceProvider::new(catalog));
        let service = Service::with_state(
            vec![(provider, ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            ThemerState::fixture(),
        )
        .unwrap();
        if attach {
            service
                .attach_theme_install(ThemeInstall::new(helper))
                .unwrap();
        }
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || server.serve_until(&worker_stop));
        Self {
            dir,
            stop,
            worker: Some(worker),
        }
    }

    fn client(&self) -> Client {
        Client::connect(self.dir.path().join("rpc")).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

const ONE_PACK: &str = "printf '%s' '{\"schema\":1,\"status\":\"installed\",\"packs\":[{\"slug\":\"aura\",\"name\":\"Aura\",\"wallpapers\":7,\"replaced\":false}]}'";
const REMOVED: &str = "printf '%s' '{\"schema\":1,\"status\":\"removed\",\"slug\":\"aura\"}'";

fn code(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<RemoteError>()
        .map(|error| error.0.data.code.clone())
        .unwrap_or_else(|| "NO_REMOTE_ERROR".into())
}

fn message(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<RemoteError>()
        .map(|error| error.0.message.clone())
        .unwrap_or_default()
}

fn install_params(url: &str, all: bool, force: bool) -> ThemeInstallParams {
    ThemeInstallParams {
        url: url.into(),
        all,
        force,
    }
}

#[test]
fn installing_requires_an_attached_installer_and_passes_a_fixed_argument_list() {
    let read_only = Running::read_only();
    let mut client = read_only.client();
    assert!(
        !client
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == "themeInstall")
    );
    let error = client
        .request::<ThemeInstallResult>(
            "themes.install",
            install_params("https://github.com/owner/omarchy-aura", false, false),
        )
        .unwrap_err();
    assert_eq!(code(&error), "METHOD_NOT_FOUND");
    assert!(read_only.calls().is_empty());

    let running = Running::start(ONE_PACK);
    let mut client = running.client();
    assert!(
        client
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == "themeInstall")
    );
    let result = client
        .request::<ThemeInstallResult>(
            "themes.install",
            install_params(
                "https://github.com/owner/omarchy-aura-theme.git/",
                false,
                false,
            ),
        )
        .unwrap();
    assert_eq!(
        result,
        ThemeInstallResult {
            schema_version: 1,
            status: "installed".into(),
            packs: vec![ThemePackSummary {
                slug: "aura".into(),
                name: "Aura".into(),
                wallpapers: 7,
                replaced: false,
            }],
        }
    );
    // The canonical URL, without `.git` or a trailing slash, and no `--all`.
    assert_eq!(
        running.calls(),
        [
            "theme-install",
            "https://github.com/owner/omarchy-aura-theme",
            "--json",
        ]
    );
}

#[test]
fn a_collection_is_named_all_and_uninstalling_names_one_slug() {
    let running = Running::start(&format!(
        "if [ \"$1\" = theme-install ]; then printf '%s' '{{\"schema\":1,\"status\":\"installed\",\"packs\":[{{\"slug\":\"aura\",\"name\":\"Aura\",\"wallpapers\":1}},{{\"slug\":\"jade\",\"name\":\"Jade\",\"wallpapers\":0,\"replaced\":true}}]}}'; else {REMOVED}; fi"
    ));
    let mut client = running.client();
    let installed = client
        .request::<ThemeInstallResult>(
            "themes.install",
            install_params("https://github.com/owner/themes", true, true),
        )
        .unwrap();
    assert_eq!(
        installed
            .packs
            .iter()
            .map(|pack| pack.slug.as_str())
            .collect::<Vec<_>>(),
        ["aura", "jade"]
    );
    assert!(installed.packs[1].replaced);

    let removed = client
        .request::<ThemeUninstallResult>(
            "themes.uninstall",
            ThemeUninstallParams {
                slug: "aura".into(),
            },
        )
        .unwrap();
    assert_eq!(
        removed,
        ThemeUninstallResult {
            schema_version: 1,
            status: "removed".into(),
            slug: "aura".into(),
        }
    );
    assert_eq!(
        running.calls(),
        [
            "theme-install",
            "https://github.com/owner/themes",
            "--all",
            "--force",
            "--json",
            "theme-uninstall",
            "aura",
            "--json",
        ]
    );
}

#[test]
fn an_invalid_url_or_slug_fails_before_the_helper_and_leaks_nothing() {
    let running = Running::start(ONE_PACK);
    let mut client = running.client();
    for url in [
        "",
        "http://github.com/owner/theme",
        "git@github.com:owner/theme.git",
        "file:///tmp/themes",
        "https://example.com/owner/theme",
        "https://github.com/owner/theme/tree/main",
    ] {
        let error = client
            .request::<ThemeInstallResult>("themes.install", install_params(url, false, false))
            .unwrap_err();
        assert_eq!(code(&error), "THEME_INSTALL_FAILED", "{url}");
        let text = message(&error);
        assert_eq!(text, "The theme information is invalid.");
        assert!(!text.contains(&running.dir.path().display().to_string()));
    }
    for slug in ["../aura", "Aura", "aura/child", ""] {
        let error = client
            .request::<ThemeUninstallResult>(
                "themes.uninstall",
                ThemeUninstallParams { slug: slug.into() },
            )
            .unwrap_err();
        assert_eq!(code(&error), "THEME_UNINSTALL_FAILED", "{slug}");
    }
    assert!(running.calls().is_empty());
}

#[test]
fn a_helper_that_fails_or_lies_is_a_fixed_failure_message() {
    let cases = [
        ("exit 3", "THEME_INSTALL_FAILED"),
        ("printf '%s' 'not json'", "THEME_INSTALL_FAILED"),
        (
            "printf '%s' '{\"schema\":1,\"status\":\"installed\",\"packs\":[]}'",
            "THEME_INSTALL_FAILED",
        ),
        (
            // Two packs without `--all` is a reply the service did not ask for.
            "printf '%s' '{\"schema\":1,\"status\":\"installed\",\"packs\":[{\"slug\":\"a\",\"name\":\"A\",\"wallpapers\":0},{\"slug\":\"b\",\"name\":\"B\",\"wallpapers\":0}]}'",
            "THEME_INSTALL_FAILED",
        ),
        (
            "printf '%s' '{\"schema\":1,\"status\":\"removed\",\"slug\":\"other\"}'",
            "THEME_INSTALL_FAILED",
        ),
    ];
    for (body, expected) in cases {
        let running = Running::start(body);
        let mut client = running.client();
        let error = client
            .request::<ThemeInstallResult>(
                "themes.install",
                install_params("https://github.com/owner/theme", false, false),
            )
            .unwrap_err();
        assert_eq!(code(&error), expected, "{body}");
        assert!(
            message(&error).starts_with("The theme helper failed or did not finish"),
            "{body}: {error}"
        );
        assert!(!message(&error).contains(&running.dir.path().display().to_string()));
    }
}

#[test]
fn a_removal_reply_must_name_the_pack_that_was_asked_for() {
    let running =
        Running::start("printf '%s' '{\"schema\":1,\"status\":\"removed\",\"slug\":\"jade\"}'");
    let mut client = running.client();
    let error = client
        .request::<ThemeUninstallResult>(
            "themes.uninstall",
            ThemeUninstallParams {
                slug: "aura".into(),
            },
        )
        .unwrap_err();
    assert_eq!(code(&error), "THEME_UNINSTALL_FAILED");
    assert_eq!(
        message(&error),
        "The theme helper failed or did not finish. Check the theme catalog before retrying."
    );
}

#[test]
fn an_operation_id_is_not_required_and_repeating_it_runs_again() {
    // A theme install is not an action: it has no operation table, so the same
    // request twice runs twice. That is why the producer, not this table, is
    // what refuses a replacement it was not asked to make.
    let running = Running::start(ONE_PACK);
    let mut client = running.client();
    for _ in 0..2 {
        client
            .request::<ThemeInstallResult>(
                "themes.install",
                install_params("https://github.com/owner/theme", false, false),
            )
            .unwrap();
    }
    assert_eq!(running.calls().len(), 6);
}
