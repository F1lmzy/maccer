//! Real-process tests for the cached read-only theme catalog.
//!
//! Every test writes its own helper script into a temp directory and points the
//! cache at it. None run mac-themer, read the user's configuration, or touch
//! the desktop.
use launcher_agent::protocol::{
    THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS, ThemePalette,
};
use launcher_agent::service::{CatalogSnapshot, ThemeCatalog};
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

fn palette(slug: &str) -> ThemePalette {
    let map = |keys: &[&str]| {
        keys.iter()
            .map(|key| ((*key).to_owned(), "#123456".to_owned()))
            .collect::<BTreeMap<_, _>>()
    };
    ThemePalette {
        schema: 1,
        slug: slug.into(),
        name: format!("{slug} pack"),
        variant: "dark".into(),
        alpha_byte: 230,
        palette: map(&THEME_PALETTE_KEYS),
        tokens: map(&THEME_TOKEN_KEYS),
        border: map(&THEME_BORDER_KEYS),
        bar: map(&THEME_BAR_KEYS),
    }
}

fn catalog_document(slugs: &[&str]) -> String {
    let themes: Vec<_> = slugs
        .iter()
        .map(|slug| {
            // The producer emits resolved palettes plus private metadata that
            // the service must drop; include both so the test proves stripping.
            let mut entry = serde_json::to_value(palette(slug)).unwrap();
            entry["generated_by"] = json!("mac-themer test");
            entry["pack"] = json!("/private/secret/pack");
            entry["backgrounds"] = json!("/private/secret/backgrounds");
            entry
        })
        .collect();
    serde_json::to_string(&json!({ "schema": 1, "themes": themes })).unwrap()
}

fn script(prefix: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("helper");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

fn valid_helper(prefix: &str, slugs: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let document = catalog_document(slugs);
    script(prefix, &format!("cat <<'JSON'\n{document}\nJSON"))
}

fn snapshot(catalog: &ThemeCatalog) -> CatalogSnapshot {
    catalog.snapshot()
}

#[test]
fn start_loads_sorts_and_strips_a_valid_catalog_over_a_real_socketless_helper() {
    let (_dir, helper) = valid_helper("mc-cat-valid-", &["b-pack", "a-pack"]);
    let catalog = ThemeCatalog::start(helper);
    let state = snapshot(&catalog);
    assert!(state.error.is_none(), "unexpected {:?}", state.error);
    let slugs: Vec<&str> = state.palettes.iter().map(|p| p.slug.as_str()).collect();
    assert_eq!(slugs, ["a-pack", "b-pack"]);
    // The DTO has no path fields, so private metadata cannot survive the round
    // trip even though the helper printed it.
    let wire = serde_json::to_string(&*state.palettes).unwrap();
    assert!(!wire.contains("/private/secret"));
}

#[test]
fn helper_receives_the_fixed_arguments_and_a_prepended_development_path() {
    let document = catalog_document(&["pack"]);
    // Exits nonzero unless both arguments and the PATH prefix are right.
    let body = format!(
        "[ \"$1\" = catalog ] || exit 4\n\
         [ \"$2\" = --json ] || exit 5\n\
         case \"$PATH\" in /opt/homebrew/bin:/usr/local/bin:*) ;; *) exit 6;; esac\n\
         cat <<'JSON'\n{document}\nJSON"
    );
    let (_dir, helper) = script("mc-cat-args-", &body);
    let state = snapshot(&ThemeCatalog::start(helper));
    assert!(state.error.is_none(), "unexpected {:?}", state.error);
    assert_eq!(state.palettes.len(), 1);
}

#[test]
fn helper_failure_is_a_public_error_without_stderr_or_paths() {
    let (_dir, helper) = script("mc-cat-fail-", "echo /private/secret >&2\nexit 3");
    let state = snapshot(&ThemeCatalog::start(helper));
    assert!(state.palettes.is_empty());
    let error = state.error.expect("degraded status");
    assert_eq!(error, "Theme catalog helper failed.");
    assert!(!error.contains("/private") && !error.contains("secret"));
}

#[test]
fn malformed_and_duplicate_catalogs_are_rejected_whole() {
    let (_dir, malformed) = script("mc-cat-bad-", "printf '%s' not-json");
    assert_eq!(
        snapshot(&ThemeCatalog::start(malformed)).error.as_deref(),
        Some("Theme catalog response is invalid.")
    );
    let (_dir, duplicate) = valid_helper("mc-cat-dup-", &["pack", "pack"]);
    assert_eq!(
        snapshot(&ThemeCatalog::start(duplicate)).error.as_deref(),
        Some("Theme catalog contains a duplicate pack.")
    );
}

#[test]
fn oversized_helper_output_is_rejected_before_parsing() {
    // 2.2 MiB of JSON-shaped noise exceeds the 2 MiB stdout limit.
    let (_dir, helper) = script("mc-cat-large-", "/usr/bin/yes a | /usr/bin/head -c 2306867");
    assert_eq!(
        snapshot(&ThemeCatalog::start(helper)).error.as_deref(),
        Some("Theme catalog response is too large.")
    );
}

#[test]
fn a_slow_helper_is_bounded_and_drop_stops_the_worker() {
    let (_dir, helper) = script("mc-cat-slow-", "/bin/sleep 30");
    let started = Instant::now();
    let catalog = ThemeCatalog::start(helper);
    // The synchronous initial load must honor the three-second deadline, not
    // block on the child's own sleep.
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(
        snapshot(&catalog).error.as_deref(),
        Some("Theme catalog helper could not run.")
    );
    let dropped = Instant::now();
    drop(catalog);
    assert!(dropped.elapsed() < Duration::from_secs(2));
}

#[test]
fn from_palettes_is_in_memory_sorted_and_deduplicated() {
    let catalog = ThemeCatalog::from_palettes(vec![palette("b"), palette("a")]).unwrap();
    let state = snapshot(&catalog);
    assert_eq!(
        state
            .palettes
            .iter()
            .map(|p| p.slug.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(state.error.is_none());
    assert!(ThemeCatalog::from_palettes(vec![palette("Bad Slug")]).is_err());
}

#[test]
fn an_empty_catalog_is_valid_and_reports_no_error() {
    let catalog = ThemeCatalog::from_palettes(Vec::new()).unwrap();
    let state = snapshot(&catalog);
    assert!(state.palettes.is_empty());
    assert!(state.error.is_none());
}
