use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The `/proc/net` table a socket was listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Tcp6,
    Udp,
    Udp6,
}

/// The daemon's answer to `listeners`, less its `type` and `t`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Listeners {
    pub listeners: Vec<Listener>,
    /// Why other users' processes may be missing from `holders`: the helper is off, refused the connection, is
    /// too old, did not answer in time, or could not read the socket tables. Absent when the helper answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holders_error: Option<String>,
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
    /// 0 for a socket the kernel opened itself (NFS, WireGuard), which no process holds.
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

const TABLES: [(&str, Protocol); 4] = [
    ("tcp", Protocol::Tcp),
    ("tcp6", Protocol::Tcp6),
    ("udp", Protocol::Udp),
    ("udp6", Protocol::Udp6),
];
const TCP_LISTEN: &str = "0A";
/// `TCP_CLOSE`, the state of a UDP socket with no remote address.
const UDP_UNCONNECTED: &str = "07";

/// Every listening TCP and bound, unconnected UDP socket in the `tcp`, `tcp6`, `udp` and `udp6` tables of a
/// `/proc/<pid>/net` directory, each once and without holders. A missing table is skipped (a kernel without IPv6
/// has no `tcp6`); none at all is an error.
pub fn read_net(dir: &Path) -> io::Result<Vec<Listener>> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut tables = 0;
    for (name, protocol) in TABLES {
        let path = dir.join(name);
        let context = |e: io::Error| io::Error::new(e.kind(), format!("reading {}: {e}", path.display()));
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(context(e)),
        };
        tables += 1;
        // read line by line: a busy server's table of connections can run to megabytes
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        while reader.read_line(&mut line).map_err(context)? > 0 {
            // the kernel fills each read on its own, so a row can repeat across reads; inode 0 is not unique
            if let Some(l) = parse_line(&line, protocol)
                && seen.insert((l.protocol, l.inode, l.address, l.port))
            {
                found.push(l);
            }
            line.clear();
        }
    }
    if tables == 0 {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!("no socket tables in {}", dir.display()),
        ));
    }
    Ok(found)
}

/// One row of a `/proc/net/{tcp,udp}[6]` table, if it is a listening or bound unconnected socket.
fn parse_line(line: &str, protocol: Protocol) -> Option<Listener> {
    let mut f = line.split_whitespace();
    let local = f.nth(1)?;
    let state = f.nth(1)?;
    let uid = f.nth(3)?.parse().ok()?;
    let inode = f.nth(1)?.parse().ok()?;
    let (address, port) = local.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let listening = match protocol {
        Protocol::Tcp | Protocol::Tcp6 => state == TCP_LISTEN,
        Protocol::Udp | Protocol::Udp6 => state == UDP_UNCONNECTED && port != 0,
    };
    if !listening {
        return None;
    }
    let address = decode_address(address, matches!(protocol, Protocol::Tcp6 | Protocol::Udp6))?;
    Some(Listener {
        protocol,
        address,
        port,
        uid,
        inode,
        holders: Vec::new(),
    })
}

fn decode_address(hex: &str, v6: bool) -> Option<IpAddr> {
    if hex.len() != if v6 { 32 } else { 8 } || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // the kernel prints each 32-bit word of the network-order address as a host-endian number
    let mut bytes = [0u8; 16];
    for i in 0..hex.len() / 8 {
        let word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    Some(if v6 {
        Ipv6Addr::from(bytes).to_canonical()
    } else {
        IpAddr::from([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str =
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    fn row(local: &str) -> String {
        format!(
            "   0: {local} 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 100 0 0 10 0\n"
        )
    }

    #[test]
    fn malformed_rows_are_skipped_not_misread() {
        // "01é007F" is eight bytes but not eight hex digits; slicing it by byte offsets would panic
        for local in [
            "0100007F",
            "0100007G:0016",
            "0100007F:XYZ",
            "01é007F:0016",
            "0100007F00:0016",
        ] {
            assert_eq!(parse_line(&row(local), Protocol::Tcp), None, "{local}");
        }
        assert_eq!(parse_line(HEAD, Protocol::Tcp), None);
        assert_eq!(
            parse_line(&row("0100007F:0016"), Protocol::Tcp).map(|l| (l.address, l.port)),
            Some((IpAddr::from([127, 0, 0, 1]), 22))
        );
    }
}
