use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Keyed by sysfs supply name (`BAT0`, `AC`, `hidpp_battery_0`).
pub type Power = BTreeMap<String, Supply>;

/// Each field is present only if the device exposes it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Supply {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_level: Option<String>,
    /// `Device` marks a peripheral battery (wireless mouse), not the system battery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_wh: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_full_wh: Option<f64>,
    /// Health is full over design; a pack that reports charge rather than energy fills the `charge_*` pair instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_full_design_wh: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_full_ah: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_full_design_ah: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voltage_v: Option<f64>,
}
