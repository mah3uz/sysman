use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use schema::netinfo::{Interface, Route};
use serde::Deserialize;
use serde_json::Value;

use crate::collector::Collector;
use crate::sys::Sys;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    /// Interface name prefixes to skip.
    pub exclude: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            exclude: vec!["lo".into(), "veth".into()],
        }
    }
}

#[derive(Deserialize)]
struct IpLink {
    ifname: String,
    operstate: Option<String>,
    address: Option<String>,
    mtu: Option<u64>,
    #[serde(default)]
    addr_info: Vec<IpAddr>,
}

#[derive(Deserialize)]
struct IpAddr {
    local: Option<String>,
    prefixlen: Option<u8>,
}

#[derive(Deserialize)]
struct IpRoute {
    gateway: Option<String>,
    dev: Option<String>,
    metric: Option<u64>,
}

pub struct Netinfo {
    sys: Sys,
    exclude: Vec<String>,
}

impl Netinfo {
    pub fn new(options: Options, sys: Sys) -> Result<Self> {
        Ok(Self {
            sys,
            exclude: options.exclude,
        })
    }
}

impl Collector for Netinfo {
    fn sample(&mut self) -> Result<Option<Value>> {
        let addr = run("ip", &["-j", "addr"], TIMEOUT)?;
        let route = run("ip", &["-j", "route", "show", "default"], TIMEOUT)?;
        self.assemble(&addr, &route).map(Some)
    }
}

impl Netinfo {
    fn assemble(&self, addr: &str, route: &str) -> Result<Value> {
        let resolv = self.sys.read_opt("/etc/resolv.conf").unwrap_or_default();
        Ok(serde_json::to_value(parse(addr, route, &resolv, &self.exclude)?)?)
    }
}

fn parse(addr: &str, route: &str, resolv: &str, exclude: &[String]) -> Result<schema::netinfo::Netinfo> {
    let links: Vec<IpLink> = serde_json::from_str(addr).context("parsing `ip -j addr`")?;
    let routes: Vec<IpRoute> = serde_json::from_str(route).context("parsing `ip -j route`")?;
    let interfaces: BTreeMap<String, Interface> = links
        .into_iter()
        .filter(|l| !exclude.iter().any(|p| l.ifname.starts_with(p.as_str())))
        .map(|l| {
            let addresses = l
                .addr_info
                .iter()
                .filter_map(|a| Some(format!("{}/{}", a.local.as_ref()?, a.prefixlen?)))
                .collect();
            (
                l.ifname,
                Interface {
                    state: l.operstate,
                    mac: l.address,
                    mtu: l.mtu,
                    addresses,
                },
            )
        })
        .collect();
    let default_routes = routes
        .into_iter()
        .map(|r| Route {
            gateway: r.gateway,
            interface: r.dev,
            metric: r.metric,
        })
        .collect();
    let dns = resolv
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            if w.next()? == "nameserver" {
                w.next().map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    Ok(schema::netinfo::Netinfo {
        interfaces,
        default_routes,
        dns,
    })
}

/// A child past `timeout` is killed and barely waited for, so a process stuck in the kernel cannot stall the daemon.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    // drained on its own thread: a child writing more than a pipe buffer would otherwise block until the timeout
    let mut stdout = child.stdout.take().context("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = tx.send(stdout.read_to_end(&mut buf).map(|_| buf));
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let grace = Instant::now();
            while child.try_wait().ok().flatten().is_none() && grace.elapsed() < Duration::from_millis(200) {
                std::thread::sleep(Duration::from_millis(10));
            }
            bail!("{program} {} did not finish within {timeout:?}", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !status.success() {
        bail!("{program} {} exited with {status}", args.join(" "));
    }
    let bytes = rx
        .recv_timeout(Duration::from_millis(500))
        .context("reading stdout timed out")?
        .context("reading stdout")?;
    String::from_utf8(bytes).context("output is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const ADDR: &str = include_str!("fixtures/ip-addr.json");
    const ROUTE: &str = include_str!("fixtures/ip-route.json");
    const RESOLV: &str =
        "# stub\nnameserver 127.0.0.53\nnameserver 2001:db8::53\noptions edns0 trust-ad\nsearch example.net\n";

    fn defaults() -> Vec<String> {
        Options::default().exclude
    }

    #[test]
    fn parses_captured_ip_output() {
        let out = parse(ADDR, ROUTE, RESOLV, &defaults()).unwrap();
        let v = serde_json::to_value(&out).unwrap();
        assert_schema::<schema::netinfo::Netinfo>(&v);
        assert_eq!(v["interfaces"]["enp11s0"]["state"], "UP");
        assert_eq!(v["interfaces"]["enp11s0"]["mac"], "60:cf:84:a9:0b:a3");
        assert_eq!(v["interfaces"]["enp11s0"]["mtu"], 1500);
        assert_eq!(v["interfaces"]["enp11s0"]["addresses"][0], "10.10.10.111/24");
        assert_eq!(
            v["default_routes"][0],
            serde_json::json!({"gateway": "10.10.10.1", "interface": "enp11s0", "metric": 100})
        );
        assert_eq!(v["dns"], serde_json::json!(["127.0.0.53", "2001:db8::53"]));
    }

    #[test]
    fn interface_without_addresses_is_kept_with_an_empty_list() {
        // a NO-CARRIER link still exists and its state is the interesting part
        let out = parse(ADDR, ROUTE, RESOLV, &defaults()).unwrap();
        assert!(out.interfaces["enp12s0"].addresses.is_empty());
        assert_eq!(out.interfaces["enp12s0"].state.as_deref(), Some("DOWN"));
    }

    #[test]
    fn excluded_prefixes_are_dropped() {
        let out = parse(ADDR, ROUTE, RESOLV, &defaults()).unwrap();
        assert!(!out.interfaces.contains_key("lo"));
        let all = parse(ADDR, ROUTE, RESOLV, &[]).unwrap();
        assert!(all.interfaces.contains_key("lo"));
    }

    #[test]
    fn no_default_route_gives_an_empty_list() {
        let out = parse(ADDR, "[]", RESOLV, &defaults()).unwrap();
        assert!(out.default_routes.is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse("not json", ROUTE, RESOLV, &defaults()).is_err());
    }

    #[test]
    fn dns_comes_from_resolv_conf_through_sys() {
        let f = Fixture::new().file("etc/resolv.conf", RESOLV);
        let v = Netinfo::new(Options::default(), f.sys())
            .unwrap()
            .assemble(ADDR, ROUTE)
            .unwrap();
        assert_eq!(v["dns"], serde_json::json!(["127.0.0.53", "2001:db8::53"]));
        // a missing resolv.conf means no servers, not a failed module
        let v = Netinfo::new(Options::default(), Fixture::new().sys())
            .unwrap()
            .assemble(ADDR, ROUTE)
            .unwrap();
        assert_eq!(v["dns"], serde_json::json!([]));
    }

    #[test]
    fn hung_child_is_killed_at_the_timeout() {
        let start = Instant::now();
        let err = run("sleep", &["30"], Duration::from_millis(150)).unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let out = run(
            "sh",
            &["-c", "head -c 300000 /dev/zero | tr '\\0' x"],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.len(), 300_000);
    }

    #[test]
    fn failing_or_missing_command_is_an_error() {
        assert!(run("false", &[], TIMEOUT).is_err());
        assert!(run("definitely-not-a-command", &[], TIMEOUT).is_err());
    }
}
