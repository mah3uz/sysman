use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Net {
    /// Keyed by interface name.
    pub interfaces: BTreeMap<String, Interface>,
    pub sockets: Sockets,
    pub tcp: Tcp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub state: Option<String>,
    pub link_mbps: Option<u64>,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub rx_pps: f64,
    pub tx_pps: f64,
    pub rx_bytes_total: u64,
    pub tx_bytes_total: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_quality: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_dbm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sockets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_in_use: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_in_use: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tcp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_in_segments_ps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_out_segments_ps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_retransmits_ps: Option<f64>,
    pub established: Option<i64>,
}
