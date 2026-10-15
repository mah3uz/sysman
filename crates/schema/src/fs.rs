use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Shortest mount point of each filesystem → usage.
pub type Fs = BTreeMap<String, Filesystem>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filesystem {
    pub device: String,
    pub fstype: String,
    /// Every mount point of this filesystem (subvolumes, bind mounts), shortest first.
    pub mounts: Vec<String>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    /// `used / (used + avail)`, as `df` reports it; `null` for a filesystem with no blocks.
    pub used_pct: Option<f64>,
    pub inodes_total: u64,
    pub inodes_free: u64,
}
