use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct System {
    /// 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    pub tasks_total: u64,
    pub procs_running: u64,
    pub procs_blocked: u64,
    pub uptime_s: u64,
    pub boot_time: u64,
    pub open_files: u64,
    pub logical_cpus: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_switches_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupts_ps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forks_ps: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMeta {
    pub host: Option<String>,
    pub kernel: Option<String>,
    pub arch: String,
    pub distro: Option<String>,
    pub boot_id: Option<String>,
    /// DMI field name → value; null where the field is unreadable.
    pub dmi: BTreeMap<String, Option<String>>,
}
