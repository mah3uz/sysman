use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
    /// `total - available`, less the ZFS ARC when present.
    pub used_bytes: u64,
    pub used_pct: f64,
    pub swap_used_bytes: u64,
    pub oom_kills_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffers_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_cached_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slab_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slab_reclaimable_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_stack_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_tables_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inactive_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zfs_arc_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_faults_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major_faults_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_in_pages_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_out_pages_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_in_kb_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_out_kb_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub zram: BTreeMap<String, Zram>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zram {
    pub orig_bytes: u64,
    pub compressed_bytes: u64,
    pub mem_used_bytes: u64,
}
