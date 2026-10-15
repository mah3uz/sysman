use std::collections::HashMap;

use anyhow::Result;
use schema::cgroups::Cgroup;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

const ROOT: &str = "/sys/fs/cgroup";

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    pub top_n: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { top_n: 10 }
    }
}

pub struct Cgroups {
    sys: Sys,
    top_n: usize,
    deltas: Deltas<String>,
}

impl Cgroups {
    pub fn new(opts: Options, sys: Sys) -> Result<Self> {
        Ok(Self {
            sys,
            top_n: opts.top_n,
            deltas: Deltas::default(),
        })
    }

    // cgroup v2 keeps processes in leaves, so a parent's totals already include its children
    fn leaves(&self, rel: &str, out: &mut Vec<String>) {
        let dir = format!("{ROOT}/{rel}");
        let subdirs: Vec<String> = self
            .sys
            .list(&dir)
            .into_iter()
            .filter(|n| self.sys.path(format!("{dir}/{n}")).is_dir())
            .collect();
        if subdirs.is_empty() && !rel.is_empty() {
            out.push(rel.to_string());
        }
        for n in subdirs {
            self.leaves(&if rel.is_empty() { n } else { format!("{rel}/{n}") }, out);
        }
    }
}

fn usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())
}

fn top(rows: &[Cgroup], key: impl Fn(&Cgroup) -> f64, n: usize) -> Vec<Cgroup> {
    let mut v: Vec<&Cgroup> = rows.iter().filter(|r| key(r) > 0.0).collect();
    v.sort_by(|a, b| key(b).total_cmp(&key(a)));
    v.into_iter().take(n).cloned().collect()
}

impl Collector for Cgroups {
    fn meta(&mut self) -> Option<Value> {
        Some(json!({"scope": "leaf cgroups: apps, services and containers", "cpu_pct": "percent of one core"}))
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let mut names = Vec::new();
        self.leaves("", &mut names);
        let mut counters = HashMap::new();
        let mut info = HashMap::new();
        for name in names {
            let dir = format!("{ROOT}/{name}");
            let Some(usage) = self
                .sys
                .read_opt(format!("{dir}/cpu.stat"))
                .as_deref()
                .and_then(usage_usec)
            else {
                continue;
            };
            let int = |f: &str| {
                self.sys
                    .int_opt(format!("{dir}/{f}"))
                    .and_then(|v| u64::try_from(v).ok())
            };
            info.insert(name.clone(), (int("memory.current"), int("pids.current")));
            counters.insert(name, vec![usage]);
        }
        let Some((dt, deltas)) = self.deltas.update(counters) else {
            return Ok(None);
        };
        let rows: Vec<Cgroup> = deltas
            .into_iter()
            .map(|(name, d)| {
                let (memory_bytes, pids) = info[&name];
                Cgroup {
                    cpu_pct: (d[0] as f64 / dt / 10_000.0 * 10.0).round() / 10.0,
                    name,
                    memory_bytes,
                    pids,
                }
            })
            .collect();
        let mut by_name = rows;
        by_name.sort_by(|a, b| a.name.cmp(&b.name));
        let out = schema::cgroups::Cgroups {
            count: info.len() as u64,
            top_cpu: top(&by_name, |r| r.cpu_pct, self.top_n),
            top_memory: top(&by_name, |r| r.memory_bytes.unwrap_or(0) as f64, self.top_n),
        };
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    fn cpu(usec: u64) -> String {
        format!("usage_usec {usec}\nuser_usec {usec}\nsystem_usec 0\n")
    }

    fn tree(a_usec: u64, b_usec: u64) -> Fixture {
        // user.slice has no processes of its own: its totals are the sum of its children
        Fixture::new()
            .file("sys/fs/cgroup/cpu.stat", &cpu(9_000_000))
            .file("sys/fs/cgroup/user.slice/cpu.stat", &cpu(a_usec + b_usec))
            .file("sys/fs/cgroup/user.slice/memory.current", "3000\n")
            .file("sys/fs/cgroup/user.slice/pids.current", "5\n")
            .file("sys/fs/cgroup/user.slice/app-a.scope/cpu.stat", &cpu(a_usec))
            .file("sys/fs/cgroup/user.slice/app-a.scope/memory.current", "1000\n")
            .file("sys/fs/cgroup/user.slice/app-a.scope/pids.current", "2\n")
            .file("sys/fs/cgroup/user.slice/app-b.scope/cpu.stat", &cpu(b_usec))
            .file("sys/fs/cgroup/user.slice/app-b.scope/memory.current", "2000\n")
            .file("sys/fs/cgroup/system.slice/sshd.service/cpu.stat", &cpu(0))
            .file("sys/fs/cgroup/system.slice/sshd.service/memory.current", "500\n")
    }

    fn set(f: &Fixture, rel: &str, contents: &str) {
        std::fs::write(f.path().join("sys/fs/cgroup").join(rel), contents).unwrap();
    }

    #[test]
    fn only_leaves_are_counted_so_usage_is_not_doubled() {
        let f = tree(1_000_000, 3_000_000);
        let mut c = Cgroups::new(Options::default(), f.sys()).unwrap();
        assert!(c.sample().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(50));
        set(&f, "user.slice/app-a.scope/cpu.stat", &cpu(1_100_000));
        set(&f, "user.slice/app-b.scope/cpu.stat", &cpu(3_400_000));
        set(&f, "user.slice/cpu.stat", &cpu(4_500_000));
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::cgroups::Cgroups>(&v);
        assert_eq!(v["count"], 3);
        let names: Vec<&str> = v["top_cpu"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["user.slice/app-b.scope", "user.slice/app-a.scope"]);
        let pct = |i: usize| v["top_cpu"][i]["cpu_pct"].as_f64().unwrap();
        assert!((pct(0) / pct(1) - 4.0).abs() < 0.1, "{} {}", pct(0), pct(1));
        let mem: Vec<&str> = v["top_memory"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(
            !mem.contains(&"user.slice"),
            "the parent's 3000 bytes already contain its children's"
        );
    }

    #[test]
    fn fields_missing_from_a_leaf_are_absent_not_zero() {
        let f = tree(1, 1);
        let mut c = Cgroups::new(Options::default(), f.sys()).unwrap();
        c.sample().unwrap();
        set(&f, "system.slice/sshd.service/cpu.stat", &cpu(1_000_000));
        let v = c.sample().unwrap().unwrap();
        let ssh = v["top_cpu"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "system.slice/sshd.service")
            .unwrap();
        assert_eq!(ssh["memory_bytes"], 500);
        assert!(ssh.get("pids").is_none());
    }

    #[test]
    fn leaf_without_usage_is_skipped_and_top_n_caps_the_lists() {
        let f = tree(1, 1).file("sys/fs/cgroup/empty.slice/memory.current", "9\n");
        let mut c = Cgroups::new(Options { top_n: 1 }, f.sys()).unwrap();
        c.sample().unwrap();
        set(&f, "user.slice/app-a.scope/cpu.stat", &cpu(5_000_000));
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["count"], 3, "empty.slice has no cpu.stat");
        assert_eq!(v["top_cpu"].as_array().unwrap().len(), 1);
        assert_eq!(v["top_memory"][0]["name"], "user.slice/app-b.scope");
    }

    #[test]
    fn unknown_option_is_rejected() {
        let t: toml::Table = "top = 3".parse().unwrap();
        assert!(crate::collector::options::<Options>(t).is_err());
    }
}
