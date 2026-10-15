use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use schema::net::{Interface, Sockets, Tcp};
use serde_json::Value;

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

pub use sysman_config::modules::net::Options;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Interface(String),
    Tcp,
}

struct Counters {
    rx_bytes: u64,
    rx_packets: u64,
    rx_errors: u64,
    rx_dropped: u64,
    tx_bytes: u64,
    tx_packets: u64,
    tx_errors: u64,
    tx_dropped: u64,
}

pub struct Net {
    sys: Sys,
    exclude: Vec<String>,
    deltas: Deltas<Key>,
}

impl Net {
    pub fn new(options: Options, sys: Sys) -> Result<Self> {
        if sys.read_opt("/proc/net/dev").is_none() {
            bail!("/proc/net/dev is not readable");
        }
        Ok(Self {
            sys,
            exclude: options.exclude,
            deltas: Deltas::default(),
        })
    }
}

impl Collector for Net {
    fn sample(&mut self) -> Result<Option<Value>> {
        // listed afresh each time so hot-plugged and removed interfaces follow without a restart
        let dev = self.sys.read("/proc/net/dev")?;
        let mut totals: BTreeMap<String, Counters> = BTreeMap::new();
        for line in dev.lines().skip(2) {
            let (name, rest) = line
                .split_once(':')
                .with_context(|| format!("/proc/net/dev: {line:?}"))?;
            let name = name.trim();
            if self.exclude.iter().any(|p| name.starts_with(p.as_str())) {
                continue;
            }
            totals.insert(
                name.to_string(),
                parse_counters(rest).with_context(|| format!("/proc/net/dev: {line:?}"))?,
            );
        }

        let tcp = self
            .sys
            .read_opt("/proc/net/snmp")
            .map(|s| tcp_stats(&s))
            .unwrap_or_default();
        let mut counters: HashMap<Key, Vec<u64>> = totals
            .iter()
            .map(|(name, c)| {
                (
                    Key::Interface(name.clone()),
                    vec![c.rx_bytes, c.rx_packets, c.tx_bytes, c.tx_packets],
                )
            })
            .collect();
        let tcp_counters: Option<Vec<u64>> = TCP_RATES
            .iter()
            .map(|k| tcp.get(*k).and_then(|v| u64::try_from(*v).ok()))
            .collect();
        if let Some(c) = &tcp_counters {
            counters.insert(Key::Tcp, c.clone());
        }

        let Some((dt, deltas)) = self.deltas.update(counters) else {
            return Ok(None);
        };

        let wifi = self
            .sys
            .read_opt("/proc/net/wireless")
            .map(|s| wireless(&s))
            .unwrap_or_default();
        let mut interfaces = BTreeMap::new();
        for (name, c) in &totals {
            let Some(d) = deltas.get(&Key::Interface(name.clone())) else {
                continue;
            };
            let speed = self.sys.int_opt(format!("/sys/class/net/{name}/speed"));
            let (link_quality, signal_dbm) = wifi.get(name).map_or((None, None), |w| (Some(w.0), Some(w.1)));
            interfaces.insert(
                name.clone(),
                Interface {
                    state: self.sys.read_opt(format!("/sys/class/net/{name}/operstate")),
                    link_mbps: speed.filter(|s| *s > 0).map(|s| s as u64),
                    rx_bps: (d[0] as f64 / dt).round() as u64,
                    tx_bps: (d[2] as f64 / dt).round() as u64,
                    rx_pps: round1(d[1] as f64 / dt),
                    tx_pps: round1(d[3] as f64 / dt),
                    rx_bytes_total: c.rx_bytes,
                    tx_bytes_total: c.tx_bytes,
                    rx_errors: c.rx_errors,
                    rx_dropped: c.rx_dropped,
                    tx_errors: c.tx_errors,
                    tx_dropped: c.tx_dropped,
                    link_quality,
                    signal_dbm,
                },
            );
        }

        let rates = deltas.get(&Key::Tcp).map(|d| d.iter().map(|v| round1(*v as f64 / dt)));
        let mut rates = rates.into_iter().flatten();
        let out = schema::net::Net {
            interfaces,
            sockets: self
                .sys
                .read_opt("/proc/net/sockstat")
                .map(|s| sockets(&s))
                .unwrap_or(Sockets {
                    tcp_in_use: None,
                    udp_in_use: None,
                }),
            tcp: Tcp {
                tcp_in_segments_ps: rates.next(),
                tcp_out_segments_ps: rates.next(),
                tcp_retransmits_ps: rates.next(),
                established: tcp.get("CurrEstab").copied(),
            },
        };
        Ok(Some(serde_json::to_value(out)?))
    }
}

const TCP_RATES: [&str; 3] = ["InSegs", "OutSegs", "RetransSegs"];

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn parse_counters(rest: &str) -> Result<Counters> {
    let p = rest
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<Vec<u64>, _>>()?;
    if p.len() < 12 {
        bail!("expected at least 12 columns, found {}", p.len());
    }
    Ok(Counters {
        rx_bytes: p[0],
        rx_packets: p[1],
        rx_errors: p[2],
        rx_dropped: p[3],
        tx_bytes: p[8],
        tx_packets: p[9],
        tx_errors: p[10],
        tx_dropped: p[11],
    })
}

/// `Tcp:` header line then value line; some values (`MaxConn`) are -1, hence signed.
fn tcp_stats(snmp: &str) -> HashMap<String, i64> {
    let lines: Vec<Vec<&str>> = snmp
        .lines()
        .filter(|l| l.starts_with("Tcp:"))
        .map(|l| l.split_whitespace().skip(1).collect())
        .collect();
    let [names, values] = lines.as_slice() else {
        return HashMap::new();
    };
    names
        .iter()
        .zip(values)
        .filter_map(|(n, v)| Some((n.to_string(), v.parse().ok()?)))
        .collect()
}

fn wireless(text: &str) -> HashMap<String, (f64, f64)> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let p: Vec<&str> = line.split_whitespace().collect();
            Some((
                p.first()?.trim_end_matches(':').to_string(),
                (p.get(2)?.parse().ok()?, p.get(3)?.parse().ok()?),
            ))
        })
        .collect()
}

fn sockets(text: &str) -> Sockets {
    let in_use = |proto: &str| {
        let line = text.lines().find_map(|l| l.strip_prefix(proto)?.strip_prefix(':'))?;
        let mut words = line.split_whitespace();
        words.find(|w| *w == "inuse")?;
        words.next()?.parse().ok()
    };
    Sockets {
        tcp_in_use: in_use("TCP"),
        udp_in_use: in_use("UDP"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const HEADER: &str = "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

    fn line(name: &str, rx_bytes: u64, tx_bytes: u64) -> String {
        format!(
            "{name:>7}: {rx_bytes} {} 3 4 0 0 0 0 {tx_bytes} {} 5 6 0 0 0 0\n",
            rx_bytes / 100,
            tx_bytes / 100
        )
    }

    fn dev(lines: &[String]) -> String {
        format!("{HEADER}{}", lines.concat())
    }

    fn collector(f: &Fixture) -> Net {
        Net::new(Options::default(), f.sys()).unwrap()
    }

    fn two_samples(before: &[String], after: &[String]) -> Value {
        let f = Fixture::new().file("proc/net/dev", &dev(before));
        let mut c = collector(&f);
        assert!(c.sample().unwrap().is_none());
        let _f = f.file("proc/net/dev", &dev(after));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::net::Net>(&v);
        v
    }

    #[test]
    fn rates_are_per_interface_not_from_summed_counters() {
        // eth1 resets while eth0 keeps counting: a summed counter would go backwards and zero eth0's rate too
        let v = two_samples(
            &[line("eth0", 1_000_000, 0), line("eth1", 9_000_000, 0)],
            &[line("eth0", 2_000_000, 0), line("eth1", 50, 0)],
        );
        let i = &v["interfaces"];
        assert!(i["eth0"]["rx_bps"].as_u64().unwrap() > 0);
        assert_eq!(i["eth1"]["rx_bps"], 0);
        assert_eq!(i["eth1"]["rx_pps"], 0.0);
        assert_eq!(i["eth0"]["rx_bytes_total"], 2_000_000);
    }

    #[test]
    fn each_interface_gets_its_own_rate() {
        let v = two_samples(
            &[line("eth0", 0, 0), line("wlan0", 0, 0)],
            &[line("eth0", 1_000_000, 0), line("wlan0", 3_000_000, 0)],
        );
        let (a, b) = (
            v["interfaces"]["eth0"]["rx_bps"].as_f64().unwrap(),
            v["interfaces"]["wlan0"]["rx_bps"].as_f64().unwrap(),
        );
        assert!((b / a - 3.0).abs() < 0.01, "{a} {b}");
    }

    #[test]
    fn removed_interface_disappears_without_disturbing_the_rest() {
        let v = two_samples(
            &[line("eth0", 1_000_000, 0), line("usb0", 9_000_000, 0)],
            &[line("eth0", 2_000_000, 0)],
        );
        let i = v["interfaces"].as_object().unwrap();
        assert_eq!(i.keys().collect::<Vec<_>>(), ["eth0"]);
        assert!(i["eth0"]["rx_bps"].as_u64().unwrap() > 0);
    }

    #[test]
    fn hot_plugged_interface_appears_on_the_sample_after_it_is_first_seen() {
        let f = Fixture::new().file("proc/net/dev", &dev(&[line("eth0", 0, 0)]));
        let mut c = collector(&f);
        assert!(c.sample().unwrap().is_none());
        let f = f.file("proc/net/dev", &dev(&[line("eth0", 100, 0), line("usb0", 500, 0)]));
        let v = c.sample().unwrap().unwrap();
        // no previous reading to difference against, so a rate would be the lifetime total
        assert_eq!(
            v["interfaces"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["eth0"]
        );
        let f = f.file("proc/net/dev", &dev(&[line("eth0", 200, 0), line("usb0", 900, 0)]));
        let v = c.sample().unwrap().unwrap();
        assert_eq!(
            v["interfaces"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["eth0", "usb0"]
        );
        drop(f);
    }

    #[test]
    fn excluded_prefixes_are_skipped() {
        let v = two_samples(
            &[line("lo", 1, 1), line("veth12", 1, 1), line("eth0", 1, 1)],
            &[line("lo", 2, 2), line("veth12", 2, 2), line("eth0", 2, 2)],
        );
        assert_eq!(
            v["interfaces"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["eth0"]
        );
    }

    #[test]
    fn link_state_tcp_sockets_and_wifi_are_read() {
        let snmp = "Tcp: RtoAlgorithm MaxConn InSegs OutSegs RetransSegs CurrEstab\nTcp: 1 -1 100 200 3 7\n";
        let snmp2 = "Tcp: RtoAlgorithm MaxConn InSegs OutSegs RetransSegs CurrEstab\nTcp: 1 -1 1100 1200 13 9\n";
        let wireless = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
             face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n \
            wlan0: 0000   70.  -40.  -256        0      0      0      0      0        0\n";
        let f = Fixture::new()
            .file("proc/net/dev", &dev(&[line("wlan0", 0, 0)]))
            .file("proc/net/snmp", snmp)
            .file(
                "proc/net/sockstat",
                "sockets: used 300\nTCP: inuse 12 orphan 0 tw 1 alloc 20 mem 3\nUDP: inuse 4 mem 1\n",
            )
            .file("proc/net/wireless", wireless)
            .file("sys/class/net/wlan0/operstate", "up\n")
            .file("sys/class/net/wlan0/speed", "-1\n");
        let mut c = collector(&f);
        c.sample().unwrap();
        let f = f
            .file("proc/net/dev", &dev(&[line("wlan0", 10, 0)]))
            .file("proc/net/snmp", snmp2);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let v = c.sample().unwrap().unwrap();
        drop(f);
        assert_schema::<schema::net::Net>(&v);
        let w = &v["interfaces"]["wlan0"];
        assert_eq!(w["state"], "up");
        // the kernel reports -1 when the speed is unknown; a link speed of 0 or -1 would be a lie
        assert_eq!(w["link_mbps"], Value::Null);
        assert_eq!(w["link_quality"], 70.0);
        assert_eq!(w["signal_dbm"], -40.0);
        assert_eq!(v["sockets"], serde_json::json!({"tcp_in_use": 12, "udp_in_use": 4}));
        assert_eq!(v["tcp"]["established"], 9);
        let (i, o) = (
            v["tcp"]["tcp_in_segments_ps"].as_f64().unwrap(),
            v["tcp"]["tcp_out_segments_ps"].as_f64().unwrap(),
        );
        assert!((o / i - 1.0).abs() < 0.01 && i > 0.0);
    }

    #[test]
    fn unreadable_dev_file_is_an_error() {
        assert!(Net::new(Options::default(), Fixture::new().sys()).is_err());
    }
}
