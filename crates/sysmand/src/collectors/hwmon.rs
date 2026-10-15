use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Result, bail};
use schema::hwmon::HwmonMeta;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::collector::Collector;
use crate::daemon::unix_now;
use crate::sys::Sys;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    /// Seconds between reads by chip-name prefix (longest match wins); unmatched chips are read every sample.
    pub chip_intervals: BTreeMap<String, u64>,
}

impl Default for Options {
    fn default() -> Self {
        // the nct67xx driver re-reads every register over port I/O when its cache is older than ~1.5 s
        Self {
            chip_intervals: BTreeMap::from([("nct67".to_string(), 10)]),
        }
    }
}

const KINDS: &[(&str, f64, &str)] = &[
    ("temp", 1000.0, "C"),
    ("fan", 1.0, "rpm"),
    ("in", 1000.0, "V"),
    ("power", 1_000_000.0, "W"),
    ("curr", 1000.0, "A"),
];
const LIMITS: &[&str] = &["min", "max", "crit", "lcrit"];

struct Sensor {
    kind: &'static str,
    divisor: f64,
    label: String,
    stem_path: String,
}

type Chips = BTreeMap<String, Vec<Sensor>>;

pub struct Hwmon {
    sys: Sys,
    signature: Vec<(String, Option<PathBuf>)>,
    chips: Chips,
    intervals: BTreeMap<String, u64>,
    /// Per chip key, the interval bucket it was last read in.
    last_read: HashMap<String, u64>,
}

impl Hwmon {
    pub fn new(opts: Options, sys: Sys) -> Result<Self> {
        if let Some((name, _)) = opts.chip_intervals.iter().find(|(_, i)| **i == 0) {
            bail!("chip_intervals.{name} must be at least 1 second");
        }
        let signature = sys.hwmon_signature();
        let chips = discover(&sys);
        Ok(Self {
            sys,
            signature,
            chips,
            intervals: opts.chip_intervals,
            last_read: HashMap::new(),
        })
    }

    /// Whether `chip` is due at unix second `now`; buckets follow the wall clock, like the daemon's ticks.
    fn due(&mut self, chip: &str, now: u64) -> bool {
        let name = chip.split('/').next().unwrap_or(chip);
        let Some(interval) = self
            .intervals
            .iter()
            .filter(|(prefix, _)| name.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, i)| *i)
        else {
            return true;
        };
        let bucket = now / interval;
        self.last_read.insert(chip.to_string(), bucket) != Some(bucket)
    }

    fn refresh(&mut self) {
        let signature = self.sys.hwmon_signature();
        if signature != self.signature {
            self.chips = discover(&self.sys);
            self.signature = signature;
        }
    }
}

fn discover(sys: &Sys) -> Chips {
    let mut chips = BTreeMap::new();
    for h in sys
        .list("/sys/class/hwmon")
        .into_iter()
        .filter(|h| h.starts_with("hwmon"))
    {
        let dir = format!("/sys/class/hwmon/{h}");
        let name = sys.read_opt(format!("{dir}/name")).unwrap_or_else(|| h.clone());
        let device = std::fs::canonicalize(sys.path(format!("{dir}/device")))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "device".to_string());
        let sensors = sensors(sys, &dir);
        if sensors.is_empty() {
            continue;
        }
        // two chips can share name and device; the first keeps the plain key
        let base = format!("{name}/{device}");
        let mut key = base.clone();
        let mut n = 2;
        while chips.contains_key(&key) {
            key = format!("{base}#{n}");
            n += 1;
        }
        chips.insert(key, sensors);
    }
    chips
}

fn sensors(sys: &Sys, dir: &str) -> Vec<Sensor> {
    let mut out: Vec<Sensor> = Vec::new();
    for file in sys.list(dir) {
        let Some(stem) = file.strip_suffix("_input") else {
            continue;
        };
        let kind = stem.trim_end_matches(|c: char| c.is_ascii_digit());
        let Some(&(kind, divisor, _)) = KINDS.iter().find(|(k, ..)| *k == kind) else {
            continue;
        };
        let mut label = sys
            .read_opt(format!("{dir}/{stem}_label"))
            .unwrap_or_else(|| stem.to_string());
        // a repeated label would overwrite a sensor in the output
        if out.iter().any(|s| s.kind == kind && s.label == label) {
            label = stem.to_string();
        }
        out.push(Sensor {
            kind,
            divisor,
            label,
            stem_path: format!("{dir}/{stem}"),
        });
    }
    out
}

impl Collector for Hwmon {
    fn meta(&mut self) -> Option<Value> {
        let mut limits = schema::hwmon::Limits::new();
        for (chip, sensors) in &self.chips {
            for s in sensors {
                let found: BTreeMap<String, f64> = LIMITS
                    .iter()
                    .filter_map(|l| {
                        Some((
                            l.to_string(),
                            self.sys.int_opt(format!("{}_{l}", s.stem_path))? as f64 / s.divisor,
                        ))
                    })
                    .collect();
                if !found.is_empty() {
                    limits
                        .entry(chip.clone())
                        .or_default()
                        .entry(s.kind.to_string())
                        .or_default()
                        .insert(s.label.clone(), found);
                }
            }
        }
        let meta = HwmonMeta {
            units: KINDS.iter().map(|(k, _, u)| (k.to_string(), u.to_string())).collect(),
            note: "Super I/O 'in' values are raw pin voltages without the board's divider scaling".to_string(),
            limits,
        };
        Some(json!(meta))
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        self.sample_at(unix_now().round() as u64)
    }
}

impl Hwmon {
    fn sample_at(&mut self, now: u64) -> Result<Option<Value>> {
        self.refresh();
        let chips: Vec<String> = self.chips.keys().cloned().collect();
        let due: Vec<String> = chips.into_iter().filter(|c| self.due(c, now)).collect();
        let mut out: schema::hwmon::Hwmon = BTreeMap::new();
        for chip in &due {
            for s in &self.chips[chip] {
                let value = self
                    .sys
                    .int_opt(format!("{}_input", s.stem_path))
                    .map(|raw| raw as f64 / s.divisor);
                out.entry(chip.clone())
                    .or_default()
                    .entry(s.kind.to_string())
                    .or_default()
                    .insert(s.label.clone(), value);
            }
        }
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    fn every_sample() -> Options {
        Options {
            chip_intervals: BTreeMap::new(),
        }
    }

    #[test]
    fn slow_chips_are_read_once_per_interval_and_the_rest_every_sample() {
        let f = Fixture::new()
            .file("sys/class/hwmon/hwmon0/name", "nct6799")
            .file("sys/class/hwmon/hwmon0/fan1_input", "1450")
            .file("sys/class/hwmon/hwmon1/name", "k10temp")
            .file("sys/class/hwmon/hwmon1/temp1_input", "55000");
        link_device(&f, "hwmon0", "nct6775.656");
        link_device(&f, "hwmon1", "0000:00:18.3");
        let mut c = Hwmon::new(Options::default(), f.sys()).unwrap();
        let chips = |v: Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            chips(c.sample_at(1000).unwrap().unwrap()),
            ["k10temp/0000:00:18.3", "nct6799/nct6775.656"]
        );
        // same 10 s window: the Super I/O chip is not refreshed again, the CPU sensor is read
        assert_eq!(chips(c.sample_at(1008).unwrap().unwrap()), ["k10temp/0000:00:18.3"]);
        assert_eq!(
            chips(c.sample_at(1010).unwrap().unwrap()),
            ["k10temp/0000:00:18.3", "nct6799/nct6775.656"]
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let opts = Options {
            chip_intervals: BTreeMap::from([("nct".to_string(), 60), ("nct67".to_string(), 10)]),
        };
        let mut c = Hwmon::new(opts, Fixture::new().sys()).unwrap();
        assert!(c.due("nct6799/x", 1000));
        // a 60 s interval would make this false; the more specific 10 s rule applies
        assert!(c.due("nct6799/x", 1010));
    }

    #[test]
    fn zero_chip_interval_is_rejected() {
        let opts = Options {
            chip_intervals: BTreeMap::from([("nvme".to_string(), 0)]),
        };
        assert!(Hwmon::new(opts, Fixture::new().sys()).is_err());
    }

    fn link_device(f: &Fixture, hwmon: &str, target: &str) {
        let dev = f.path().join("sys/devices").join(target);
        std::fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink(&dev, f.path().join(format!("sys/class/hwmon/{hwmon}/device"))).unwrap();
    }

    fn k10temp() -> Fixture {
        let f = Fixture::new()
            .file("sys/class/hwmon/hwmon3/name", "k10temp\n")
            .file("sys/class/hwmon/hwmon3/temp1_input", "45250\n")
            .file("sys/class/hwmon/hwmon3/temp1_label", "Tctl\n")
            .file("sys/class/hwmon/hwmon3/temp1_max", "95000\n")
            .file("sys/class/hwmon/hwmon3/temp1_crit", "100000\n")
            .file("sys/class/hwmon/hwmon3/temp3_input", "-61000\n")
            .file("sys/class/hwmon/hwmon3/fan2_input", "1180\n")
            .file("sys/class/hwmon/hwmon3/in0_input", "1056\n")
            .file("sys/class/hwmon/hwmon3/in0_label", "vddgfx\n")
            .file("sys/class/hwmon/hwmon3/power1_input", "28500000\n")
            .file("sys/class/hwmon/hwmon3/curr1_input", "2500\n")
            .file("sys/class/hwmon/hwmon3/pwm1", "128\n");
        link_device(&f, "hwmon3", "pci0000:00/0000:00:18.3");
        f
    }

    #[test]
    fn reads_every_kind_scaled_and_keyed_by_device_not_hwmon_number() {
        let f = k10temp();
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::hwmon::Hwmon>(&v);
        let chip = &v["k10temp/0000:00:18.3"];
        assert_eq!(chip["temp"]["Tctl"], 45.25);
        assert_eq!(chip["fan"]["fan2"], 1180.0);
        assert_eq!(chip["in"]["vddgfx"], 1.056);
        assert_eq!(chip["power"]["power1"], 28.5);
        assert_eq!(chip["curr"]["curr1"], 2.5);
        assert!(chip.get("pwm").is_none());
    }

    #[test]
    fn placeholder_values_are_reported_not_filtered() {
        // a sensor that suddenly reads a placeholder can be evidence of a crash
        let f = k10temp();
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["k10temp/0000:00:18.3"]["temp"]["temp3"], -61.0);
    }

    #[test]
    fn unreadable_sensor_is_null_not_zero() {
        let f = k10temp();
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        std::fs::write(f.path().join("sys/class/hwmon/hwmon3/fan2_input"), "not a number\n").unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::hwmon::Hwmon>(&v);
        assert!(v["k10temp/0000:00:18.3"]["fan"]["fan2"].is_null());
    }

    #[test]
    fn chips_with_the_same_name_and_device_get_distinct_keys() {
        // equal keys would silently drop one chip's readings
        let mut f = Fixture::new();
        for h in ["hwmon1", "hwmon2"] {
            f = f
                .file(&format!("sys/class/hwmon/{h}/name"), "nvme\n")
                .file(&format!("sys/class/hwmon/{h}/temp1_input"), "30000\n");
            link_device(&f, h, "pci0000:00/0000:00:01.1");
        }
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["nvme/0000:00:01.1", "nvme/0000:00:01.1#2"]);
    }

    #[test]
    fn meta_has_units_and_scaled_limits() {
        let f = k10temp();
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let m = c.meta().unwrap();
        assert_schema::<HwmonMeta>(&m);
        assert_eq!(m["units"]["power"], "W");
        assert_eq!(m["limits"]["k10temp/0000:00:18.3"]["temp"]["Tctl"]["crit"], 100.0);
        assert!(m["limits"]["k10temp/0000:00:18.3"]["fan"].is_null());
    }

    #[test]
    fn chip_without_a_device_link_or_name_still_reports() {
        let f = Fixture::new().file("sys/class/hwmon/hwmon0/temp1_input", "20000\n");
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["hwmon0/device"]["temp"]["temp1"], 20.0);
    }

    #[test]
    fn values_follow_the_chip_when_hwmon_numbers_are_swapped() {
        let mut f = Fixture::new();
        for (h, name, milli, dev) in [
            ("hwmon0", "nvme", "30000", "pci0000:00/0000:00:01.1"),
            ("hwmon1", "k10temp", "60000", "pci0000:00/0000:00:18.3"),
        ] {
            f = f
                .file(&format!("sys/class/hwmon/{h}/name"), &format!("{name}\n"))
                .file(&format!("sys/class/hwmon/{h}/temp1_input"), &format!("{milli}\n"));
            link_device(&f, h, dev);
        }
        let mut c = Hwmon::new(every_sample(), f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["nvme/0000:00:01.1"]["temp"]["temp1"], 30.0);

        let hw = f.path().join("sys/class/hwmon");
        std::fs::rename(hw.join("hwmon0"), hw.join("tmp")).unwrap();
        std::fs::rename(hw.join("hwmon1"), hw.join("hwmon0")).unwrap();
        std::fs::rename(hw.join("tmp"), hw.join("hwmon1")).unwrap();
        std::fs::write(hw.join("hwmon1/temp1_input"), "31000\n").unwrap();

        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["nvme/0000:00:01.1"]["temp"]["temp1"], 31.0);
        assert_eq!(v["k10temp/0000:00:18.3"]["temp"]["temp1"], 60.0);
    }
}
