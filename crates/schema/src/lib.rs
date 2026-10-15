use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod cgroups;
pub mod cpu;
pub mod db;
pub mod disk;
pub mod fs;
pub mod gpu;
pub mod helper;
pub mod hwmon;
pub mod memory;
pub mod net;
pub mod netinfo;
pub mod power;
pub mod pressure;
pub mod processes;
pub mod series;
pub mod storage;
pub mod system;

pub const SCHEMA: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Record {
    Meta(Meta),
    Sample(Box<Sample>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema: u32,
    pub t: f64,
    pub time: String,
    pub config: Value,
    pub modules: BTreeMap<String, ModuleMeta>,
    pub failed_modules: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleMeta {
    pub interval_s: u64,
    pub info: Option<Value>,
}

/// Every module key is optional: a sample carries only the modules due on that tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub schema: u32,
    pub t: f64,
    pub time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroups: Option<cgroups::Cgroups>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<cpu::Cpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<disk::Disk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<fs::Fs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<gpu::Gpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<helper::Helper>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwmon: Option<hwmon::Hwmon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<memory::Memory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<net::Net>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netinfo: Option<netinfo::Netinfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<power::Power>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<pressure::Pressure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<processes::Processes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<storage::Storage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<system::System>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, String>,
}
