use serde::{Deserialize, Serialize};

/// A leaf cgroup. `name` is the path under `/sys/fs/cgroup`; `cpu_pct` is percent of one core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cgroup {
    pub name: String,
    pub cpu_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cgroups {
    pub count: u64,
    pub top_cpu: Vec<Cgroup>,
    pub top_memory: Vec<Cgroup>,
}
