use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    pub total_pct: f64,
    pub breakdown_pct: Breakdown,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_pct: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_mhz: Option<u64>,
    /// Per logical CPU; null where the frequency is unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_mhz: Option<Vec<Option<u64>>>,
    /// Package temperature from k10temp, zenpower or coretemp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    /// Label → °C for the chip's other sensors (CCDs, cores, further packages).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub temps_c: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
}

/// Share of all CPU time; `user` and `nice` exclude guest time, which is `guest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub iowait: f64,
    pub irq: f64,
    pub softirq: f64,
    pub steal: f64,
    pub guest: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuMeta {
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub driver: Option<String>,
    pub governor: Option<String>,
    pub energy_performance_preference: Option<String>,
    pub min_mhz: Option<u64>,
    pub max_mhz: Option<u64>,
    /// hwmon chip (`name/device`) the temperatures come from.
    pub temp_source: Option<String>,
    pub power_source: Option<String>,
    pub topology: Topology,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<u32>,
    /// Physical cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    pub threads: u32,
    /// CPUs sharing an L3 cache; on Zen one group per CCD.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub l3_groups: Vec<L3Group>,
    /// Only on hybrid CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_types: Option<CoreTypes>,
    /// amd-pstate preferred-core ranking by logical CPU number (higher is preferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefcore_ranking: Option<Vec<Option<u64>>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L3Group {
    pub cpus: Vec<u32>,
    pub l3_kib: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreTypes {
    pub performance: Vec<u32>,
    pub efficiency: Vec<u32>,
}
