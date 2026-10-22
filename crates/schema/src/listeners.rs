use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// The `/proc/net` table a socket was listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Tcp6,
    Udp,
    Udp6,
}

/// A listening TCP socket or a bound, unconnected UDP socket, from the daemon's `listeners` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listener {
    pub protocol: Protocol,
    /// The bound address; a v4-mapped IPv6 address (`::ffff:a.b.c.d`) is given as IPv4.
    pub address: IpAddr,
    pub port: u16,
    /// Who created the socket, known even when no holder is.
    pub uid: u32,
    pub inode: u64,
    /// Every process instance holding the socket, by pid; empty when none could be read. Several when it was
    /// inherited or passed on: worker processes, or PID 1 or a user `systemd` for a socket unit, alongside its service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holders: Vec<Holder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// As in `processes::Process::start_time`; with `pid` it identifies the instance.
    pub start_time: u64,
    pub comm: String,
}
