use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Access to `/proc` and `/sys` under a root that tests point at a fixture tree.
#[derive(Debug, Clone)]
pub struct Sys {
    root: PathBuf,
}

impl Sys {
    pub fn host() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }

    /// The trimmed contents of a file that must exist.
    pub fn read(&self, path: impl AsRef<Path>) -> Result<String> {
        let full = self.path(path);
        let text = std::fs::read_to_string(&full).with_context(|| format!("reading {}", full.display()))?;
        Ok(text.trim().to_string())
    }

    /// The trimmed contents of a file that may be missing or unreadable.
    pub fn read_opt(&self, path: impl AsRef<Path>) -> Option<String> {
        std::fs::read_to_string(self.path(path))
            .ok()
            .map(|s| s.trim().to_string())
    }

    pub fn int_opt(&self, path: impl AsRef<Path>) -> Option<i64> {
        self.read_opt(path)?.parse().ok()
    }

    /// Sorted entry names of a directory; empty if it cannot be read.
    pub fn list(&self, path: impl AsRef<Path>) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(path))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }
}
