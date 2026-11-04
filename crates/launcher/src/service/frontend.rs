//! Backend-owned frontend selection, presentation leases, and launch deduplication.
use crate::protocol::{FrontendKind, FrontendPollParams, FrontendPollResult, RpcError};
use anyhow::{Context, Result, bail};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LEASE: Duration = Duration::from_secs(3);
const STARTUP_GRACE: Duration = Duration::from_secs(10);

struct Owner {
    session: String,
    expires: Instant,
    offered: Option<String>,
}
struct Launch {
    started: Instant,
    child: Option<Child>,
}
struct State {
    owner: Option<Owner>,
    pending: Option<String>,
    next_serial: u64,
    launch: Option<Launch>,
}

/// The configured frontend is the only one allowed to lease presentation control.
pub struct FrontendRouter {
    kind: FrontendKind,
    path: PathBuf,
    socket: PathBuf,
    state: Mutex<State>,
}

impl FrontendRouter {
    pub fn new(kind: FrontendKind, path: PathBuf, socket: PathBuf) -> Result<Arc<Self>> {
        validate_target(kind, &path)?;
        if !socket.is_absolute()
            || socket
                .to_str()
                .is_none_or(|s| s.is_empty() || s.contains('\0'))
        {
            bail!("frontend service socket must be an absolute UTF-8 path");
        }
        Ok(Arc::new(Self {
            kind,
            path,
            socket,
            state: Mutex::new(State {
                owner: None,
                pending: None,
                next_serial: 0,
                launch: None,
            }),
        }))
    }

    /// Queue a show request, or launch the configured frontend if no client owns the lease.
    pub fn present(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        self.reap_or_expire(&mut state, now);
        state.next_serial = state.next_serial.wrapping_add(1).max(1);
        // Coalesce repeated presses until a registered frontend acknowledges delivery.
        if state.pending.is_none() {
            state.pending = Some(format!("show-{:016x}", state.next_serial));
        }
        if state.owner.is_none() {
            self.launch_if_needed(&mut state, now)?;
        }
        Ok(())
    }

    pub fn poll(
        &self,
        session: &str,
        params: FrontendPollParams,
    ) -> Result<FrontendPollResult, RpcError> {
        if session.is_empty() || session.len() > 128 || session.contains('\0') {
            return Err(RpcError::new(
                -32602,
                "INVALID_SESSION",
                "invalid frontend session",
            ));
        }
        if params
            .ack
            .as_deref()
            .is_some_and(|ack| ack.is_empty() || ack.len() > 128 || ack.contains('\0'))
        {
            return Err(RpcError::new(
                -32602,
                "INVALID_ACK",
                "invalid presentation acknowledgment",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        self.reap_or_expire(&mut state, now);
        if params.kind != self.kind {
            return Ok(FrontendPollResult {
                registered: false,
                show: None,
            });
        }
        match state.owner.as_mut() {
            Some(owner) if owner.session == session => owner.expires = now + LEASE,
            Some(_) => {
                return Ok(FrontendPollResult {
                    registered: false,
                    show: None,
                });
            }
            None => {
                state.owner = Some(Owner {
                    session: session.to_owned(),
                    expires: now + LEASE,
                    offered: None,
                })
            }
        }
        if let Some(ack) = params.ack.as_deref()
            && state.pending.as_deref() == Some(ack)
            && state
                .owner
                .as_ref()
                .is_some_and(|owner| owner.offered.as_deref() == Some(ack))
        {
            state.pending = None;
        }
        let show = state.pending.clone();
        state.owner.as_mut().expect("registered owner").offered = show.clone();
        Ok(FrontendPollResult {
            registered: true,
            show,
        })
    }

    pub fn release(&self, session: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.session == session)
        {
            state.owner = None;
        }
    }

    /// Reap exited launch helpers even when no frontend ever connects.
    pub fn maintain(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reap_or_expire(&mut state, Instant::now());
    }

    fn reap_or_expire(&self, state: &mut State, now: Instant) {
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| now >= owner.expires)
        {
            state.owner = None;
        }
        if let Some(launch) = state.launch.as_mut()
            && let Some(child) = launch.child.as_mut()
            && child.try_wait().ok().flatten().is_some()
        {
            launch.child = None;
        }
        if state.launch.as_ref().is_some_and(|launch| {
            launch.child.is_none() && now.duration_since(launch.started) >= STARTUP_GRACE
        }) {
            state.launch = None;
        }
    }

    fn launch_if_needed(&self, state: &mut State, now: Instant) -> Result<()> {
        if state.launch.as_ref().is_some_and(|launch| {
            launch.child.is_some() || now.duration_since(launch.started) < STARTUP_GRACE
        }) {
            return Ok(());
        }
        let mut command = match self.kind {
            FrontendKind::Gpui => {
                // A live process can lose its control lease under suspension or
                // transport failure. Never create a second resident GPUI instance.
                let presence = crate::macos::FrontendInstance::claim(&self.socket)?;
                if presence.is_none() {
                    bail!("the GPUI frontend is running but not responding; restart that frontend");
                }
                drop(presence);
                let mut command = Command::new(&self.path);
                command.args([
                    "--service-socket",
                    self.socket.to_str().expect("validated UTF-8 socket"),
                    "--show",
                ]);
                command
            }
            FrontendKind::Pill => {
                let mut command = Command::new("/usr/bin/open");
                command
                    .args([
                        "-a",
                        self.path.to_str().expect("validated UTF-8 app path"),
                        "--env",
                    ])
                    .arg(format!("MACCER_SERVICE_SOCKET={}", self.socket.display()))
                    .args(["--args", "--show-launcher"]);
                command
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let child = command.spawn().context("launching configured frontend")?;
        state.launch = Some(Launch {
            started: now,
            child: Some(child),
        });
        Ok(())
    }
}

fn validate_target(kind: FrontendKind, path: &PathBuf) -> Result<()> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.is_empty() || s.contains('\0') || s.len() > 4096)
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        bail!("configured frontend path must be absolute, UTF-8, and contain no '..'");
    }
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading configured frontend {}", path.display()))?;
    match kind {
        FrontendKind::Gpui => {
            if !metadata.is_file() {
                bail!("GPUI frontend path must name an executable file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    bail!("GPUI frontend file is not executable");
                }
            }
        }
        FrontendKind::Pill
            if !metadata.is_dir() || path.extension().is_none_or(|ext| ext != "app") =>
        {
            bail!("Pill frontend path must name an existing .app directory");
        }
        FrontendKind::Pill => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn target() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("frontend");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }

    #[test]
    fn selected_kind_exclusive_lease_ack_and_release() {
        let (dir, path) = target();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, dir.path().join("rpc")).unwrap();
        router.present().unwrap();
        let wrong = router
            .poll(
                "pill-1",
                FrontendPollParams {
                    kind: FrontendKind::Pill,
                    ack: None,
                },
            )
            .unwrap();
        assert!(!wrong.registered);
        let first = router
            .poll(
                "gpui-1",
                FrontendPollParams {
                    kind: FrontendKind::Gpui,
                    ack: None,
                },
            )
            .unwrap();
        assert!(first.registered);
        let show = first.show.unwrap();
        assert!(
            !router
                .poll(
                    "gpui-2",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: None
                    }
                )
                .unwrap()
                .registered
        );
        assert_eq!(
            router
                .poll(
                    "gpui-1",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: Some("not-issued".into())
                    }
                )
                .unwrap()
                .show
                .as_deref(),
            Some(show.as_str())
        );
        assert!(
            router
                .poll(
                    "gpui-1",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: Some(show)
                    }
                )
                .unwrap()
                .show
                .is_none()
        );
        router.release("gpui-1");
        assert!(
            router
                .poll(
                    "gpui-2",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: None
                    }
                )
                .unwrap()
                .registered
        );
    }

    #[test]
    fn launch_is_deduplicated_during_startup_grace() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("frontend");
        let marker = dir.path().join("launch-count");
        fs::write(
            &path,
            format!("#!/bin/sh\nprintf x >> '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, dir.path().join("rpc")).unwrap();
        router.present().unwrap();
        router.present().unwrap();
        // Launch returns before the child runs. Allow the same startup grace
        // as production instead of asserting a two-second scheduler deadline.
        let deadline = Instant::now() + STARTUP_GRACE;
        while Instant::now() < deadline && fs::read_to_string(&marker).ok().as_deref() != Some("x")
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(marker).ok().as_deref(),
            Some("x"),
            "the owned helper must launch exactly once within the startup grace"
        );
    }

    #[test]
    fn launch_failure_is_reported_and_does_not_claim_owner() {
        let (dir, path) = target();
        let router =
            FrontendRouter::new(FrontendKind::Gpui, path.clone(), dir.path().join("rpc")).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(router.present().is_err());
        assert!(
            router
                .poll(
                    "gpui-1",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: None
                    }
                )
                .unwrap()
                .registered
        );
    }

    #[test]
    fn expired_owner_cannot_steal_a_new_lease_or_ack_an_unseen_show() {
        let (dir, path) = target();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, dir.path().join("rpc")).unwrap();
        let params = |ack| FrontendPollParams {
            kind: FrontendKind::Gpui,
            ack,
        };
        assert!(router.poll("old", params(None)).unwrap().registered);
        router.present().unwrap();
        let serial = router.poll("old", params(None)).unwrap().show.unwrap();
        {
            let mut state = router.state.lock().unwrap();
            state.owner.as_mut().unwrap().expires = Instant::now() - Duration::from_millis(1);
        }
        // Even a valid old serial cannot clear a request not offered to the new owner.
        let reply = router.poll("new", params(Some(serial.clone()))).unwrap();
        assert!(reply.registered);
        assert_eq!(reply.show.as_deref(), Some(serial.as_str()));
        assert!(
            !router
                .poll("old", params(Some(serial.clone())))
                .unwrap()
                .registered
        );
        router.release("old");
        assert!(
            router
                .poll("new", params(Some(serial)))
                .unwrap()
                .show
                .is_none()
        );
    }

    #[test]
    fn living_gpui_without_a_lease_is_not_launched_twice() {
        let (dir, path) = target();
        let socket = dir.path().join("rpc");
        let _presence = crate::macos::FrontendInstance::claim(&socket)
            .unwrap()
            .unwrap();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, socket).unwrap();
        let error = router.present().unwrap_err().to_string();
        assert!(error.contains("running but not responding"), "{error}");
        assert!(router.state.lock().unwrap().launch.is_none());
    }

    #[test]
    fn maintenance_reaps_an_exited_launch_without_a_frontend() {
        let (dir, path) = target();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, dir.path().join("rpc")).unwrap();
        router.present().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            router.maintain();
            if router
                .state
                .lock()
                .unwrap()
                .launch
                .as_ref()
                .unwrap()
                .child
                .is_none()
            {
                break;
            }
            assert!(Instant::now() < deadline, "launch child was not reaped");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn repeated_presents_coalesce_until_ack() {
        let (dir, path) = target();
        let router = FrontendRouter::new(FrontendKind::Gpui, path, dir.path().join("rpc")).unwrap();
        router
            .poll(
                "gpui-1",
                FrontendPollParams {
                    kind: FrontendKind::Gpui,
                    ack: None,
                },
            )
            .unwrap();
        router.present().unwrap();
        let show = router
            .poll(
                "gpui-1",
                FrontendPollParams {
                    kind: FrontendKind::Gpui,
                    ack: None,
                },
            )
            .unwrap()
            .show
            .unwrap();
        router.present().unwrap();
        assert_eq!(
            router
                .poll(
                    "gpui-1",
                    FrontendPollParams {
                        kind: FrontendKind::Gpui,
                        ack: None
                    }
                )
                .unwrap()
                .show
                .as_deref(),
            Some(show.as_str())
        );
    }
}
