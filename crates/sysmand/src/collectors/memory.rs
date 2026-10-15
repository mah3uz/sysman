use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use schema::memory::Zram;
use serde_json::Value;

use crate::collector::{Collector, NoOptions};
use crate::deltas::Deltas;
use crate::sys::Sys;

const ARCSTATS: &str = "/proc/spl/kstat/zfs/arcstats";
const VMSTAT_RATES: [&str; 6] = ["pgfault", "pgmajfault", "pswpin", "pswpout", "pgpgin", "pgpgout"];

pub struct Memory {
    sys: Sys,
    deltas: Deltas<&'static str>,
}

impl Memory {
    pub fn new(_: NoOptions, sys: Sys) -> Result<Self> {
        Ok(Self {
            sys,
            deltas: Deltas::default(),
        })
    }

    fn zfs_arc_bytes(&self) -> Option<u64> {
        self.sys.read_opt(ARCSTATS)?.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == "size").then(|| words.nth(1)?.parse().ok())?
        })
    }

    fn zram(&self) -> BTreeMap<String, Zram> {
        let mut out = BTreeMap::new();
        for dev in self
            .sys
            .list("/sys/block")
            .into_iter()
            .filter(|d| d.starts_with("zram"))
        {
            let Some(text) = self.sys.read_opt(format!("/sys/block/{dev}/mm_stat")) else {
                continue;
            };
            let v: Vec<u64> = text.split_whitespace().take(3).filter_map(|w| w.parse().ok()).collect();
            if let [orig_bytes, compressed_bytes, mem_used_bytes] = v[..] {
                out.insert(
                    dev,
                    Zram {
                        orig_bytes,
                        compressed_bytes,
                        mem_used_bytes,
                    },
                );
            }
        }
        out
    }
}

impl Collector for Memory {
    fn sample(&mut self) -> Result<Option<Value>> {
        let mut info: HashMap<String, u64> = HashMap::new();
        for line in self.sys.read("/proc/meminfo")?.lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            let mut words = rest.split_whitespace();
            let Some(n) = words.next().and_then(|n| n.parse::<u64>().ok()) else {
                continue;
            };
            info.insert(key.to_string(), if words.next() == Some("kB") { n * 1024 } else { n });
        }
        let need = |k: &str| {
            info.get(k)
                .copied()
                .with_context(|| format!("/proc/meminfo has no {k}"))
        };
        let get = |k: &str| info.get(k).copied();
        let (total, available) = (need("MemTotal")?, need("MemAvailable")?);
        if total == 0 {
            bail!("/proc/meminfo: MemTotal is 0");
        }
        let (swap_total, swap_free) = (need("SwapTotal")?, need("SwapFree")?);

        let zfs_arc_bytes = self.zfs_arc_bytes();
        // MemAvailable does not count the ARC as reclaimable, though it shrinks under pressure
        let used = total
            .saturating_sub(available)
            .saturating_sub(zfs_arc_bytes.unwrap_or(0));
        let cache_bytes =
            (|| Some((get("Cached")? + get("SReclaimable")? + get("Buffers")?).saturating_sub(get("Shmem")?)))();

        let mut vm: HashMap<&'static str, Vec<u64>> = HashMap::new();
        let mut oom_kills_total = None;
        for line in self.sys.read("/proc/vmstat")?.lines() {
            let mut words = line.split_whitespace();
            let (Some(key), Some(value)) = (words.next(), words.next().and_then(|v| v.parse().ok())) else {
                continue;
            };
            if key == "oom_kill" {
                oom_kills_total = Some(value);
            } else if let Some(k) = VMSTAT_RATES.iter().find(|k| **k == key) {
                vm.insert(k, vec![value]);
            }
        }
        let rates = self.deltas.update(vm);
        let rate = |key: &str| -> Option<u64> {
            let (dt, d) = rates.as_ref()?;
            Some((*d.get(key)?.first()? as f64 / dt).round() as u64)
        };

        let out = schema::memory::Memory {
            total_bytes: total,
            available_bytes: available,
            swap_total_bytes: swap_total,
            swap_free_bytes: swap_free,
            used_bytes: used,
            used_pct: (1000.0 * used as f64 / total as f64).round() / 10.0,
            swap_used_bytes: swap_total.saturating_sub(swap_free),
            oom_kills_total,
            free_bytes: get("MemFree"),
            buffers_bytes: get("Buffers"),
            cached_bytes: get("Cached"),
            swap_cached_bytes: get("SwapCached"),
            dirty_bytes: get("Dirty"),
            writeback_bytes: get("Writeback"),
            anon_bytes: get("AnonPages"),
            mapped_bytes: get("Mapped"),
            shared_bytes: get("Shmem"),
            slab_bytes: get("Slab"),
            slab_reclaimable_bytes: get("SReclaimable"),
            kernel_stack_bytes: get("KernelStack"),
            page_tables_bytes: get("PageTables"),
            active_bytes: get("Active"),
            inactive_bytes: get("Inactive"),
            cache_bytes,
            zfs_arc_bytes,
            page_faults_ps: rate("pgfault"),
            major_faults_ps: rate("pgmajfault"),
            swap_in_pages_ps: rate("pswpin"),
            swap_out_pages_ps: rate("pswpout"),
            page_in_kb_ps: rate("pgpgin"),
            page_out_kb_ps: rate("pgpgout"),
            zram: self.zram(),
        };
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const MEMINFO: &str = "MemTotal:       131000708 kB
MemFree:        96207092 kB
MemAvailable:   114574888 kB
Buffers:            2624 kB
Cached:         23937696 kB
SwapCached:            0 kB
Active:          7851364 kB
Inactive:       23637492 kB
SwapTotal:       4194300 kB
SwapFree:        4194300 kB
Dirty:              1536 kB
Writeback:             0 kB
AnonPages:       7427452 kB
Mapped:          2118580 kB
Shmem:            299792 kB
Slab:             903036 kB
SReclaimable:     498984 kB
KernelStack:       40000 kB
PageTables:       103988 kB
HugePages_Total:       0
";
    const VMSTAT: &str = "pgpgin 12431758
pgpgout 12670293
pswpin 0
pswpout 0
pgfault 217167922
pgmajfault 15728
oom_kill 2
";

    fn fixture(meminfo: &str) -> Fixture {
        Fixture::new()
            .file("proc/meminfo", meminfo)
            .file("proc/vmstat", VMSTAT)
            .file(
                "sys/block/zram0/mm_stat",
                "    4096       64    20480        0    20480        0        0        0        0\n",
            )
    }

    fn sample(f: &Fixture) -> Value {
        let mut c = Memory::new(NoOptions {}, f.sys()).unwrap();
        c.sample().unwrap().unwrap()
    }

    #[test]
    fn reads_meminfo_in_bytes_and_derives_used_from_available() {
        let v = sample(&fixture(MEMINFO));
        assert_schema::<schema::memory::Memory>(&v);
        assert_eq!(v["total_bytes"], 131000708u64 * 1024);
        assert_eq!(v["used_bytes"], (131000708u64 - 114574888) * 1024);
        assert_eq!(v["used_pct"], 12.5);
        assert_eq!(v["swap_used_bytes"], 0);
        assert_eq!(v["oom_kills_total"], 2);
        assert_eq!(v["zram"]["zram0"]["mem_used_bytes"], 20480);
        assert!(v.get("zfs_arc_bytes").is_none());
    }

    #[test]
    fn cache_excludes_shmem_which_is_inside_cached() {
        let v = sample(&fixture(MEMINFO));
        assert_eq!(v["cache_bytes"], (23937696u64 + 498984 + 2624 - 299792) * 1024);
    }

    #[test]
    fn zfs_arc_is_taken_out_of_used() {
        let arc = "name type data\nhits 4 100\nsize 4 3221225472\nc 4 9\n";
        let f = fixture(MEMINFO).file("proc/spl/kstat/zfs/arcstats", arc);
        let v = sample(&f);
        assert_schema::<schema::memory::Memory>(&v);
        let used = (131000708u64 - 114574888) * 1024 - 3221225472;
        assert_eq!(v["zfs_arc_bytes"], 3221225472u64);
        assert_eq!(v["used_bytes"], used);
        assert_eq!(
            v["used_pct"],
            (1000.0 * used as f64 / (131000708u64 * 1024) as f64).round() / 10.0
        );
    }

    #[test]
    fn arc_larger_than_used_and_inverted_counters_saturate_at_zero() {
        // /proc files change between reads; a wrapped u64 would chart as 18 exabytes
        let meminfo = MEMINFO
            .replace("MemAvailable:   114574888", "MemAvailable:   131000708")
            .replace("Shmem:            299792", "Shmem:         99999999")
            .replace("SwapFree:        4194300", "SwapFree:        9999999");
        let f = fixture(&meminfo).file("proc/spl/kstat/zfs/arcstats", "size 4 99999999999\n");
        let v = sample(&f);
        assert_schema::<schema::memory::Memory>(&v);
        assert_eq!(v["used_bytes"], 0);
        assert_eq!(v["used_pct"], 0.0);
        assert_eq!(v["cache_bytes"], 0);
        assert_eq!(v["swap_used_bytes"], 0);
    }

    #[test]
    fn available_above_total_does_not_wrap() {
        let meminfo = MEMINFO.replace("MemAvailable:   114574888", "MemAvailable:   999999999");
        let v = sample(&fixture(&meminfo));
        assert_eq!(v["used_bytes"], 0);
    }

    #[test]
    fn rates_need_two_readings() {
        let f = fixture(MEMINFO);
        let mut c = Memory::new(NoOptions {}, f.sys()).unwrap();
        let first = c.sample().unwrap().unwrap();
        assert!(first.get("page_faults_ps").is_none());
        std::fs::write(
            f.path().join("proc/vmstat"),
            VMSTAT.replace("pgfault 217167922", "pgfault 217267922"),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = c.sample().unwrap().unwrap();
        assert_schema::<schema::memory::Memory>(&second);
        assert!(second["page_faults_ps"].as_u64().unwrap() > 0);
        assert_eq!(second["major_faults_ps"], 0);
    }

    #[test]
    fn missing_available_is_an_error_not_a_guess() {
        let f = fixture(&MEMINFO.replace("MemAvailable:   114574888 kB\n", ""));
        let mut c = Memory::new(NoOptions {}, f.sys()).unwrap();
        assert!(c.sample().is_err());
    }
}
