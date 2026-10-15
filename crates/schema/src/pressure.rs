use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `resource` (cpu, memory, io, irq) → `some` | `full` → stall averages.
pub type Pressure = BTreeMap<String, BTreeMap<String, Stall>>;

/// Percent of time tasks were stalled, averaged over 10 s and 60 s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stall {
    pub avg10: f64,
    pub avg60: f64,
}
