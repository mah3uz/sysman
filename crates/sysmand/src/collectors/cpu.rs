use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use schema::cpu::{Breakdown, CoreTypes, L3Group, Topology};
use serde::Deserialize;
use serde_json::Value;

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

const CPU_DIR: &str = "/sys/devices/system/cpu";
const HWMON: &str = "/sys/class/hwmon";
const RAPL: &str = "/sys/class/powercap/intel-rapl:0";
/// hwmon chip name → labels that mean "package temperature", most preferred first.
const TEMP_CHIPS: [(&str, &[&str]); 3] = [
    ("k10temp", &["Tdie", "Tctl"]),
    ("zenpower", &["Tdie", "Tctl"]),
    ("coretemp", &["Package id 0"]),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    pub per_core: bool,
    pub frequency: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            per_core: true,
            frequency: true,
        }
    }
}

struct TempSource {
    key: String,
    package: String,
    others: Vec<(String, String)>,
}

struct Rapl {
    energy: String,
    max_range_uj: u64,
    prev: Option<(u64, Instant)>,
}

impl Rapl {
    fn new(sys: &Sys) -> Option<Self> {
        // energy_uj is root-only on most systems; an unreadable counter means no power field
        sys.int_opt(format!("{RAPL}/energy_uj"))?;
        Some(Self {
            energy: format!("{RAPL}/energy_uj"),
            max_range_uj: sys.int_opt(format!("{RAPL}/max_energy_range_uj"))?.try_into().ok()?,
            prev: None,
        })
    }

    fn watts(&mut self, sys: &Sys, now: Instant) -> Option<f64> {
        let energy = u64::try_from(sys.int_opt(&self.energy)?).ok()?;
        self.step(energy, now)
    }

    fn step(&mut self, energy: u64, now: Instant) -> Option<f64> {
        let (old, t) = self.prev.replace((energy, now))?;
        let dt = now.duration_since(t).as_secs_f64();
        let uj = if energy >= old {
            energy - old
        } else {
            // the counter wrapped at max_energy_range_uj
            self.max_range_uj.checked_sub(old)? + energy
        };
        (dt > 0.0).then(|| uj as f64 / 1e6 / dt)
    }
}

pub struct Cpu {
    sys: Sys,
    per_core: bool,
    frequency: bool,
    deltas: Deltas<String>,
    temp: Option<TempSource>,
    rapl: Option<Rapl>,
}

impl Cpu {
    pub fn new(options: Options, sys: Sys) -> Result<Self> {
        let temp = resolve_temp(&sys);
        let rapl = Rapl::new(&sys);
        Ok(Self {
            sys,
            per_core: options.per_core,
            frequency: options.frequency,
            deltas: Deltas::default(),
            temp,
            rapl,
        })
    }

    fn cpu_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .sys
            .list(CPU_DIR)
            .iter()
            .filter_map(|n| n.strip_prefix("cpu")?.parse().ok())
            .filter(|id| self.sys.read_opt(format!("{CPU_DIR}/cpu{id}/online")).as_deref() != Some("0"))
            .collect();
        ids.sort_unstable();
        ids
    }

    fn topology(&self) -> Topology {
        let ids = self.cpu_ids();
        let mut packages = BTreeSet::new();
        let mut cores = BTreeSet::new();
        let mut l3: BTreeMap<Vec<u32>, u64> = BTreeMap::new();
        for id in &ids {
            let cpu = format!("{CPU_DIR}/cpu{id}");
            let pkg = self.sys.int_opt(format!("{cpu}/topology/physical_package_id"));
            let core = self.sys.int_opt(format!("{cpu}/topology/core_id"));
            packages.extend(pkg);
            if let (Some(pkg), Some(core)) = (pkg, core) {
                cores.insert((pkg, core));
            }
            let group = self.sys.read_opt(format!("{cpu}/cache/index3/shared_cpu_list"));
            let kib = self
                .sys
                .read_opt(format!("{cpu}/cache/index3/size"))
                .and_then(|s| parse_kib(&s));
            if let (Some(group), Some(kib)) = (group, kib) {
                l3.insert(parse_cpulist(&group), kib);
            }
        }

        let list = |path: &str| self.sys.read_opt(path).map(|s| parse_cpulist(&s));
        let (performance, efficiency) = (list("/sys/devices/cpu_core/cpus"), list("/sys/devices/cpu_atom/cpus"));
        let core_types = (performance.is_some() || efficiency.is_some()).then(|| CoreTypes {
            performance: performance.unwrap_or_default(),
            efficiency: efficiency.unwrap_or_default(),
        });

        let ranking: Vec<Option<u64>> = (0..ids.last().map_or(0, |m| m + 1))
            .map(|id| {
                let v = self
                    .sys
                    .int_opt(format!("{CPU_DIR}/cpu{id}/cpufreq/amd_pstate_prefcore_ranking"))?;
                u64::try_from(v).ok()
            })
            .collect();

        Topology {
            packages: (!packages.is_empty()).then_some(packages.len() as u32),
            cores: (!cores.is_empty()).then_some(cores.len() as u32),
            threads: ids.len() as u32,
            l3_groups: l3.into_iter().map(|(cpus, l3_kib)| L3Group { cpus, l3_kib }).collect(),
            core_types,
            prefcore_ranking: ranking.iter().any(Option::is_some).then_some(ranking),
        }
    }

    fn temps(&self) -> (Option<f64>, BTreeMap<String, f64>) {
        let Some(t) = &self.temp else {
            return (None, BTreeMap::new());
        };
        let others = t
            .others
            .iter()
            .filter_map(|(label, path)| Some((label.clone(), read_temp(&self.sys, path)?)))
            .collect();
        (read_temp(&self.sys, &t.package), others)
    }
}

impl Collector for Cpu {
    fn meta(&mut self) -> Option<Value> {
        let cpuinfo = self.sys.read_opt("/proc/cpuinfo").unwrap_or_default();
        let field = |name: &str| {
            cpuinfo
                .lines()
                .find_map(|l| l.strip_prefix(name)?.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        };
        let freq = |leaf: &str| self.sys.read_opt(format!("{CPU_DIR}/cpu0/cpufreq/{leaf}"));
        let mhz = |leaf: &str| Some(freq(leaf)?.parse::<u64>().ok()? / 1000);
        let meta = schema::cpu::CpuMeta {
            model: field("model name"),
            vendor: field("vendor_id"),
            driver: freq("scaling_driver"),
            governor: freq("scaling_governor"),
            energy_performance_preference: freq("energy_performance_preference"),
            min_mhz: mhz("cpuinfo_min_freq"),
            max_mhz: mhz("cpuinfo_max_freq"),
            temp_source: self.temp.as_ref().map(|t| t.key.clone()),
            power_source: self.rapl.is_some().then(|| "rapl".to_string()),
            topology: self.topology(),
        };
        serde_json::to_value(meta).ok()
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let power_w = self.rapl.as_mut().and_then(|r| r.watts(&self.sys, Instant::now()));
        let counters = parse_stat(&self.sys.read("/proc/stat")?)?;
        let Some((_, d)) = self.deltas.update(counters) else {
            return Ok(None);
        };
        let Some(all) = d.get("cpu") else {
            return Ok(None);
        };

        let total: u64 = all.iter().sum();
        let share = |v: u64| round1(100.0 * v as f64 / total.max(1) as f64);
        let breakdown = Breakdown {
            user: share(all[0]),
            nice: share(all[1]),
            system: share(all[2]),
            iowait: share(all[4]),
            irq: share(all[5]),
            softirq: share(all[6]),
            steal: share(all[7]),
            guest: share(all[8]),
        };

        let mut cores: Vec<(u32, &Vec<u64>)> = d
            .iter()
            .filter_map(|(n, v)| Some((n.strip_prefix("cpu")?.parse().ok()?, v)))
            .collect();
        cores.sort_unstable_by_key(|(id, _)| *id);

        let mhz: Vec<Option<u64>> = if self.frequency {
            cores
                .iter()
                .map(|(id, _)| {
                    let khz = self
                        .sys
                        .int_opt(format!("{CPU_DIR}/cpu{id}/cpufreq/scaling_cur_freq"))?;
                    u64::try_from(khz).ok().map(|k| k / 1000)
                })
                .collect()
        } else {
            Vec::new()
        };
        let known: Vec<u64> = mhz.iter().flatten().copied().collect();
        let (temp_c, temps_c) = self.temps();

        let out = schema::cpu::Cpu {
            total_pct: busy_pct(all),
            breakdown_pct: breakdown,
            core_pct: self.per_core.then(|| cores.iter().map(|(_, v)| busy_pct(v)).collect()),
            avg_mhz: (!known.is_empty())
                .then(|| (known.iter().sum::<u64>() as f64 / known.len() as f64).round() as u64),
            core_mhz: (self.per_core && !known.is_empty()).then_some(mhz),
            temp_c,
            temps_c,
            power_w,
        };
        Ok(Some(serde_json::to_value(out)?))
    }
}

/// Per `cpu`/`cpuN` line: user, nice, system, idle, iowait, irq, softirq, steal, guest.
/// The kernel already counts guest time inside user and nice, so it is moved out of them.
fn parse_stat(text: &str) -> Result<HashMap<String, Vec<u64>>> {
    let mut out = HashMap::new();
    for line in text.lines().filter(|l| l.starts_with("cpu")) {
        let mut words = line.split_whitespace();
        let name = words.next().unwrap_or_default();
        let mut v: Vec<u64> = words
            .map(|w| w.parse())
            .collect::<Result<_, _>>()
            .with_context(|| format!("/proc/stat: {line:?}"))?;
        if v.len() < 4 {
            bail!("/proc/stat: {line:?}");
        }
        v.resize(10, 0);
        out.insert(
            name.to_string(),
            vec![
                v[0].saturating_sub(v[8]),
                v[1].saturating_sub(v[9]),
                v[2],
                v[3],
                v[4],
                v[5],
                v[6],
                v[7],
                v[8] + v[9],
            ],
        );
    }
    if !out.contains_key("cpu") {
        bail!("/proc/stat has no cpu line");
    }
    Ok(out)
}

fn busy_pct(d: &[u64]) -> f64 {
    let total: u64 = d.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let busy = total.saturating_sub(d[3]).saturating_sub(d[4]);
    round1(100.0 * busy as f64 / total as f64).min(100.0)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn parse_cpulist(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.split(',') {
        match part.trim().split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.trim().parse::<u32>()),
        }
    }
    out
}

fn parse_kib(s: &str) -> Option<u64> {
    match s.strip_suffix('K') {
        Some(n) => n.parse().ok(),
        None => Some(s.strip_suffix('M')?.parse::<u64>().ok()? * 1024),
    }
}

/// Millidegrees to °C; 0 and values past 200 are placeholders from drivers without a reading.
fn read_temp(sys: &Sys, path: &str) -> Option<f64> {
    let c = sys.int_opt(path)? as f64 / 1000.0;
    (c > 0.0 && c < 200.0).then_some(c)
}

fn resolve_temp(sys: &Sys) -> Option<TempSource> {
    struct Chip {
        name: String,
        key: String,
        dir: String,
    }
    let chips: Vec<Chip> = sys
        .list(HWMON)
        .into_iter()
        .filter(|h| h.starts_with("hwmon"))
        .map(|h| {
            let dir = format!("{HWMON}/{h}");
            let name = sys.read_opt(format!("{dir}/name")).unwrap_or(h);
            // hwmon numbers change between boots; the device name does not
            let device = std::fs::canonicalize(sys.path(format!("{dir}/device")))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "device".to_string());
            Chip {
                key: format!("{name}/{device}"),
                name,
                dir,
            }
        })
        .collect();

    for (name, preferred) in TEMP_CHIPS {
        let mut matching = chips.iter().filter(|c| c.name == name);
        // coretemp has one chip per package; the others are a single chip
        let selected: Vec<&Chip> = if name == "coretemp" {
            matching.collect()
        } else {
            matching.next().into_iter().collect()
        };

        // (label, path, index into selected)
        let mut sensors: Vec<(String, String, usize)> = Vec::new();
        for (i, chip) in selected.iter().enumerate() {
            let inputs: Vec<(String, String)> = sys
                .list(&chip.dir)
                .into_iter()
                .filter_map(|f| {
                    let stem = f.strip_suffix("_input")?;
                    stem.strip_prefix("temp")?.parse::<u32>().ok()?;
                    let label = sys
                        .read_opt(format!("{}/{stem}_label", chip.dir))
                        .unwrap_or(stem.to_string());
                    Some((label, format!("{}/{f}", chip.dir)))
                })
                .collect();
            let package = inputs.iter().find_map(|(l, _)| l.strip_prefix("Package id "));
            // core numbers restart in every package
            let prefix = package.filter(|p| *p != "0").map(|p| format!("pkg{p} "));
            for (label, path) in &inputs {
                let label = match &prefix {
                    Some(p) if !label.starts_with("Package id ") => format!("{p}{label}"),
                    _ => label.clone(),
                };
                sensors.push((label, path.clone(), i));
            }
        }

        let Some(at) = preferred
            .iter()
            .find_map(|p| sensors.iter().position(|(l, _, _)| l == p))
        else {
            continue;
        };
        let (_, package, chip) = sensors.remove(at);
        return Some(TempSource {
            key: selected[chip].key.clone(),
            package,
            others: sensors.into_iter().map(|(l, p, _)| (l, p)).collect(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    fn stat(cpu: &str, cores: &[&str]) -> String {
        let mut s = format!("cpu  {cpu}\n");
        for (i, c) in cores.iter().enumerate() {
            s += &format!("cpu{i} {c}\n");
        }
        s + "intr 1\nctxt 2\n"
    }

    fn cpu_entry(f: Fixture, id: u32, pkg: u32, core: u32, l3: &str, l3_size: &str) -> Fixture {
        let d = format!("sys/devices/system/cpu/cpu{id}");
        f.file(&format!("{d}/topology/physical_package_id"), &format!("{pkg}\n"))
            .file(&format!("{d}/topology/core_id"), &format!("{core}\n"))
            .file(&format!("{d}/cache/index3/shared_cpu_list"), &format!("{l3}\n"))
            .file(&format!("{d}/cache/index3/size"), &format!("{l3_size}\n"))
    }

    fn hwmon(f: Fixture, n: u32, name: &str, device: &str, sensors: &[(u32, Option<&str>, i64)]) -> Fixture {
        let dir = format!("sys/class/hwmon/hwmon{n}");
        let mut f = f.file(&format!("{dir}/name"), &format!("{name}\n"));
        for (i, label, milli) in sensors {
            f = f.file(&format!("{dir}/temp{i}_input"), &format!("{milli}\n"));
            if let Some(l) = label {
                f = f.file(&format!("{dir}/temp{i}_label"), &format!("{l}\n"));
            }
        }
        let target = f.path().join("sys/devices").join(device);
        std::fs::create_dir_all(&target).unwrap();
        symlink(format!("../../../devices/{device}"), f.path().join(&dir).join("device")).unwrap();
        f
    }

    fn sample_twice(f: &Fixture, first: &str, second: &str) -> (Cpu, Value) {
        let mut c = Cpu::new(Options::default(), f.sys()).unwrap();
        std::fs::create_dir_all(f.path().join("proc")).unwrap();
        std::fs::write(f.path().join("proc/stat"), first).unwrap();
        assert!(
            c.sample().unwrap().is_none(),
            "the first reading has nothing to difference against"
        );
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(f.path().join("proc/stat"), second).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::cpu::Cpu>(&v);
        (c, v)
    }

    const IDLE_THEN_BUSY: (&str, &str) = ("100 0 100 800 0 0 0 0 0 0", "200 0 200 1600 0 0 0 0 0 0");

    fn amd() -> Fixture {
        let mut f = Fixture::new().file(
            "proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 9 7950X3D 16-Core Processor\n",
        );
        f = hwmon(
            f,
            0,
            "nvme",
            "pci0000:00/0000:03:00.0",
            &[(1, Some("Composite"), 35000)],
        );
        f = hwmon(
            f,
            5,
            "k10temp",
            "pci0000:00/0000:00:18.3",
            &[
                (1, Some("Tctl"), 56125),
                (3, Some("Tccd1"), 51375),
                (4, Some("Tccd2"), 44875),
            ],
        );
        for id in 0..32u32 {
            let ccd = (id % 16) / 8;
            let (list, size) = if ccd == 0 {
                ("0-7,16-23", "98304K")
            } else {
                ("8-15,24-31", "32768K")
            };
            f = cpu_entry(f, id, 0, id % 16, list, size);
            let d = format!("sys/devices/system/cpu/cpu{id}/cpufreq");
            f = f
                .file(
                    &format!("{d}/amd_pstate_prefcore_ranking"),
                    &format!("{}\n", 201 + 25 * ccd),
                )
                .file(&format!("{d}/scaling_cur_freq"), "2985835\n");
        }
        let d = "sys/devices/system/cpu/cpu0/cpufreq";
        f.file(&format!("{d}/scaling_driver"), "amd-pstate-epp\n")
            .file(&format!("{d}/scaling_governor"), "powersave\n")
            .file(&format!("{d}/energy_performance_preference"), "balance_performance\n")
            .file(&format!("{d}/cpuinfo_min_freq"), "416629\n")
            .file(&format!("{d}/cpuinfo_max_freq"), "5763356\n")
            .file("sys/class/powercap/intel-rapl:0/name", "package-0\n")
            .file("sys/class/powercap/intel-rapl:0/max_energy_range_uj", "65532610987\n")
    }

    #[test]
    fn amd_reports_tctl_as_package_and_ccds_as_extras() {
        let f = amd();
        let (mut c, v) = sample_twice(
            &f,
            &stat(IDLE_THEN_BUSY.0, &[IDLE_THEN_BUSY.0, IDLE_THEN_BUSY.0]),
            &stat(IDLE_THEN_BUSY.1, &[IDLE_THEN_BUSY.1, IDLE_THEN_BUSY.1]),
        );
        assert_eq!(v["temp_c"], 56.125);
        assert_eq!(v["temps_c"], serde_json::json!({"Tccd1": 51.375, "Tccd2": 44.875}));
        assert_eq!(v["total_pct"], 20.0);
        assert_eq!(v["core_pct"], serde_json::json!([20.0, 20.0]));
        assert_eq!(v["core_mhz"], serde_json::json!([2985, 2985]));
        // energy_uj is root-only here, so there is no counter to read
        assert!(v.get("power_w").is_none());

        let m = c.meta().unwrap();
        assert_schema::<schema::cpu::CpuMeta>(&m);
        assert_eq!(m["vendor"], "AuthenticAMD");
        assert_eq!(m["model"], "AMD Ryzen 9 7950X3D 16-Core Processor");
        assert_eq!(m["driver"], "amd-pstate-epp");
        assert_eq!(m["max_mhz"], 5763);
        assert_eq!(m["temp_source"], "k10temp/0000:00:18.3");
        assert!(m["power_source"].is_null());
        let t = &m["topology"];
        assert_eq!(
            (&t["packages"], &t["cores"], &t["threads"]),
            (&1.into(), &16.into(), &32.into())
        );
        // two CCDs share nothing else that tells them apart; the V-Cache one is the 96 MiB group
        assert_eq!(t["l3_groups"][0]["l3_kib"], 98304);
        assert_eq!(
            t["l3_groups"][0]["cpus"],
            serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23])
        );
        assert_eq!(t["l3_groups"][1]["l3_kib"], 32768);
        assert_eq!(t["l3_groups"].as_array().unwrap().len(), 2);
        assert!(t.get("core_types").is_none());
        assert_eq!(t["prefcore_ranking"].as_array().unwrap().len(), 32);
        assert_eq!(t["prefcore_ranking"][0], 201);
        assert_eq!(t["prefcore_ranking"][8], 226);
    }

    fn intel_hybrid() -> Fixture {
        let mut f = Fixture::new().file(
            "proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: 13th Gen Intel(R) Core(TM) i7-13700K\n",
        );
        f = hwmon(
            f,
            3,
            "coretemp",
            "platform/coretemp.0",
            &[
                (1, Some("Package id 0"), 61000),
                (2, Some("Core 0"), 58000),
                (6, Some("Core 4"), 59000),
                (10, Some("Core 8"), 55000),
            ],
        );
        // P-cores 0-3 have two threads each (0-7), E-cores 8-11 one thread each
        for id in 0..12u32 {
            let core = if id < 8 { id / 2 * 4 } else { 16 + (id - 8) * 4 };
            f = cpu_entry(f, id, 0, core, "0-11", "30720K");
        }
        f.file("sys/devices/cpu_core/cpus", "0-7\n")
            .file("sys/devices/cpu_atom/cpus", "8-11\n")
            .file("sys/class/powercap/intel-rapl:0/energy_uj", "1000000\n")
            .file("sys/class/powercap/intel-rapl:0/max_energy_range_uj", "262143328850\n")
    }

    #[test]
    fn intel_hybrid_uses_coretemp_package_and_reports_core_types() {
        let f = intel_hybrid();
        let (mut c, v) = sample_twice(&f, &stat(IDLE_THEN_BUSY.0, &[]), &stat(IDLE_THEN_BUSY.1, &[]));
        assert_eq!(v["temp_c"], 61.0);
        assert_eq!(
            v["temps_c"],
            serde_json::json!({"Core 0": 58.0, "Core 4": 59.0, "Core 8": 55.0})
        );
        assert!(
            v.get("avg_mhz").is_none(),
            "no cpufreq files means no frequency, not 0 MHz"
        );

        let m = c.meta().unwrap();
        assert_schema::<schema::cpu::CpuMeta>(&m);
        assert_eq!(m["vendor"], "GenuineIntel");
        assert_eq!(m["temp_source"], "coretemp/coretemp.0");
        assert_eq!(m["power_source"], "rapl");
        let t = &m["topology"];
        assert_eq!(
            t["core_types"],
            serde_json::json!({"performance": [0, 1, 2, 3, 4, 5, 6, 7], "efficiency": [8, 9, 10, 11]})
        );
        assert_eq!((&t["cores"], &t["threads"]), (&8.into(), &12.into()));
        assert_eq!(t["l3_groups"].as_array().unwrap().len(), 1);
        assert!(t.get("prefcore_ranking").is_none());
    }

    #[test]
    fn several_coretemp_packages_keep_their_extra_sensors_apart() {
        // core numbers restart in each package; unprefixed they would overwrite each other
        let mut f = Fixture::new().file("proc/stat", &stat(IDLE_THEN_BUSY.0, &[]));
        f = hwmon(
            f,
            1,
            "coretemp",
            "platform/coretemp.0",
            &[(1, Some("Package id 0"), 50000), (2, Some("Core 0"), 48000)],
        );
        f = hwmon(
            f,
            2,
            "coretemp",
            "platform/coretemp.1",
            &[(1, Some("Package id 1"), 70000), (2, Some("Core 0"), 68000)],
        );
        let mut c = Cpu::new(Options::default(), f.sys()).unwrap();
        c.sample().unwrap();
        std::fs::write(f.path().join("proc/stat"), stat(IDLE_THEN_BUSY.1, &[])).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["temp_c"], 50.0);
        assert_eq!(
            v["temps_c"],
            serde_json::json!({"Core 0": 48.0, "Package id 1": 70.0, "pkg1 Core 0": 68.0})
        );
    }

    #[test]
    fn without_a_cpu_temperature_chip_the_fields_are_absent_and_sampling_works() {
        let f = hwmon(
            Fixture::new(),
            0,
            "nvme",
            "pci/0000:03:00.0",
            &[(1, Some("Composite"), 35000)],
        );
        let (mut c, v) = sample_twice(&f, &stat(IDLE_THEN_BUSY.0, &[]), &stat(IDLE_THEN_BUSY.1, &[]));
        assert!(v.get("temp_c").is_none());
        assert!(v.get("temps_c").is_none());
        let m = c.meta().unwrap();
        assert_schema::<schema::cpu::CpuMeta>(&m);
        assert!(m["temp_source"].is_null());
    }

    #[test]
    fn placeholder_temperatures_are_dropped() {
        let f = hwmon(
            Fixture::new(),
            0,
            "k10temp",
            "pci/0000:00:18.3",
            &[
                (1, Some("Tctl"), 0),
                (2, Some("Tccd1"), 200000),
                (3, Some("Tccd2"), -5000),
                (4, Some("Tccd3"), 199900),
                (5, Some("Tccd4"), 1),
            ],
        );
        let (_, v) = sample_twice(&f, &stat(IDLE_THEN_BUSY.0, &[]), &stat(IDLE_THEN_BUSY.1, &[]));
        assert!(
            v.get("temp_c").is_none(),
            "a 0 °C package reading is a driver placeholder"
        );
        assert_eq!(v["temps_c"], serde_json::json!({"Tccd3": 199.9, "Tccd4": 0.001}));
    }

    #[test]
    fn tdie_is_preferred_over_the_offset_tctl_on_older_zen() {
        let f = hwmon(
            Fixture::new(),
            0,
            "k10temp",
            "pci/0000:00:18.3",
            &[(1, Some("Tctl"), 80000), (2, Some("Tdie"), 53000)],
        );
        let (_, v) = sample_twice(&f, &stat(IDLE_THEN_BUSY.0, &[]), &stat(IDLE_THEN_BUSY.1, &[]));
        assert_eq!(v["temp_c"], 53.0);
        assert_eq!(v["temps_c"], serde_json::json!({"Tctl": 80.0}));
    }

    #[test]
    fn rapl_power_is_energy_over_time() {
        let mut r = Rapl {
            energy: String::new(),
            max_range_uj: 1_000_000,
            prev: None,
        };
        let t0 = Instant::now();
        assert!(r.step(100_000, t0).is_none());
        let w = r.step(300_000, t0 + Duration::from_secs(2)).unwrap();
        assert!((w - 0.1).abs() < 1e-9);
    }

    #[test]
    fn rapl_counter_wrap_is_not_a_negative_or_huge_reading() {
        let mut r = Rapl {
            energy: String::new(),
            max_range_uj: 1_000_000,
            prev: None,
        };
        let t0 = Instant::now();
        r.step(900_000, t0);
        // 100_000 µJ to the wrap point plus 50_000 after it
        let w = r.step(50_000, t0 + Duration::from_secs(1)).unwrap();
        assert!((w - 0.15).abs() < 1e-9);
        // a counter above its own range is garbage, not a wrap
        r.step(2_000_000, t0 + Duration::from_secs(2));
        assert!(r.step(10, t0 + Duration::from_secs(3)).is_none());
    }

    #[test]
    fn collector_reports_power_when_energy_is_readable() {
        let f = intel_hybrid();
        let mut c = Cpu::new(Options::default(), f.sys()).unwrap();
        std::fs::write(f.path().join("proc/stat"), stat(IDLE_THEN_BUSY.0, &[])).unwrap();
        assert!(c.sample().unwrap().is_none());
        std::fs::write(f.path().join("sys/class/powercap/intel-rapl:0/energy_uj"), "5000000\n").unwrap();
        std::fs::write(f.path().join("proc/stat"), stat(IDLE_THEN_BUSY.1, &[])).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::cpu::Cpu>(&v);
        assert!(v["power_w"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn guest_time_is_not_counted_twice() {
        // user already includes guest: 100 busy ticks (all guest) and 100 idle ticks must read 50%
        let f = Fixture::new();
        let (_, v) = sample_twice(
            &f,
            &stat("1000 0 0 1000 0 0 0 0 400 0", &[]),
            &stat("1100 0 0 1100 0 0 0 0 500 0", &[]),
        );
        assert_eq!(v["total_pct"], 50.0);
        assert_eq!(v["breakdown_pct"]["guest"], 50.0);
        assert_eq!(v["breakdown_pct"]["user"], 0.0);
    }

    #[test]
    fn guest_nice_comes_out_of_nice() {
        let f = Fixture::new();
        let (_, v) = sample_twice(
            &f,
            &stat("0 1000 0 1000 0 0 0 0 0 400", &[]),
            &stat("0 1100 0 1100 0 0 0 0 0 500", &[]),
        );
        assert_eq!(v["total_pct"], 50.0);
        assert_eq!(v["breakdown_pct"]["nice"], 0.0);
        assert_eq!(v["breakdown_pct"]["guest"], 50.0);
    }

    #[test]
    fn guest_larger_than_user_does_not_wrap() {
        let c = parse_stat("cpu 5 3 0 10 0 0 0 0 9 9\n").unwrap();
        assert_eq!(c["cpu"][..2], [0, 0]);
    }

    #[test]
    fn old_kernels_without_guest_columns_still_parse() {
        let c = parse_stat("cpu 5 3 1 10 2 0 0\n").unwrap();
        assert_eq!(c["cpu"], [5, 3, 1, 10, 2, 0, 0, 0, 0]);
    }

    #[test]
    fn options_can_turn_off_per_core_and_frequency() {
        let f = amd();
        let mut c = Cpu::new(
            Options {
                per_core: false,
                frequency: false,
            },
            f.sys(),
        )
        .unwrap();
        let a = stat(IDLE_THEN_BUSY.0, &[IDLE_THEN_BUSY.0]);
        std::fs::write(f.path().join("proc/stat"), a).unwrap();
        c.sample().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(f.path().join("proc/stat"), stat(IDLE_THEN_BUSY.1, &[IDLE_THEN_BUSY.1])).unwrap();
        let v = c.sample().unwrap().unwrap();
        for k in ["core_pct", "core_mhz", "avg_mhz"] {
            assert!(v.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn unknown_option_is_rejected() {
        let t: toml::Table = "per_cpu = true".parse().unwrap();
        assert!(crate::collector::options::<Options>(t).is_err());
    }

    #[test]
    fn cpulist_ranges_and_singles() {
        assert_eq!(parse_cpulist("0-2,5,8-9"), [0, 1, 2, 5, 8, 9]);
    }
}
