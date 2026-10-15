use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `chip` (`name/device`) → kind (`temp` °C, `fan` rpm, `in` V, `power` W, `curr` A) → label → value;
/// `null` when the sensor could not be read.
pub type Hwmon = BTreeMap<String, BTreeMap<String, BTreeMap<String, Option<f64>>>>;

/// chip → kind → label → `min` | `max` | `crit` | `lcrit` → value
pub type Limits = BTreeMap<String, BTreeMap<String, BTreeMap<String, BTreeMap<String, f64>>>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HwmonMeta {
    pub units: BTreeMap<String, String>,
    pub note: String,
    pub limits: Limits,
}
