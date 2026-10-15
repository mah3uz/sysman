use std::borrow::Cow;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::collectors;
use crate::sys::Sys;

pub trait Collector: Send {
    /// Static information for the meta record: units, inventory, limits.
    fn meta(&mut self) -> Option<Value> {
        None
    }

    /// `None` contributes nothing this tick (for example, the first call of a rate).
    fn sample(&mut self) -> Result<Option<Value>>;
}

pub struct Spec {
    pub name: &'static str,
    pub default_interval: u64,
    /// Store the full module value in the database's snapshots table.
    pub snapshot: bool,
    /// Top-level keys of the module value that are kept out of the JSONL crash log.
    pub unlogged: &'static [&'static str],
    pub build: fn(toml::Table, Sys) -> Result<Box<dyn Collector>>,
}

pub const MODULES: &[Spec] = &[
    Spec {
        name: "cgroups",
        default_interval: 10,
        snapshot: true,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::cgroups::Cgroups::new(options(o)?, sys)?)),
    },
    Spec {
        name: "cpu",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::cpu::Cpu::new(options(o)?, sys)?)),
    },
    Spec {
        name: "disk",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::disk::Disk::new(options(o)?, sys)?)),
    },
    Spec {
        name: "fs",
        default_interval: 30,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::fs::Fs::new(options(o)?, sys)?)),
    },
    Spec {
        name: "gpu",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::gpu::Gpu::new(options(o)?, sys)?)),
    },
    Spec {
        name: "hwmon",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::hwmon::Hwmon::new(options(o)?, sys)?)),
    },
    Spec {
        name: "memory",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::memory::Memory::new(options(o)?, sys)?)),
    },
    Spec {
        name: "net",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::net::Net::new(options(o)?, sys)?)),
    },
    Spec {
        name: "netinfo",
        default_interval: 60,
        snapshot: true,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::netinfo::Netinfo::new(options(o)?, sys)?)),
    },
    Spec {
        name: "power",
        default_interval: 60,
        snapshot: true,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::power::Power::new(options(o)?, sys)?)),
    },
    Spec {
        name: "pressure",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::pressure::Pressure::new(options(o)?, sys)?)),
    },
    Spec {
        name: "processes",
        default_interval: 10,
        snapshot: true,
        unlogged: &["table"],
        build: |o, sys| Ok(Box::new(collectors::processes::Processes::new(options(o)?, sys)?)),
    },
    Spec {
        name: "storage",
        default_interval: 60,
        snapshot: true,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::storage::Storage::new(options(o)?, sys)?)),
    },
    Spec {
        name: "system",
        default_interval: 2,
        snapshot: false,
        unlogged: &[],
        build: |o, sys| Ok(Box::new(collectors::system::System::new(options(o)?, sys)?)),
    },
];

/// The record as the JSONL log should see it: borrowed unless a module in it has `unlogged` keys.
pub fn loggable<'a>(record: &'a Value, specs: &[Spec]) -> Cow<'a, Value> {
    let affected: Vec<&Spec> = specs
        .iter()
        .filter(|s| s.unlogged.iter().any(|k| record[s.name].get(k).is_some()))
        .collect();
    let Some(fields) = record.as_object().filter(|_| !affected.is_empty()) else {
        return Cow::Borrowed(record);
    };
    // rebuilt field by field so the unlogged values are never cloned
    let mut out = Map::new();
    for (name, value) in fields {
        match (affected.iter().find(|s| s.name == name), value.as_object()) {
            (Some(spec), Some(module)) => {
                let kept = module.iter().filter(|(k, _)| !spec.unlogged.contains(&k.as_str()));
                out.insert(
                    name.clone(),
                    Value::Object(kept.map(|(k, v)| (k.clone(), v.clone())).collect()),
                );
            }
            _ => {
                out.insert(name.clone(), value.clone());
            }
        }
    }
    Cow::Owned(Value::Object(out))
}

/// Parse a module's own options, rejecting keys it does not know.
pub fn options<T: DeserializeOwned>(table: toml::Table) -> Result<T> {
    toml::Value::Table(table).try_into().context("invalid module options")
}

/// For modules without options; any key is a typo.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOptions {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unlogged_keys_are_dropped_from_the_log_copy_only() {
        // the table is for the database and the app; in the crash log it would cost about 1 GB a day
        let record = json!({"t": 1, "processes": {"count": 2, "table": [{"pid": 1}]}, "cpu": {"table": 1}});
        let logged = loggable(&record, MODULES);
        assert_eq!(logged["processes"], json!({"count": 2}));
        assert_eq!(logged["cpu"], json!({"table": 1}), "only declared modules lose keys");
        assert!(record["processes"].get("table").is_some(), "the original keeps it");
    }

    #[test]
    fn record_without_unlogged_keys_is_not_copied() {
        let record = json!({"t": 1, "processes": {"count": 2}});
        assert!(matches!(loggable(&record, MODULES), Cow::Borrowed(_)));
    }
}
