use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// Keyed by PCI slot, e.g. `0000:01:00.0`.
    pub devices: BTreeMap<String, Device>,
    /// Ranked by utilisation, then memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<Process>,
    /// PCI slot → why that device's backend gave no data this tick.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backend_errors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// `nvidia`, `amd`, `intel`, or the PCI vendor id for others.
    pub vendor: String,
    /// Kernel driver: `nvidia`, `amdgpu`, `i915`, `xe`, …
    pub driver: String,
    /// `nvml`, `nvidia-smi` or `sysfs`.
    pub backend: String,
    /// PCI power state, `D0` … `D3cold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<f64>,
    /// Memory controller busy, not capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_busy_pct: Option<f64>,
    /// Shared encode/decode engine (AMD VCN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_busy_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_total_bytes: Option<u64>,
    /// System RAM mapped for the GPU; a limit, not a reservation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gtt_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gtt_total_bytes: Option<u64>,
    /// Main sensor: NVML GPU, amdgpu `edge`, xe `pkg`, i915 `temp1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    /// Other sensors by label (`junction`, `mem`, `vram`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub temps_c: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_rpm: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_mhz: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_clock_mhz: Option<u64>,
    /// NVIDIA performance state, `P0` … `P15`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pstate: Option<String>,
    /// Active reasons in snake_case; empty when known to be none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_reasons: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcie_gen: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcie_width: Option<u32>,
    /// Intel only: share of time the GT spent in RC6 / gt-c6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Process {
    /// PCI slot of the device.
    pub pci: String,
    pub pid: u32,
    pub comm: String,
    /// Busiest engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub devices: BTreeMap<String, DeviceInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// PCI ids as in sysfs, e.g. `0x10de`.
    pub vendor_id: String,
    pub device_id: String,
    /// `nvml`, `gpu_busy_percent`, `fdinfo: own processes only` or `nvidia-smi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_min_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_max_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcie_max_gen: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcie_max_width: Option<u32>,
}
