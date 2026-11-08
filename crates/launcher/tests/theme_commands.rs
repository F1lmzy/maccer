//! The Themes scope's own commands: install a pack from a repository, remove one.
//!
//! A command belongs to the page, not to a row: it is published as a scope action
//! with its own reference, so a strip of palettes stays a strip of palettes and
//! the commands appear in the frontend's action panel. What makes them the
//! provider's business is that it publishes the field each one needs — a
//! repository URL, or one of the installed packs — and is the only thing that
//! validates it and runs the helper.
use launcher_agent::client::{Client, RemoteError};
use launcher_agent::core::{HistoryStore, ProviderConfig};
use launcher_agent::protocol::{
    ACTION_INPUT_SELECT, ACTION_INPUT_TEXT, InvokeActionParams, Route, ScopeAction, ThemePalette,
    ViewSnapshot,
};
use launcher_agent::service::{
    Server, Service, ServiceProvider, ThemeApply, ThemeCatalog, ThemeInstall, ThemeServiceProvider,
    ThemerState,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

fn palettes() -> Vec<ThemePalette> {
    let mut dark = ThemerState::fixture().snapshot().palette.unwrap();
    dark.slug = "night-blue".into();
    dark.name = "Night Blue".into();
    let mut light = dark.clone();
    light.slug = "day-white".into();
    light.name = "Day White".into();
    light.variant = "light".into();
    vec![dark, light]
}

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    /// A service whose Themes scope can apply, install and remove. The helper is
    /// a script that records its arguments and prints `body`.
    fn start(body: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-cmd-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let helper = dir.path().join("mac-themer");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${{0%/*}}/calls\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = ThemeCatalog::from_palettes(palettes()).unwrap();
        Self::serve(
            dir,
            Arc::new(
                ThemeServiceProvider::with_apply(catalog.clone(), ThemeApply::new(helper.clone()))
                    .with_install(ThemeInstall::new(helper)),
            ),
        )
    }

    /// A read-only service: it must advertise neither command.
    fn read_only() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-cmd-ro-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self::serve(
            dir,
            Arc::new(ThemeServiceProvider::new(
                ThemeCatalog::from_palettes(palettes()).unwrap(),
            )),
        )
    }

    fn serve(dir: tempfile::TempDir, provider: Arc<dyn ServiceProvider>) -> Self {
        let service = Service::with_state(
            vec![(
                provider,
                ProviderConfig {
                    enabled: true,
                    max_results: 50,
                    ..Default::default()
                },
            )],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            ThemerState::fixture(),
        )
        .unwrap();
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

fn scope(client: &mut Client) -> ViewSnapshot {
    client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "",
            20,
        )
        .unwrap()
}

fn rows(view: &ViewSnapshot) -> Vec<&str> {
    view.sections
        .iter()
        .flat_map(|section| &section.items)
        .map(|item| item.id.as_str())
        .collect()
}

fn command<'a>(view: &'a ViewSnapshot, id: &str) -> &'a ScopeAction {
    view.scope_actions
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("no {id} command in the themes scope"))
}

fn invoke(
    view: &ViewSnapshot,
    id: &str,
    operation: &str,
    confirmed: bool,
    input: &[(&str, &str)],
) -> InvokeActionParams {
    InvokeActionParams {
        view_id: view.view_id.clone(),
        revision: view.revision.clone(),
        action_ref: command(view, id).action_ref.clone(),
        operation_id: operation.into(),
        confirmed,
        input: input
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

fn code(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<RemoteError>()
        .map(|error| error.0.data.code.clone())
        .unwrap_or_default()
}

#[test]
fn the_commands_belong_to_the_scope_not_to_its_rows() {
    let running = Running::start("exit 0");
    let mut client = running.client();
    let view = scope(&mut client);

    // The strip is palettes only: no command is ever a row, in a browsed scope
    // or in a query that names one.
    assert_eq!(rows(&view), ["themes:day-white", "themes:night-blue"]);
    for query in ["", "uninstall", "install theme from github"] {
        let searched = client
            .open_view(
                Route::Provider {
                    provider_id: "themes".into(),
                },
                query,
                20,
            )
            .unwrap();
        assert!(
            rows(&searched).iter().all(|id| id.starts_with("themes:")
                && !matches!(*id, "themes:install" | "themes:uninstall")),
            "query {query:?} surfaced a command as a row: {:?}",
            rows(&searched)
        );
    }

    // The page publishes them instead, in the order it offers them, and each is
    // authorized by its own reference.
    let ids: Vec<&str> = view
        .scope_actions
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(ids, ["themes:install", "themes:uninstall"]);
    assert!(
        view.scope_actions
            .iter()
            .all(|entry| !entry.action_ref.is_empty())
    );
    assert!(
        view.scope_actions
            .iter()
            .all(|entry| entry.provider_id == "themes")
    );

    let install = command(&view, "themes:install");
    assert_eq!(install.title, "Install a theme from GitHub…");
    assert_eq!(install.action_id, "install");
    assert_eq!(install.confirmation, None);
    let field = install.input.as_ref().unwrap();
    assert_eq!(field.id, "url");
    assert_eq!(field.kind, ACTION_INPUT_TEXT);
    assert!(field.placeholder.as_deref().unwrap().contains("github.com"));
    assert!(field.options.is_empty());

    let uninstall = command(&view, "themes:uninstall");
    assert_eq!(uninstall.title, "Uninstall a theme…");
    assert_eq!(uninstall.action_id, "uninstall");
    // Removing a pack is destructive, so it carries the provider's prompt.
    assert!(
        uninstall
            .confirmation
            .as_deref()
            .unwrap()
            .starts_with("Are you sure you want to uninstall this theme?")
    );
    let field = uninstall.input.as_ref().unwrap();
    assert_eq!(field.id, "slug");
    assert_eq!(field.kind, ACTION_INPUT_SELECT);
    assert_eq!(
        field
            .options
            .iter()
            .map(|option| (option.value.as_str(), option.label.as_str()))
            .collect::<Vec<_>>(),
        [
            ("day-white", "Day White (day-white)"),
            ("night-blue", "Night Blue (night-blue)")
        ]
    );
    assert!(running.calls().is_empty());
}

#[test]
fn a_read_only_scope_advertises_neither_command() {
    let running = Running::read_only();
    let mut client = running.client();
    let view = scope(&mut client);
    assert_eq!(rows(&view), ["themes:day-white", "themes:night-blue"]);
    assert!(view.scope_actions.is_empty());
}

#[test]
fn installing_needs_the_url_the_provider_asked_for_and_runs_the_helper() {
    let running = Running::start(
        "printf '%s' '{\"schema\":1,\"status\":\"installed\",\"packs\":[{\"slug\":\"aura\",\"name\":\"Aura\",\"wallpapers\":2}]}'",
    );
    let mut client = running.client();
    let view = scope(&mut client);

    // Without the published value the provider refuses rather than falling back
    // to anything of its own.
    let missing = invoke(&view, "themes:install", "op-1", false, &[]);
    let error = client.invoke(&missing).unwrap_err();
    assert_eq!(code(&error), "ACTION_FAILED");
    assert!(error.to_string().contains("Enter a GitHub repository URL"));
    assert!(running.calls().is_empty());

    // A non-GitHub value is refused before the helper could see it.
    for (index, url) in ["file:///tmp/theme", "https://example.com/a/b", "not a url"]
        .into_iter()
        .enumerate()
    {
        let bad = invoke(
            &view,
            "themes:install",
            &format!("op-bad-{index}"),
            false,
            &[("url", url)],
        );
        let error = client.invoke(&bad).unwrap_err();
        assert_eq!(code(&error), "ACTION_FAILED", "{url}");
        assert!(running.calls().is_empty());
    }
    // The collected value is part of the operation, so the same id with a
    // different URL is a different request rather than a cached replay.
    let first = invoke(
        &view,
        "themes:install",
        "op-repeat",
        false,
        &[("url", "https://example.com/one")],
    );
    let second = invoke(
        &view,
        "themes:install",
        "op-repeat",
        false,
        &[("url", "https://example.com/two")],
    );
    assert_eq!(code(&client.invoke(&first).unwrap_err()), "ACTION_FAILED");
    assert_eq!(
        code(&client.invoke(&second).unwrap_err()),
        "OPERATION_ID_REUSE"
    );
    assert!(running.calls().is_empty());

    let ok = invoke(
        &view,
        "themes:install",
        "op-3",
        false,
        &[("url", "https://github.com/owner/aura")],
    );
    let result = client.invoke(&ok).unwrap();
    // The catalog was reloaded by the executor, so the caller is told to ask
    // for the collection again instead of being handed a stale strip.
    assert!(matches!(
        result.presentation,
        Some(launcher_agent::protocol::Presentation::SetQuery { .. })
    ));
    assert_eq!(
        running.calls(),
        ["theme-install", "https://github.com/owner/aura", "--json"]
    );
}

#[test]
fn uninstalling_needs_consent_and_the_chosen_slug() {
    let running = Running::start(
        "printf '%s' '{\"schema\":1,\"status\":\"removed\",\"slug\":\"day-white\"}'",
    );
    let mut client = running.client();
    let view = scope(&mut client);

    // Consent first: an unconfirmed invoke never reaches the helper.
    let unconfirmed = invoke(
        &view,
        "themes:uninstall",
        "op-1",
        false,
        &[("slug", "day-white")],
    );
    let error = client.invoke(&unconfirmed).unwrap_err();
    assert_eq!(code(&error), "CONFIRMATION_REQUIRED");
    assert!(running.calls().is_empty());

    // A value the publisher never offered is refused by the helper's own
    // validation, and no shell text can ride along with it.
    let forged = invoke(
        &view,
        "themes:uninstall",
        "op-2",
        true,
        &[("slug", "../night-blue")],
    );
    assert_eq!(code(&client.invoke(&forged).unwrap_err()), "ACTION_FAILED");
    assert!(running.calls().is_empty());

    let confirmed = invoke(
        &view,
        "themes:uninstall",
        "op-3",
        true,
        &[("slug", "day-white")],
    );
    let result = client.invoke(&confirmed).unwrap();
    assert!(matches!(
        result.presentation,
        Some(launcher_agent::protocol::Presentation::SetQuery { .. })
    ));
    assert_eq!(running.calls(), ["theme-uninstall", "day-white", "--json"]);
}

#[test]
fn the_scope_commands_are_pinned_to_the_snapshot_that_published_them() {
    let running = Running::start("exit 0");
    let mut client = running.client();
    let older = scope(&mut client);
    // A second view of the same scope mints its own references, so the first
    // view's can no longer be replayed against newer state.
    let _newer = scope(&mut client);
    let stale = invoke(
        &older,
        "themes:uninstall",
        "op-stale",
        true,
        &[("slug", "day-white")],
    );
    let error = client.invoke(&stale).unwrap_err();
    assert!(matches!(
        code(&error).as_str(),
        "STALE_ACTION" | "INVALID_ACTION_REF" | "ACTION_NOT_FOUND"
    ));
    assert!(running.calls().is_empty());
}

#[test]
fn the_service_bounds_what_a_frontend_can_send_back() {
    let running = Running::start("exit 0");
    let mut client = running.client();
    let view = scope(&mut client);
    for (input, expected) in [
        (
            vec![("URL", "https://github.com/a/b")],
            "INVALID_ACTION_INPUT",
        ),
        (vec![("url", "")], "INVALID_ACTION_INPUT"),
        (
            vec![
                ("url", "https://github.com/a/b"),
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
            ],
            "INVALID_ACTION_INPUT",
        ),
    ] {
        let request = invoke(&view, "themes:install", "op-x", false, &input);
        let error = client.invoke(&request).unwrap_err();
        assert_eq!(code(&error), expected, "{input:?}");
    }
    let oversized = "x".repeat(513);
    let request = invoke(
        &view,
        "themes:install",
        "op-y",
        false,
        &[("url", &oversized)],
    );
    let error = client.invoke(&request).unwrap_err();
    assert_eq!(code(&error), "INVALID_ACTION_INPUT");
    assert!(running.calls().is_empty());
}
