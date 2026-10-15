mod fdinfo;
mod nvidia;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use schema::gpu::{Device, DeviceInfo, Process};
use serde::Deserialize;
use serde_json::Value;

use crate::collector::Collector;
use crate::sys::Sys;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    /// Per-process usage (NVML or nvidia-smi, and DRM fdinfo).
    pub processes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { processes: true }
    }
}

pub struct Gpu {
    sys: Sys,
    processes: bool,
    nvidia: nvidia::Nvidia,
    fdinfo: fdinfo::Scanner,
    intel: HashMap<String, Counters>,
}

/// Intel cumulative counters from the previous sample.
struct Counters {
    t: Instant,
    energy_uj: Option<u64>,
    idle_ms: Option<u64>,
}

struct Card {
    name: String,
    pci: String,
    driver: String,
    /// `/sys/class/drm/<card>/device`
    dev: String,
}

impl Gpu {
    pub fn new(o: Options, sys: Sys) -> Result<Self> {
        Ok(Self::with(o, sys, nvidia::real(), "nvidia-smi".into()))
    }

    fn with(o: Options, sys: Sys, nvml: nvidia::Init, smi: PathBuf) -> Self {
        Self {
            sys,
            processes: o.processes,
            nvidia: nvidia::Nvidia::new(nvml, smi),
            fdinfo: fdinfo::Scanner::default(),
            intel: HashMap::new(),
        }
    }

    fn sample_at(&mut self, now: Instant) -> schema::gpu::Gpu {
        let sys = &self.sys;
        let cards = cards(sys);
        let mut out = schema::gpu::Gpu::default();
        let mut procs = Vec::new();
        let mut nv = Vec::new();
        let mut intel_active = Vec::new();
        let mut nodes = HashMap::new();
        for card in &cards {
            let mut d = Device {
                vendor: vendor(sys, &card.dev),
                driver: card.driver.clone(),
                backend: "sysfs".into(),
                power_state: sys.read_opt(format!("{}/power_state", card.dev)),
                ..Device::default()
            };
            // reading anything else from a runtime-suspended device wakes it
            if !suspended(sys, &card.dev) {
                (d.pcie_gen, d.pcie_width) = pcie(sys, &card.dev, "current");
                match card.driver.as_str() {
                    "nvidia" => nv.push(card.pci.clone()),
                    "amdgpu" => amd(sys, &card.dev, &mut d),
                    "i915" | "xe" => {
                        intel(sys, card, &mut d, &mut self.intel, now);
                        intel_active.push(card.pci.clone());
                    }
                    _ => {}
                }
                if matches!(card.driver.as_str(), "amdgpu" | "i915" | "xe") {
                    for node in sys.list(format!("{}/drm", card.dev)) {
                        nodes.insert(node, card.pci.clone());
                    }
                }
            }
            out.devices.insert(card.pci.clone(), d);
        }

        if !nv.is_empty() {
            let mut r = self.nvidia.sample(&nv, self.processes);
            for pci in &nv {
                let (Some(d), Some(reading)) = (out.devices.get_mut(pci), r.devices.remove(pci)) else {
                    continue;
                };
                let Device {
                    vendor,
                    driver,
                    power_state,
                    pcie_gen,
                    pcie_width,
                    ..
                } = std::mem::take(d);
                *d = Device {
                    vendor,
                    driver,
                    backend: r.backend.into(),
                    power_state,
                    pcie_gen,
                    pcie_width,
                    ..reading.device
                };
                for mut p in reading.processes {
                    if let Some(comm) = sys.read_opt(format!("/proc/{}/comm", p.pid)) {
                        p.comm = comm;
                        procs.push(p);
                    }
                }
            }
            out.backend_errors = r.errors;
        }

        if self.processes && !nodes.is_empty() {
            let rated = self.fdinfo.scanned_before;
            let mut busy: HashMap<String, BTreeMap<String, f64>> = HashMap::new();
            for u in self.fdinfo.scan(sys, &nodes, now) {
                let amd = out.devices.get(&u.pci).is_some_and(|d| d.driver == "amdgpu");
                let max = |pick: &dyn Fn(&str) -> bool| {
                    u.engines
                        .as_ref()
                        .and_then(|e| e.iter().filter(|(k, _)| pick(k)).map(|(_, v)| *v).reduce(f64::max))
                };
                procs.push(Process {
                    util_pct: u.engines.as_ref().map(|_| max(&|_| true).unwrap_or(0.0)),
                    encoder_pct: max(&|k| amd && k.starts_with("enc")),
                    decoder_pct: max(&|k| amd && k == "dec"),
                    pci: u.pci.clone(),
                    pid: u.pid,
                    comm: u.comm,
                    mem_bytes: u.mem_bytes,
                });
                if let Some(engines) = u.engines {
                    let dev = busy.entry(u.pci).or_default();
                    for (class, pct) in engines {
                        *dev.entry(class).or_insert(0.0) += pct;
                    }
                }
            }
            if rated {
                for pci in &intel_active {
                    let max = busy.get(pci).and_then(|b| b.values().copied().reduce(f64::max));
                    if let Some(d) = out.devices.get_mut(pci) {
                        d.util_pct = Some(max.unwrap_or(0.0).min(100.0));
                    }
                }
            }
        }

        procs.sort_by(|a, b| {
            let key = |p: &Process| (p.util_pct.unwrap_or(0.0), p.mem_bytes.unwrap_or(0));
            key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
        out.processes = procs;
        out
    }
}

impl Collector for Gpu {
    fn meta(&mut self) -> Option<Value> {
        let sys = &self.sys;
        let cards = cards(sys);
        let nv: Vec<String> = cards
            .iter()
            .filter(|c| c.driver == "nvidia" && !suspended(sys, &c.dev))
            .map(|c| c.pci.clone())
            .collect();
        let (nv_source, mut nv_info) = if nv.is_empty() {
            (None, HashMap::new())
        } else {
            self.nvidia.info(&nv)
        };
        let mut meta = schema::gpu::Meta::default();
        for card in &cards {
            let dev = &card.dev;
            let id = |f: &str| sys.read_opt(format!("{dev}/{f}")).unwrap_or_default();
            let (pcie_max_gen, pcie_max_width) = pcie(sys, dev, "max");
            let mut info = DeviceInfo {
                vendor_id: id("vendor"),
                device_id: id("device"),
                pcie_max_gen,
                pcie_max_width,
                ..DeviceInfo::default()
            };
            match card.driver.as_str() {
                "nvidia" => {
                    let i = nv_info.remove(&card.pci).unwrap_or_default();
                    info.name = i.name;
                    info.power_limit_min_w = i.power_limit_min_w;
                    info.power_limit_max_w = i.power_limit_max_w;
                    info.util_source = nv_source.map(String::from);
                }
                "amdgpu" => {
                    info.name = Some(amd_name(sys, dev));
                    if sys.read_opt(format!("{dev}/gpu_busy_percent")).is_some() {
                        info.util_source = Some("gpu_busy_percent".into());
                    }
                    if let Some(hw) = hwmon(sys, dev).filter(|_| !suspended(sys, dev)) {
                        info.power_limit_min_w = micro(sys, &format!("{hw}/power1_cap_min"));
                        info.power_limit_max_w = micro(sys, &format!("{hw}/power1_cap_max"));
                    }
                }
                "i915" | "xe" => {
                    info.name = Some(format!("Intel GPU ({})", info.device_id));
                    if self.processes {
                        info.util_source = Some("fdinfo: own processes only".into());
                    }
                }
                _ => {}
            }
            meta.devices.insert(card.pci.clone(), info);
        }
        serde_json::to_value(meta).ok()
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        Ok(Some(serde_json::to_value(self.sample_at(Instant::now()))?))
    }
}

fn cards(sys: &Sys) -> Vec<Card> {
    sys.list("/sys/class/drm")
        .into_iter()
        // card0-DP-1 and the like are connectors, not devices
        .filter(|n| {
            n.strip_prefix("card")
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|name| {
            let dev = format!("/sys/class/drm/{name}/device");
            Some(Card {
                pci: link_name(sys, &dev)?,
                driver: link_name(sys, &format!("{dev}/driver")).unwrap_or_default(),
                name,
                dev,
            })
        })
        .collect()
}

fn link_name(sys: &Sys, path: &str) -> Option<String> {
    let target = std::fs::read_link(sys.path(path)).ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}

fn suspended(sys: &Sys, dev: &str) -> bool {
    sys.read_opt(format!("{dev}/power/runtime_status")).as_deref() == Some("suspended")
}

fn vendor(sys: &Sys, dev: &str) -> String {
    let id = sys.read_opt(format!("{dev}/vendor")).unwrap_or_default();
    match id.as_str() {
        "0x10de" => "nvidia".into(),
        "0x1002" => "amd".into(),
        "0x8086" => "intel".into(),
        _ => id,
    }
}

/// PCIe generation and lane count from the PCI core's `<which>_link_speed` and `_width`.
fn pcie(sys: &Sys, dev: &str, which: &str) -> (Option<u32>, Option<u32>) {
    let generation = sys
        .read_opt(format!("{dev}/{which}_link_speed"))
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .and_then(|gts| match gts {
            2.5 => Some(1),
            5.0 => Some(2),
            8.0 => Some(3),
            16.0 => Some(4),
            32.0 => Some(5),
            64.0 => Some(6),
            _ => None,
        });
    let width = sys
        .int_opt(format!("{dev}/{which}_link_width"))
        .and_then(|w| u32::try_from(w).ok())
        .filter(|w| *w > 0);
    (generation, width)
}

fn hwmon(sys: &Sys, dev: &str) -> Option<String> {
    let name = sys.list(format!("{dev}/hwmon")).into_iter().next()?;
    Some(format!("{dev}/hwmon/{name}"))
}

/// A microunit file (µW) in base units.
fn micro(sys: &Sys, path: &str) -> Option<f64> {
    sys.int_opt(path).map(|v| v as f64 / 1e6)
}

/// The `main` sensor by label, and every other one by label (`tempN` when unlabelled).
fn temps(sys: &Sys, hw: &str, main: &str) -> (Option<f64>, BTreeMap<String, f64>) {
    let mut others = BTreeMap::new();
    let mut first = None;
    for f in sys.list(hw) {
        let Some(n) = f.strip_prefix("temp").and_then(|r| r.strip_suffix("_input")) else {
            continue;
        };
        let Some(c) = sys.int_opt(format!("{hw}/{f}")).map(|m| m as f64 / 1000.0) else {
            continue;
        };
        let label = sys
            .read_opt(format!("{hw}/temp{n}_label"))
            .unwrap_or_else(|| format!("temp{n}"));
        if label == main {
            first = Some(c);
        } else {
            others.insert(label, c);
        }
    }
    (first, others)
}

/// The index of the `<kind>N_label` that reads `label`, else 1.
fn labelled(sys: &Sys, hw: &str, kind: &str, label: &str) -> String {
    sys.list(hw)
        .into_iter()
        .find_map(|f| {
            let n = f.strip_prefix(kind)?.strip_suffix("_label")?;
            (sys.read_opt(format!("{hw}/{f}")).as_deref() == Some(label)).then(|| n.to_string())
        })
        .unwrap_or_else(|| "1".into())
}

fn amd(sys: &Sys, dev: &str, d: &mut Device) {
    let int = |f: &str| sys.int_opt(format!("{dev}/{f}"));
    let pct = |f: &str| int(f).map(|v| v as f64);
    let bytes = |f: &str| int(f).and_then(|v| u64::try_from(v).ok());
    d.util_pct = pct("gpu_busy_percent");
    d.mem_busy_pct = pct("mem_busy_percent");
    d.video_busy_pct = pct("vcn_busy_percent");
    d.vram_used_bytes = bytes("mem_info_vram_used");
    d.vram_total_bytes = bytes("mem_info_vram_total");
    d.gtt_used_bytes = bytes("mem_info_gtt_used");
    d.gtt_total_bytes = bytes("mem_info_gtt_total");
    let Some(hw) = hwmon(sys, dev) else {
        return;
    };
    (d.temp_c, d.temps_c) = temps(sys, &hw, "edge");
    d.power_w = micro(sys, &format!("{hw}/power1_average")).or_else(|| micro(sys, &format!("{hw}/power1_input")));
    d.power_limit_w = micro(sys, &format!("{hw}/power1_cap"));
    let mhz = |f: &str| sys.int_opt(format!("{hw}/{f}")).map(|hz| (hz / 1_000_000) as u64);
    d.clock_mhz = mhz("freq1_input");
    d.mem_clock_mhz = mhz("freq2_input");
    d.fan_rpm = sys
        .int_opt(format!("{hw}/fan1_input"))
        .and_then(|r| u64::try_from(r).ok());
    d.fan_pct = sys
        .int_opt(format!("{hw}/pwm1"))
        .map(|p| (p as f64 * 100.0 / 255.0 * 10.0).round() / 10.0);
}

fn amd_name(sys: &Sys, dev: &str) -> String {
    if let Some(name) = sys.read_opt(format!("{dev}/product_name")).filter(|n| !n.is_empty()) {
        return name;
    }
    let device = sys.read_opt(format!("{dev}/device")).unwrap_or_default();
    let hex = |s: &str| s.trim_start_matches("0x").to_uppercase();
    let (id, rev) = (
        hex(&device),
        hex(&sys.read_opt(format!("{dev}/revision")).unwrap_or_default()),
    );
    let ids = sys.read_opt("/usr/share/libdrm/amdgpu.ids").unwrap_or_default();
    ids.lines()
        .find_map(|line| {
            let mut f = line.splitn(3, ',').map(str::trim);
            (f.next()? == id && f.next()? == rev).then(|| f.next().map(String::from))?
        })
        .unwrap_or_else(|| format!("AMD GPU ({device})"))
}

fn intel(sys: &Sys, card: &Card, d: &mut Device, prev: &mut HashMap<String, Counters>, now: Instant) {
    let dev = &card.dev;
    let xe = card.driver == "xe";
    let gt = if xe {
        sys.list(dev)
            .into_iter()
            .filter(|t| t.starts_with("tile"))
            .map(|t| format!("{dev}/{t}/gt0"))
            .find(|p| sys.path(p).is_dir())
            .unwrap_or_else(|| format!("{dev}/tile0/gt0"))
    } else {
        format!("/sys/class/drm/{}/gt/gt0", card.name)
    };
    let int = |p: String| sys.int_opt(p).and_then(|v| u64::try_from(v).ok());
    let idle_ms = if xe {
        d.clock_mhz = int(format!("{gt}/freq0/act_freq"));
        d.throttle_reasons = sys.read_opt(format!("{gt}/freq0/throttle/reasons")).map(|r| {
            r.split_whitespace()
                .filter(|w| *w != "none")
                .map(String::from)
                .collect()
        });
        int(format!("{gt}/gtidle/idle_residency_ms"))
    } else {
        d.clock_mhz = int(format!("{gt}/rps_act_freq_mhz"))
            .or_else(|| int(format!("/sys/class/drm/{}/gt_act_freq_mhz", card.name)));
        let reasons: Vec<(String, bool)> = sys
            .list(&gt)
            .into_iter()
            .filter_map(|f| {
                let name = f.strip_prefix("throttle_reason_")?;
                (name != "status").then(|| (name.to_string(), int(format!("{gt}/{f}")) == Some(1)))
            })
            .collect();
        if !reasons.is_empty() {
            d.throttle_reasons = Some(reasons.into_iter().filter(|r| r.1).map(|r| r.0).collect());
        }
        int(format!("{gt}/rc6_residency_ms")).or_else(|| int(format!("{dev}/power/rc6_residency_ms")))
    };

    let mut energy_uj = None;
    if let Some(hw) = hwmon(sys, dev) {
        (d.temp_c, d.temps_c) = temps(sys, &hw, if xe { "pkg" } else { "temp1" });
        let e = labelled(sys, &hw, "energy", "card");
        energy_uj = int(format!("{hw}/energy{e}_input"));
        let p = labelled(sys, &hw, "power", "card");
        d.power_limit_w = micro(sys, &format!("{hw}/power{p}_max"));
        d.fan_rpm = int(format!("{hw}/fan1_input"));
    }

    let last = prev.insert(
        card.pci.clone(),
        Counters {
            t: now,
            energy_uj,
            idle_ms,
        },
    );
    if let Some(last) = last {
        let dt = now.duration_since(last.t).as_secs_f64();
        d.power_w = watts(last.energy_uj, energy_uj, dt);
        d.idle_pct = idle_pct(last.idle_ms, idle_ms, !xe, dt);
    }
}

/// Average power from a µJ counter; a counter that went backwards was reset, so no value.
fn watts(old: Option<u64>, new: Option<u64>, dt: f64) -> Option<f64> {
    let (old, new) = (old?, new?);
    (new >= old && dt > 0.0).then(|| (new - old) as f64 / 1e6 / dt)
}

/// Percent of `dt` spent idle. i915 prints the residency as a u32, which wraps.
fn idle_pct(old: Option<u64>, new: Option<u64>, wraps: bool, dt: f64) -> Option<f64> {
    let (old, new) = (old?, new?);
    let delta = match (new >= old, wraps) {
        (true, _) => new - old,
        (false, true) => new + (1 << 32) - old,
        (false, false) => return None,
    } as f64;
    let dt_ms = dt * 1000.0;
    // more idle time than wall time means a reset, or a wrap modulus that is wrong
    (dt_ms > 0.0 && delta <= dt_ms * 1.1).then(|| (100.0 * delta / dt_ms).min(100.0))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::nvidia::tests::{Hung, script, starts};
    use super::nvidia::{Api, Info, Reading, SWITCHED};
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    /// `/sys/class/drm/<name>` → a PCI device directory holding `files`, bound to `driver`.
    fn card(f: &Fixture, name: &str, pci: &str, driver: &str, files: &[(&str, &str)]) {
        let dev = f.path().join(format!("sys/devices/pci0000:00/{pci}"));
        for (p, text) in files {
            let full = dev.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, format!("{text}\n")).unwrap();
        }
        std::fs::create_dir_all(&dev).unwrap();
        let class = f.path().join(format!("sys/class/drm/{name}"));
        std::fs::create_dir_all(&class).unwrap();
        symlink(&dev, class.join("device")).unwrap();
        symlink(format!("../../../bus/pci/drivers/{driver}"), dev.join("driver")).unwrap();
    }

    fn write(f: &Fixture, path: &str, text: &str) {
        let full = f.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, format!("{text}\n")).unwrap();
    }

    /// card0 of this machine (Raphael iGPU), copied from sysfs.
    const RAPHAEL: &[(&str, &str)] = &[
        ("vendor", "0x1002"),
        ("device", "0x164e"),
        ("revision", "0xc9"),
        ("power_state", "D0"),
        ("power/runtime_status", "active"),
        ("current_link_speed", "16.0 GT/s PCIe"),
        ("current_link_width", "16"),
        ("max_link_speed", "16.0 GT/s PCIe"),
        ("max_link_width", "16"),
        ("gpu_busy_percent", "3"),
        ("vcn_busy_percent", "0"),
        ("mem_info_vram_used", "16687104"),
        ("mem_info_vram_total", "536870912"),
        ("mem_info_gtt_used", "14139392"),
        ("mem_info_gtt_total", "67072360448"),
        ("hwmon/hwmon14/name", "amdgpu"),
        ("hwmon/hwmon14/freq1_input", "600000000"),
        ("hwmon/hwmon14/freq1_label", "sclk"),
        ("hwmon/hwmon14/in0_input", "1079"),
        ("hwmon/hwmon14/in0_label", "vddgfx"),
        ("hwmon/hwmon14/power1_input", "43267000"),
        ("hwmon/hwmon14/power1_label", "PPT"),
        ("hwmon/hwmon14/temp1_input", "50000"),
        ("hwmon/hwmon14/temp1_label", "edge"),
        ("drm/card0", ""),
        ("drm/renderD129", ""),
    ];

    fn no_nvml() -> nvidia::Init {
        Box::new(|| anyhow::bail!("libnvidia-ml.so.1: cannot open shared object file"))
    }

    fn gpu(f: &Fixture, nvml: nvidia::Init, smi: PathBuf) -> Gpu {
        Gpu::with(Options::default(), f.sys(), nvml, smi)
    }

    #[test]
    fn amd_from_this_machines_igpu() {
        let f = Fixture::new();
        card(&f, "card0", "0000:7a:00.0", "amdgpu", RAPHAEL);
        write(&f, "sys/class/drm/card0-DP-3/status", "connected");
        write(
            &f,
            "usr/share/libdrm/amdgpu.ids",
            "# List of AMDGPU IDs\n164E,\tD8,\tAMD Radeon 610M\n",
        );
        // a driver we have no backend for still shows up, with what PCI sysfs says
        card(
            &f,
            "card2",
            "0000:03:00.0",
            "radeon",
            &[("vendor", "0x1002"), ("power_state", "D0")],
        );
        let mut g = gpu(&f, no_nvml(), "/nonexistent".into());
        let v = g.sample().unwrap().unwrap();
        assert_schema::<schema::gpu::Gpu>(&v);
        let d: Device = serde_json::from_value(v["devices"]["0000:7a:00.0"].clone()).unwrap();
        assert_eq!(
            (d.vendor.as_str(), d.driver.as_str(), d.backend.as_str()),
            ("amd", "amdgpu", "sysfs")
        );
        assert_eq!(d.util_pct, Some(3.0));
        // the iGPU has no mem_busy_percent: absent, not a 0 % that would chart as idle
        assert_eq!(d.mem_busy_pct, None);
        assert_eq!(d.video_busy_pct, Some(0.0));
        // GTT is a 62 GiB limit on system RAM; folding it into VRAM would show a 63 GiB card
        assert_eq!(d.vram_total_bytes, Some(536870912));
        assert_eq!(d.gtt_total_bytes, Some(67072360448));
        assert_eq!(d.temp_c, Some(50.0));
        assert!(d.temps_c.is_empty());
        assert_eq!(d.power_w, Some(43.267));
        assert_eq!(d.clock_mhz, Some(600), "freq1_input is in Hz");
        assert_eq!((d.pcie_gen, d.pcie_width), (Some(4), Some(16)));
        assert_eq!(
            v["devices"].as_object().unwrap().len(),
            2,
            "the connector is not a device"
        );
        assert_eq!(
            v["devices"]["0000:03:00.0"],
            serde_json::json!({"vendor": "amd", "driver": "radeon", "backend": "sysfs", "power_state": "D0"})
        );

        let m = g.meta().unwrap();
        assert_schema::<schema::gpu::Meta>(&m);
        let info = &m["devices"]["0000:7a:00.0"];
        // revision c9 is not in amdgpu.ids; a neighbouring revision's name could be a different product
        assert_eq!(info["name"], "AMD GPU (0x164e)");
        assert_eq!(info["util_source"], "gpu_busy_percent");
        assert_eq!(
            (info["pcie_max_gen"].as_u64(), info["vendor_id"].as_str()),
            (Some(4), Some("0x1002"))
        );
    }

    #[test]
    fn amd_name_from_libdrm_ids() {
        let f = Fixture::new();
        card(
            &f,
            "card0",
            "0000:7a:00.0",
            "amdgpu",
            &[("device", "0x164e"), ("revision", "0xd8")],
        );
        write(&f, "usr/share/libdrm/amdgpu.ids", "164E,\tD8,\tAMD Radeon 610M\n");
        assert_eq!(amd_name(&f.sys(), "/sys/class/drm/card0/device"), "AMD Radeon 610M");
    }

    #[test]
    fn amd_dgpu_sensors_by_label_and_unit() {
        let f = Fixture::new();
        card(
            &f,
            "card1",
            "0000:03:00.0",
            "amdgpu",
            &[
                ("vendor", "0x1002"),
                ("mem_busy_percent", "12"),
                ("hwmon/hwmon3/temp1_input", "41000"),
                ("hwmon/hwmon3/temp1_label", "edge"),
                ("hwmon/hwmon3/temp2_input", "55000"),
                ("hwmon/hwmon3/temp2_label", "junction"),
                ("hwmon/hwmon3/temp3_input", "60000"),
                ("hwmon/hwmon3/temp3_label", "mem"),
                ("hwmon/hwmon3/power1_average", "120000000"),
                ("hwmon/hwmon3/power1_input", "150000000"),
                ("hwmon/hwmon3/power1_cap", "250000000"),
                ("hwmon/hwmon3/freq2_input", "1000000000"),
                ("hwmon/hwmon3/fan1_input", "1500"),
                ("hwmon/hwmon3/pwm1", "102"),
            ],
        );
        let v = gpu(&f, no_nvml(), "/nonexistent".into()).sample().unwrap().unwrap();
        assert_schema::<schema::gpu::Gpu>(&v);
        let d = &v["devices"]["0000:03:00.0"];
        assert_eq!(d["temp_c"], 41.0);
        assert_eq!(d["temps_c"], serde_json::json!({"junction": 55.0, "mem": 60.0}));
        // the average is the steadier figure when both exist
        assert_eq!(
            (d["power_w"].as_f64(), d["power_limit_w"].as_f64()),
            (Some(120.0), Some(250.0))
        );
        assert_eq!(
            (d["mem_clock_mhz"].as_u64(), d["fan_rpm"].as_u64()),
            (Some(1000), Some(1500))
        );
        assert_eq!(d["fan_pct"], 40.0);
    }

    fn i915(f: &Fixture, rc6: u64, energy: u64) {
        card(
            f,
            "card0",
            "0000:03:00.0",
            "i915",
            &[
                ("vendor", "0x8086"),
                ("device", "0x56a0"),
                ("power_state", "D0"),
                ("hwmon/hwmon5/name", "i915"),
                ("hwmon/hwmon5/temp1_input", "45000"),
                ("hwmon/hwmon5/energy1_input", &energy.to_string()),
                ("hwmon/hwmon5/power1_max", "190000000"),
                ("drm/renderD128", ""),
            ],
        );
        let gt = "sys/class/drm/card0/gt/gt0";
        write(f, &format!("{gt}/rps_act_freq_mhz"), "1100");
        write(f, &format!("{gt}/rc6_residency_ms"), &rc6.to_string());
        write(f, &format!("{gt}/throttle_reason_status"), "1");
        write(f, &format!("{gt}/throttle_reason_pl1"), "1");
        write(f, &format!("{gt}/throttle_reason_thermal"), "0");
    }

    fn set(f: &Fixture, path: &str, text: &str) {
        std::fs::write(f.path().join(path), text).unwrap();
    }

    #[test]
    fn i915_power_idle_and_throttle() {
        let f = Fixture::new();
        i915(&f, 1000, 5_000_000);
        let mut g = gpu(&f, no_nvml(), "/nonexistent".into());
        let t0 = Instant::now();
        let first = g.sample_at(t0);
        assert_eq!(
            first.devices["0000:03:00.0"].power_w, None,
            "a counter needs two readings"
        );

        set(
            &f,
            "sys/devices/pci0000:00/0000:03:00.0/hwmon/hwmon5/energy1_input",
            "25000000",
        );
        set(&f, "sys/class/drm/card0/gt/gt0/rc6_residency_ms", "2500");
        let d = g
            .sample_at(t0 + Duration::from_secs(2))
            .devices
            .remove("0000:03:00.0")
            .unwrap();
        assert_eq!(d.power_w, Some(10.0), "20 J over 2 s");
        assert_eq!(d.idle_pct, Some(75.0), "1.5 s of RC6 in 2 s");
        assert_eq!(
            (d.clock_mhz, d.temp_c, d.power_limit_w),
            (Some(1100), Some(45.0), Some(190.0))
        );
        // `status` is the OR of the others, not a reason of its own
        assert_eq!(d.throttle_reasons, Some(vec!["pl1".to_string()]));
        assert_eq!(d.util_pct, Some(0.0), "no own process used it between the two scans");
        assert_schema::<schema::gpu::Gpu>(&serde_json::to_value(g.sample_at(t0 + Duration::from_secs(4))).unwrap());

        let m = g.meta().unwrap();
        assert_schema::<schema::gpu::Meta>(&m);
        assert_eq!(m["devices"]["0000:03:00.0"]["name"], "Intel GPU (0x56a0)");
        assert_eq!(
            m["devices"]["0000:03:00.0"]["util_source"],
            "fdinfo: own processes only"
        );
    }

    #[test]
    fn energy_reset_drops_the_sample_instead_of_a_huge_or_negative_power() {
        let f = Fixture::new();
        i915(&f, 1000, 900_000_000);
        let mut g = gpu(&f, no_nvml(), "/nonexistent".into());
        let t0 = Instant::now();
        g.sample_at(t0);
        set(
            &f,
            "sys/devices/pci0000:00/0000:03:00.0/hwmon/hwmon5/energy1_input",
            "1000",
        );
        let d = &g.sample_at(t0 + Duration::from_secs(2)).devices["0000:03:00.0"];
        assert_eq!(d.power_w, None);
        assert!(d.idle_pct.is_some(), "only the reset counter is dropped");
    }

    #[test]
    fn i915_rc6_wrap_is_a_small_step_not_a_reset() {
        assert_eq!(idle_pct(Some((1 << 32) - 500), Some(1000), true, 2.0), Some(75.0));
        // xe keeps a 64-bit count, so going backwards there is a reset
        assert_eq!(idle_pct(Some((1 << 32) - 500), Some(1000), false, 2.0), None);
        assert_eq!(
            idle_pct(Some(0), Some(50_000), true, 2.0),
            None,
            "more idle than wall time"
        );
    }

    #[test]
    fn xe_by_label_with_fdinfo_utilisation() {
        let f = Fixture::new();
        let hw = "hwmon/hwmon7";
        card(
            &f,
            "card1",
            "0000:03:00.0",
            "xe",
            &[
                ("vendor", "0x8086"),
                ("device", "0xe20b"),
                ("tile0/gt0/freq0/act_freq", "2400"),
                ("tile0/gt0/freq0/throttle/reasons", "pl1 thermal"),
                ("tile0/gt0/gtidle/idle_residency_ms", "100"),
                ("tile0/gt1/freq0/act_freq", "900"),
                (&format!("{hw}/name"), "xe"),
                (&format!("{hw}/energy1_input"), "7000000"),
                (&format!("{hw}/energy1_label"), "pkg"),
                (&format!("{hw}/energy2_input"), "1000000"),
                (&format!("{hw}/energy2_label"), "card"),
                (&format!("{hw}/power1_label"), "pkg"),
                (&format!("{hw}/power1_max"), "60000000"),
                (&format!("{hw}/power2_label"), "card"),
                (&format!("{hw}/power2_max"), "150000000"),
                (&format!("{hw}/temp2_input"), "52000"),
                (&format!("{hw}/temp2_label"), "pkg"),
                (&format!("{hw}/temp3_input"), "58000"),
                (&format!("{hw}/temp3_label"), "vram"),
                ("drm/renderD128", ""),
            ],
        );
        let proc = f.path().join("proc/42");
        std::fs::create_dir_all(proc.join("fd")).unwrap();
        symlink("/dev/dri/renderD128", proc.join("fd/5")).unwrap();
        write(&f, "proc/42/comm", "gnome-shell");
        let fdinfo = |rcs: u64, total: u64| {
            format!(
                "drm-driver:\txe\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t3\ndrm-cycles-rcs:\t{rcs}\ndrm-total-cycles-rcs:\t{total}\ndrm-cycles-vcs:\t0\ndrm-total-cycles-vcs:\t{total}\ndrm-resident-system:\t4 MiB\ndrm-resident-vram0:\t16 MiB\n"
            )
        };
        write(&f, "proc/42/fdinfo/5", &fdinfo(0, 1000));
        let mut g = gpu(&f, no_nvml(), "/nonexistent".into());
        let t0 = Instant::now();
        g.sample_at(t0);
        let dev = "sys/devices/pci0000:00/0000:03:00.0";
        set(&f, &format!("{dev}/{hw}/energy1_input"), "99000000");
        set(&f, &format!("{dev}/{hw}/energy2_input"), "31000000");
        set(&f, &format!("{dev}/tile0/gt0/gtidle/idle_residency_ms"), "1100");
        set(&f, "proc/42/fdinfo/5", &fdinfo(300, 2000));
        let s = g.sample_at(t0 + Duration::from_secs(2));
        let d = &s.devices["0000:03:00.0"];
        // the card rail, chosen by label: energy1 is the package here
        assert_eq!(d.power_w, Some(15.0));
        assert_eq!(d.power_limit_w, Some(150.0));
        assert_eq!((d.temp_c, d.temps_c.get("vram")), (Some(52.0), Some(&58.0)));
        assert_eq!(d.clock_mhz, Some(2400), "gt0 is the render GT, gt1 the media GT");
        assert_eq!(d.idle_pct, Some(50.0));
        assert_eq!(d.throttle_reasons, Some(vec!["pl1".into(), "thermal".into()]));
        assert_eq!(d.util_pct, Some(30.0), "busiest engine class over GPU cycles");
        assert_eq!(s.processes[0].comm, "gnome-shell");
        assert_eq!(s.processes[0].mem_bytes, Some(20 << 20));
        assert_eq!(s.processes[0].encoder_pct, None, "only amdgpu splits encode and decode");
        assert_schema::<schema::gpu::Gpu>(&serde_json::to_value(&s).unwrap());
    }

    fn rtx(f: &Fixture, runtime: &str, power_state: &str) {
        card(
            f,
            "card1",
            "0000:01:00.0",
            "nvidia",
            &[
                ("vendor", "0x10de"),
                ("device", "0x2684"),
                ("power_state", power_state),
                ("power/runtime_status", runtime),
                ("current_link_speed", "16.0 GT/s PCIe"),
                ("current_link_width", "8"),
                ("drm/renderD128", ""),
            ],
        );
    }

    #[test]
    fn a_suspended_dgpu_is_not_read() {
        let f = Fixture::new();
        rtx(&f, "suspended", "D3cold");
        let calls = Arc::new(AtomicUsize::new(0));
        let hung = Hung {
            calls: calls.clone(),
            delay: Duration::ZERO,
        };
        let dir = tempfile::tempdir().unwrap();
        let smi = script(dir.path(), "exit 0");
        let mut g = gpu(&f, Box::new(move || Ok(Box::new(hung) as Box<dyn Api>)), smi);
        let v = g.sample().unwrap().unwrap();
        g.meta();
        // any NVML or nvidia-smi call would pull a laptop dGPU out of D3cold
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(starts(dir.path()), 0);
        assert_eq!(
            v["devices"]["0000:01:00.0"],
            serde_json::json!({"vendor": "nvidia", "driver": "nvidia", "backend": "sysfs", "power_state": "D3cold"})
        );
    }

    struct Fixed;
    impl Api for Fixed {
        fn sample(&mut self, pci: &str, _: bool) -> anyhow::Result<Reading> {
            Ok(Reading {
                device: Device {
                    util_pct: Some(25.0),
                    vram_used_bytes: Some(1 << 30),
                    power_state: Some("from nvml".into()),
                    ..Device::default()
                },
                processes: vec![
                    Process {
                        pci: pci.into(),
                        pid: 7,
                        mem_bytes: Some(1 << 20),
                        ..Process::default()
                    },
                    Process {
                        pci: pci.into(),
                        pid: 8,
                        ..Process::default()
                    },
                ],
            })
        }
        fn info(&mut self, _: &str) -> anyhow::Result<Info> {
            Ok(Info {
                name: Some("NVIDIA GeForce RTX 4090".into()),
                power_limit_min_w: Some(150.0),
                power_limit_max_w: Some(480.0),
            })
        }
    }

    #[test]
    fn nvml_readings_join_sysfs_identity_by_pci_slot() {
        let f = Fixture::new();
        rtx(&f, "active", "D0");
        write(&f, "proc/7/comm", "Hyprland");
        let mut g = gpu(
            &f,
            Box::new(|| Ok(Box::new(Fixed) as Box<dyn Api>)),
            "/nonexistent".into(),
        );
        let v = g.sample().unwrap().unwrap();
        assert_schema::<schema::gpu::Gpu>(&v);
        let d = &v["devices"]["0000:01:00.0"];
        assert_eq!(
            (d["backend"].as_str(), d["util_pct"].as_f64()),
            (Some("nvml"), Some(25.0))
        );
        // PCI sysfs owns power state and link; the backend's measured fields fill the rest
        assert_eq!(
            (d["power_state"].as_str(), d["pcie_width"].as_u64()),
            (Some("D0"), Some(8))
        );
        // pid 8 exited between NVML's list and our /proc read
        assert_eq!(
            v["processes"],
            serde_json::json!([{"pci": "0000:01:00.0", "pid": 7, "comm": "Hyprland", "mem_bytes": 1048576}])
        );
        let m = g.meta().unwrap();
        assert_schema::<schema::gpu::Meta>(&m);
        assert_eq!(m["devices"]["0000:01:00.0"]["util_source"], "nvml");
        assert_eq!(m["devices"]["0000:01:00.0"]["power_limit_max_w"], 480.0);
    }

    #[test]
    fn hung_nvml_is_abandoned_for_nvidia_smi_and_other_gpus_still_report() {
        let f = Fixture::new();
        rtx(&f, "active", "D0");
        card(&f, "card0", "0000:7a:00.0", "amdgpu", RAPHAEL);
        let calls = Arc::new(AtomicUsize::new(0));
        let hung = Hung {
            calls: calls.clone(),
            delay: Duration::from_secs(2),
        };
        let dir = tempfile::tempdir().unwrap();
        let smi = script(
            dir.path(),
            "printf '0, 00000000:01:00.0, 43, 54.54, 450.00, 31, 25, 14, 0, 0, 2441, 24564, 1245, 5001, P3, 0x1\\n'",
        );
        let mut g = Gpu::with(
            Options { processes: false },
            f.sys(),
            Box::new(move || Ok(Box::new(hung) as Box<dyn Api>)),
            smi,
        );

        let started = Instant::now();
        let v = g.sample().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the hung driver call held up the sample"
        );
        assert_eq!(v["backend_errors"]["0000:01:00.0"], SWITCHED);
        assert_eq!(v["devices"]["0000:7a:00.0"]["util_pct"], 3.0);
        assert_schema::<schema::gpu::Gpu>(&v);

        let v = g.sample().unwrap().unwrap();
        assert_eq!(v["devices"]["0000:01:00.0"]["backend"], "nvidia-smi");
        assert_eq!(v["devices"]["0000:01:00.0"]["temp_c"], 43.0);
        // repeated once: the pool discards the late switch sample, so this is the first one recorded
        assert_eq!(v["backend_errors"]["0000:01:00.0"], SWITCHED);
        let v = g.sample().unwrap().unwrap();
        assert!(v.get("backend_errors").is_none(), "{v}");
        std::thread::sleep(Duration::from_secs(2));
        g.sample().unwrap();
        g.meta();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the hung backend was called again");
        assert_eq!(starts(dir.path()), 4, "three samples and one meta via nvidia-smi");
    }

    #[test]
    fn no_gpus_is_an_empty_map_not_an_error() {
        let v = gpu(&Fixture::new(), no_nvml(), "/nonexistent".into())
            .sample()
            .unwrap()
            .unwrap();
        assert_eq!(v, serde_json::json!({"devices": {}}));
    }
}
