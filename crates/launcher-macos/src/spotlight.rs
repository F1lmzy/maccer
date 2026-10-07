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

/// A broad subsequence prefilter; final relevance is decided by fzf/nucleo.
fn fuzzy_filename_query(text: &str) -> Result<String> {
    filename_query(text)?; // Shared length/control-character validation.
    let mut terms = Vec::new();
    for word in text.split_whitespace() {
        let mut pattern = String::from("*");
        for ch in word.chars() {
            if matches!(ch, '\\' | '"' | '*' | '?') {
                pattern.push('\\');
            }
            pattern.push(ch);
            pattern.push('*');
        }
        terms.push(format!("(kMDItemFSName == \"{pattern}\"cd)"));
    }
    if terms.len() > 16 {
        bail!("file query has too many words");
    }
    Ok(terms.join(" && "))
}

/// Optional metadata used for ranking. All timestamps are Unix seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileMetadata {
    pub last_used: Option<i64>,
    pub use_count: u64,
    pub modified: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct DirectoryUsage {
    pub path: PathBuf,
    pub score: f64,
}

pub trait FileSearch: Send + Sync {
    /// Indexed backends own their filtering and support full-path/empty queries.
    fn is_indexed(&self) -> bool {
        false
    }
    fn refresh_index(&self, _token: &CancellationToken) -> Result<()> {
        bail!("this file backend does not support reindexing")
    }
    fn revision(&self) -> u64 {
        0
    }
    fn status(&self) -> Option<String> {
        None
    }
    fn preview_revision(&self, _path: &Path) -> u64 {
        self.revision()
    }
    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn metadata(&self, _path: &Path) -> FileMetadata {
        FileMetadata::default()
    }
    fn preferred_directories(&self, _token: &CancellationToken) -> Vec<DirectoryUsage> {
        vec![]
    }
    /// None means unavailable; an empty vector means the filter found no matches.
    fn fuzzy_matches(
        &self,
        _text: &str,
        _paths: &[PathBuf],
        _token: &CancellationToken,
    ) -> Option<Vec<PathBuf>> {
        None
    }

    fn search(
        &self,
        text: &str,
        root: Option<&Path>,
        token: &CancellationToken,
        limit: usize,
    ) -> Result<Vec<PathBuf>>;
}

pub struct Spotlight {
    pub use_zoxide: bool,
    pub use_fzf: bool,
}
impl Default for Spotlight {
    fn default() -> Self {
        Self {
            use_zoxide: true,
            use_fzf: true,
        }
    }
}
impl FileSearch for Spotlight {
    fn metadata(&self, path: &Path) -> FileMetadata {
        crate::file_metadata::metadata(path)
    }
    fn preferred_directories(&self, token: &CancellationToken) -> Vec<DirectoryUsage> {
        if self.use_zoxide {
            crate::file_tools::preferred_directories(token)
        } else {
            vec![]
        }
    }
    fn fuzzy_matches(
        &self,
        text: &str,
        paths: &[PathBuf],
        token: &CancellationToken,
    ) -> Option<Vec<PathBuf>> {
        if self.use_fzf {
            crate::file_tools::fuzzy_matches(text, paths, token)
                .ok()
                .flatten()
        } else {
            None
        }
    }
    fn search(
        &self,
        text: &str,
        root: Option<&Path>,
        token: &CancellationToken,
        limit: usize,
    ) -> Result<Vec<PathBuf>> {
        let predicate = fuzzy_filename_query(text)?;
        let mut command = Command::new("/usr/bin/mdfind");
        command.arg("-0");
        if let Some(root) = root {
            if !root.is_absolute() || !root.is_dir() {
                bail!("Spotlight scope must be an existing absolute directory");
            }
            command.arg("-onlyin").arg(root);
        }
        command.arg(predicate);
        let output = run_bounded(&mut command, token, Duration::from_secs(3), 2 * 1024 * 1024)?;
        if !output.status.success() {
            bail!("Spotlight search failed ({})", output.status);
        }
        Ok(output
            .stdout
            .split_inclusive(|b| *b == 0)
            .filter(|bytes| bytes.last() == Some(&0))
            .filter_map(|bytes| std::str::from_utf8(&bytes[..bytes.len() - 1]).ok())
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
    fn fuzzy_discovery_allows_missing_letters_and_independent_words() {
        assert_eq!(
            fuzzy_filename_query("rpt pdf").unwrap(),
            "(kMDItemFSName == \"*r*p*t*\"cd) && (kMDItemFSName == \"*p*d*f*\"cd)"
        );
        assert!(fuzzy_filename_query("a\"*").unwrap().contains("\\\"*\\**"));
    }

    #[test]
    fn rejects_empty_control_and_oversized_queries() {
        for query in ["".to_string(), "\n".into(), "\0".into(), "x".repeat(1025)] {
            assert!(filename_query(&query).is_err());
        }
    }
}
