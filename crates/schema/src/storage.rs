use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    /// Keyed by block device name.
    pub devices: BTreeMap<String, Device>,
    /// Keyed by filesystem uuid.
    pub btrfs: BTreeMap<String, Btrfs>,
    /// Keyed by block device name; present only while the privileged helper runs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub smart: BTreeMap<String, Smart>,
}

/// Drive health. NVMe fields map to the SMART / Health Information log; SATA to the ATA attributes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Smart {
    /// The drive's own overall verdict.
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    /// Vendor estimate of life used; may exceed 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_spare_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_on_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_cycles: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsafe_shutdowns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_errors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_read_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_written_bytes: Option<u64>,
    /// Raw critical-warning bits (NVMe) or failing-attribute names (SATA); empty when healthy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
