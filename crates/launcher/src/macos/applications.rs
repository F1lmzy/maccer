use anyhow::{Context, Result};
use plist::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// Upper bound on directory entries examined by one `discover_in` call. Keeps a
/// pathological directory tree from stalling startup. Decremented through
/// [`claim_entry`] so it can never underflow.
const ENTRY_BUDGET: usize = 30_000;

/// Metadata used by the application provider. Icon loading remains the UI's concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Application {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    /// Name shown to the user. Finder's localized name when available, otherwise
    /// the raw bundle display name or the file name.
    pub display_name: String,
    /// Raw, non-localized bundle name when it differs from `display_name`, kept
    /// as a secondary search alias.
    pub unlocalized_name: Option<String>,
    pub executable_name: Option<String>,
    pub icon_path: Option<PathBuf>,
}

/// Default roots scanned for installed applications, in precedence order.
///
/// Roots are directories; the final entry is an explicit bundle because Finder
/// lives outside the application directories. When two bundles share a bundle
/// identifier or resolve to the same path, the one from the earlier root wins.
pub fn application_search_paths() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Library/CoreServices/Applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    // Finder is outside the application directories. Include its bundle
    // explicitly rather than exposing internal CoreServices helper apps.
    roots.push(PathBuf::from("/System/Library/CoreServices/Finder.app"));
    roots
}

/// Scan the default application locations. A missing root does not fail discovery.
pub fn discover_applications() -> Result<Vec<Application>> {
    let roots = application_search_paths();
    discover_in(&roots)
}

/// Scan directories or explicit app bundles without descending into bundle contents.
///
/// Each directory root contributes its immediate `.app` entries and the `.app`
/// entries one subfolder level below it. A bundle's own contents are never
/// searched for nested apps, except for the deliberate companion bundles under
/// `Contents/Applications`. Roots are processed in order; duplicates are dropped
/// before the result is sorted, so a bundle reachable from an earlier root keeps
/// its spot.
pub fn discover_in(roots: &[PathBuf]) -> Result<Vec<Application>> {
    discover_with_budget(roots, ENTRY_BUDGET)
}

fn discover_with_budget(roots: &[PathBuf], budget: usize) -> Result<Vec<Application>> {
    let mut found = Vec::new();
    let mut remaining = budget;
    let mut exhausted = false;
    for root in roots {
        scan_root(root, &mut remaining, &mut exhausted, &mut found);
        if exhausted {
            anyhow::bail!("application discovery exceeded the {budget}-entry scan limit");
        }
    }
    deduplicate_preserving_order(&mut found);
    found.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(found)
}

fn scan_root(
    root: &Path,
    remaining: &mut usize,
    exhausted: &mut bool,
    found: &mut Vec<Application>,
) {
    if is_app_bundle(root) {
        if root.is_dir() {
            add_bundle(root, remaining, exhausted, found);
        }
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !claim_entry(remaining, exhausted) {
            return;
        }
        let path = entry.path();
        if is_app_bundle(&path) {
            if path.is_dir() {
                add_bundle(&path, remaining, exhausted, found);
            }
        } else if path.is_dir() {
            scan_one_level(&path, remaining, exhausted, found);
        }
    }
}

/// Search exactly one directory level below a root and then stop. Deeper
/// directories are intentionally ignored so discovery stays bounded.
fn scan_one_level(
    dir: &Path,
    remaining: &mut usize,
    exhausted: &mut bool,
    found: &mut Vec<Application>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !claim_entry(remaining, exhausted) {
            return;
        }
        let path = entry.path();
        if is_app_bundle(&path) && path.is_dir() {
            add_bundle(&path, remaining, exhausted, found);
        }
    }
}

/// Record one bundle plus the companion apps it ships under
/// `Contents/Applications`. Bundles inside those companions are not expanded.
fn add_bundle(
    bundle: &Path,
    remaining: &mut usize,
    exhausted: &mut bool,
    found: &mut Vec<Application>,
) {
    if let Some(app) = read_application(bundle) {
        found.push(app);
    }
    let companions = bundle.join("Contents").join("Applications");
    let Ok(entries) = fs::read_dir(&companions) else {
        return;
    };
    for entry in entries.flatten() {
        if !claim_entry(remaining, exhausted) {
            return;
        }
        let path = entry.path();
        if is_app_bundle(&path)
            && path.is_dir()
            && let Some(app) = read_application(&path)
        {
            found.push(app);
        }
    }
}

/// Consume one unit of the entry budget. Returns `false` when the budget is
/// exhausted, signalling the caller to stop walking.
fn claim_entry(remaining: &mut usize, exhausted: &mut bool) -> bool {
    if *remaining == 0 {
        *exhausted = true;
        return false;
    }
    *remaining -= 1;
    true
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

fn deduplicate_preserving_order(found: &mut Vec<Application>) {
    let mut paths = HashSet::new();
    let mut bundles = HashSet::new();
    found.retain(|app| {
        let path = fs::canonicalize(&app.path).unwrap_or_else(|_| app.path.clone());
        if !paths.insert(path) {
            return false;
        }
        if let Some(id) = &app.bundle_id
            && !bundles.insert(id.to_lowercase())
        {
            return false;
        }
        true
    });
}

fn read_application(path: &Path) -> Option<Application> {
    let info = path.join("Contents/Info.plist");
    let value = Value::from_file(&info)
        .with_context(|| format!("reading {}", info.display()))
        .ok()?;
    let dict = value.as_dictionary()?;
    let bundle_id = dict
        .get("CFBundleIdentifier")
        .and_then(Value::as_string)
        .map(str::to_owned);
    let executable_name = dict
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .map(str::to_owned);
    // Raw, non-localized names come straight from the bundle plist.
    let raw_name = ["CFBundleDisplayName", "CFBundleName"]
        .iter()
        .find_map(|key| dict.get(key).and_then(Value::as_string))
        .map(str::to_owned);
    // Finder's name is localized; fall back to the plist and then the file name.
    let display_name = finder_display_name(path)
        .or_else(|| raw_name.clone())
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))?;
    let unlocalized_name = raw_name.filter(|name| !name.is_empty() && *name != display_name);
    let icon_name = dict
        .get("CFBundleIconFile")
        .and_then(Value::as_string)
        .map(|s| s.strip_suffix(".icns").unwrap_or(s));
    let resources = path.join("Contents/Resources");
    let icon_path = icon_name
        .map(|name| resources.join(format!("{name}.icns")))
        .filter(|p| p.is_file());
    Some(Application {
        path: path.to_path_buf(),
        bundle_id,
        display_name,
        unlocalized_name,
        executable_name,
        icon_path,
    })
}

/// Finder's localized display name for a bundle, with a trailing `.app` and
/// soft hyphens removed. Returns `None` off macOS.
#[cfg(target_os = "macos")]
fn finder_display_name(path: &Path) -> Option<String> {
    use objc2_foundation::{NSFileManager, NSString};
    let path = path.to_str()?;
    // NSFileManager is thread-safe; keep its autoreleased return local.
    objc2::rc::autoreleasepool(|_| {
        let name = NSFileManager::defaultManager()
            .displayNameAtPath(&NSString::from_str(path))
            .to_string();
        clean_display_name(name)
    })
}

#[cfg(not(target_os = "macos"))]
fn finder_display_name(_path: &Path) -> Option<String> {
    None
}

/// Strip a trailing `.app` (Finder hides known extensions) and soft hyphens.
fn clean_display_name(mut name: String) -> Option<String> {
    // Compare bytes so a multibyte tail cannot split a UTF-8 boundary. Finder
    // can hand back "Name.APP.app" for a folder whose name already carries an
    // extension, so strip the bundle suffix case-insensitively until it is gone.
    while name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".app") {
        name.truncate(name.len() - 4);
    }
    name = name.replace('\u{00AD}', "");
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn fixture() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "maccer-app-scan-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn scans_app_bundles_and_does_not_descend_into_nested_apps() {
        let root = fixture();
        let app = root.join("Editor.app/Contents");
        fs::create_dir_all(&app).unwrap();
        plist::to_file_xml(
            app.join("Info.plist"),
            &Value::Dictionary(plist::Dictionary::from_iter([
                (
                    "CFBundleIdentifier",
                    Value::String("dev.example.editor".into()),
                ),
                ("CFBundleName", Value::String("Editor".into())),
                ("CFBundleExecutable", Value::String("editor".into())),
            ])),
        )
        .unwrap();
        let nested = root.join("Editor.app/Contents/Resources/Helper.app/Contents");
        fs::create_dir_all(&nested).unwrap();
        plist::to_file_xml(
            nested.join("Info.plist"),
            &Value::Dictionary(plist::Dictionary::from_iter([(
                "CFBundleName",
                Value::String("Helper".into()),
            )])),
        )
        .unwrap();
        let apps = discover_in(std::slice::from_ref(&root)).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].display_name, "Editor");
        assert_eq!(apps[0].executable_name.as_deref(), Some("editor"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_explicit_finder_bundle_without_scanning_core_services() {
        let root = fixture();
        for name in ["Finder", "InternalHelper"] {
            let contents = root.join(format!("CoreServices/{name}.app/Contents"));
            fs::create_dir_all(&contents).unwrap();
            plist::to_file_xml(
                contents.join("Info.plist"),
                &Value::Dictionary(plist::Dictionary::from_iter([(
                    "CFBundleName",
                    Value::String(name.into()),
                )])),
            )
            .unwrap();
        }
        let finder = root.join("CoreServices/Finder.app");
        let apps = discover_in(&[finder.clone(), finder, root.join("missing.app")]).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].display_name, "Finder");
        assert_eq!(apps[0].path, root.join("CoreServices/Finder.app"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn standard_discovery_includes_finder() {
        let apps = discover_applications().unwrap();
        assert!(
            apps.iter()
                .any(|app| app.bundle_id.as_deref() == Some("com.apple.finder")
                    && app.path == Path::new("/System/Library/CoreServices/Finder.app"))
        );
    }

    #[test]
    fn ignores_missing_roots_and_deduplicates_canonical_app_path() {
        let root = fixture();
        let contents = root.join("One.app/Contents");
        fs::create_dir_all(&contents).unwrap();
        plist::to_file_xml(
            contents.join("Info.plist"),
            &Value::Dictionary(plist::Dictionary::from_iter([(
                "CFBundleName",
                Value::String("One".into()),
            )])),
        )
        .unwrap();
        let apps = discover_in(&[root.clone(), root.join("absent")]).unwrap();
        assert_eq!(apps.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    fn write_app(dir: &Path, name: &str, bundle: Option<&str>) {
        let contents = dir.join("Contents");
        fs::create_dir_all(&contents).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleName".into(), Value::String(name.into()));
        if let Some(bundle) = bundle {
            dict.insert("CFBundleIdentifier".into(), Value::String(bundle.into()));
        }
        plist::to_file_xml(contents.join("Info.plist"), &Value::Dictionary(dict)).unwrap();
    }

    #[test]
    fn scan_covers_the_root_and_one_subfolder_level_only() {
        let root = fixture();
        write_app(&root.join("Foo.app"), "Foo", None);
        write_app(&root.join("Group/Bar.app"), "Bar", None);
        write_app(&root.join("Group/Deep/Baz.app"), "Baz", None);
        let mut names: Vec<_> = discover_in(std::slice::from_ref(&root))
            .unwrap()
            .into_iter()
            .map(|app| app.display_name)
            .collect();
        names.sort();
        assert_eq!(names, ["Bar", "Foo"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn embedded_companion_apps_are_included_but_bundle_internals_are_not() {
        let root = fixture();
        let main = root.join("Main.app");
        write_app(&main, "Main", Some("dev.example.main"));
        write_app(
            &main.join("Contents/Applications/Companion.app"),
            "Companion",
            Some("dev.example.companion"),
        );
        write_app(
            &main.join("Contents/Resources/Helper.app"),
            "Helper",
            Some("dev.example.helper"),
        );
        let mut names: Vec<_> = discover_in(std::slice::from_ref(&root))
            .unwrap()
            .into_iter()
            .map(|app| app.display_name)
            .collect();
        names.sort();
        assert_eq!(names, ["Companion", "Main"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn earlier_root_wins_when_bundle_identifiers_collide() {
        let first = fixture();
        let second = fixture();
        write_app(&first.join("Zeta.app"), "Zeta", Some("dup.example"));
        write_app(&second.join("Alpha.app"), "Alpha", Some("dup.example"));
        let apps = discover_in(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].path, first.join("Zeta.app"));
        fs::remove_dir_all(first).unwrap();
        fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn default_roots_are_listed_in_precedence_order() {
        let roots = application_search_paths();
        assert_eq!(roots[0], Path::new("/Applications"));
        assert!(roots.contains(&PathBuf::from("/System/Applications")));
        assert!(roots.contains(&PathBuf::from("/System/Library/CoreServices/Applications")));
        assert!(roots.contains(&PathBuf::from("/System/Library/CoreServices/Finder.app")));
    }

    #[test]
    fn case_insensitive_app_suffix_is_recognized() {
        let root = fixture();
        write_app(&root.join("Shout.APP"), "Shout", None);
        let apps = discover_in(std::slice::from_ref(&root)).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].display_name, "Shout");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn raw_plist_name_is_kept_as_alias_when_it_differs_from_display_name() {
        let root = fixture();
        let contents = root.join("Editor.app/Contents");
        fs::create_dir_all(&contents).unwrap();
        plist::to_file_xml(
            contents.join("Info.plist"),
            &Value::Dictionary(plist::Dictionary::from_iter([
                (
                    "CFBundleIdentifier",
                    Value::String("dev.example.editor".into()),
                ),
                ("CFBundleDisplayName", Value::String("Editor Pro".into())),
            ])),
        )
        .unwrap();
        let apps = discover_in(std::slice::from_ref(&root)).unwrap();
        assert_eq!(apps[0].display_name, "Editor");
        assert_eq!(apps[0].unlocalized_name.as_deref(), Some("Editor Pro"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matching_raw_name_is_not_repeated_as_alias() {
        let root = fixture();
        write_app(&root.join("Editor.app"), "Editor", None);
        let apps = discover_in(std::slice::from_ref(&root)).unwrap();
        assert_eq!(apps[0].display_name, "Editor");
        assert_eq!(apps[0].unlocalized_name, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_display_name_handles_extensions_and_soft_hyphens() {
        assert_eq!(
            clean_display_name("Editor.app".into()).as_deref(),
            Some("Editor")
        );
        assert_eq!(
            clean_display_name("Shout.APP".into()).as_deref(),
            Some("Shout")
        );
        assert_eq!(
            clean_display_name("Shout.APP.app".into()).as_deref(),
            Some("Shout")
        );
        assert_eq!(
            clean_display_name("ca\u{00AD}fe.app".into()).as_deref(),
            Some("cafe")
        );
        // Multibyte tails must not panic and must be preserved.
        assert_eq!(
            clean_display_name("日本語".into()).as_deref(),
            Some("日本語")
        );
        assert_eq!(clean_display_name(".app".into()), None);
    }

    #[test]
    fn scan_limit_returns_error_instead_of_a_partial_catalog() {
        let first = fixture();
        let second = fixture();
        write_app(&first.join("Alpha.app"), "Alpha", None);
        write_app(&second.join("Beta.app"), "Beta", None);
        let error = discover_with_budget(&[first.clone(), second.clone()], 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeded the 1-entry scan limit")
        );
        fs::remove_dir_all(first).unwrap();
        fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn discovery_is_repeatable_for_snapshot_comparison() {
        let root = fixture();
        write_app(&root.join("Alpha.app"), "Alpha", Some("dev.example.alpha"));
        write_app(
            &root.join("Group/Beta.app"),
            "Beta",
            Some("dev.example.beta"),
        );
        let first = discover_in(std::slice::from_ref(&root)).unwrap();
        let second = discover_in(std::slice::from_ref(&root)).unwrap();
        assert_eq!(first, second);
        fs::remove_dir_all(root).unwrap();
    }
}
