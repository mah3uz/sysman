use std::collections::BTreeMap;

use anyhow::Result;
use schema::hwmon::HwmonMeta;
use serde_json::{Value, json};

use crate::collector::{Collector, NoOptions};
use crate::sys::Sys;

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

pub struct Hwmon {
    sys: Sys,
    chips: BTreeMap<String, Vec<Sensor>>,
}

impl Hwmon {
    pub fn new(_: NoOptions, sys: Sys) -> Result<Self> {
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
            let sensors = sensors(&sys, &dir);
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
        Ok(Self { sys, chips })
    }
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
        let mut out: schema::hwmon::Hwmon = BTreeMap::new();
        for (chip, sensors) in &self.chips {
            for s in sensors {
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
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
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
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["k10temp/0000:00:18.3"]["temp"]["temp3"], -61.0);
    }

    #[test]
    fn unreadable_sensor_is_null_not_zero() {
        let f = k10temp();
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
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
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["nvme/0000:00:01.1", "nvme/0000:00:01.1#2"]);
    }

    #[test]
    fn meta_has_units_and_scaled_limits() {
        let f = k10temp();
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
        let m = c.meta().unwrap();
        assert_schema::<HwmonMeta>(&m);
        assert_eq!(m["units"]["power"], "W");
        assert_eq!(m["limits"]["k10temp/0000:00:18.3"]["temp"]["Tctl"]["crit"], 100.0);
        assert!(m["limits"]["k10temp/0000:00:18.3"]["fan"].is_null());
    }

    #[test]
    fn chip_without_a_device_link_or_name_still_reports() {
        let f = Fixture::new().file("sys/class/hwmon/hwmon0/temp1_input", "20000\n");
        let mut c = Hwmon::new(NoOptions {}, f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(v["hwmon0/device"]["temp"]["temp1"], 20.0);
    }
}
