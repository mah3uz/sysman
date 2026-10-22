use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Whether the privileged helper is reachable, as seen by sysmand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Helper {
    /// `running`, `stopped` (no socket: not enabled or not installed) or `error`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `rapl`, `proc_io`, `smart`, `net` → whether that source works on this machine. `proc_io` also covers
    /// `sockets`, which reads the same `/proc/<pid>/fd` under the same permission.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A request to `sysman-helper`, one JSON object per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    Read,
    Sockets,
}

/// The helper's answer to `status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    /// Same keys as [`Helper::features`].
    pub features: BTreeMap<String, bool>,
    /// Feature → why it is off or partial.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, String>,
}

/// The helper's answer to `read`: cumulative counters; rates are computed by the reader.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// Keyed by powercap zone directory (`intel-rapl:0`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rapl: BTreeMap<String, RaplZone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcCounters>,
    /// Keyed by block device name, as in `storage.smart`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub smart: BTreeMap<String, crate::storage::Smart>,
}

/// The helper's answer to `sockets`: socket inodes and nothing else, so no other open file is revealed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sockets {
    /// Only processes holding at least one socket.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcSockets>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcSockets {
    pub pid: u32,
    /// As in [`ProcCounters::start_ticks`].
    pub start_ticks: u64,
    /// The `N` of each `socket:[N]` link in `/proc/<pid>/fd`, sorted, each once.
    pub inodes: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaplZone {
    /// The zone's own name (`package-0`, `core`).
    pub name: String,
    pub energy_uj: u64,
    /// `energy_uj` wraps to zero after this value.
    pub max_energy_range_uj: u64,
}

/// Cumulative counters of one process instance; a field is absent when its source is off or unreadable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcCounters {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat`: clock ticks after boot. With `pid` it identifies the instance.
    pub start_ticks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fds: Option<u64>,
    /// TCP and UDP bytes, loopback excluded, since the helper started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bytes: Option<u64>,
}
