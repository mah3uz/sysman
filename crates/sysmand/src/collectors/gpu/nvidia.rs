use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use schema::gpu::{Device, Process};

// well inside the daemon's per-tick deadline (0.75 × 2 s), so the switch sample is not discarded
pub const DEADLINE: Duration = Duration::from_millis(1000);
pub const SWITCHED: &str = "nvml timeout; switched to nvidia-smi";

/// What the collector needs from NVML, so a hung driver can be faked in tests.
pub trait Api: Send {
    fn sample(&mut self, pci: &str, processes: bool) -> Result<Reading>;
    fn info(&mut self, pci: &str) -> Result<Info>;
}

pub type Init = Box<dyn FnOnce() -> Result<Box<dyn Api>> + Send>;

/// Measured fields only; identity, power state and PCIe link come from sysfs.
#[derive(Debug, Default)]
pub struct Reading {
    pub device: Device,
    /// `comm` is left empty for the caller to fill from `/proc`.
    pub processes: Vec<Process>,
}

#[derive(Debug, Default, Clone)]
pub struct Info {
    pub name: Option<String>,
    pub power_limit_min_w: Option<f64>,
    pub power_limit_max_w: Option<f64>,
}

#[derive(Default)]
pub struct Out {
    pub backend: &'static str,
    pub devices: HashMap<String, Reading>,
    pub errors: BTreeMap<String, String>,
}

enum Job {
    Sample(Vec<String>, bool),
    Info(Vec<String>),
}

enum Done {
    Sample(Vec<(String, Result<Reading, String>)>),
    Info(Vec<(String, Result<Info, String>)>),
}

struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Result<Done, String>>,
}

enum Call {
    Done(Done),
    Timeout,
    Off,
}

pub struct Nvidia {
    init: Option<Init>,
    nvml: Option<Worker>,
    smi: Smi,
    /// The switch sample itself is late, so the pool discards it; repeat the notice until one arrives.
    announce: bool,
}

impl Nvidia {
    pub fn new(init: Init, smi: PathBuf) -> Self {
        Self {
            init: Some(init),
            nvml: None,
            smi: Smi { cmd: smi, child: None },
            announce: false,
        }
    }

    pub fn sample(&mut self, slots: &[String], processes: bool) -> Out {
        let started = Instant::now();
        let mut out = match self.call(Job::Sample(slots.to_vec(), processes)) {
            Call::Done(Done::Sample(results)) => {
                let mut out = Out {
                    backend: "nvml",
                    ..Out::default()
                };
                for (pci, r) in results {
                    match r {
                        Ok(reading) => drop(out.devices.insert(pci, reading)),
                        Err(e) => drop(out.errors.insert(pci, e)),
                    }
                }
                out
            }
            Call::Timeout => Out {
                backend: "nvml",
                errors: slots.iter().map(|s| (s.clone(), SWITCHED.to_string())).collect(),
                ..Out::default()
            },
            Call::Done(Done::Info(_)) | Call::Off => self.smi.sample(slots, processes),
        };
        if self.announce && out.backend == "nvidia-smi" {
            for s in slots {
                out.errors.entry(s.clone()).or_insert_with(|| SWITCHED.to_string());
            }
            if started.elapsed() < Duration::from_secs(1) {
                self.announce = false;
            }
        }
        out
    }

    /// Returns the backend that answered, if any, and per-slot static info.
    pub fn info(&mut self, slots: &[String]) -> (Option<&'static str>, HashMap<String, Info>) {
        match self.call(Job::Info(slots.to_vec())) {
            Call::Done(Done::Info(results)) => (
                Some("nvml"),
                results.into_iter().filter_map(|(p, r)| Some((p, r.ok()?))).collect(),
            ),
            Call::Timeout => (None, HashMap::new()),
            Call::Done(Done::Sample(_)) | Call::Off => match self.smi.info(slots) {
                Some(info) => (Some("nvidia-smi"), info),
                None => (None, HashMap::new()),
            },
        }
    }

    fn call(&mut self, job: Job) -> Call {
        if self.nvml.is_none()
            && let Some(init) = self.init.take()
        {
            self.nvml = Some(spawn(init));
        }
        let Some(worker) = &self.nvml else {
            return Call::Off;
        };
        if worker.jobs.send(job).is_err() {
            self.nvml = None;
            return Call::Off;
        }
        match worker.done.recv_timeout(DEADLINE) {
            Ok(Ok(done)) => Call::Done(done),
            // init failed (no library or no driver): nvidia-smi from now on
            Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                self.nvml = None;
                Call::Off
            }
            Err(RecvTimeoutError::Timeout) => {
                // abandoned, never joined: a thread stuck in the driver cannot be stopped
                self.nvml = None;
                self.announce = true;
                Call::Timeout
            }
        }
    }
}

fn spawn(init: Init) -> Worker {
    let (jobs, rx) = mpsc::channel::<Job>();
    let (tx, done) = mpsc::channel();
    // if the thread cannot start, the dropped channel makes the first call fall back to nvidia-smi
    let _ = thread::Builder::new().name("gpu-nvml".into()).spawn(move || {
        let mut api = init().map_err(|e| format!("{e:#}"));
        for job in rx {
            let reply = api.as_mut().map(|api| run(api.as_mut(), job)).map_err(|e| e.clone());
            if tx.send(reply).is_err() {
                break;
            }
        }
    });
    Worker { jobs, done }
}

fn run(api: &mut dyn Api, job: Job) -> Done {
    let err = |e: anyhow::Error| format!("{e:#}");
    match job {
        Job::Sample(slots, procs) => Done::Sample(
            slots
                .into_iter()
                .map(|s| {
                    let r = api.sample(&s, procs).map_err(err);
                    (s, r)
                })
                .collect(),
        ),
        Job::Info(slots) => Done::Info(
            slots
                .into_iter()
                .map(|s| {
                    let r = api.info(&s).map_err(err);
                    (s, r)
                })
                .collect(),
        ),
    }
}

pub fn real() -> Init {
    Box::new(|| {
        let api: Box<dyn Api> = Box::new(Real {
            nvml: Nvml::init()?,
            last_seen: HashMap::new(),
        });
        Ok(api)
    })
}

struct Real {
    nvml: Nvml,
    /// Per device, the newest per-process utilisation timestamp already reported.
    last_seen: HashMap<String, u64>,
}

impl Api for Real {
    fn sample(&mut self, pci: &str, processes: bool) -> Result<Reading> {
        let dev = self.nvml.device_by_pci_bus_id(pci)?;
        let mut d = Device::default();
        if let Ok(u) = dev.utilization_rates() {
            d.util_pct = Some(u.gpu.into());
            d.mem_busy_pct = Some(u.memory.into());
        }
        d.encoder_pct = dev.encoder_utilization().ok().map(|u| u.utilization.into());
        d.decoder_pct = dev.decoder_utilization().ok().map(|u| u.utilization.into());
        if let Ok(m) = dev.memory_info() {
            d.vram_used_bytes = Some(m.used);
            d.vram_total_bytes = Some(m.total);
        }
        d.temp_c = dev.temperature(TemperatureSensor::Gpu).ok().map(f64::from);
        d.power_w = dev.power_usage().ok().map(|mw| f64::from(mw) / 1000.0);
        d.power_limit_w = dev.enforced_power_limit().ok().map(|mw| f64::from(mw) / 1000.0);
        if dev.num_fans().is_ok_and(|n| n > 0) {
            d.fan_pct = dev.fan_speed(0).ok().map(f64::from);
            d.fan_rpm = dev.fan_speed_rpm(0).ok().map(u64::from);
        }
        d.clock_mhz = dev.clock_info(Clock::Graphics).ok().map(u64::from);
        d.mem_clock_mhz = dev.clock_info(Clock::Memory).ok().map(u64::from);
        d.pstate = dev
            .performance_state()
            .ok()
            .map(|p| p.as_c())
            .filter(|p| *p <= 15)
            .map(|p| format!("P{p}"));
        d.throttle_reasons = dev.current_throttle_reasons().ok().map(throttle_names);

        let mut procs: BTreeMap<u32, Process> = BTreeMap::new();
        if processes {
            let lists = [dev.running_graphics_processes(), dev.running_compute_processes()];
            for p in lists.into_iter().flatten().flatten() {
                let e = procs.entry(p.pid).or_insert_with(|| process(pci, p.pid));
                if let UsedGpuMemory::Used(b) = p.used_gpu_memory {
                    // a C+G process is in both lists with the same allocation
                    e.mem_bytes = Some(e.mem_bytes.unwrap_or(0).max(b));
                }
            }
            let since = self.last_seen.get(pci).copied();
            let mut latest: HashMap<u32, (u64, [u32; 3])> = HashMap::new();
            for s in dev.process_utilization_stats(since).unwrap_or_default() {
                let e = latest.entry(s.pid).or_insert((0, [0; 3]));
                if s.timestamp >= e.0 {
                    *e = (s.timestamp, [s.sm_util, s.enc_util, s.dec_util]);
                }
            }
            if let Some(newest) = latest.values().map(|(t, _)| *t).max() {
                self.last_seen.insert(pci.to_string(), newest);
            }
            for (pid, (_, [sm, enc, dec])) in latest {
                let e = procs.entry(pid).or_insert_with(|| process(pci, pid));
                e.util_pct = Some(sm.into());
                e.encoder_pct = Some(enc.into());
                e.decoder_pct = Some(dec.into());
            }
        }
        Ok(Reading {
            device: d,
            processes: procs.into_values().collect(),
        })
    }

    fn info(&mut self, pci: &str) -> Result<Info> {
        let dev = self.nvml.device_by_pci_bus_id(pci)?;
        let limits = dev.power_management_limit_constraints().ok();
        Ok(Info {
            name: dev.name().ok(),
            power_limit_min_w: limits.as_ref().map(|l| f64::from(l.min_limit) / 1000.0),
            power_limit_max_w: limits.as_ref().map(|l| f64::from(l.max_limit) / 1000.0),
        })
    }
}

fn process(pci: &str, pid: u32) -> Process {
    Process {
        pci: pci.to_string(),
        pid,
        ..Process::default()
    }
}

fn throttle_names(r: ThrottleReasons) -> Vec<String> {
    r.iter_names()
        .filter(|(_, f)| !f.is_empty())
        .map(|(n, _)| n.to_lowercase())
        .collect()
}

/// `nvidia-smi` as a child that is killed, never waited on, after the deadline.
struct Smi {
    cmd: PathBuf,
    /// A child that has not been reaped yet; while it lives no other is started.
    child: Option<Child>,
}

const QUERY: &[&str] = &[
    "index",
    "pci.bus_id",
    "temperature.gpu",
    "power.draw",
    "enforced.power.limit",
    "fan.speed",
    "utilization.gpu",
    "utilization.memory",
    "utilization.encoder",
    "utilization.decoder",
    "memory.used",
    "memory.total",
    "clocks.gr",
    "clocks.mem",
    "pstate",
    "clocks_throttle_reasons.active",
];

const MIB: u64 = 1 << 20;

impl Smi {
    fn run(&mut self, args: &[&str], deadline: Instant) -> Result<String, String> {
        if let Some(child) = &mut self.child {
            if matches!(child.try_wait(), Ok(None)) {
                return Err("nvidia-smi still running".into());
            }
            self.child = None;
        }
        let mut child = Command::new(&self.cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => "no NVML and no nvidia-smi".to_string(),
                _ => format!("nvidia-smi: {e}"),
            })?;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut text = String::new();
                    if let Some(mut out) = child.stdout.take() {
                        let _ = out.read_to_string(&mut text);
                    }
                    if !status.success() {
                        return Err(format!("nvidia-smi: {status}"));
                    }
                    return Ok(text);
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => {
                    // no wait(): a child stuck in the driver cannot be reaped and would hang us
                    let _ = child.kill();
                    self.child = Some(child);
                    return Err("nvidia-smi timeout".into());
                }
            }
        }
    }

    fn sample(&mut self, slots: &[String], processes: bool) -> Out {
        let mut out = Out {
            backend: "nvidia-smi",
            ..Out::default()
        };
        let deadline = Instant::now() + DEADLINE;
        let query = format!("--query-gpu={}", QUERY.join(","));
        let text = match self.run(&[&query, "--format=csv,noheader,nounits"], deadline) {
            Ok(t) => t,
            Err(e) => {
                out.errors = slots.iter().map(|s| (s.clone(), e.clone())).collect();
                return out;
            }
        };
        let mut by_index = HashMap::new();
        for line in text.lines() {
            let v: Vec<&str> = line.split(',').map(str::trim).collect();
            if v.len() != QUERY.len() {
                continue;
            }
            let Some(pci) = bus_id(v[1]).filter(|p| slots.contains(p)) else {
                continue;
            };
            let f = |i: usize| num(v[i]);
            let d = Device {
                temp_c: f(2),
                power_w: f(3),
                power_limit_w: f(4),
                fan_pct: f(5),
                util_pct: f(6),
                mem_busy_pct: f(7),
                encoder_pct: f(8),
                decoder_pct: f(9),
                vram_used_bytes: f(10).map(|m| m as u64 * MIB),
                vram_total_bytes: f(11).map(|m| m as u64 * MIB),
                clock_mhz: f(12).map(|m| m as u64),
                mem_clock_mhz: f(13).map(|m| m as u64),
                pstate: Some(v[14].to_string()).filter(|p| p.starts_with('P')),
                throttle_reasons: v[15]
                    .strip_prefix("0x")
                    .and_then(|h| u64::from_str_radix(h, 16).ok())
                    .map(|b| throttle_names(ThrottleReasons::from_bits_truncate(b))),
                ..Device::default()
            };
            by_index.insert(v[0].to_string(), pci.clone());
            out.devices.insert(
                pci,
                Reading {
                    device: d,
                    processes: Vec::new(),
                },
            );
        }
        if processes {
            match self.run(&["pmon", "-c", "1", "-s", "um"], deadline) {
                Ok(text) => {
                    for p in pmon(&text, &by_index) {
                        if let Some(r) = out.devices.get_mut(&p.pci) {
                            r.processes.push(p);
                        }
                    }
                }
                Err(e) => {
                    for s in slots {
                        out.errors.insert(s.clone(), format!("processes: {e}"));
                    }
                }
            }
        }
        out
    }

    fn info(&mut self, slots: &[String]) -> Option<HashMap<String, Info>> {
        let deadline = Instant::now() + DEADLINE;
        let text = self
            .run(
                &[
                    "--query-gpu=pci.bus_id,name,power.min_limit,power.max_limit",
                    "--format=csv,noheader,nounits",
                ],
                deadline,
            )
            .ok()?;
        let mut out = HashMap::new();
        for line in text.lines() {
            let v: Vec<&str> = line.split(',').map(str::trim).collect();
            if let [bus, name, min, max] = v[..]
                && let Some(pci) = bus_id(bus).filter(|p| slots.contains(p))
            {
                let info = Info {
                    name: Some(name.to_string()),
                    power_limit_min_w: num(min),
                    power_limit_max_w: num(max),
                };
                out.insert(pci, info);
            }
        }
        Some(out)
    }
}

/// `00000000:01:00.0` (8-digit domain, upper case) → sysfs `0000:01:00.0`.
fn bus_id(s: &str) -> Option<String> {
    let (domain, rest) = s.split_once(':')?;
    let domain = u32::from_str_radix(domain, 16).ok()?;
    Some(format!("{domain:04x}:{}", rest.to_lowercase()))
}

fn num(s: &str) -> Option<f64> {
    s.parse().ok()
}

fn pmon(text: &str, by_index: &HashMap<String, String>) -> Vec<Process> {
    let mut columns: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if line.starts_with('#') {
            if columns.is_empty() {
                columns = parts[1..].to_vec();
            }
            continue;
        }
        let row: HashMap<&str, &str> = columns.iter().copied().zip(parts.iter().copied()).collect();
        let (Some(pci), Some(pid)) = (
            row.get("gpu").and_then(|g| by_index.get(*g)),
            row.get("pid").and_then(|p| p.parse().ok()),
        ) else {
            continue;
        };
        let get = |c: &str| row.get(c).and_then(|v| num(v));
        out.push(Process {
            pci: pci.clone(),
            pid,
            comm: String::new(),
            util_pct: get("sm"),
            mem_bytes: get("fb").map(|m| m as u64 * MIB),
            encoder_pct: get("enc"),
            decoder_pct: get("dec"),
        });
    }
    out
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Sleeps `delay` on every call and counts calls.
    pub struct Hung {
        pub calls: Arc<AtomicUsize>,
        pub delay: Duration,
    }

    impl Api for Hung {
        fn sample(&mut self, _: &str, _: bool) -> Result<Reading> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(self.delay);
            Ok(Reading::default())
        }
        fn info(&mut self, _: &str) -> Result<Info> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(self.delay);
            Ok(Info::default())
        }
    }

    /// A fake `nvidia-smi` that logs each start to `calls` and then runs `body`.
    pub fn script(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("nvidia-smi");
        let log = dir.join("calls");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub fn starts(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("calls")).map_or(0, |s| s.lines().count())
    }

    const QUERY_OUT: &str =
        "0, 00000000:01:00.0, 43, 54.54, 450.00, 31, 25, 14, 0, 0, 2441, 24564, 1245, 5001, P3, 0x0000000000000005\n";
    const PMON_OUT: &str = "# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command \n# Idx           #    C/G      %      %      %      %      %      %     MB     MB    name \n    0       2584     G      -      -      -      -      -      -    301      0    Hyprland       \n    0       3019     G     14      6      -      -      -      -    443      0    qs             \n";

    #[test]
    fn nvidia_smi_output_maps_to_the_vendor_neutral_fields() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("case \"$1\" in pmon) printf '%s' '{PMON_OUT}';; *) printf '%s' '{QUERY_OUT}';; esac");
        let mut smi = Smi {
            cmd: script(dir.path(), &body),
            child: None,
        };
        let out = smi.sample(&["0000:01:00.0".into()], true);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let r = &out.devices["0000:01:00.0"];
        assert_eq!(r.device.vram_used_bytes, Some(2441 * MIB));
        assert_eq!(r.device.pstate.as_deref(), Some("P3"));
        // 0x5 = GPU_IDLE | SW_POWER_CAP: names, not a hex mask, so the app need not know NVIDIA's bits
        assert_eq!(
            r.device.throttle_reasons.as_deref(),
            Some(&["gpu_idle".to_string(), "sw_power_cap".to_string()][..])
        );
        // "-" means the driver reported nothing for that process, not 0 %
        assert_eq!(r.processes[0].util_pct, None);
        assert_eq!(r.processes[1].util_pct, Some(14.0));
        assert_eq!(r.processes[1].mem_bytes, Some(443 * MIB));
    }

    fn smi_only(cmd: PathBuf) -> Nvidia {
        Nvidia::new(Box::new(|| anyhow::bail!("no libnvidia-ml")), cmd)
    }

    #[test]
    fn a_hung_nvidia_smi_is_killed_without_blocking_and_reaped_later() {
        let dir = tempfile::tempdir().unwrap();
        let mut nv = smi_only(script(dir.path(), "exec sleep 30"));
        let slots = ["0000:01:00.0".to_string()];
        let started = Instant::now();
        let out = nv.sample(&slots, true);
        assert!(started.elapsed() < Duration::from_secs(2), "waited on a hung child");
        assert_eq!(out.errors[&slots[0]], "nvidia-smi timeout");
        assert!(out.devices.is_empty());
        let child = nv.smi.child.as_mut().expect("the killed child is kept for reaping");
        thread::sleep(Duration::from_millis(100));
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            child.try_wait().unwrap().and_then(|s| s.signal()),
            Some(9),
            "not killed"
        );
        // once it can be reaped, the next sample starts a fresh one
        nv.sample(&slots, true);
        assert_eq!(starts(dir.path()), 2);
    }

    #[test]
    fn at_most_one_nvidia_smi_child_is_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let mut nv = smi_only(script(dir.path(), "exit 0"));
        // stands in for a killed child stuck in the driver, which no signal can end
        nv.smi.child = Some(Command::new("sleep").arg("30").spawn().unwrap());
        let slots = ["0000:01:00.0".to_string()];
        for _ in 0..3 {
            let started = Instant::now();
            let out = nv.sample(&slots, true);
            assert!(started.elapsed() < Duration::from_millis(500));
            assert_eq!(out.errors[&slots[0]], "nvidia-smi still running");
        }
        assert_eq!(starts(dir.path()), 0, "spawned beside a child that is still alive");
        let mut stuck = nv.smi.child.take().unwrap();
        stuck.kill().unwrap();
        stuck.wait().unwrap();
        nv.smi.child = Some(stuck);
        nv.sample(&slots, false);
        assert_eq!(starts(dir.path()), 1);
    }

    #[test]
    fn nvml_bus_id_matches_the_sysfs_slot() {
        assert_eq!(bus_id("00000000:01:00.0").as_deref(), Some("0000:01:00.0"));
        assert_eq!(bus_id("00000000:0A:00.0").as_deref(), Some("0000:0a:00.0"));
    }
}
