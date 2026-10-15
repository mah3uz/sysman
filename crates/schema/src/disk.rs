use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Device (kernel name; device-mapper devices by their `dm/name`) → I/O over the last interval.
pub type Disk = BTreeMap<String, Device>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub read_bps: u64,
    pub write_bps: u64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub read_await_ms: f64,
    pub write_await_ms: f64,
    pub busy_pct: f64,
    /// Average number of requests in the device queue.
    pub queue_depth: f64,
    pub in_flight: u64,
    pub read_bytes_total: u64,
    pub write_bytes_total: u64,
}
