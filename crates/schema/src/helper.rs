use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Whether the privileged helper is reachable, as seen by sysmand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Helper {
    /// `running`, `stopped` (no socket: not enabled or not installed) or `error`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `rapl`, `proc_io`, `smart`, `net` → whether that source works on this machine.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
