use anyhow::{Context, Result};
use plist::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// Metadata used by the application provider. Icon loading remains the UI's concern.
#[derive(Clone, Debug)]
pub struct Application {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub display_name: String,
    pub executable_name: Option<String>,
    pub icon_path: Option<PathBuf>,
}

/// Scan standard macOS application locations. A missing root does not fail discovery.
pub fn discover_applications() -> Result<Vec<Application>> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        // Finder is outside the application directories. Include its bundle
        // explicitly rather than exposing internal CoreServices helper apps.
        PathBuf::from("/System/Library/CoreServices/Finder.app"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    discover_in(&roots)
}

/// Scan directories or explicit app bundles without descending into bundle contents.
pub fn discover_in(roots: &[PathBuf]) -> Result<Vec<Application>> {
    let mut found = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        if root
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            if let Some(app) = read_application(root) {
                found.push(app);
            }
            continue;
        }
        let mut remaining_entries = 30_000;
        scan_directory(root, 0, &mut remaining_entries, &mut found);
    }
    found.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
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
    Ok(found)
}

fn scan_directory(dir: &Path, depth: usize, remaining: &mut usize, found: &mut Vec<Application>) {
    if depth >= 12 || *remaining == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        *remaining -= 1;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        {
            if let Some(app) = read_application(&path) {
                found.push(app);
            }
        } else if path.is_dir() {
            scan_directory(&path, depth + 1, remaining, found);
        }
    }
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
    let display_name = ["CFBundleDisplayName", "CFBundleName"]
        .iter()
        .find_map(|key| dict.get(key).and_then(Value::as_string))
        .map(str::to_owned)
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))?;
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
        executable_name,
        icon_path,
    })
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
}
