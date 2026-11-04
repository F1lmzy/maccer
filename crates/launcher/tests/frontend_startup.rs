use launcher_agent::agent::frontend_startup::{
    claim, maccer_agent_claim_frontend, maccer_agent_frontend_claim_free,
};
use launcher_agent::protocol::FrontendKind;
use launcher_agent::service::{FrontendRouter, Server, Service};
use std::{
    ffi::{CStr, CString},
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}
impl Running {
    fn new(pill_selected: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-admission-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let service = Service::fixture().unwrap();
        if pill_selected {
            let target = dir.path().join("Owned.app");
            std::fs::create_dir(&target).unwrap();
            service
                .attach_frontend(
                    FrontendRouter::new(FrontendKind::Pill, target, dir.path().join("rpc"))
                        .unwrap(),
                )
                .unwrap();
        }
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = Some(std::thread::spawn(move || server.serve_until(&worker_stop)));
        Self { dir, stop, worker }
    }
    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("rpc")
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}
#[test]
fn selected_pill_rejects_gpui_even_before_any_frontend_is_running() {
    let running = Running::new(true);
    let error = claim(&running.socket(), "gpui")
        .err()
        .expect("unselected GPUI must be refused");
    assert!(error.to_string().contains("selected frontend"));
    let pill = claim(&running.socket(), "pill").unwrap();
    assert!(claim(&running.socket(), "gpui").is_err());
    assert!(claim(&running.socket(), "pill").is_err());
    // Read-only clients do not claim the native frontend slot.
    assert_eq!(
        launcher_agent::client::Client::connect(running.socket())
            .unwrap()
            .providers()
            .unwrap()
            .len(),
        8
    );
    drop(pill);
    assert!(claim(&running.socket(), "gpui").is_err());
}
#[test]
fn shared_lock_excludes_both_kinds_without_a_configured_router() {
    let running = Running::new(false);
    let first = claim(&running.socket(), "gpui").unwrap();
    assert!(claim(&running.socket(), "pill").is_err());
    drop(first);
    let second = claim(&running.socket(), "pill").unwrap();
    assert!(claim(&running.socket(), "gpui").is_err());
    drop(second);
    assert!(claim(&running.socket(), "gpui").is_ok());
}
#[test]
fn c_abi_claim_owns_slot_and_returns_owned_errors() {
    let running = Running::new(false);
    let path = CString::new(running.socket().to_str().unwrap()).unwrap();
    let kind = CString::new("pill").unwrap();
    let mut error = std::ptr::null_mut();
    unsafe {
        let guard = maccer_agent_claim_frontend(path.as_ptr(), kind.as_ptr(), &mut error);
        assert!(!guard.is_null());
        assert!(error.is_null());
        let denied = maccer_agent_claim_frontend(path.as_ptr(), kind.as_ptr(), &mut error);
        assert!(denied.is_null());
        assert!(!error.is_null());
        assert!(
            CStr::from_ptr(error)
                .to_str()
                .unwrap()
                .contains("already running")
        );
        launcher_agent::agent::maccer_agent_string_free(error);
        maccer_agent_frontend_claim_free(guard);
        maccer_agent_frontend_claim_free(std::ptr::null_mut());
    }
    assert!(claim(&running.socket(), "gpui").is_ok());
    assert!(claim(&running.socket(), "unknown").is_err());
}
