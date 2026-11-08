//! Guarded theme-pack installation and removal for the shared service.
//!
//! mac-themer's `theme-install URL [--all] [--force] --json` fetches a theme
//! repository, keeps its palette and wallpapers and installs the result as a
//! pack; `theme-uninstall SLUG --json` removes one again. This module runs
//! those two fixed commands through the bounded subprocess runner, one at a
//! time, and projects the fixed JSON replies into path-free results. Helper
//! stderr, filesystem paths and the producer's own error text never reach the
//! caller.
//!
//! The URL and the slug are the only inputs. The URL is the one third-party
//! string in this protocol that becomes a process argument, so it is validated
//! here as well as by the producer: a whole `https://github.com/<owner>/<repo>`
//! repository, no other host, scheme, port, credentials, query or path depth.
//! The producer's rules are the authority; this check exists so a malformed URL
//! never reaches a helper at all.

use crate::core::CancellationToken;
use crate::macos::process::run_bounded;
use crate::protocol::{
    MAX_REPO_URL_BYTES, ThemeInstallResult, ThemePackSummary, ThemeUninstallResult, is_theme_slug,
};
use crate::service::ThemeCatalog;
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    ffi::OsString,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Hard deadline for one install. Fetching a repository is the slowest thing
/// this service asks a helper to do, and it is still bounded: a clone that has
/// not finished is a clone that is not going to.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Removal only renames and deletes a directory, so it needs far less.
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Reject a response whose stdout exceeds this many bytes. The runner captures
/// `+1` so an over-limit payload is observable rather than silently truncated.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Homebrew Python must precede Apple's older system Python. Other environment
/// variables, including mac-themer overrides, are inherited unchanged.
const HELPER_PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:";
/// Most packs one collection may install in one call.
const MAX_PACKS: usize = 128;
const BUSY_MESSAGE: &str =
    "Another theme change is already in progress. Wait for it to finish, then try again.";
const INSTALL_FAILED_MESSAGE: &str = "The theme helper failed or did not finish. A fetched theme \
may be partly installed; check the theme catalog before retrying.";
const UNINSTALL_FAILED_MESSAGE: &str = "The theme helper failed or did not finish. Check the \
theme catalog before retrying.";
const INVALID_MESSAGE: &str = "The theme information is invalid.";

/// Shared executor for theme installation and removal. Cheap to clone behind an
/// `Arc`; only one mutation may run at a time across every client.
///
/// This lock is separate from the Apply executor's: installing replaces a pack
/// directory in one rename, so an Apply either reads the pack it validated or
/// fails its own palette recheck. It is not a filesystem lock against another
/// process editing the same collection.
pub struct ThemeInstall {
    helper: PathBuf,
    /// The catalog a mutation publishes into. When present it is reloaded
    /// before the reply, so a client can search the pack it just installed
    /// instead of waiting for the next periodic refresh.
    catalog: Option<Arc<ThemeCatalog>>,
    install_timeout: Duration,
    uninstall_timeout: Duration,
    busy: AtomicBool,
}

impl ThemeInstall {
    /// Create an executor that runs the helper at `helper` (an executable path).
    pub fn new(helper: PathBuf) -> Arc<Self> {
        Self::with_timeouts(helper, None, INSTALL_TIMEOUT, UNINSTALL_TIMEOUT)
    }

    /// Create an executor that reloads `catalog` after every mutation.
    pub fn with_catalog(helper: PathBuf, catalog: Arc<ThemeCatalog>) -> Arc<Self> {
        Self::with_timeouts(helper, Some(catalog), INSTALL_TIMEOUT, UNINSTALL_TIMEOUT)
    }

    fn with_timeouts(
        helper: PathBuf,
        catalog: Option<Arc<ThemeCatalog>>,
        install_timeout: Duration,
        uninstall_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            helper,
            catalog,
            install_timeout,
            uninstall_timeout,
            busy: AtomicBool::new(false),
        })
    }

    /// Whether a mutation is currently running. A second call is rejected with
    /// a fixed message instead of queuing.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Install the theme at `url`. `all` accepts a repository that states
    /// several themes; `force` replaces a pack the producer did not install.
    ///
    /// Returns the packs that are now on disk. A concurrent attempt, an
    /// unusable URL, or any helper failure is an error: a failed install may
    /// have fetched a repository but never publishes a half-made pack, and the
    /// caller is told so rather than left to guess.
    pub fn install(&self, url: &str, all: bool, force: bool) -> Result<ThemeInstallResult> {
        let url = valid_repo_url(url).ok_or_else(|| anyhow::anyhow!(INVALID_MESSAGE))?;
        let _busy = self.enter()?;
        let mut command = Command::new(&self.helper);
        command.arg("theme-install").arg(&url);
        if all {
            command.arg("--all");
        }
        if force {
            command.arg("--force");
        }
        command.arg("--json");
        let stdout = self.run(command, self.install_timeout, INSTALL_FAILED_MESSAGE)?;
        let result = installed(&stdout, all)?;
        self.reload();
        Ok(result)
    }

    /// Remove one installed pack by the exact slug a catalog entry published.
    pub fn uninstall(&self, slug: &str) -> Result<ThemeUninstallResult> {
        if !is_theme_slug(slug) {
            bail!(INVALID_MESSAGE);
        }
        let _busy = self.enter()?;
        let mut command = Command::new(&self.helper);
        command.arg("theme-uninstall").arg(slug).arg("--json");
        let stdout = self.run(command, self.uninstall_timeout, UNINSTALL_FAILED_MESSAGE)?;
        let result = removed(&stdout, slug)?;
        self.reload();
        Ok(result)
    }

    /// Republish the collection after a mutation that changed it.
    fn reload(&self) {
        if let Some(catalog) = &self.catalog {
            catalog.refresh();
        }
    }

    /// Take the single-flight lock, or fail with the fixed busy message.
    fn enter(&self) -> Result<BusyGuard<'_>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            bail!(BUSY_MESSAGE);
        }
        Ok(BusyGuard(&self.busy))
    }

    /// Run one fixed command to completion, bounded in time and output.
    ///
    /// `failure` is the message for this command, so a failed install warns
    /// about a possibly half-installed pack and a failed removal does not.
    fn run(
        &self,
        mut command: Command,
        timeout: Duration,
        failure: &'static str,
    ) -> Result<Vec<u8>> {
        command.env("PATH", helper_path());
        // An accepted mutation must survive the query that produced it, so the
        // run is not tied to any session cancellation token.
        let output = run_bounded(
            &mut command,
            &CancellationToken::default(),
            timeout,
            MAX_OUTPUT_BYTES + 1,
        )
        .map_err(|_| anyhow::anyhow!(failure))?;
        if !output.status.success() || output.stdout.len() > MAX_OUTPUT_BYTES {
            bail!(failure);
        }
        Ok(output.stdout)
    }
}

/// Reset the single-flight flag when the in-flight mutation leaves scope.
struct BusyGuard<'a>(&'a AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// `https://github.com/<owner>/<repo>` and nothing else, normalized without a
/// trailing `.git` or slash. Anything that could name a transport helper, a
/// local path, a credentials prompt or a second repository fails here.
fn valid_repo_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_REPO_URL_BYTES {
        return None;
    }
    if url
        .chars()
        .any(|character| character.is_control() || matches!(character, '%' | '\\' | '"' | '\''))
    {
        return None;
    }
    let rest = url.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !segment(owner) || !segment(repo) {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

/// One GitHub owner or repository name: bounded, and only the characters a
/// repository name uses. Narrower than GitHub on purpose.
fn segment(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 100
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The fixed producer reply to `theme-install --json`. Unknown top-level fields
/// are ignored rather than forwarded, so a future producer cannot leak a path
/// through this response.
#[derive(Deserialize)]
struct InstalledDocument {
    schema: u16,
    status: String,
    packs: Vec<ThemePackSummary>,
}

/// Validate the producer's reply and return the packs it reported.
///
/// A reply that names a pack the service did not ask for, a duplicate, a slug
/// that cannot round-trip, or an install that produced several packs without
/// `all` is a failure rather than a partial success.
fn installed(bytes: &[u8], all: bool) -> Result<ThemeInstallResult> {
    let document: InstalledDocument =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!(INSTALL_FAILED_MESSAGE))?;
    if document.schema != 1 || document.status != "installed" {
        bail!(INSTALL_FAILED_MESSAGE);
    }
    if document.packs.is_empty()
        || document.packs.len() > MAX_PACKS
        || (!all && document.packs.len() != 1)
    {
        bail!(INSTALL_FAILED_MESSAGE);
    }
    let mut seen = std::collections::BTreeSet::new();
    for pack in &document.packs {
        pack.validate()
            .map_err(|_| anyhow::anyhow!(INSTALL_FAILED_MESSAGE))?;
        if !seen.insert(pack.slug.as_str()) {
            bail!(INSTALL_FAILED_MESSAGE);
        }
    }
    Ok(ThemeInstallResult {
        schema_version: 1,
        status: "installed".into(),
        packs: document.packs,
    })
}

/// The fixed producer reply to `theme-uninstall --json`.
#[derive(Deserialize)]
struct RemovedDocument {
    schema: u16,
    status: String,
    slug: String,
}

/// Validate the producer's reply against the exact slug that was requested.
fn removed(bytes: &[u8], slug: &str) -> Result<ThemeUninstallResult> {
    let document: RemovedDocument =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!(UNINSTALL_FAILED_MESSAGE))?;
    if document.schema != 1 || document.status != "removed" || document.slug != slug {
        bail!(UNINSTALL_FAILED_MESSAGE);
    }
    Ok(ThemeUninstallResult {
        schema_version: 1,
        status: "removed".into(),
        slug: document.slug,
    })
}

fn helper_path() -> OsString {
    let mut path = OsString::from(HELPER_PATH_PREFIX);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Write an executable helper under `dir` (which may contain spaces).
    fn install_helper(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("mac-themer");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// A helper that records its arguments, writes attacker-controlled stderr,
    /// then prints the response file. `$dir` is resolved from the script's own
    /// location, so a path with spaces never needs quoting from the caller.
    const RECORDING_HELPER: &str = "#!/bin/sh\n\
        dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
        printf '%s\\n' \"$@\" > \"$dir/args.txt\"\n\
        printf 'stderr /private/secret path\\n' >&2\n\
        cat \"$dir/response.json\"\n";

    fn write_response(dir: &Path, json: &str) {
        std::fs::write(dir.join("response.json"), json).unwrap();
    }

    fn read_args(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("args.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    const INSTALLED_ONE: &str = r#"{"schema":1,"status":"installed","packs":[{"slug":"aura","name":"Aura","wallpapers":7,"replaced":false}]}"#;

    #[test]
    fn url_validation_matches_the_producer_contract() {
        assert_eq!(
            valid_repo_url("https://github.com/basecamp/omarchy"),
            Some("https://github.com/basecamp/omarchy".into())
        );
        assert_eq!(
            valid_repo_url(" https://github.com/a/omarchy-b-theme.git/ "),
            Some("https://github.com/a/omarchy-b-theme".into())
        );
        assert_eq!(
            valid_repo_url("https://github.com/a/omarchy-b-theme.git"),
            Some("https://github.com/a/omarchy-b-theme".into())
        );
        for url in [
            "",
            "github.com/a/b",
            "git@github.com:a/b.git",
            "http://github.com/a/b",
            "ssh://github.com/a/b",
            "ext::sh -c touch% /tmp/pwned",
            "https://user:token@github.com/a/b",
            "https://github.com:443/a/b",
            "https://gitlab.com/a/b",
            "https://github.com.evil/a/b",
            "https://github.com/a/b?ref=main",
            "https://github.com/a/b#main",
            "https://github.com/a",
            "https://github.com/a/b/tree/main",
            "https://github.com/a/..",
            "https://github.com/a/b\\c",
        ] {
            assert_eq!(valid_repo_url(url), None, "{url}");
        }
        assert_eq!(
            valid_repo_url(&format!("https://github.com/{}", "a".repeat(200))),
            None
        );
    }

    #[test]
    fn installing_passes_fixed_arguments_and_reports_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let spacey = dir.path().join("with space");
        let path = install_helper(&spacey, RECORDING_HELPER);
        write_response(&spacey, INSTALLED_ONE);
        let executor = ThemeInstall::new(path);

        let result = executor
            .install(
                "https://github.com/owner/omarchy-aura-theme.git",
                false,
                false,
            )
            .unwrap();
        assert_eq!(result.schema_version, 1);
        assert_eq!(result.status, "installed");
        assert_eq!(
            result.packs,
            [ThemePackSummary {
                slug: "aura".into(),
                name: "Aura".into(),
                wallpapers: 7,
                replaced: false,
            }]
        );
        assert_eq!(
            read_args(&spacey),
            [
                "theme-install",
                "https://github.com/owner/omarchy-aura-theme",
                "--json",
            ]
        );
        assert!(!executor.is_busy());
    }

    #[test]
    fn all_and_force_are_named_only_when_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        write_response(
            dir.path(),
            r#"{"schema":1,"status":"installed","packs":[{"slug":"a","name":"A","wallpapers":0},{"slug":"b","name":"B","wallpapers":1,"replaced":true}]}"#,
        );
        let executor = ThemeInstall::new(path);
        let result = executor
            .install("https://github.com/owner/collection", true, true)
            .unwrap();
        assert_eq!(result.packs.len(), 2);
        assert_eq!(
            read_args(dir.path()),
            [
                "theme-install",
                "https://github.com/owner/collection",
                "--all",
                "--force",
                "--json",
            ]
        );
        // Two packs without `all` is a reply the service never asked for.
        assert_eq!(
            executor
                .install("https://github.com/owner/collection", false, false)
                .unwrap_err()
                .to_string(),
            INSTALL_FAILED_MESSAGE
        );
    }

    #[test]
    fn uninstalling_names_one_exact_slug_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        write_response(
            dir.path(),
            r#"{"schema":1,"status":"removed","slug":"aura"}"#,
        );
        let executor = ThemeInstall::new(path);
        let result = executor.uninstall("aura").unwrap();
        assert_eq!(result.slug, "aura");
        assert_eq!(result.status, "removed");
        assert_eq!(read_args(dir.path()), ["theme-uninstall", "aura", "--json"]);
        assert!(!executor.is_busy());
    }

    #[test]
    fn an_unusable_url_or_slug_never_reaches_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(
            dir.path(),
            "#!/bin/sh\nprintf 'ran' > \"$(dirname -- \"$0\")/ran.txt\"\n",
        );
        let executor = ThemeInstall::new(path);
        for url in [
            "",
            "http://github.com/a/b",
            "git@github.com:a/b.git",
            "file:///tmp/x",
        ] {
            assert_eq!(
                executor.install(url, false, false).unwrap_err().to_string(),
                INVALID_MESSAGE,
                "{url:?}"
            );
        }
        for slug in ["", ".", "..", "../aura", "Aura", "a/b", "a\0b"] {
            assert_eq!(
                executor.uninstall(slug).unwrap_err().to_string(),
                INVALID_MESSAGE,
                "{slug:?}"
            );
        }
        assert!(!dir.path().join("ran.txt").exists());
        assert!(!executor.is_busy());
    }

    #[test]
    fn a_reply_the_service_did_not_ask_for_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        let executor = ThemeInstall::new(path);
        for response in [
            "not json",
            r#"{"schema":2,"status":"installed","packs":[{"slug":"aura","name":"Aura","wallpapers":0}]}"#,
            r#"{"schema":1,"status":"removed","packs":[{"slug":"aura","name":"Aura","wallpapers":0}]}"#,
            r#"{"schema":1,"status":"installed","packs":[]}"#,
            r#"{"schema":1,"status":"installed","packs":[{"slug":"Aura","name":"Aura","wallpapers":0}]}"#,
            r#"{"schema":1,"status":"installed","packs":[{"slug":"aura","name":"","wallpapers":0}]}"#,
            r#"{"schema":1,"status":"installed","packs":[{"slug":"aura","wallpapers":0}]}"#,
            // The same pack twice is not two packs.
            r#"{"schema":1,"status":"installed","packs":[{"slug":"aura","name":"Aura","wallpapers":0},{"slug":"aura","name":"Aura","wallpapers":0}]}"#,
        ] {
            write_response(dir.path(), response);
            assert_eq!(
                executor
                    .install("https://github.com/owner/omarchy-aura", true, false)
                    .unwrap_err()
                    .to_string(),
                INSTALL_FAILED_MESSAGE,
                "{response}"
            );
        }
        assert!(!executor.is_busy());
    }

    #[test]
    fn a_removal_reply_must_name_the_pack_that_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        let executor = ThemeInstall::new(path);
        for response in [
            r#"{"schema":1,"status":"removed","slug":"other"}"#,
            r#"{"schema":1,"status":"removed"}"#,
            r#"{"schema":1,"status":"installed","slug":"aura"}"#,
        ] {
            write_response(dir.path(), response);
            assert_eq!(
                executor.uninstall("aura").unwrap_err().to_string(),
                UNINSTALL_FAILED_MESSAGE,
                "{response}"
            );
        }
    }

    #[test]
    fn a_failing_or_slow_helper_is_reported_without_leaking_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let failing = install_helper(dir.path(), "#!/bin/sh\necho /private/secret >&2\nexit 3\n");
        let executor = ThemeInstall::new(failing);
        let error = executor
            .install("https://github.com/owner/a", false, false)
            .unwrap_err()
            .to_string();
        assert_eq!(error, INSTALL_FAILED_MESSAGE);
        assert!(!error.contains("/private/secret"));
        assert!(!executor.is_busy());

        let slow = install_helper(&dir.path().join("slow"), "#!/bin/sh\nsleep 5\n");
        let executor = ThemeInstall::with_timeouts(
            slow,
            None,
            Duration::from_millis(150),
            Duration::from_millis(150),
        );
        assert_eq!(
            executor.uninstall("aura").unwrap_err().to_string(),
            UNINSTALL_FAILED_MESSAGE
        );
        assert!(!executor.is_busy());
    }

    #[test]
    fn oversized_output_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(
            dir.path(),
            "#!/bin/sh\ni=0\nwhile [ $i -lt 3000 ]; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaa\\n'; i=$((i+1)); done\n",
        );
        let executor = ThemeInstall::with_timeouts(
            path,
            None,
            Duration::from_secs(10),
            Duration::from_secs(10),
        );
        assert_eq!(
            executor
                .install("https://github.com/owner/a", false, false)
                .unwrap_err()
                .to_string(),
            INSTALL_FAILED_MESSAGE
        );
    }

    #[test]
    fn single_flight_rejects_a_second_mutation_then_resets() {
        let dir = tempfile::tempdir().unwrap();
        let slow = install_helper(
            &dir.path().join("slow dir"),
            "#!/bin/sh\nsleep 0.6\n\
             dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
             printf '%s\\n' \"$@\" > \"$dir/args.txt\"\n\
             cat \"$dir/response.json\"\n",
        );
        write_response(&dir.path().join("slow dir"), INSTALLED_ONE);
        let executor = ThemeInstall::with_timeouts(
            slow,
            None,
            Duration::from_secs(10),
            Duration::from_secs(10),
        );

        let worker = {
            let executor = Arc::clone(&executor);
            std::thread::spawn(move || executor.install("https://github.com/owner/a", false, false))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !executor.is_busy() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first install never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            executor.uninstall("aura").unwrap_err().to_string(),
            BUSY_MESSAGE
        );
        worker.join().unwrap().unwrap();
        assert!(
            !executor.is_busy(),
            "the guard resets after a successful run"
        );
        write_response(
            &dir.path().join("slow dir"),
            r#"{"schema":1,"status":"removed","slug":"aura"}"#,
        );
        assert_eq!(executor.uninstall("aura").unwrap().slug, "aura");
    }
}
