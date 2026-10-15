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

pub const MODULES: &[Spec] = &[Spec {
    name: "pressure",
    default_interval: 2,
    snapshot: false,
    build: |o, sys| Ok(Box::new(collectors::pressure::Pressure::new(options(o)?, sys)?)),
}];

/// Parse a module's own options, rejecting keys it does not know.
pub fn options<T: DeserializeOwned>(table: toml::Table) -> Result<T> {
    toml::Value::Table(table).try_into().context("invalid module options")
}

/// For modules without options; any key is a typo.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOptions {}
