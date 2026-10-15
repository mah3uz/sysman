use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::helper::{self, HelperClient};

/// Access to `/proc` and `/sys` under a root that tests point at a fixture tree.
#[derive(Debug, Clone)]
pub struct Sys {
    root: PathBuf,
    /// Shared by every clone, so the modules of one tick share one helper reading.
    helper: Arc<HelperClient>,
}

impl Sys {
    pub fn host() -> Self {
        Self::at("/")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let helper = Arc::new(HelperClient::new(root.join(helper::SOCKET.trim_start_matches('/'))));
        Self { root, helper }
    }

    pub fn helper(&self) -> &HelperClient {
        &self.helper
    }

    pub fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }

    /// `path` resolved under the root, written into a reusable buffer.
    pub fn path_into(&self, buf: &mut PathBuf, path: &str) {
        buf.clear();
        buf.push(&self.root);
        buf.push(path.strip_prefix('/').unwrap_or(path));
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

    /// Names of the subdirectories of a directory, sorted; the entry type comes from the directory read, with no stat.
    pub fn subdirs(&self, path: impl AsRef<Path>) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(path))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    /// Each `/sys/class/hwmon` entry with its device link target; it changes when hwmon numbers are reassigned.
    pub fn hwmon_signature(&self) -> Vec<(String, Option<PathBuf>)> {
        self.list("/sys/class/hwmon")
            .into_iter()
            .map(|h| {
                let link = std::fs::read_link(self.path(format!("/sys/class/hwmon/{h}/device"))).ok();
                (h, link)
            })
            .collect()
    }
}
