use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

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
    pub build: fn(toml::Table, Sys) -> Result<Box<dyn Collector>>,
}

pub const MODULES: &[Spec] = &[
    Spec {
        name: "cgroups",
        default_interval: 10,
        snapshot: true,
        build: |o, sys| Ok(Box::new(collectors::cgroups::Cgroups::new(options(o)?, sys)?)),
    },
    Spec {
        name: "cpu",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::cpu::Cpu::new(options(o)?, sys)?)),
    },
    Spec {
        name: "disk",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::disk::Disk::new(options(o)?, sys)?)),
    },
    Spec {
        name: "fs",
        default_interval: 30,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::fs::Fs::new(options(o)?, sys)?)),
    },
    Spec {
        name: "gpu",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::gpu::Gpu::new(options(o)?, sys)?)),
    },
    Spec {
        name: "hwmon",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::hwmon::Hwmon::new(options(o)?, sys)?)),
    },
    Spec {
        name: "memory",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::memory::Memory::new(options(o)?, sys)?)),
    },
    Spec {
        name: "net",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::net::Net::new(options(o)?, sys)?)),
    },
    Spec {
        name: "netinfo",
        default_interval: 60,
        snapshot: true,
        build: |o, sys| Ok(Box::new(collectors::netinfo::Netinfo::new(options(o)?, sys)?)),
    },
    Spec {
        name: "power",
        default_interval: 60,
        snapshot: true,
        build: |o, sys| Ok(Box::new(collectors::power::Power::new(options(o)?, sys)?)),
    },
    Spec {
        name: "pressure",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::pressure::Pressure::new(options(o)?, sys)?)),
    },
    Spec {
        name: "processes",
        default_interval: 10,
        snapshot: true,
        build: |o, sys| Ok(Box::new(collectors::processes::Processes::new(options(o)?, sys)?)),
    },
    Spec {
        name: "storage",
        default_interval: 60,
        snapshot: true,
        build: |o, sys| Ok(Box::new(collectors::storage::Storage::new(options(o)?, sys)?)),
    },
    Spec {
        name: "system",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| Ok(Box::new(collectors::system::System::new(options(o)?, sys)?)),
    },
];

/// Parse a module's own options, rejecting keys it does not know.
pub fn options<T: DeserializeOwned>(table: toml::Table) -> Result<T> {
    toml::Value::Table(table).try_into().context("invalid module options")
}

/// For modules without options; any key is a typo.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOptions {}
