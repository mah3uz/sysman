use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `read_bps`, `write_bps`, `user`, `fds` and `exe` are absent for processes the daemon's user may not inspect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub state: String,
    pub threads: u64,
    pub rss_bytes: u64,
    pub cpu_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fds: Option<u64>,
    /// Bytes per second over TCP and UDP, from the privileged helper's eBPF counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bps: Option<u64>,
    pub cmdline: String,
    /// Absolute Unix seconds; with `pid` it identifies the process instance.
    #[serde(default)]
    pub start_time: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i64>,
    /// The kernel's scheduling priority: 0–39 for normal tasks, negative for real-time ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vsize_bytes: Option<u64>,
    /// User plus system CPU time since the process started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_s: Option<f64>,
    /// Target of `/proc/<pid>/exe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// Processes sharing a `comm`, summed. Shared memory is counted once per process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub count: u64,
    pub cpu_pct: f64,
    pub rss_bytes: u64,
}

/// Processes first seen on this scan have no rate yet, so they are counted but have no row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub count: u64,
    pub threads: u64,
    /// State letter → number of processes.
    pub states: BTreeMap<String, u64>,
    pub rows: Vec<Process>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Processes {
    pub count: u64,
    pub threads: u64,
    pub states: BTreeMap<String, u64>,
    pub top_cpu: Vec<Process>,
    pub top_memory: Vec<Process>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_io: Option<Vec<Process>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_cpu: Option<Vec<Group>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_memory: Option<Vec<Group>>,
    /// Every process with a rate, for history; the top lists are views of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table: Vec<Process>,
}
