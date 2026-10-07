//! Optional non-interactive helpers. Candidate paths are data, never commands.
use crate::{
    process::{run_bounded, run_bounded_with_input},
    spotlight::DirectoryUsage,
};
use anyhow::{Result, bail};
use launcher_core::CancellationToken;
use std::{
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    paths
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|p| p.join(name))
        .find(|p| {
            p.is_file() && {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    p.metadata()
                        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
        })
}

fn parse_directories(bytes: &[u8]) -> Vec<DirectoryUsage> {
    let mut dirs: Vec<_> = String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let (score, path) = line.trim_start().split_once(char::is_whitespace)?;
            let score: f64 = score.parse().ok()?;
            let path = PathBuf::from(path.trim_start());
            (score.is_finite() && score > 0.0 && path.is_absolute())
                .then_some(DirectoryUsage { path, score })
        })
        .take(2048)
        .collect();
    dirs.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    dirs
}

type DirectoryCache = Option<(Instant, Vec<DirectoryUsage>)>;
static DIRECTORIES: OnceLock<Mutex<DirectoryCache>> = OnceLock::new();

pub fn preferred_directories(token: &CancellationToken) -> Vec<DirectoryUsage> {
    let Some(executable) = find_executable("zoxide") else {
        return vec![];
    };
    let Ok(mut cache) = DIRECTORIES.get_or_init(|| Mutex::new(None)).try_lock() else {
        return vec![];
    };
    if let Some((updated, dirs)) = &*cache
        && updated.elapsed() < Duration::from_secs(30)
    {
        return dirs.clone();
    }
    let output = run_bounded(
        Command::new(executable).args(["query", "--list", "--score"]),
        token,
        Duration::from_secs(1),
        512 * 1024,
    );
    let dirs = output
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_directories(&o.stdout))
        .unwrap_or_default();
    if !token.is_cancelled() {
        *cache = Some((Instant::now(), dirs.clone()));
    }
    dirs
}

pub fn fuzzy_matches(
    text: &str,
    paths: &[PathBuf],
    token: &CancellationToken,
) -> Result<Option<Vec<PathBuf>>> {
    let Some(executable) = find_executable("fzf") else {
        return Ok(None);
    };
    let words: Vec<_> = text.split_whitespace().collect();
    if words.len() > 16 {
        bail!("file query has too many words");
    }
    let mut matches = paths.to_vec();
    // Each word is an ordinary fuzzy term. Disable fzf's operators and shell
    // customizations so a filename containing '!' or '$' stays literal.
    for word in words {
        if matches.is_empty() {
            break;
        }
        let mut input = Vec::new();
        for path in &matches {
            input.extend_from_slice(path.to_string_lossy().as_bytes());
            input.push(0);
        }
        let mut command = Command::new(&executable);
        command
            .args([
                "--read0",
                "--print0",
                "--no-extended",
                "--ignore-case",
                "--scheme=path",
                "--delimiter=/",
                "--nth=-1",
                "--tiebreak=index",
            ])
            .arg(format!("--filter={word}"))
            .env_remove("FZF_DEFAULT_OPTS")
            .env_remove("FZF_DEFAULT_OPTS_FILE")
            .env_remove("FZF_DEFAULT_COMMAND");
        let output = run_bounded_with_input(
            &mut command,
            token,
            Duration::from_secs(1),
            input.len().saturating_add(1),
            input,
        )?;
        if output.status.code() == Some(1) {
            return Ok(Some(vec![]));
        }
        if !output.status.success() {
            bail!("fzf filter failed ({})", output.status);
        }
        let allowed: std::collections::HashSet<_> = matches.iter().collect();
        matches = output
            .stdout
            .split_inclusive(|b| *b == 0)
            .filter(|bytes| bytes.last() == Some(&0))
            .filter_map(|bytes| std::str::from_utf8(&bytes[..bytes.len() - 1]).ok())
            .map(PathBuf::from)
            .filter(|path| allowed.contains(path))
            .collect();
    }
    Ok(Some(matches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn zoxide_scores_preserve_spaces_and_ignore_malformed_rows() {
        let dirs = parse_directories(b"  900.5 /Users/test/My Project\nNaN /tmp\n-1 /tmp\n5 relative/path\n3 /Users/test/other\n");
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].path, Path::new("/Users/test/My Project"));
        assert_eq!(dirs[0].score, 900.5);
    }
    #[test]
    fn real_fzf_filters_fuzzy_filenames_with_nul_safe_paths() {
        if find_executable("fzf").is_none() {
            return;
        }
        let paths = vec![
            PathBuf::from("/tmp/unrelated.txt"),
            PathBuf::from("/tmp/with spaces/monthly-report.pdf"),
            PathBuf::from("/tmp/new\nline/monthly-report-🦀.pdf"),
        ];
        let matches = fuzzy_matches("mntrpt", &paths, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(matches.len(), 2);
        assert!(!matches.contains(&paths[0]));
        assert!(matches.contains(&paths[2]));
        assert!(
            fuzzy_matches("doesnotexist", &paths, &CancellationToken::default())
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }
}
