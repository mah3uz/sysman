use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    /// Keyed by block device name.
    pub devices: BTreeMap<String, Device>,
    /// Keyed by filesystem uuid.
    pub btrfs: BTreeMap<String, Btrfs>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Controller state (`live` for a healthy NVMe controller); `null` when the device has none.
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Btrfs {
    pub label: Option<String>,
    /// Keyed by btrfs devid.
    pub device_errors: BTreeMap<String, DeviceErrors>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceErrors {
    /// 1 when the device is absent from the filesystem (degraded), 0 when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<u8>,
    /// Cumulative `error_stats` counters (`write_errs`, `read_errs`, `flush_errs`, `corruption_errs`, `generation_errs`).
    #[serde(flatten)]
    pub errors: BTreeMap<String, u64>,
}

/// Drive inventory in the meta record, keyed by block device name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageMeta {
    pub devices: BTreeMap<String, Drive>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drive {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub size_bytes: Option<u64>,
    pub rotational: bool,
}
