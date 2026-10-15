use std::collections::HashMap;

use schema::net::Net;
use schema::netinfo::Route;

/// Interfaces whose traffic is the machine's own: not loopback, and not container or VM plumbing,
/// whose bytes are already counted on the physical link they leave by.
pub fn counted(name: &str) -> bool {
    name != "lo" && !["veth", "docker", "br-", "virbr"].iter().any(|p| name.starts_with(p))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Ethernet,
    Wifi,
    Bridge,
    Virtual,
}

impl Kind {
    pub fn of(name: &str, has_signal: bool) -> Self {
        if has_signal || name.starts_with("wl") {
            Self::Wifi
        } else if ["en", "eth"].iter().any(|p| name.starts_with(p)) {
            Self::Ethernet
        } else if ["br", "virbr", "docker"].iter().any(|p| name.starts_with(p)) {
            Self::Bridge
        } else {
            Self::Virtual
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ethernet => "Ethernet",
            Self::Wifi => "Wi-Fi",
            Self::Bridge => "Bridge",
            Self::Virtual => "Virtual",
        }
    }
}

/// The default route the kernel prefers: the lowest metric, where an unset metric is 0.
pub fn primary_route(routes: &[Route]) -> Option<&Route> {
    routes
        .iter()
        .filter(|r| r.interface.is_some())
        .min_by_key(|r| r.metric.unwrap_or(0))
}

/// Tunnels report operstate UNKNOWN even while carrying traffic, so an address counts as connected for them.
/// `net` reports the state in lower case and `netinfo` in upper case.
pub fn connected(state: Option<&str>, has_address: bool) -> bool {
    match state.map(str::to_ascii_lowercase).as_deref() {
        Some("up") => true,
        Some("unknown") => has_address,
        _ => false,
    }
}

pub fn signal_quality(dbm: f64) -> &'static str {
    if dbm >= -50. {
        "Excellent"
    } else if dbm >= -60. {
        "Good"
    } else if dbm >= -70. {
        "Fair"
    } else {
        "Weak"
    }
}

/// More than 1% of sent segments retransmitted points at loss on the path.
pub fn retransmits_high(retransmits_ps: f64, out_segments_ps: f64) -> bool {
    out_segments_ps > 0. && retransmits_ps > out_segments_ps * 0.01
}

/// Bytes moved since the app opened, per interface, from the kernel's cumulative counters.
#[derive(Debug, Default)]
pub struct Session {
    started: bool,
    last: HashMap<String, (u64, u64)>,
    moved: HashMap<String, (u64, u64)>,
}

impl Session {
    pub fn observe(&mut self, net: &Net) {
        for (name, i) in &net.interfaces {
            let now = (i.rx_bytes_total, i.tx_bytes_total);
            // An interface that appears after the first sample was created since, so all its bytes are this session's.
            let before = match self.last.insert(name.clone(), now) {
                Some(before) => before,
                None if self.started => (0, 0),
                None => continue,
            };
            let moved = self.moved.entry(name.clone()).or_default();
            moved.0 += delta(before.0, now.0);
            moved.1 += delta(before.1, now.1);
        }
        self.started = true;
    }

    /// `(rx, tx)` summed over the interfaces `keep` accepts; `None` before the first sample.
    pub fn totals(&self, keep: impl Fn(&str) -> bool) -> Option<(u64, u64)> {
        self.started.then(|| {
            self.moved
                .iter()
                .filter(|(name, _)| keep(name))
                .fold((0, 0), |(rx, tx), (_, m)| (rx + m.0, tx + m.1))
        })
    }
}

/// A counter that went backwards was reset (interface re-created, driver reloaded) and restarted from zero.
fn delta(before: u64, now: u64) -> u64 {
    if now >= before { now - before } else { now }
}

/// `192.168.1.5/24` to `192.168.1.5`.
pub fn host(cidr: &str) -> &str {
    cidr.split_once('/').map_or(cidr, |(a, _)| a)
}

/// IPv4 addresses, then IPv6 ones with link-local addresses only when nothing else is configured.
pub fn split_addresses(addresses: &[String]) -> (Vec<&str>, Vec<&str>) {
    let (v6, v4): (Vec<&str>, Vec<&str>) = addresses.iter().map(|a| host(a)).partition(|a| a.contains(':'));
    let global: Vec<&str> = v6.iter().copied().filter(|a| !a.starts_with("fe80:")).collect();
    (v4, if global.is_empty() { v6 } else { global })
}

#[cfg(test)]
mod tests {
    use schema::net::{Interface, Sockets, Tcp};

    use super::*;

    fn net(interfaces: &[(&str, u64, u64)]) -> Net {
        Net {
            interfaces: interfaces
                .iter()
                .map(|&(name, rx, tx)| {
                    (
                        name.to_owned(),
                        Interface {
                            state: Some("UP".into()),
                            link_mbps: None,
                            rx_bps: 0,
                            tx_bps: 0,
                            rx_pps: 0.,
                            tx_pps: 0.,
                            rx_bytes_total: rx,
                            tx_bytes_total: tx,
                            rx_errors: 0,
                            rx_dropped: 0,
                            tx_errors: 0,
                            tx_dropped: 0,
                            link_quality: None,
                            signal_dbm: None,
                        },
                    )
                })
                .collect(),
            sockets: Sockets {
                tcp_in_use: None,
                udp_in_use: None,
            },
            tcp: Tcp {
                tcp_in_segments_ps: None,
                tcp_out_segments_ps: None,
                tcp_retransmits_ps: None,
                established: None,
            },
        }
    }

    #[test]
    fn container_and_loopback_traffic_is_not_the_machines_own() {
        for own in ["enp11s0", "wlp10s0", "eth0", "tailscale0", "wg0", "usb0"] {
            assert!(counted(own), "{own}");
        }
        for plumbing in ["lo", "veth7faffa5", "docker0", "br-3f2a9c", "virbr0"] {
            assert!(!counted(plumbing), "{plumbing}");
        }
    }

    #[test]
    fn session_counts_from_the_first_sample_not_from_boot() {
        let mut s = Session::default();
        assert_eq!(s.totals(|_| true), None, "nothing measured yet is unknown, not zero");
        s.observe(&net(&[("eth0", 10_000_000, 5_000_000)]));
        assert_eq!(s.totals(|_| true), Some((0, 0)));
        s.observe(&net(&[("eth0", 10_001_000, 5_000_200)]));
        s.observe(&net(&[("eth0", 10_003_000, 5_000_300)]));
        assert_eq!(s.totals(|_| true), Some((3000, 300)));
    }

    #[test]
    fn a_counter_reset_never_reads_as_negative_or_huge() {
        let mut s = Session::default();
        s.observe(&net(&[("eth0", 5000, 5000)]));
        s.observe(&net(&[("eth0", 6000, 5500)]));
        s.observe(&net(&[("eth0", 300, 40)]));
        assert_eq!(
            s.totals(|_| true),
            Some((1300, 540)),
            "after the reset only the bytes since it count"
        );
        s.observe(&net(&[("eth0", 400, 40)]));
        assert_eq!(s.totals(|_| true), Some((1400, 540)));
    }

    #[test]
    fn a_late_interface_counts_whole_and_filtered_ones_are_left_out() {
        let mut s = Session::default();
        s.observe(&net(&[("eth0", 100, 100), ("docker0", 100, 100)]));
        s.observe(&net(&[("eth0", 200, 100), ("docker0", 900, 900), ("usb0", 50, 20)]));
        assert_eq!(s.totals(counted), Some((150, 20)));
        assert_eq!(s.totals(|_| true), Some((950, 820)));
    }

    #[test]
    fn primary_is_the_lowest_metric_default_route() {
        let route = |iface: &str, metric: Option<u64>| Route {
            gateway: Some(format!("gw-{iface}")),
            interface: Some(iface.into()),
            metric,
        };
        let routes = [route("wlp10s0", Some(600)), route("enp11s0", Some(100))];
        assert_eq!(
            primary_route(&routes).and_then(|r| r.interface.as_deref()),
            Some("enp11s0")
        );
        let routes = [route("wlp10s0", Some(600)), route("wg0", None)];
        assert_eq!(
            primary_route(&routes).and_then(|r| r.interface.as_deref()),
            Some("wg0"),
            "an unset metric is 0, which wins"
        );
        assert!(primary_route(&[]).is_none());
    }

    #[test]
    fn kinds_follow_the_interface_name() {
        assert_eq!(Kind::of("enp11s0", false), Kind::Ethernet);
        assert_eq!(Kind::of("wlp10s0", false), Kind::Wifi);
        assert_eq!(Kind::of("eth0", true), Kind::Wifi, "a signal reading means radio");
        assert_eq!(Kind::of("docker0", false), Kind::Bridge);
        assert_eq!(Kind::of("virbr0", false), Kind::Bridge);
        assert_eq!(Kind::of("tailscale0", false), Kind::Virtual);
    }

    #[test]
    fn a_tunnel_with_an_address_is_connected_but_a_down_link_is_not() {
        assert!(connected(Some("UP"), false));
        assert!(connected(Some("up"), false), "the live module's spelling");
        assert!(connected(Some("UNKNOWN"), true));
        assert!(!connected(Some("UNKNOWN"), false));
        assert!(!connected(Some("DOWN"), true));
        assert!(!connected(None, true));
    }

    #[test]
    fn signal_and_retransmit_thresholds() {
        assert_eq!(signal_quality(-45.), "Excellent");
        assert_eq!(signal_quality(-60.), "Good");
        assert_eq!(signal_quality(-65.), "Fair");
        assert_eq!(signal_quality(-80.), "Weak");
        assert!(!retransmits_high(1., 100.), "exactly 1% is still fine");
        assert!(retransmits_high(1.5, 100.));
        assert!(!retransmits_high(0.5, 0.), "an idle link has nothing to compare");
    }

    #[test]
    fn addresses_split_by_family_and_hide_link_local_when_there_is_more() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let both = a(&["10.10.10.111/24", "2406:2d40::bab0/64", "fe80::192a/64"]);
        assert_eq!(split_addresses(&both), (vec!["10.10.10.111"], vec!["2406:2d40::bab0"]));
        let local = a(&["fe80::1/64"]);
        assert_eq!(split_addresses(&local), (vec![], vec!["fe80::1"]));
    }
}
