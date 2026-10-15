use std::collections::HashMap;

use anyhow::{Context, Result};
use schema::disk::Device;
use serde_json::Value;

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

pub use sysman_config::modules::disk::Options;

const READS: usize = 0;
const READ_BYTES: usize = 1;
const READ_MS: usize = 2;
const WRITES: usize = 3;
const WRITE_BYTES: usize = 4;
const WRITE_MS: usize = 5;
const IO_TICKS_MS: usize = 6;
const WEIGHTED_MS: usize = 7;
const MS_FIELDS: [usize; 4] = [READ_MS, WRITE_MS, IO_TICKS_MS, WEIGHTED_MS];

struct Stats {
    counters: [u64; 8],
    in_flight: u64,
}

pub struct Disk {
    sys: Sys,
    opts: Options,
    deltas: Deltas<String>,
    /// Per device: last raw value and the running total of each millisecond field.
    unwrapped: HashMap<String, [(u64, u64); 4]>,
}

impl Disk {
    pub fn new(opts: Options, sys: Sys) -> Result<Self> {
        Ok(Self {
            sys,
            opts,
            deltas: Deltas::default(),
            unwrapped: HashMap::new(),
        })
    }

    fn device_key(&self, name: &str) -> String {
        if name.starts_with("dm-")
            && let Some(dm) = self
                .sys
                .read_opt(format!("/sys/block/{name}/dm/name"))
                .filter(|n| !n.is_empty())
        {
            return dm;
        }
        name.to_string()
    }

    fn read_stats(&self) -> Result<HashMap<String, Stats>> {
        let whole = self.sys.list("/sys/block");
        let text = self.sys.read("/proc/diskstats")?;
        let mut out = HashMap::new();
        for line in text.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() < 14 {
                continue;
            }
            let name = p[2];
            if self.opts.exclude.iter().any(|x| name.starts_with(x.as_str())) {
                continue;
            }
            if !self.opts.partitions && !whole.iter().any(|w| w == name) {
                continue;
            }
            let n = |i: usize| {
                p[i].parse::<u64>()
                    .with_context(|| format!("/proc/diskstats: {line:?}"))
            };
            let counters = [
                n(3)?,
                n(5)?.saturating_mul(512),
                n(6)?,
                n(7)?,
                n(9)?.saturating_mul(512),
                n(10)?,
                n(12)?,
                n(13)?,
            ];
            out.insert(
                self.device_key(name),
                Stats {
                    counters,
                    in_flight: n(11)?,
                },
            );
        }
        Ok(out)
    }

    /// Accumulates the millisecond fields so a 32-bit wrap shows up as a normal increase.
    fn unwrap_ms(&mut self, stats: &HashMap<String, Stats>) -> HashMap<String, Vec<u64>> {
        self.unwrapped.retain(|k, _| stats.contains_key(k));
        let mut out = HashMap::new();
        for (key, s) in stats {
            let mut counters = s.counters.to_vec();
            match self.unwrapped.get_mut(key) {
                None => {
                    self.unwrapped
                        .insert(key.clone(), MS_FIELDS.map(|f| (s.counters[f], s.counters[f])));
                }
                Some(state) => {
                    for (slot, f) in state.iter_mut().zip(MS_FIELDS) {
                        let raw = s.counters[f];
                        *slot = (raw, slot.1.saturating_add(wrap_delta(slot.0, raw)));
                    }
                }
            }
            for (slot, f) in self.unwrapped[key].iter().zip(MS_FIELDS) {
                counters[f] = slot.1;
            }
            out.insert(key.clone(), counters);
        }
        out
    }
}

const WRAP: u64 = 1 << 32;

/// Some kernels print this counter as 32 bits, so a drop from below 2^32 is a wrap, not a reset.
fn wrap_delta(old: u64, new: u64) -> u64 {
    if new >= old {
        new - old
    } else if old < WRAP {
        (new + WRAP - old) % WRAP
    } else {
        0
    }
}

fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

fn device(d: &[u64], dt: f64, s: &Stats) -> Device {
    let per_op = |ms: u64, ops: u64| {
        if ops == 0 {
            0.0
        } else {
            round(ms as f64 / ops as f64, 2)
        }
    };
    Device {
        read_bps: (d[READ_BYTES] as f64 / dt).round() as u64,
        write_bps: (d[WRITE_BYTES] as f64 / dt).round() as u64,
        read_iops: round(d[READS] as f64 / dt, 1),
        write_iops: round(d[WRITES] as f64 / dt, 1),
        read_await_ms: per_op(d[READ_MS], d[READS]),
        write_await_ms: per_op(d[WRITE_MS], d[WRITES]),
        busy_pct: round((d[IO_TICKS_MS] as f64 / dt / 10.0).min(100.0), 1),
        queue_depth: round(d[WEIGHTED_MS] as f64 / (dt * 1000.0), 2),
        in_flight: s.in_flight,
        read_bytes_total: s.counters[READ_BYTES],
        write_bytes_total: s.counters[WRITE_BYTES],
    }
}

impl Collector for Disk {
    fn sample(&mut self) -> Result<Option<Value>> {
        let stats = self.read_stats()?;
        let counters = self.unwrap_ms(&stats);
        let Some((dt, deltas)) = self.deltas.update(counters) else {
            return Ok(None);
        };
        if deltas.is_empty() {
            return Ok(None);
        }
        let out: schema::disk::Disk = deltas
            .iter()
            .map(|(k, d)| (k.clone(), device(d, dt, &stats[k])))
            .collect();
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    fn line(name: &str, reads: u64, read_ms: u64, writes: u64, write_ms: u64, ticks: u64, weighted: u64) -> String {
        format!(
            " 259 0 {name} {reads} 0 {} {read_ms} {writes} 0 {} {write_ms} 3 {ticks} {weighted} 0 0 0 0 0 0\n",
            reads * 8,
            writes * 8
        )
    }

    fn fixture(diskstats: &str) -> Fixture {
        Fixture::new()
            .file("proc/diskstats", diskstats)
            .file("sys/block/nvme0n1/size", "1\n")
            .file("sys/block/dm-0/dm/name", "cryptroot\n")
            .file("sys/block/loop0/size", "1\n")
    }

    fn two_samples(first: &str, second: &str, opts: Options) -> Value {
        let f = fixture(first);
        let mut c = Disk::new(opts, f.sys()).unwrap();
        assert!(c.sample().unwrap().is_none());
        std::fs::write(f.path().join("proc/diskstats"), second).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        c.sample().unwrap().unwrap()
    }

    #[test]
    fn whole_devices_only_unless_partitions_and_loop_excluded() {
        let stats = |n| {
            line("nvme0n1", n, 10 * n, 0, 0, n, n)
                + &line("nvme0n1p1", n, 10 * n, 0, 0, n, n)
                + &line("loop0", n, 10 * n, 0, 0, n, n)
        };
        let v = two_samples(&stats(100), &stats(200), Options::default());
        assert_schema::<schema::disk::Disk>(&v);
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["nvme0n1"]);
        assert_eq!(v["nvme0n1"]["read_await_ms"], 10.0);
        assert_eq!(v["nvme0n1"]["read_bytes_total"], 200 * 8 * 512);

        let v = two_samples(
            &stats(100),
            &stats(200),
            Options {
                partitions: true,
                ..Options::default()
            },
        );
        assert!(v.get("nvme0n1p1").is_some());
        assert!(v.get("loop0").is_none());
    }

    #[test]
    fn first_sample_has_no_rate() {
        let f = fixture(&line("nvme0n1", 1, 1, 1, 1, 1, 1));
        let mut c = Disk::new(Options::default(), f.sys()).unwrap();
        assert!(c.sample().unwrap().is_none());
    }

    #[test]
    fn dm_devices_are_keyed_by_stable_name_not_dm_number() {
        // dm-N numbers change between boots, which would split one device's history in two
        let stats = |n| line("dm-0", n, n, 0, 0, 0, 0);
        let v = two_samples(&stats(1), &stats(5), Options::default());
        assert!(v.get("dm-0").is_none());
        assert!(v["cryptroot"]["read_iops"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn dm_device_without_a_name_file_keeps_its_kernel_name() {
        let f = Fixture::new()
            .file("proc/diskstats", &line("dm-3", 1, 1, 0, 0, 0, 0))
            .file("sys/block/dm-3/size", "1\n");
        let c = Disk::new(Options::default(), f.sys()).unwrap();
        assert!(c.read_stats().unwrap().contains_key("dm-3"));
    }

    #[test]
    fn wrapped_32_bit_millisecond_counter_gives_the_true_latency() {
        // without wrap handling the delta saturates to 0 and a busy disk reads as idle
        let old = 4_294_967_000;
        let stats = |reads, ms| line("nvme0n1", reads, ms, 0, 0, ms, ms);
        let v = two_samples(&stats(100, old), &stats(110, 500), Options::default());
        // 796 ms over 10 reads
        assert_eq!(v["nvme0n1"]["read_await_ms"], 79.6);
        assert!(v["nvme0n1"]["busy_pct"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn wrap_delta_distinguishes_wrap_from_reset() {
        assert_eq!(wrap_delta(10, 25), 15);
        assert_eq!(wrap_delta(WRAP - 100, 50), 150);
        // a 64-bit counter that went backwards was reset, not wrapped
        assert_eq!(wrap_delta(WRAP + 5, 3), 0);
    }

    #[test]
    fn wrap_survives_across_several_samples() {
        let f = fixture(&line("nvme0n1", 0, WRAP - 100, 0, 0, 0, 0));
        let mut c = Disk::new(Options::default(), f.sys()).unwrap();
        let stats = c.read_stats().unwrap();
        c.unwrap_ms(&stats);
        std::fs::write(f.path().join("proc/diskstats"), line("nvme0n1", 0, 50, 0, 0, 0, 0)).unwrap();
        let stats = c.read_stats().unwrap();
        assert_eq!(c.unwrap_ms(&stats)["nvme0n1"][READ_MS], WRAP + 50);
        std::fs::write(f.path().join("proc/diskstats"), line("nvme0n1", 0, 80, 0, 0, 0, 0)).unwrap();
        let stats = c.read_stats().unwrap();
        assert_eq!(c.unwrap_ms(&stats)["nvme0n1"][READ_MS], WRAP + 80);
    }

    #[test]
    fn queue_depth_is_weighted_time_over_elapsed_time() {
        // 4000 ms of weighted queue time in 2 s means two requests queued on average
        let s = Stats {
            counters: [0; 8],
            in_flight: 3,
        };
        let mut d = vec![0u64; 8];
        d[WEIGHTED_MS] = 4000;
        d[IO_TICKS_MS] = 1000;
        let dev = device(&d, 2.0, &s);
        assert_eq!(dev.queue_depth, 2.0);
        assert_eq!(dev.busy_pct, 50.0);
        assert_eq!(dev.in_flight, 3);
    }

    #[test]
    fn rates_are_not_capped() {
        // a fast array can exceed any fixed ceiling; capping would hide real data
        let s = Stats {
            counters: [0; 8],
            in_flight: 0,
        };
        let mut d = vec![0u64; 8];
        d[READ_BYTES] = 200_000_000_000;
        assert_eq!(device(&d, 2.0, &s).read_bps, 100_000_000_000);
    }
}
