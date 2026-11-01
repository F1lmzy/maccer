//! Guarded, single-flight theme application for the shared service.
//!
//! mac-themer's `apply SLUG --expected-palette JSON --json` reloads and
//! revalidates its catalog, compares the full resolved, path-free palette
//! against the service's trusted copy, then applies the theme with wallpaper
//! changes disabled. This module runs that one fixed command through the
//! bounded subprocess runner under a single-flight lock and projects the fixed
//! JSON response into a path-free result. Helper stderr, filesystem paths and
//! client-supplied slugs never reach the caller.
//!
//! The expected palette is the only input and always comes from the service's
//! own [`ThemeCatalog`](super::ThemeCatalog) snapshot, never from a frontend.
//! The producer rejects a mismatched palette before writing. This check is not
//! a filesystem lock against another process editing user-managed pack files.

use anyhow::{Result, bail};
use launcher_core::CancellationToken;
use launcher_macos::process::run_bounded;
use launcher_protocol::ThemePalette;
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

/// Hard deadline for one apply invocation. Comfortably under the client's
/// 35-second action read timeout, leaving time for the failure response.
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Reject a response whose stdout exceeds this many bytes. The runner captures
/// `+1` so an over-limit payload is observable rather than silently truncated.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Homebrew Python must precede Apple's older system Python. Other environment
/// variables, including mac-themer overrides, are inherited unchanged.
const HELPER_PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:";
const BUSY_MESSAGE: &str =
    "Another theme change is already in progress. Wait for it to finish, then confirm again.";
const PARTIAL_MESSAGE: &str = "The theme helper failed or did not finish. The theme may be partly \
applied; check the active theme before retrying.";
const INVALID_MESSAGE: &str = "The theme information is invalid.";
/// Warning codes the producer may report without the change being a failure.
const KNOWN_WARNINGS: [&str; 1] = ["ghosttyReloadFailed"];

/// Shared executor for confirmed theme application. Cheap to clone behind an
/// `Arc`; only one apply may run at a time across every client.
pub struct ThemeApply {
    helper: PathBuf,
    timeout: Duration,
    busy: AtomicBool,
}

impl ThemeApply {
    /// Create an executor that runs the helper at `helper` (an executable
    /// path) with the production 30-second deadline.
    pub fn new(helper: PathBuf) -> Arc<Self> {
        Self::with_timeout(helper, APPLY_TIMEOUT)
    }

    fn with_timeout(helper: PathBuf, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            helper,
            timeout,
            busy: AtomicBool::new(false),
        })
    }

    /// Whether an apply is currently running. A second [`apply`](Self::apply)
    /// call is rejected with a fixed message instead of queuing.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Apply the exact catalog palette in `expected`. Returns a path-free,
    /// frontend-safe success message. A concurrent attempt, an invalid
    /// expected palette, or any helper failure is an error: helper failures
    /// explicitly warn that the theme may be partly applied and never trigger
    /// an automatic retry.
    pub fn apply(&self, expected: &ThemePalette) -> Result<String> {
        expected
            .validate()
            .map_err(|_| anyhow::anyhow!(INVALID_MESSAGE))?;
        if !valid_slug(&expected.slug) {
            bail!(INVALID_MESSAGE);
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            bail!(BUSY_MESSAGE);
        }
        // Resets the single-flight flag on every normal exit path.
        let _busy = BusyGuard(&self.busy);
        self.run(expected)
    }

    fn run(&self, expected: &ThemePalette) -> Result<String> {
        let expected_json =
            serde_json::to_string(expected).map_err(|_| anyhow::anyhow!(INVALID_MESSAGE))?;
        let mut command = Command::new(&self.helper);
        command
            .arg("apply")
            .arg(&expected.slug)
            .arg("--expected-palette")
            .arg(&expected_json)
            .arg("--json");
        command.env("PATH", helper_path());
        // An accepted mutation must survive the query that produced it, so the
        // run is not tied to any session cancellation token.
        let output = run_bounded(
            &mut command,
            &CancellationToken::default(),
            self.timeout,
            MAX_OUTPUT_BYTES + 1,
        )
        .map_err(|_| anyhow::anyhow!(PARTIAL_MESSAGE))?;
        if !output.status.success() || output.stdout.len() > MAX_OUTPUT_BYTES {
            bail!(PARTIAL_MESSAGE);
        }
        let warnings = warnings_from(&output.stdout, &expected.slug)
            .ok_or_else(|| anyhow::anyhow!(PARTIAL_MESSAGE))?;
        Ok(message(expected, &warnings))
    }

    /// A test-only handle to the private constructor, kept out of the public
    /// surface so production code always uses the 30-second deadline.
    #[cfg(test)]
    fn with_test_timeout(helper: PathBuf, timeout: Duration) -> Arc<Self> {
        Self::with_timeout(helper, timeout)
    }
}

/// Reset the single-flight flag when the in-flight apply leaves scope.
struct BusyGuard<'a>(&'a AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The fixed producer reply. Unknown top-level fields are ignored rather than
/// forwarded, so a future producer cannot leak a path through this response.
#[derive(Deserialize)]
struct Applied {
    schema: u16,
    status: String,
    theme: String,
    #[serde(default)]
    warnings: Vec<String>,
}

/// Validate the producer's reply against the requested slug and the known
/// warning set. Any mismatch is treated as a partial failure by the caller.
fn warnings_from(bytes: &[u8], slug: &str) -> Option<Vec<String>> {
    let applied: Applied = serde_json::from_slice(bytes).ok()?;
    if applied.schema != 1 || applied.status != "applied" || applied.theme != slug {
        return None;
    }
    if applied
        .warnings
        .iter()
        .any(|warning| !KNOWN_WARNINGS.contains(&warning.as_str()))
    {
        return None;
    }
    Some(applied.warnings)
}

fn message(expected: &ThemePalette, warnings: &[String]) -> String {
    let mut text = format!("Applied {}. Wallpaper was not changed.", expected.name);
    if warnings
        .iter()
        .any(|warning| warning == "ghosttyReloadFailed")
    {
        text.push_str(" Ghostty did not reload; reload it manually.");
    }
    text
}

fn helper_path() -> OsString {
    let mut path = OsString::from(HELPER_PATH_PREFIX);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing);
    }
    path
}

/// `[a-z0-9][a-z0-9_-]{0,127}`, matching the producer and catalog contract.
fn valid_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_protocol::{
        THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    fn palette(slug: &str) -> ThemePalette {
        let map = |keys: &[&str]| {
            keys.iter()
                .map(|key| ((*key).to_owned(), "#123456".to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        ThemePalette {
            schema: 1,
            slug: slug.into(),
            name: "Test Pack".into(),
            variant: "dark".into(),
            alpha_byte: 230,
            palette: map(&THEME_PALETTE_KEYS),
            tokens: map(&THEME_TOKEN_KEYS),
            border: map(&THEME_BORDER_KEYS),
            bar: map(&THEME_BAR_KEYS),
        }
    }

    /// Write an executable helper under `dir` (which may contain spaces) and
    /// return its path.
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

    #[test]
    fn invalid_expected_palette_is_rejected_without_running_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(
            dir.path(),
            "#!/bin/sh\nprintf 'ran' > \"$(dirname -- \"$0\")/ran.txt\"\n",
        );
        let apply = ThemeApply::new(path);

        // A slug that is not ASCII-safe fails the producer contract.
        assert_eq!(
            apply.apply(&palette("Bad Slug")).unwrap_err().to_string(),
            INVALID_MESSAGE
        );
        // A padded/incomplete palette fails validate().
        let mut broken = palette("pack");
        broken.palette.remove("background");
        assert!(apply.apply(&broken).is_err());
        assert!(!dir.path().join("ran.txt").exists());
        assert!(!apply.is_busy());
    }

    #[test]
    fn success_passes_fixed_arguments_and_reports_no_wallpaper_change() {
        let dir = tempfile::tempdir().unwrap();
        let spacey = dir.path().join("with space");
        let path = install_helper(&spacey, RECORDING_HELPER);
        write_response(
            &spacey,
            r#"{"schema":1,"status":"applied","theme":"pack","warnings":[]}"#,
        );
        let apply = ThemeApply::new(path);

        let expected = palette("pack");
        assert_eq!(
            apply.apply(&expected).unwrap(),
            "Applied Test Pack. Wallpaper was not changed."
        );
        let recorded = read_args(&spacey);
        assert_eq!(recorded.len(), 5);
        assert_eq!(recorded[0], "apply");
        assert_eq!(recorded[1], "pack");
        assert_eq!(recorded[2], "--expected-palette");
        assert_eq!(recorded[3], serde_json::to_string(&expected).unwrap());
        assert_eq!(recorded[4], "--json");
        assert!(!apply.is_busy());
    }

    #[test]
    fn known_ghostty_warning_is_reported_without_claiming_complete_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        write_response(
            dir.path(),
            r#"{"schema":1,"status":"applied","theme":"pack","warnings":["ghosttyReloadFailed"]}"#,
        );
        let apply = ThemeApply::new(path);
        let message = apply.apply(&palette("pack")).unwrap();
        assert!(message.starts_with("Applied Test Pack."));
        assert!(message.contains("Ghostty did not reload"));
    }

    #[test]
    fn malformed_wrong_theme_unknown_warning_and_nonzero_are_partial_failures() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            "not json",
            r#"{"schema":1,"status":"applied","theme":"other","warnings":[]}"#,
            r#"{"schema":1,"status":"applied","theme":"pack","warnings":["mystery"]}"#,
            r#"{"schema":2,"status":"applied","theme":"pack","warnings":[]}"#,
        ];
        for json in cases {
            write_response(dir.path(), json);
            let path = install_helper(dir.path(), RECORDING_HELPER);
            let apply = ThemeApply::new(path);
            assert_eq!(
                apply.apply(&palette("pack")).unwrap_err().to_string(),
                PARTIAL_MESSAGE,
                "case {json}"
            );
        }

        let failing = install_helper(dir.path(), "#!/bin/sh\nexit 3\n");
        let apply = ThemeApply::new(failing);
        assert_eq!(
            apply.apply(&palette("pack")).unwrap_err().to_string(),
            PARTIAL_MESSAGE
        );
        assert!(!apply.is_busy());
    }

    #[test]
    fn oversized_output_is_rejected_as_a_partial_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(
            dir.path(),
            "#!/bin/sh\ni=0\nwhile [ $i -lt 3000 ]; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaa\\n'; i=$((i+1)); done\n",
        );
        let apply = ThemeApply::with_test_timeout(path, Duration::from_secs(10));
        assert_eq!(
            apply.apply(&palette("pack")).unwrap_err().to_string(),
            PARTIAL_MESSAGE
        );
    }

    #[test]
    fn helper_stderr_and_filesystem_paths_never_reach_success_or_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING_HELPER);
        write_response(
            dir.path(),
            r#"{"schema":1,"status":"applied","theme":"pack","warnings":[]}"#,
        );
        let apply = ThemeApply::new(path);
        let ok = apply.apply(&palette("pack")).unwrap();
        assert!(!ok.contains("/private/secret"));
        assert!(!ok.contains(&dir.path().display().to_string()));

        let failing = install_helper(dir.path(), "#!/bin/sh\necho /private/secret >&2\nexit 1\n");
        let apply = ThemeApply::new(failing);
        let error = apply.apply(&palette("pack")).unwrap_err().to_string();
        assert_eq!(error, PARTIAL_MESSAGE);
        assert!(!error.contains("/private/secret"));
        assert!(!error.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn single_flight_rejects_a_second_apply_then_resets() {
        let dir = tempfile::tempdir().unwrap();
        let slow = install_helper(
            &dir.path().join("slow dir"),
            "#!/bin/sh\nsleep 0.6\n\
             dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
             printf '%s\\n' \"$@\" > \"$dir/args.txt\"\n\
             cat \"$dir/response.json\"\n",
        );
        write_response(
            &dir.path().join("slow dir"),
            r#"{"schema":1,"status":"applied","theme":"pack","warnings":[]}"#,
        );
        let apply = ThemeApply::with_test_timeout(slow, Duration::from_secs(10));

        let worker = {
            let apply = Arc::clone(&apply);
            let expected = palette("pack");
            std::thread::spawn(move || apply.apply(&expected))
        };
        // Wait until the first apply owns the single-flight flag.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !apply.is_busy() {
            assert!(
                std::time::Instant::now() < deadline,
                "first apply never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            apply.apply(&palette("pack")).unwrap_err().to_string(),
            BUSY_MESSAGE
        );
        worker.join().unwrap().unwrap();
        assert!(!apply.is_busy(), "guard resets after a successful run");
        // Not wedged: a later apply runs to completion instead of reporting
        // the busy message forever.
        assert_eq!(
            apply.apply(&palette("pack")).unwrap(),
            "Applied Test Pack. Wallpaper was not changed."
        );
    }

    #[test]
    fn timeout_is_a_partial_failure_and_does_not_wedge_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), "#!/bin/sh\nsleep 5\n");
        let apply = ThemeApply::with_test_timeout(path, Duration::from_millis(150));
        assert_eq!(
            apply.apply(&palette("pack")).unwrap_err().to_string(),
            PARTIAL_MESSAGE
        );
        assert!(!apply.is_busy());
        // Still usable: the next attempt runs (and also times out) instead of
        // reporting the busy message forever.
        assert_ne!(
            apply.apply(&palette("pack")).unwrap_err().to_string(),
            BUSY_MESSAGE
        );
    }

    #[test]
    fn slug_validation_matches_the_producer_contract() {
        assert!(valid_slug("a"));
        assert!(valid_slug("a-b_c9"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("A"));
        assert!(!valid_slug("-lead"));
        assert!(!valid_slug("bad space"));
        assert!(!valid_slug(&"a".repeat(129)));
        assert!(valid_slug(&"a".repeat(128)));
    }
}
