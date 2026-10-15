use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Netinfo {
    /// Keyed by interface name.
    pub interfaces: BTreeMap<String, Interface>,
    pub default_routes: Vec<Route>,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub state: Option<String>,
    pub mac: Option<String>,
    pub mtu: Option<u64>,
    /// CIDR notation.
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub gateway: Option<String>,
    pub interface: Option<String>,
    pub metric: Option<u64>,
}
