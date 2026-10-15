use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::collector::{Collector, NoOptions};
use crate::deltas::Deltas;
use crate::sys::Sys;

const DMI: [&str; 7] = [
    "sys_vendor",
    "product_name",
    "board_vendor",
    "board_name",
    "bios_vendor",
    "bios_version",
    "bios_date",
];
const RATE_KEYS: [&str; 3] = ["ctxt", "intr", "processes"];

pub struct System {
    sys: Sys,
    deltas: Deltas<&'static str>,
}

impl System {
    pub fn new(_: NoOptions, sys: Sys) -> Result<Self> {
        Ok(Self {
            sys,
            deltas: Deltas::default(),
        })
    }
}

impl Collector for System {
    fn meta(&mut self) -> Option<Value> {
        let distro = self.sys.read_opt("/etc/os-release").and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        });
        let meta = schema::system::SystemMeta {
            host: self.sys.read_opt("/proc/sys/kernel/hostname"),
            kernel: self.sys.read_opt("/proc/sys/kernel/osrelease"),
            arch: self
                .sys
                .read_opt("/proc/sys/kernel/arch")
                .unwrap_or_else(|| std::env::consts::ARCH.to_string()),
            distro,
            boot_id: self.sys.read_opt("/proc/sys/kernel/random/boot_id"),
            dmi: DMI
                .iter()
                .map(|k| (k.to_string(), self.sys.read_opt(format!("/sys/class/dmi/id/{k}"))))
                .collect::<BTreeMap<_, _>>(),
        };
        serde_json::to_value(meta).ok()
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let stat = self.sys.read("/proc/stat")?;
        let mut first: HashMap<&str, u64> = HashMap::new();
        let mut logical_cpus = 0;
        for line in stat.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else { continue };
            if key.strip_prefix("cpu").is_some_and(|n| n.parse::<u32>().is_ok()) {
                logical_cpus += 1;
            } else if let Some(v) = words.next().and_then(|v| v.parse().ok()) {
                first.insert(key, v);
            }
        }
        let get = |k: &str| {
            first
                .get(k)
                .copied()
                .with_context(|| format!("/proc/stat has no {k} line"))
        };

        let counters = HashMap::from([("stat", RATE_KEYS.iter().map(|k| get(k)).collect::<Result<Vec<_>>>()?)]);
        let rates = self.deltas.update(counters).and_then(|(dt, mut d)| {
            let d = d.remove("stat")?;
            let ps = |v: u64| (v as f64 / dt).round() as u64;
            Some((ps(d[0]), ps(d[1]), ps(d[2])))
        });

        let loadavg = self.sys.read("/proc/loadavg")?;
        let load: Vec<&str> = loadavg.split_whitespace().collect();
        let loads = |i: usize| -> Result<f64> {
            load.get(i)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow!("/proc/loadavg: {loadavg:?}"))
        };
        let tasks_total = load
            .get(3)
            .and_then(|v| v.split_once('/'))
            .and_then(|(_, n)| n.parse().ok())
            .ok_or_else(|| anyhow!("/proc/loadavg: {loadavg:?}"))?;
        let uptime = self.sys.read("/proc/uptime")?;
        let uptime: f64 = uptime
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| anyhow!("/proc/uptime: {uptime:?}"))?;
        let file_nr = self.sys.read("/proc/sys/fs/file-nr")?;
        let open_files = file_nr
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| anyhow!("/proc/sys/fs/file-nr: {file_nr:?}"))?;

        let out = schema::system::System {
            load: [loads(0)?, loads(1)?, loads(2)?],
            tasks_total,
            procs_running: get("procs_running")?,
            procs_blocked: get("procs_blocked")?,
            uptime_s: uptime.round() as u64,
            boot_time: get("btime")?,
            open_files,
            logical_cpus,
            context_switches_ps: rates.map(|r| r.0),
            interrupts_ps: rates.map(|r| r.1),
            forks_ps: rates.map(|r| r.2),
        };
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const STAT: &str = "cpu  862993 4155 201401 24564627 17136 61659 14758 0 0 0
cpu0 28601 264 7767 759170 375 849 6714 0 0 0
cpu1 17243 201 5577 775603 583 707 1309 0 0 0
intr 118349261 0 5 0 0
ctxt 4000
btime 1760000000
processes 100
procs_running 3
procs_blocked 1
softirq 1 2 3
";

    fn fixture(stat: &str) -> Fixture {
        Fixture::new()
            .file("proc/stat", stat)
            .file("proc/loadavg", "1.06 1.18 1.02 5/2481 129679\n")
            .file("proc/uptime", "7971.81 245646.36\n")
            .file("proc/sys/fs/file-nr", "12592\t0\t9223372036854775807\n")
            .file("proc/sys/kernel/hostname", "box\n")
            .file("proc/sys/kernel/osrelease", "7.2.9-arch1-1\n")
            .file("etc/os-release", "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\n")
            .file("sys/class/dmi/id/board_name", "ROG\n")
    }

    #[test]
    fn reads_load_tasks_and_counts_only_numbered_cpu_lines() {
        let f = fixture(STAT);
        let mut c = System::new(NoOptions {}, f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::system::System>(&v);
        assert_eq!(v["load"], serde_json::json!([1.06, 1.18, 1.02]));
        assert_eq!(v["tasks_total"], 2481);
        assert_eq!(v["procs_running"], 3);
        assert_eq!(v["uptime_s"], 7972);
        assert_eq!(v["boot_time"], 1760000000u64);
        assert_eq!(v["open_files"], 12592);
        // the aggregate `cpu` line is not a CPU
        assert_eq!(v["logical_cpus"], 2);
    }

    #[test]
    fn rates_appear_only_once_there_is_a_previous_reading() {
        let f = fixture(STAT);
        let mut c = System::new(NoOptions {}, f.sys()).unwrap();
        let first = c.sample().unwrap().unwrap();
        assert!(first.get("context_switches_ps").is_none());
        std::fs::write(f.path().join("proc/stat"), STAT.replace("ctxt 4000", "ctxt 9000")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = c.sample().unwrap().unwrap();
        assert_schema::<schema::system::System>(&second);
        assert!(second["context_switches_ps"].as_u64().unwrap() > 0);
        assert_eq!(second["forks_ps"], 0);
    }

    #[test]
    fn missing_stat_is_an_error_not_zeroes() {
        let f = Fixture::new();
        let mut c = System::new(NoOptions {}, f.sys()).unwrap();
        assert!(c.sample().is_err());
    }

    #[test]
    fn meta_keeps_unreadable_dmi_fields_as_null() {
        let f = fixture(STAT);
        let mut c = System::new(NoOptions {}, f.sys()).unwrap();
        let m = c.meta().unwrap();
        assert_schema::<schema::system::SystemMeta>(&m);
        assert_eq!(m["distro"], "Arch Linux");
        assert_eq!(m["dmi"]["board_name"], "ROG");
        assert!(m["dmi"]["bios_date"].is_null());
    }
}
