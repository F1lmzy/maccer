//! Spotlight filename queries. No recursive crawling and no shell interpolation.
use crate::process::run_bounded;
use anyhow::{Result, bail};
use launcher_core::CancellationToken;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub fn filename_query(text: &str) -> Result<String> {
    if text.trim().is_empty() || text.len() > 1024 || text.chars().any(char::is_control) {
        bail!("file search requires 1–1024 bytes without control characters");
    }
    let mut escaped = String::new();
    for ch in text.chars() {
        if matches!(ch, '\\' | '"' | '*' | '?') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    Ok(format!("kMDItemFSName == \"*{escaped}*\"cd"))
}

pub trait FileSearch: Send + Sync {
    fn search(
        &self,
        text: &str,
        root: Option<&Path>,
        token: &CancellationToken,
        limit: usize,
    ) -> Result<Vec<PathBuf>>;
}

pub struct Spotlight;
impl FileSearch for Spotlight {
    fn search(
        &self,
        text: &str,
        root: Option<&Path>,
        token: &CancellationToken,
        limit: usize,
    ) -> Result<Vec<PathBuf>> {
        let predicate = filename_query(text)?;
        let mut command = Command::new("/usr/bin/mdfind");
        command.arg("-0");
        if let Some(root) = root {
            if !root.is_absolute() || !root.is_dir() {
                bail!("Spotlight scope must be an existing absolute directory");
            }
            command.arg("-onlyin").arg(root);
        }
        command.arg(predicate);
        let output = run_bounded(&mut command, token, Duration::from_secs(3), 512 * 1024)?;
        if !output.status.success() {
            bail!("Spotlight search failed ({})", output.status);
        }
        Ok(output
            .stdout
            .split(|b| *b == 0)
            .filter_map(|bytes| std::str::from_utf8(bytes).ok())
            .filter(|text| !text.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .take(limit)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_escapes_metacharacters_instead_of_interpreting_predicates() {
        assert_eq!(
            filename_query("report").unwrap(),
            "kMDItemFSName == \"*report*\"cd"
        );
        assert_eq!(
            filename_query("a\"b*?\\").unwrap(),
            "kMDItemFSName == \"*a\\\"b\\*\\?\\\\*\"cd"
        );
    }
    #[test]
    fn rejects_empty_control_and_oversized_queries() {
        for query in ["".to_string(), "\n".into(), "\0".into(), "x".repeat(1025)] {
            assert!(filename_query(&query).is_err());
        }
    }
}
