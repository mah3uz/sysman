use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Div, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::gpu::Meta as GpuMeta;
use schema::power::{Power, Supply};
use schema::series::escape;

use crate::charts::flow::{End, EnergyFlow, Hub, Link, Sink, Source as FlowSource};
use crate::charts::format::Unit;
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale, clock};
use crate::data::hardware::{Domain, domain_of};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::source::{Combine, Source};
use crate::ui::shell::Tab;
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::card::card;
use crate::ui::widgets::hairline;
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::{caption_label, section};
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};
use crate::ui::widgets::text::{caption, facts, footnote, proportion};

const BUCKETS: u32 = 120;

/// Buckets with the span and bucket width they were read for.
type Plotted = (Rc<[Bucket]>, (f64, f64), i64);
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 160.;
const HEALTH_SPAN_S: i64 = 90 * 86400;
const CYCLES_SPAN_S: i64 = 365 * 86400;
const FLOW_CONSUMERS: usize = 5;
/// Runtime estimates past a week say more about a near-idle reading than about the battery.
const MAX_RUNTIME_MIN: f64 = 7. * 24. * 60.;
/// Dot speed for a line whose watts are unknown; it animates the diagram, it is not a reading.
const NOMINAL_W: f64 = 12.;
/// A sensor whose ring has had nothing newer for this long has gone away.
const STALE_S: f64 = 60.;
const ROW_SPARK_HEIGHT: gpui::Pixels = px(16.);

/// Which page to show: the system battery's, or the desktop's measured components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Battery(String),
    Desktop,
}

/// A battery that powers the machine; peripherals report `scope = Device`.
pub fn is_system_battery(s: &Supply) -> bool {
    s.kind.as_deref() == Some("Battery") && s.scope.as_deref() != Some("Device")
}

pub fn layout(power: &Power) -> Layout {
    power
        .iter()
        .find(|(_, s)| is_system_battery(s))
        .map_or(Layout::Desktop, |(name, _)| Layout::Battery(name.clone()))
}

/// Full-charge capacity as a share of design capacity, from whichever pair the pack reports.
pub fn health_pct(s: &Supply) -> Option<f64> {
    let ratio = |full: Option<f64>, design: Option<f64>| match (full, design) {
        (Some(f), Some(d)) if d > 0. => Some(f / d * 100.),
        _ => None,
    };
    ratio(s.energy_full_wh, s.energy_full_design_wh).or_else(|| ratio(s.charge_full_ah, s.charge_full_design_ah))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Runtime {
    ToEmpty(f64),
    ToFull(f64),
}

/// Minutes left at the present draw, from `energy_wh / power_w`.
pub fn runtime(s: &Supply) -> Option<Runtime> {
    let w = s.power_w.filter(|w| *w > 0.1)?;
    let now = s.energy_wh?;
    let minutes = match s.status.as_deref()? {
        "Discharging" => Runtime::ToEmpty(now / w * 60.),
        "Charging" => Runtime::ToFull((s.energy_full_wh? - now).max(0.) / w * 60.),
        _ => return None,
    };
    let m = match minutes {
        Runtime::ToEmpty(m) | Runtime::ToFull(m) => m,
    };
    (m <= MAX_RUNTIME_MIN).then_some(minutes)
}

/// Percentage points per hour, negative while discharging.
pub fn charge_rate(s: &Supply) -> Option<f64> {
    let w = s.power_w.filter(|w| *w > 0.1)?;
    let full = s.energy_full_wh.filter(|f| *f > 0.)?;
    let sign = match s.status.as_deref()? {
        "Discharging" => -1.,
        "Charging" => 1.,
        _ => return None,
    };
    Some(sign * w / full * 100.)
}

/// The charge line carried forward at the present rate until it hits empty, full or the horizon.
pub fn forecast(now: f64, charge: f64, rate_per_h: f64, horizon_s: f64, step_s: f64) -> Vec<Bucket> {
    let mut out = Vec::new();
    let mut t = 0.;
    while t <= horizon_s {
        let v = (charge + rate_per_h * t / 3600.).clamp(0., 100.);
        out.push(Bucket {
            t: (now + t) as i64,
            min: v,
            avg: v,
            max: v,
        });
        if v <= 0. || v >= 100. {
            break;
        }
        t += step_s;
    }
    out
}

/// Runtime at each bucket's recorded draw, only where the charge was not rising, since a charging pack has no runtime.
pub fn runtime_history(energy: &[Bucket], draw: &[Bucket], charge: &[Bucket]) -> Vec<Bucket> {
    let draw: HashMap<i64, f64> = draw.iter().map(|b| (b.t, b.avg)).collect();
    let charge: HashMap<i64, f64> = charge.iter().map(|b| (b.t, b.avg)).collect();
    let mut previous: Option<f64> = None;
    let mut out = Vec::new();
    for e in energy {
        let now = charge.get(&e.t).copied();
        let rising = matches!((previous, now), (Some(p), Some(n)) if n > p + 0.05);
        previous = now.or(previous);
        let Some(w) = draw.get(&e.t).copied().filter(|w| *w > 0.1) else {
            continue;
        };
        let m = e.avg / w * 60.;
        if !rising && m <= MAX_RUNTIME_MIN {
            out.push(Bucket {
                t: e.t,
                min: m,
                avg: m,
                max: m,
            });
        }
    }
    out
}

/// Full over design per bucket where both were recorded.
pub fn ratio_history(full: &[Bucket], design: &[Bucket]) -> Vec<Bucket> {
    let design: HashMap<i64, f64> = design.iter().map(|b| (b.t, b.avg)).collect();
    full.iter()
        .filter_map(|b| {
            let d = design.get(&b.t).copied().filter(|d| *d > 0.)?;
            Some(Bucket {
                t: b.t,
                min: b.min / d * 100.,
                avg: b.avg / d * 100.,
                max: b.max / d * 100.,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Critical,
    Low,
    Good,
}

impl Level {
    pub fn of_percent(p: f64) -> Self {
        if p < 20. {
            Self::Critical
        } else if p < 40. {
            Self::Low
        } else {
            Self::Good
        }
    }

    /// The kernel's coarse `capacity_level`, for devices that give no percentage.
    pub fn of_text(level: &str) -> Option<Self> {
        match level {
            "Critical" => Some(Self::Critical),
            "Low" => Some(Self::Low),
            "Normal" | "High" | "Full" => Some(Self::Good),
            _ => None,
        }
    }

    fn colour(self, t: &Theme) -> Hsla {
        match self {
            Self::Critical => t.red,
            Self::Low => t.orange,
            Self::Good => t.green,
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Critical => IconName::BatteryLow,
            Self::Low => IconName::BatteryMedium,
            Self::Good => IconName::BatteryFull,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Mouse,
    Keyboard,
    Headphones,
    Gamepad,
    Other,
}

impl Glyph {
    pub fn of(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        let any = |words: &[&str]| words.iter().any(|w| n.contains(w));
        if any(&["mouse", "trackball", "mx master", "mx anywhere"]) {
            Self::Mouse
        } else if any(&["keyboard", "keys"]) {
            Self::Keyboard
        } else if any(&["headset", "headphone", "buds", "earbud", "wh-", "airpods"]) {
            Self::Headphones
        } else if any(&["controller", "gamepad", "joy-con", "dualsense", "dualshock", "xbox"]) {
            Self::Gamepad
        } else {
            Self::Other
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Mouse => IconName::Mouse,
            Self::Keyboard => IconName::Keyboard,
            Self::Headphones => IconName::Headphones,
            Self::Gamepad => IconName::Gamepad2,
            Self::Other => IconName::Battery,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Accessory {
    pub key: String,
    pub name: String,
    pub glyph: Glyph,
    pub percent: Option<i64>,
    pub level: Option<Level>,
    /// The coarse level's own word, shown when there is no percentage.
    pub level_text: Option<String>,
    pub charging: bool,
    pub offline: bool,
}

pub fn accessories(power: &Power) -> Vec<Accessory> {
    power
        .iter()
        .filter(|(_, s)| s.scope.as_deref() == Some("Device"))
        .map(|(key, s)| {
            let name = s.model_name.clone().unwrap_or_else(|| key.clone());
            let percent = s.capacity.filter(|p| (0..=100).contains(p));
            let level_text = s.capacity_level.clone().filter(|l| l != "Unknown");
            Accessory {
                key: key.clone(),
                glyph: Glyph::of(&name),
                name,
                percent,
                level: percent
                    .map(|p| Level::of_percent(p as f64))
                    .or_else(|| level_text.as_deref().and_then(Level::of_text)),
                level_text,
                charging: s.status.as_deref() == Some("Charging"),
                offline: s.online == Some(0),
            }
        })
        .collect()
}

/// Whether a mains or USB supply is online; `None` when the machine reports no adapter at all.
pub fn on_mains(power: &Power) -> Option<bool> {
    let adapters: Vec<&Supply> = power
        .values()
        .filter(|s| matches!(s.kind.as_deref(), Some("Mains" | "USB")) && s.scope.as_deref() != Some("Device"))
        .collect();
    (!adapters.is_empty()).then(|| adapters.iter().any(|s| s.online == Some(1)))
}

/// The busiest process groups and each one's share of all processes' CPU time.
pub fn consumers(groups: &[(String, f64)], all_cpu: f64, n: usize) -> Vec<(String, f64)> {
    let total = all_cpu.max(groups.iter().map(|g| g.1).sum());
    if total <= 0. {
        return Vec::new();
    }
    let mut ranked: Vec<_> = groups.iter().filter(|g| g.1 > 0.).cloned().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
        .into_iter()
        .take(n)
        .map(|(name, cpu)| (name, cpu / total))
        .collect()
}

/// The sum of the readings that exist and how many there were; nothing measured is `None`, not zero.
pub fn total_measured(readings: &[Option<f64>]) -> Option<(f64, usize)> {
    let known: Vec<f64> = readings.iter().flatten().copied().collect();
    (!known.is_empty()).then(|| (known.iter().sum(), known.len()))
}

/// One hwmon sensor's latest value from the live rings.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub chip: String,
    pub kind: String,
    pub label: String,
    pub path: String,
    pub value: f64,
}

/// Splits a `/hwmon/<chip>/<kind>/<label>` series path into its unescaped parts.
pub fn hwmon_parts(path: &str) -> Option<(String, String, String)> {
    let unescape = |s: &str| s.replace("~1", "/").replace("~0", "~");
    let mut parts = path.strip_prefix("/hwmon/")?.split('/');
    let (chip, kind, label) = (parts.next()?, parts.next()?, parts.next()?);
    parts
        .next()
        .is_none()
        .then(|| (unescape(chip), kind.to_owned(), unescape(label)))
}

/// The electrical readings a desktop exposes, sorted into the panel's groups.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Electrical {
    /// Per GPU PCI slot, the voltage and power sensors of that GPU's own hwmon chip.
    pub gpu: BTreeMap<String, Vec<Reading>>,
    /// Motherboard controller `in` pins that read above zero.
    pub board: Vec<Reading>,
    /// Fans turning now.
    pub fans: Vec<Reading>,
    /// Fan headers reading 0 rpm: stopped or nothing connected.
    pub idle_fans: usize,
}

/// `in2` before `in10`.
fn natural(label: &str) -> (String, u64) {
    let digits = label.len() - label.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (stem, n) = label.split_at(label.len() - digits);
    (stem.to_owned(), n.parse().unwrap_or(0))
}

pub fn electrical(readings: Vec<Reading>, gpus: &[&str]) -> Electrical {
    let mut out = Electrical::default();
    for r in readings {
        let device = r.chip.split_once('/').map_or("", |(_, d)| d);
        let gpu = gpus.iter().find(|pci| **pci == device);
        match (r.kind.as_str(), gpu) {
            ("in" | "power", Some(pci)) => out.gpu.entry((*pci).to_owned()).or_default().push(r),
            ("in", None) if domain_of(&r.chip) == Domain::Board && r.value > 0. => out.board.push(r),
            ("fan", _) if r.value > 0. => out.fans.push(r),
            ("fan", _) => out.idle_fans += 1,
            _ => {}
        }
    }
    let order = |a: &Reading, b: &Reading| (&a.chip, natural(&a.label)).cmp(&(&b.chip, natural(&b.label)));
    out.board.sort_by(order);
    out.fans.sort_by(order);
    for rails in out.gpu.values_mut() {
        // voltages first, then the power sensor
        rails.sort_by_key(|r| (r.kind != "in", natural(&r.label)));
    }
    out
}

/// The GPU chip's one power sensor, which is where the driver's power reading comes from, so it is shown once.
/// Matching by kind rather than value keeps the label steady while two reads of a busy GPU differ.
pub fn power_twin(rails: &[Reading], watts: Option<f64>) -> Option<&Reading> {
    watts?;
    let mut power = rails.iter().filter(|r| r.kind == "power");
    match (power.next(), power.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

pub fn reading_value(r: &Reading) -> String {
    match r.kind.as_str() {
        "in" => format!("{:.3} V", r.value),
        "power" => format!("{:.1} W", r.value),
        "fan" => format!("{:.0} rpm", r.value),
        _ => format!("{:.1}", r.value),
    }
}

pub fn hours_minutes(minutes: f64) -> String {
    let m = minutes.round() as i64;
    format!("{}:{:02}", m / 60, m % 60)
}

fn power_path(supply: &str, field: &str) -> String {
    format!("/power/{}/{field}", escape(supply))
}

fn gpu_power_path(pci: &str) -> String {
    format!("/gpu/devices/{}/power_w", escape(pci))
}

/// The hwmon temperature the kernel registers for a battery, under the supply's own name.
fn battery_temp(sample: &Sample, battery: &str) -> Option<(String, f64)> {
    let hwmon = sample.hwmon.as_ref()?;
    hwmon.iter().find_map(|(chip, kinds)| {
        let name = chip.split('/').next().unwrap_or(chip);
        if !name.eq_ignore_ascii_case(battery) {
            return None;
        }
        let (label, v) = kinds.get("temp")?.iter().find_map(|(l, v)| Some((l, (*v)?)))?;
        Some((format!("/hwmon/{}/temp/{}", escape(chip), escape(label)), v))
    })
}

#[derive(Debug, Clone, PartialEq)]
struct GpuPower {
    pci: String,
    name: String,
    watts: Option<f64>,
    limit: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuPower {
    Measured,
    NeedsHelper,
    Unsupported,
}

/// Everything the page draws from one moment of live data.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    power: Power,
    temp: Option<(String, f64)>,
    cpu_w: Option<f64>,
    helper_rapl: Option<bool>,
    gpus: Vec<GpuPower>,
    consumers: Vec<(String, f64)>,
    /// When the power module last reported.
    checked: Option<f64>,
    electrical: Electrical,
}

/// The hwmon sensors still reporting. Read from the live rings, since a sample omits chips read less often.
fn readings(live: &Live, now: f64) -> Vec<Reading> {
    live.paths()
        .filter_map(|path| {
            let (chip, kind, label) = hwmon_parts(path)?;
            let &(t, value) = live.series(path)?.back()?;
            (now - t <= STALE_S).then(|| Reading {
                chip,
                kind,
                label,
                path: path.to_owned(),
                value,
            })
        })
        .collect()
}

impl Snapshot {
    fn from_live(live: &Live, last_power: Option<&(f64, Power)>, fake: bool) -> Self {
        let Some(sample) = live.latest.as_ref() else {
            return Self::default();
        };
        let names: Option<GpuMeta> = live
            .meta
            .as_ref()
            .and_then(|m| m.modules.get("gpu")?.info.clone())
            .and_then(|info| serde_json::from_value(info).ok());
        let gpus: Vec<GpuPower> = sample
            .gpu
            .as_ref()
            .map(|g| {
                g.devices
                    .iter()
                    .map(|(pci, d)| GpuPower {
                        pci: pci.clone(),
                        name: names
                            .as_ref()
                            .and_then(|m| m.devices.get(pci)?.name.clone())
                            .unwrap_or_else(|| pci.clone()),
                        watts: d.power_w,
                        limit: d.power_limit_w,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let consumers = sample.processes.as_ref().map_or_else(Vec::new, |p| {
            let groups: Vec<(String, f64)> = match &p.groups_cpu {
                Some(g) => g.iter().map(|g| (g.name.clone(), g.cpu_pct)).collect(),
                None => p.top_cpu.iter().map(|p| (p.comm.clone(), p.cpu_pct)).collect(),
            };
            let all: f64 = p.table.iter().map(|p| p.cpu_pct).sum();
            consumers(&groups, all, FLOW_CONSUMERS)
        });
        let mut power = sample
            .power
            .clone()
            .or_else(|| last_power.map(|p| p.1.clone()))
            .unwrap_or_default();
        let mut temp = None;
        if fake {
            power = fixture::laptop();
            temp = Some((power_path("BAT0", "temp"), fixture::TEMP_C));
        } else if let Layout::Battery(name) = layout(&power) {
            temp = battery_temp(sample, &name);
        }
        let checked = power.keys().find_map(|name| {
            let prefix = format!("/power/{}/", escape(name));
            live.paths()
                .filter(|p| p.starts_with(&prefix))
                .filter_map(|p| live.series(p)?.back().map(|v| v.0))
                .reduce(f64::max)
        });
        let gpu_slots: Vec<&str> = gpus.iter().map(|g| g.pci.as_str()).collect();
        let electrical = electrical(readings(live, sample.t), &gpu_slots);
        Self {
            electrical,
            temp,
            cpu_w: sample.cpu.as_ref().and_then(|c| c.power_w),
            helper_rapl: sample
                .helper
                .as_ref()
                .filter(|h| h.state == "running")
                .map(|h| h.features.get("rapl").copied().unwrap_or(false)),
            gpus,
            consumers,
            checked: if fake {
                Some(sample.t)
            } else {
                checked.or(last_power.map(|p| p.0))
            },
            power,
        }
    }

    fn cpu_power(&self) -> CpuPower {
        match (self.cpu_w, self.helper_rapl) {
            (Some(_), _) => CpuPower::Measured,
            (None, Some(false)) => CpuPower::Unsupported,
            _ => CpuPower::NeedsHelper,
        }
    }

    fn total(&self) -> Option<(f64, usize)> {
        let readings: Vec<Option<f64>> = std::iter::once(self.cpu_w)
            .chain(self.gpus.iter().map(|g| g.watts))
            .collect();
        total_measured(&readings)
    }
}

/// A plotted series, read from history and topped up from the live rings.
#[derive(Debug, Clone, PartialEq)]
struct Query {
    key: String,
    source: Source,
    span_s: i64,
}

fn queries(layout: &Layout, snap: &Snapshot, range: Range) -> Vec<Query> {
    let q = |key: &str, paths: Vec<String>, combine, span_s| Query {
        key: key.into(),
        source: Source::new(paths, combine),
        span_s,
    };
    let span = range.span_s();
    match layout {
        Layout::Battery(name) => {
            let mut out = vec![
                q("charge", vec![power_path(name, "capacity")], Combine::Max, span),
                q("draw", vec![power_path(name, "power_w")], Combine::Max, span),
                q("voltage", vec![power_path(name, "voltage_v")], Combine::Max, span),
                q("energy", vec![power_path(name, "energy_wh")], Combine::Max, span),
                q(
                    "full",
                    vec![power_path(name, "energy_full_wh")],
                    Combine::Max,
                    HEALTH_SPAN_S,
                ),
                q(
                    "design",
                    vec![power_path(name, "energy_full_design_wh")],
                    Combine::Max,
                    HEALTH_SPAN_S,
                ),
                q(
                    "cycles",
                    vec![power_path(name, "cycle_count")],
                    Combine::Max,
                    CYCLES_SPAN_S,
                ),
            ];
            if let Some((path, _)) = &snap.temp {
                out.push(q("temp", vec![path.clone()], Combine::Max, span));
            }
            out
        }
        Layout::Desktop => {
            let mut out = vec![q("cpu", vec!["/cpu/power_w".into()], Combine::Max, span)];
            let mut all = vec!["/cpu/power_w".to_owned()];
            for g in &snap.gpus {
                out.push(q(
                    &format!("gpu:{}", g.pci),
                    vec![gpu_power_path(&g.pci)],
                    Combine::Max,
                    span,
                ));
                all.push(gpu_power_path(&g.pci));
            }
            out.push(q("total", all, Combine::Sum, span));
            let e = &snap.electrical;
            for r in e.gpu.values().flatten().chain(&e.board).chain(&e.fans) {
                out.push(q(&r.path, vec![r.path.clone()], Combine::Max, span));
            }
            out
        }
    }
}

fn load(queries: Vec<Query>, to: i64) -> Result<HashMap<String, Vec<Bucket>>> {
    let history = History::open(&db_path())?;
    queries
        .into_iter()
        .map(|q| Ok((q.key, q.source.query(&history, to - q.span_s, to, BUCKETS)?)))
        .collect()
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardKind {
    Charge,
    Runtime,
    Draw,
    Temp,
    Health,
    Cycles,
    Cpu,
    Gpu(usize),
    Total,
}

impl CardKind {
    fn explanation(self) -> (&'static str, &'static str) {
        match self {
            Self::Charge => (
                "The charge available now, as a share of what the battery holds when full today.",
                "The kernel's capacity reading for the pack. Recorded gaps stay gaps.",
            ),
            Self::Runtime => (
                "An estimate of how long this charge lasts at the present draw, or how long until full while charging. It changes when the workload does.",
                "Energy left divided by the present draw. While charging, the energy still missing divided by the charging power. Estimates past a week are not shown.",
            ),
            Self::Draw => (
                "The power flowing out of the battery, which on battery is the whole machine's draw. While charging it is the power going in.",
                "The pack's own power reading, or voltage times current where that is all it reports.",
            ),
            Self::Temp => (
                "The battery pack's temperature, not the CPU's. Charging and sustained load warm it.",
                "The temperature sensor the kernel registers for the battery.",
            ),
            Self::Health => (
                "How much charge the battery can hold compared with when it was new. Small changes can be calibration rather than wear.",
                "Full-charge capacity divided by design capacity, one point per day of recorded history.",
            ),
            Self::Cycles => (
                "A cycle is one full battery's worth of use, not each time it is plugged in.",
                "The pack's own cycle counter, as recorded each day.",
            ),
            Self::Cpu => (
                "Power drawn by the CPU package: cores, caches and the memory controller.",
                "The RAPL energy counter read by the privileged helper, as a rate.",
            ),
            Self::Gpu(_) => (
                "Power drawn by this graphics card or integrated GPU as its driver reports it.",
                "NVML for NVIDIA cards, the amdgpu power sensor for AMD.",
            ),
            Self::Total => (
                "The sum of the parts that report their power. Drives, fans, memory and the power supply's own losses are not metered, so the wall draw is higher.",
                "CPU package power, when the helper reads it, plus each GPU's reported power.",
            ),
        }
    }
}

#[derive(Default)]
struct Charts {
    main: Rc<RefCell<ChartState>>,
    second: Rc<RefCell<ChartState>>,
    sheet: Rc<RefCell<ChartState>>,
}

pub struct Energy {
    live: Entity<Live>,
    range: Range,
    fake: bool,
    stored: Option<(Range, Layout, HashMap<String, Vec<Bucket>>)>,
    loading: bool,
    attempt: Option<(Range, Layout, f64)>,
    error: Option<SharedString>,
    merged: HashMap<String, Plotted>,
    version: u64,
    charts: Charts,
    sheet: Option<CardKind>,
    /// The power module reports once a minute, so its last stored reading fills the page until then.
    last_power: Option<(f64, Power)>,
    scroll: gpui::ScrollHandle,
    dev_scroll: Option<f32>,
    rail_scroll: gpui::ScrollHandle,
    /// `SYSMAN_DEV_SCROLL=rail:<y>` scrolls the side rail instead of the page.
    dev_rail: bool,
    board_open: bool,
}

impl Energy {
    pub fn new(live: Entity<Live>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        #[cfg(debug_assertions)]
        let fake = std::env::var_os("SYSMAN_DEV_FAKE_BATTERY").is_some();
        #[cfg(not(debug_assertions))]
        let fake = false;
        let mut this = Self {
            live,
            range: remembered(Tab::Energy, cx),
            fake,
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            merged: HashMap::new(),
            version: 0,
            charts: Charts::default(),
            sheet: None,
            last_power: None,
            scroll: gpui::ScrollHandle::new(),
            dev_scroll: None,
            rail_scroll: gpui::ScrollHandle::new(),
            dev_rail: false,
            board_open: true,
        };
        #[cfg(debug_assertions)]
        {
            this.sheet = match std::env::var("SYSMAN_DEV_ENERGY_SHEET").as_deref() {
                Ok("charge") => Some(CardKind::Charge),
                Ok("health") => Some(CardKind::Health),
                Ok("gpu") => Some(CardKind::Gpu(0)),
                Ok("total") => Some(CardKind::Total),
                _ => None,
            };
            if let Some(y) = std::env::var("SYSMAN_DEV_SCROLL").ok().and_then(|v| {
                this.dev_rail = v.starts_with("rail:");
                v.trim_start_matches("rail:").parse::<f32>().ok()
            }) {
                this.dev_scroll = Some(y);
            }
            this.board_open = std::env::var("SYSMAN_DEV_ENERGY_BOARD").as_deref() != Ok("closed");
        }
        let task = cx.background_executor().spawn(async {
            let (t, json) = History::open(&db_path()).ok()?.snapshot("power").ok()??;
            Some((t as f64, serde_json::from_str::<Power>(&json).ok()?))
        });
        cx.spawn(async move |this, cx| {
            let last = task.await;
            this.update(cx, |this, cx| {
                this.last_power = last;
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
        this.refresh(cx);
        this
    }

    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        self.live.read(cx).latest.is_some() && !self.loading && self.attempt.is_some()
    }

    fn snapshot(&self, cx: &App) -> Snapshot {
        Snapshot::from_live(self.live.read(cx), self.last_power.as_ref(), self.fake)
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Energy, range, cx);
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.live.read(cx).latest.is_none() {
            return;
        }
        let snap = self.snapshot(cx);
        let layout = layout(&snap.power);
        let now = self.now(cx);
        let stale = self.attempt.as_ref().is_none_or(|(r, l, at)| {
            *r != self.range
                || *l != layout
                || self
                    .live
                    .read(cx)
                    .reload_due(*at, now, self.range.span_s() as f64, RELOAD_S)
        });
        let qs = queries(&layout, &snap, self.range);
        // a sensor read less often than each sample appears after the first load
        let unread = self
            .stored
            .as_ref()
            .filter(|(r, l, _)| *r == self.range && *l == layout)
            .is_some_and(|(_, _, s)| qs.iter().any(|q| !s.contains_key(&q.key)));
        if (stale || unread) && !self.loading {
            self.attempt = Some((self.range, layout.clone(), now));
            if self.fake {
                self.stored = Some((self.range, layout.clone(), fixture::history(&qs, now)));
            } else {
                self.load(qs, layout.clone(), now, cx);
            }
        }
        self.merge(&layout, &snap, now, cx);
    }

    fn load(&mut self, qs: Vec<Query>, layout: Layout, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        let range = self.range;
        let task = cx.background_executor().spawn(async move { load(qs, now as i64) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(stored) => {
                        this.error = None;
                        this.stored = Some((range, layout, stored));
                    }
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn merge(&mut self, layout: &Layout, snap: &Snapshot, now: f64, cx: &App) {
        let live = self.live.read(cx);
        let stored = self
            .stored
            .as_ref()
            .filter(|(r, l, _)| *r == self.range && l == layout)
            .map(|s| &s.2);
        self.merged.clear();
        for q in queries(layout, snap, self.range) {
            let to = now as i64;
            let from = to - q.span_s;
            let width = bucket_width(from, to, BUCKETS);
            let history = stored.and_then(|s| s.get(&q.key)).map_or(&[][..], Vec::as_slice);
            let data = if self.fake {
                history.to_vec()
            } else {
                q.source.top_up(history, live, from, width)
            };
            self.merged.insert(q.key, (data.into(), (from as f64, now), width));
        }
        let derived = |merged: &HashMap<String, Plotted>, a: &str| {
            merged
                .get(a)
                .map(|m| m.0.clone())
                .unwrap_or_else(|| Rc::from(Vec::new()))
        };
        if let Some((_, span, w)) = self.merged.get("energy").cloned() {
            let runtime = runtime_history(
                &derived(&self.merged, "energy"),
                &derived(&self.merged, "draw"),
                &derived(&self.merged, "charge"),
            );
            self.merged.insert("runtime".into(), (runtime.into(), span, w));
        }
        if let Some((_, span, w)) = self.merged.get("full").cloned() {
            let health = ratio_history(&derived(&self.merged, "full"), &derived(&self.merged, "design"));
            self.merged.insert("health".into(), (health.into(), span, w));
        }
        self.version += 1;
    }

    fn series(&self, key: &str) -> (Rc<[Bucket]>, (f64, f64), i64) {
        self.merged
            .get(key)
            .cloned()
            .unwrap_or_else(|| (Rc::from(Vec::new()), (0., 1.), 1))
    }

    fn gap_s(width: i64) -> f64 {
        (width as f64 * 1.5).max(30. * 2.5)
    }

    fn spark(&self, key: &str, colour: Hsla) -> Sparkline {
        let (data, span, w) = self.series(key);
        Sparkline::new(
            SharedString::from(format!("energy-spark-{key}")),
            data,
            colour,
            span,
            w as f64,
            Self::gap_s(w),
        )
    }

    /// A row-sized sparkline of a merged series, or `None` without history.
    fn row_spark(&self, key: &str, colour: Hsla) -> Option<Sparkline> {
        let (data, span, w) = self.series(key);
        (!data.is_empty()).then(|| {
            Sparkline::new(
                SharedString::from(format!("energy-row-{key}")),
                data,
                colour,
                span,
                w as f64,
                Self::gap_s(w),
            )
            .compact(ROW_SPARK_HEIGHT)
        })
    }

    fn chart(&self, state: &Rc<RefCell<ChartState>>, key: &str) -> TrendChart {
        let (_, span, w) = self.series(key);
        TrendChart::new(state.clone(), self.version, span, w as f64, Self::gap_s(w)).height(px(CHART_HEIGHT))
    }

    fn open_sheet(&mut self, kind: CardKind, cx: &mut Context<Self>) {
        self.sheet = Some(kind);
        cx.notify();
    }
}

fn divider(cx: &App) -> Div {
    let t = theme(cx);
    div().h(t.hairline_width).bg(t.hairline)
}

/// Label, a tiny sparkline where there is history, and the value.
fn reading_row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    colour: Hsla,
    spark: Option<Sparkline>,
    cx: &App,
) -> Div {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .h(px(18.))
        .text_size(t.caption)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(t.text_secondary)
                .child(label.into()),
        )
        .child(div().w(px(56.)).flex_shrink_0().children(spark))
        .child(
            div()
                .w(px(66.))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .font_features(t.numbers.clone())
                .text_color(colour)
                .child(value.into()),
        )
}

fn big_value(value: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .text_size(t.card_value)
        .font_weight(FontWeight::SEMIBOLD)
        .font_features(t.numbers.clone())
        .child(value.into())
}

impl Energy {
    #[allow(clippy::too_many_arguments)]
    fn metric_card(
        &self,
        id: &'static str,
        kind: CardKind,
        label: &str,
        colour: Hsla,
        value: String,
        detail: Option<String>,
        body: AnyElement,
        range: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        card(id, label, colour, cx)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.open_sheet(kind, cx)))
            .child(big_value(value, cx))
            .child(caption(detail.unwrap_or_default(), cx).font_features(theme(cx).numbers.clone()))
            .child(body)
            .child(
                div()
                    .text_size(theme(cx).small_caption)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme(cx).text_secondary)
                    .child(range.to_owned()),
            )
            .into_any_element()
    }

    fn header(&self, subtitle: String, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let t = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(t.gap)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Energy"),
                    )
                    .child(caption(subtitle, cx))
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_size(t.caption).text_color(t.orange).child(e))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(5.))
                    .child(caption_label("History", cx))
                    .child(range_picker(
                        self.range,
                        move |range, _, cx| {
                            this.update(cx, |e, cx| e.set_range(range, cx)).ok();
                        },
                        cx,
                    )),
            )
    }

    fn battery_cards(&self, s: &Supply, snap: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (green, blue, orange, teal, indigo) = (t.green, t.blue, t.orange, t.teal, t.indigo);
        let range = self.range.label();
        let mains = on_mains(&snap.power);
        let mut cards = Vec::new();
        let state = s.status.clone().unwrap_or_else(|| "–".into());
        let charge_detail = match (s.status.as_deref(), mains) {
            (Some("Charging"), _) => "Charging".to_owned(),
            (Some("Discharging"), _) => "On battery".to_owned(),
            (_, Some(true)) => "On adapter".to_owned(),
            _ => state,
        };
        cards.push(self.metric_card(
            "card-charge",
            CardKind::Charge,
            "Charge",
            green,
            s.capacity.map_or("–".into(), |c| format!("{c}%")),
            Some(charge_detail),
            self.spark("charge", green).fixed(100., "100%").into_any_element(),
            range,
            cx,
        ));
        let rt = runtime(s);
        let (rt_label, rt_value) = match rt {
            Some(Runtime::ToFull(m)) => ("Time to full", hours_minutes(m)),
            Some(Runtime::ToEmpty(m)) => ("Estimated runtime", hours_minutes(m)),
            None if mains == Some(true) => ("Estimated runtime", "On adapter".into()),
            None => ("Estimated runtime", "–".into()),
        };
        cards.push(self.metric_card(
            "card-runtime",
            CardKind::Runtime,
            rt_label,
            blue,
            rt_value,
            matches!(rt, Some(Runtime::ToEmpty(_))).then(|| "at the present draw".to_owned()),
            self.spark("runtime", blue).into_any_element(),
            range,
            cx,
        ));
        let flow = match s.status.as_deref() {
            Some("Charging") => "into the battery",
            Some("Discharging") => "from the battery",
            _ => "battery flow",
        };
        cards.push(self.metric_card(
            "card-draw",
            CardKind::Draw,
            "Power draw",
            orange,
            s.power_w.map_or("–".into(), |w| format!("{w:.2} W")),
            s.power_w.map(|_| flow.to_owned()),
            self.spark("draw", orange).into_any_element(),
            range,
            cx,
        ));
        if let Some((_, c)) = &snap.temp {
            cards.push(self.metric_card(
                "card-temp",
                CardKind::Temp,
                "Battery temperature",
                teal,
                format!("{c:.1}°C"),
                None,
                self.spark("temp", teal).into_any_element(),
                range,
                cx,
            ));
        }
        let health = health_pct(s);
        cards.push(self.metric_card(
            "card-health",
            CardKind::Health,
            "Health",
            green,
            health.map_or("–".into(), |h| format!("{h:.0}%")),
            None,
            self.spark("health", green).into_any_element(),
            "90 days",
            cx,
        ));
        cards.push(self.metric_card(
            "card-cycles",
            CardKind::Cycles,
            "Cycles",
            indigo,
            s.cycle_count.map_or("–".into(), |c| c.to_string()),
            None,
            self.spark("cycles", indigo).into_any_element(),
            "1 year",
            cx,
        ));
        div().flex().flex_row().gap(px(10.)).children(cards)
    }

    fn desktop_cards(&self, snap: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (green, blue, orange, purple) = (t.green, t.blue, t.orange, t.purple);
        let range = self.range.label();
        let watts = |w: Option<f64>| w.map_or("–".into(), |w| format!("{w:.1} W"));
        let mut cards = Vec::new();
        let cpu = snap.cpu_power();
        let cpu_body = match cpu {
            CpuPower::Measured => self.spark("cpu", green).into_any_element(),
            CpuPower::NeedsHelper => div()
                .h(px(57.))
                .child(helper_hint(
                    "energy-enable",
                    "CPU package power needs the privileged helper",
                    cx,
                ))
                .into_any_element(),
            CpuPower::Unsupported => div()
                .h(px(57.))
                .child(caption("This CPU has no readable power counter.", cx))
                .into_any_element(),
        };
        cards.push(self.metric_card(
            "card-cpu",
            CardKind::Cpu,
            "CPU power",
            green,
            watts(snap.cpu_w),
            Some("package".into()),
            cpu_body,
            range,
            cx,
        ));
        const GPU_IDS: [&str; 4] = ["card-gpu-0", "card-gpu-1", "card-gpu-2", "card-gpu-3"];
        for (i, g) in snap.gpus.iter().enumerate().take(GPU_IDS.len()) {
            let colour = if i == 0 { orange } else { purple };
            cards.push(self.metric_card(
                GPU_IDS[i],
                CardKind::Gpu(i),
                "GPU power",
                colour,
                watts(g.watts),
                Some(g.name.clone()),
                self.spark(&format!("gpu:{}", g.pci), colour).into_any_element(),
                range,
                cx,
            ));
        }
        let total = snap.total();
        cards.push(self.metric_card(
            "card-total",
            CardKind::Total,
            "Total measured",
            blue,
            watts(total.map(|t| t.0)),
            Some(measured_parts(snap)),
            self.spark("total", blue).into_any_element(),
            range,
            cx,
        ));
        div().flex().flex_row().gap(px(10.)).children(cards)
    }

    fn battery_flow(&self, s: &Supply, snap: &Snapshot, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let charge = s.capacity.unwrap_or(0) as f64;
        let mains = on_mains(&snap.power) == Some(true);
        let charging = s.status.as_deref() == Some("Charging");
        let draw = s.power_w.unwrap_or(0.);
        let battery_caption: SharedString = if charging {
            format!("+{draw:.1} W").into()
        } else if mains {
            (if charge >= 99. { "full" } else { "holding" }).into()
        } else if draw < 0.1 {
            "–".into()
        } else {
            format!("−{draw:.1} W").into()
        };
        let battery = FlowSource::Battery {
            percent: charge,
            fill: Level::of_percent(charge).colour(t),
            caption: battery_caption,
            caption_color: if charging || (!mains && draw >= 0.1) {
                t.green
            } else {
                t.text_secondary
            },
        };
        let load = if mains { NOMINAL_W } else { draw.max(0.5) };
        let mut sources = Vec::new();
        let mut links = Vec::new();
        if mains {
            sources.push(FlowSource::Plug {
                caption: "AC".into(),
                color: t.blue,
            });
            sources.push(battery);
            links.push(Link {
                from: End::Source(0),
                to: End::HubIn(0, -12.),
                color: t.blue,
                watts: Some(load + if charging { draw } else { 0. }),
            });
            if charging {
                links.push(Link {
                    from: End::HubIn(0, 12.),
                    to: End::Source(1),
                    color: t.green,
                    watts: Some(draw),
                });
            }
        } else {
            sources.push(battery);
            links.push(Link {
                from: End::Source(0),
                to: End::HubIn(0, 0.),
                color: t.green,
                watts: Some(load),
            });
        }
        let sinks = self.sinks(snap, load, End::HubOut(0), &mut links, cx);
        let flow = EnergyFlow {
            sources,
            hubs: vec![Hub {
                icon: IconName::Laptop,
                title: None,
                value: format!("{charge:.0}%").into(),
            }],
            sinks,
            links,
            animate: true,
        };
        let mut status = Vec::new();
        if charging {
            status.push("Charging".to_owned());
            status.push(format!("+{draw:.1} W into the battery"));
            if let Some(Runtime::ToFull(m)) = runtime(s) {
                status.push(format!("{} to full", hours_minutes(m)));
            }
        } else if mains {
            status.push("On power adapter".to_owned());
            status.push(
                if charge >= 99. {
                    "battery full"
                } else {
                    "battery holding"
                }
                .to_owned(),
            );
        } else {
            status.push("On battery".to_owned());
            status.push(format!("{draw:.1} W out"));
            if let Some(Runtime::ToEmpty(m)) = runtime(s) {
                status.push(format!("{} remaining", hours_minutes(m)));
            }
        }
        self.flow_panel(flow, status.join(" · "), cx)
    }

    fn desktop_flow(&self, snap: &Snapshot, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let total = snap.total();
        let mut hubs = vec![Hub {
            icon: IconName::Cpu,
            title: Some("CPU".into()),
            value: snap.cpu_w.map_or("not measured".into(), |w| format!("{w:.1} W").into()),
        }];
        let mut links = vec![Link {
            from: End::Source(0),
            to: End::HubIn(0, 0.),
            color: t.blue,
            watts: Some(snap.cpu_w.unwrap_or(NOMINAL_W)),
        }];
        for (i, g) in snap.gpus.iter().enumerate() {
            hubs.push(Hub {
                icon: IconName::Monitor,
                title: Some(short_gpu(&g.name).into()),
                value: g.watts.map_or("–".into(), |w| format!("{w:.1} W").into()),
            });
            links.push(Link {
                from: End::Source(0),
                to: End::HubIn(i + 1, 0.),
                color: t.blue,
                watts: g.watts,
            });
        }
        let sinks = self.sinks(snap, snap.cpu_w.unwrap_or(NOMINAL_W), End::HubOut(0), &mut links, cx);
        let flow = EnergyFlow {
            sources: vec![FlowSource::Plug {
                caption: total.map_or("AC power".into(), |(w, _)| {
                    format!("AC power · {w:.0} W measured").into()
                }),
                color: t.blue,
            }],
            hubs,
            sinks,
            links,
            animate: true,
        };
        let status = match (total, snap.cpu_w) {
            (Some((w, _)), Some(_)) => {
                format!("Measured {w:.1} W at the CPU and GPUs · drives, fans and the power supply are not metered")
            }
            (Some((w, _)), None) => format!("Measured {w:.1} W at the GPUs · CPU power needs the privileged helper"),
            (None, _) => "On AC power · no component reports its power".to_owned(),
        };
        self.flow_panel(flow, status, cx)
    }

    fn sinks(&self, snap: &Snapshot, load: f64, from: End, links: &mut Vec<Link>, cx: &App) -> Vec<Sink> {
        let t = theme(cx);
        snap.consumers
            .iter()
            .enumerate()
            .map(|(i, (name, share))| {
                links.push(Link {
                    from,
                    to: End::Sink(i),
                    color: t.orange,
                    watts: Some(load * share),
                });
                Sink {
                    name: name.clone().into(),
                    detail: format!("{:.0}% of CPU time", share * 100.).into(),
                }
            })
            .collect()
    }

    fn flow_panel(&self, flow: EnergyFlow, status: String, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        section(IconName::Activity, "Energy flow", cx)
            .child(flow)
            .child(divider(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .flex_1()
                            .text_size(t.caption)
                            .font_weight(FontWeight::MEDIUM)
                            .child(status),
                    )
                    .child(caption(
                        "Branches show each process's share of CPU time, not measured energy.",
                        cx,
                    )),
            )
    }

    fn charge_panel(&self, s: &Supply, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let (data, span, w) = self.series("charge");
        let mut chart_span = span;
        let mut forecast_data = None;
        if let (Some(rate), Some(charge)) = (charge_rate(s), s.capacity) {
            let horizon = (span.1 - span.0) / 3.;
            let f = forecast(span.1, charge as f64, rate, horizon, (w as f64).max(1.));
            chart_span.1 = span.1 + horizon;
            forecast_data = Some(f);
        }
        let mut chart = TrendChart::new(
            self.charts.main.clone(),
            self.version,
            chart_span,
            w as f64,
            Self::gap_s(w),
        )
        .height(px(CHART_HEIGHT))
        .line(data, t.green, "Charge")
        .unit(Unit::Percent)
        .y(YScale::Fixed(0., 100.))
        .fill();
        if let Some(f) = forecast_data {
            chart = chart.dashed(f.into(), t.green, "Forecast");
        }
        section(IconName::BatteryCharging, "Charge over time", cx)
            .child(chart)
            .child(legend(w, cx))
            .child(footnote(
                "The battery level over the selected range, 0–100%. The slope shows how fast it charged or drained; the dashed line carries the present rate forward and is a forecast, not a reading.",
                cx,
            ))
    }

    fn power_history(&self, snap: &Snapshot, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let (_, _, w) = self.series("total");
        let mut chart = self.chart(&self.charts.main, "total").unit(Unit::Watts).y(YScale::Zero);
        let mut keys = Vec::new();
        if snap.cpu_w.is_some() {
            chart = chart.line(self.series("cpu").0, t.green, "CPU");
            keys.push((t.green, "CPU".to_owned()));
        }
        for (i, g) in snap.gpus.iter().enumerate() {
            let colour = if i == 0 { t.orange } else { t.purple };
            chart = chart.line(self.series(&format!("gpu:{}", g.pci)).0, colour, short_gpu(&g.name));
            keys.push((colour, g.name.clone()));
        }
        let key_row = div()
            .flex()
            .flex_row()
            .gap(px(16.))
            .text_size(t.small_caption)
            .text_color(t.text_secondary)
            .children(keys.into_iter().map(|(colour, name)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(div().w(px(10.)).h(px(3.)).rounded(px(1.)).bg(colour))
                    .child(name)
            }));
        let total = self
            .chart(&self.charts.second, "total")
            .line(self.series("total").0, t.blue, "Total measured")
            .unit(Unit::Watts)
            .y(YScale::Zero)
            .fill();
        div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(
                section(IconName::Zap, "Power by component", cx)
                    .child(chart)
                    .child(div().flex().flex_row().justify_between().child(legend(w, cx)).child(key_row)),
            )
            .child(
                section(IconName::ChartLine, "Total measured power", cx)
                    .child(total)
                    .child(legend(w, cx))
                    .child(footnote(
                        "The sum of the components that report power. The wall draw is higher: drives, fans, memory and the power supply's losses are not metered.",
                        cx,
                    )),
            )
    }

    fn accessories_panel(&self, snap: &Snapshot, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let list = accessories(&snap.power);
        let mut p = section(IconName::BatteryFull, "Accessories", cx);
        if list.is_empty() {
            p = p.child(caption("No accessory batteries reported.", cx));
        }
        let count = list.len();
        for (i, a) in list.into_iter().enumerate() {
            let colour = a.level.map_or(t.text_secondary, |l| l.colour(t));
            let value = a
                .percent
                .map(|p| format!("{p}%"))
                .or(a.level_text.clone())
                .or(a.offline.then(|| "Offline".to_owned()))
                .unwrap_or_else(|| "Not reported".into());
            p = p.child(
                div()
                    .id(SharedString::from(format!("accessory-{}", a.key)))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(Icon::new(a.glyph.icon()).small().text_color(t.text_secondary))
                            .child(
                                div()
                                    .text_size(t.caption)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(a.name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .pl(px(26.))
                            .text_size(t.small_caption)
                            .child(div().text_color(t.text_secondary).child("Charge"))
                            .child(div().flex_1())
                            .when(a.charging, |d| {
                                d.child(Icon::new(IconName::Zap).xsmall().text_color(t.green))
                            })
                            .child(
                                Icon::new(if a.charging {
                                    IconName::BatteryCharging
                                } else {
                                    a.level.map_or(IconName::Battery, Level::icon)
                                })
                                .small()
                                .text_color(colour),
                            )
                            .child(
                                div()
                                    .text_size(if a.percent.is_some() { px(15.) } else { t.caption })
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .font_features(t.numbers.clone())
                                    .child(value),
                            ),
                    ),
            );
            if i + 1 < count {
                p = p.child(divider(cx));
            }
        }
        p.when_some(snap.checked, |p, at| {
            p.child(caption(format!("Checked {}", clock(at, "%H:%M")), cx).font_weight(FontWeight::SEMIBOLD))
        })
    }

    fn health_panel(&self, s: &Supply, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let health = health_pct(s);
        let colour = match health {
            Some(h) if h < 80. => t.red,
            Some(h) if h < 90. => t.orange,
            Some(_) => t.green,
            None => t.text_secondary,
        };
        let (condition, condition_colour) = match health {
            Some(h) if h < 80. => ("Service recommended", t.orange),
            Some(_) => ("Normal", t.green),
            None => ("Not reported", t.text_secondary),
        };
        let (full, design) = match (s.energy_full_wh, s.energy_full_design_wh) {
            (Some(f), d) => (Some(format!("{f:.1} Wh")), d.map(|d| format!("{d:.1} Wh"))),
            _ => (
                s.charge_full_ah.map(|f| format!("{:.0} mAh", f * 1000.)),
                s.charge_full_design_ah.map(|d| format!("{:.0} mAh", d * 1000.)),
            ),
        };
        section(IconName::HeartPulse, "Battery health", cx)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_features(t.numbers.clone())
                            .text_color(colour)
                            .child(health.map_or("–".into(), |h| format!("{h:.0}%"))),
                    )
                    .child(caption("max capacity", cx)),
            )
            .child(divider(cx))
            .child(facts(
                vec![
                    ("Cycle count", s.cycle_count.map(|c| c.to_string()), t.text),
                    ("Condition", Some(condition.into()), condition_colour),
                    ("Manufacturer", s.manufacturer.clone(), t.text),
                    ("Model", s.model_name.clone(), t.text),
                    ("Full charge", full, t.text),
                    ("Design", design, t.text),
                ],
                cx,
            ))
            .child(footnote(
                "Maximum capacity is today's full-charge capacity as a share of the original design capacity, the standard measure of wear. Below about 80% the battery is worth replacing.",
                cx,
            ))
    }

    fn electrical_panel(&self, s: &Supply, snap: &Snapshot, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let current = match (s.power_w, s.voltage_v) {
            (Some(p), Some(v)) if v > 0. => Some(format!("{:.2} A", p / v)),
            _ => None,
        };
        let energy = s.energy_wh.map(|e| format!("{e:.1} Wh"));
        let adapter = on_mains(&snap.power).map(|on| if on { "Connected" } else { "Not connected" }.to_owned());
        let rows = [
            (
                "Power",
                s.power_w.map(|w| format!("{w:.2} W")),
                t.orange,
                self.row_spark("draw", t.orange),
            ),
            (
                "Voltage",
                s.voltage_v.map(|v| format!("{v:.2} V")),
                t.text,
                self.row_spark("voltage", t.blue),
            ),
            ("Current", current, t.text, None),
            (
                "Temperature",
                snap.temp.as_ref().map(|(_, c)| format!("{c:.1}°C")),
                t.teal,
                self.row_spark("temp", t.teal),
            ),
            ("Pack energy", energy, t.text, self.row_spark("energy", t.green)),
            ("Status", s.status.clone(), t.text, None),
            ("Adapter", adapter, t.text, None),
        ];
        section(IconName::Activity, "Electrical", cx).child(
            div().flex().flex_col().gap(px(5.)).children(
                rows.into_iter()
                    .filter_map(|(label, value, colour, spark)| Some(reading_row(label, value?, colour, spark, cx))),
            ),
        )
    }

    /// What a machine without a battery exposes: the supply, each GPU's power against its limit and its own rails,
    /// CPU package power, the board controller's pins and the fans.
    fn desktop_electrical(&self, snap: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let e = &snap.electrical;
        let (text, orange, purple, green, blue) = (t.text, t.orange, t.purple, t.green, t.blue);
        let watts = |w: Option<f64>| w.map_or("–".into(), |w| format!("{w:.1} W"));
        let group = |label: String, cx: &App| {
            div()
                .text_size(theme(cx).caption)
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(label)
        };
        let rows = |gap: f32| div().flex().flex_col().gap(px(gap));
        let (ac, ac_colour) = match on_mains(&snap.power) {
            Some(false) => ("Offline", orange),
            _ => ("Online", green),
        };
        let mut p =
            section(IconName::Activity, "Electrical", cx).child(reading_row("AC power", ac, ac_colour, None, cx));
        for (i, g) in snap.gpus.iter().enumerate() {
            let colour = if i == 0 { orange } else { purple };
            let rails = e.gpu.get(&g.pci).map_or(&[][..], Vec::as_slice);
            let twin = power_twin(rails, g.watts);
            let power_label = twin.map_or("Board power".to_owned(), |r| format!("Package power ({})", r.label));
            let mut block = rows(5.).child(group(g.name.clone(), cx)).child(reading_row(
                power_label,
                watts(g.watts),
                text,
                self.row_spark(&format!("gpu:{}", g.pci), colour),
                cx,
            ));
            if let (Some(w), Some(limit)) = (g.watts, g.limit.filter(|l| *l > 0.)) {
                block = block
                    .child(reading_row("Power limit", format!("{limit:.0} W"), text, None, cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex_1().child(proportion(w / limit, colour, cx)))
                            .child(
                                caption(format!("{:.0}% of limit", w / limit * 100.), cx)
                                    .font_features(t.numbers.clone()),
                            ),
                    );
            }
            for r in rails.iter().filter(|r| twin.is_none_or(|t| t.path != r.path)) {
                block = block.child(reading_row(
                    r.label.clone(),
                    reading_value(r),
                    text,
                    self.row_spark(&r.path, colour),
                    cx,
                ));
            }
            p = p.child(divider(cx)).child(block);
        }
        let cpu = rows(5.).child(group("CPU".into(), cx));
        let cpu = match snap.cpu_power() {
            CpuPower::Measured => cpu.child(reading_row(
                "Package power",
                watts(snap.cpu_w),
                text,
                self.row_spark("cpu", green),
                cx,
            )),
            CpuPower::NeedsHelper => cpu.child(helper_hint(
                "energy-electrical-enable",
                "Package power needs the privileged helper",
                cx,
            )),
            CpuPower::Unsupported => cpu.child(caption("This CPU has no readable power counter.", cx)),
        };
        p = p.child(divider(cx)).child(cpu);
        if let Some(first) = e.board.first() {
            let chip = first.chip.split('/').next().unwrap_or(&first.chip).to_owned();
            let open = self.board_open;
            let mut block = rows(5.).child(
                div()
                    .id("energy-board-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.board_open = !this.board_open;
                        cx.notify();
                    }))
                    .child(group("Board voltage pins".into(), cx))
                    .child(div().flex_1())
                    .child(caption(format!("{} · {}", chip, e.board.len()), cx))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(t.text_secondary),
                    ),
            );
            if open {
                block = block
                    .child(caption(
                        "Raw pin readings, unscaled: the board's voltage dividers are not applied, so these are not the rails' real voltages.",
                        cx,
                    ))
                    .children(e.board.iter().map(|r| {
                        reading_row(r.label.clone(), reading_value(r), text, self.row_spark(&r.path, blue), cx)
                    }));
            }
            p = p.child(divider(cx)).child(block);
        }
        if !e.fans.is_empty() || e.idle_fans > 0 {
            let block = rows(5.)
                .child(group("Fans".into(), cx))
                .children(e.fans.iter().map(|r| {
                    reading_row(
                        r.label.clone(),
                        reading_value(r),
                        text,
                        self.row_spark(&r.path, t.teal),
                        cx,
                    )
                }))
                .when(e.idle_fans > 0, |d| {
                    let n = e.idle_fans;
                    d.child(caption(
                        format!(
                            "{n} more {} 0 rpm: stopped or not connected.",
                            if n == 1 { "header reads" } else { "headers read" }
                        ),
                        cx,
                    ))
                });
            p = p.child(divider(cx)).child(block);
        }
        p
    }

    fn sheet(&self, kind: CardKind, snap: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (title, key, unit, colour, range): (String, String, Unit, Hsla, String) = match kind {
            CardKind::Charge => (
                "Charge".into(),
                "charge".into(),
                Unit::Percent,
                t.green,
                self.range.label().into(),
            ),
            CardKind::Runtime => (
                "Estimated runtime, minutes".into(),
                "runtime".into(),
                Unit::Count,
                t.blue,
                self.range.label().into(),
            ),
            CardKind::Draw => (
                "Power draw".into(),
                "draw".into(),
                Unit::Watts,
                t.orange,
                self.range.label().into(),
            ),
            CardKind::Temp => (
                "Battery temperature".into(),
                "temp".into(),
                Unit::Celsius,
                t.teal,
                self.range.label().into(),
            ),
            CardKind::Health => (
                "Health".into(),
                "health".into(),
                Unit::Percent,
                t.green,
                "90 days".into(),
            ),
            CardKind::Cycles => ("Cycles".into(), "cycles".into(), Unit::Count, t.indigo, "1 year".into()),
            CardKind::Cpu => (
                "CPU power".into(),
                "cpu".into(),
                Unit::Watts,
                t.green,
                self.range.label().into(),
            ),
            CardKind::Gpu(i) => (
                snap.gpus
                    .get(i)
                    .map_or("GPU power".into(), |g| format!("GPU power · {}", g.name)),
                snap.gpus.get(i).map_or(String::new(), |g| format!("gpu:{}", g.pci)),
                Unit::Watts,
                if i == 0 { t.orange } else { t.purple },
                self.range.label().into(),
            ),
            CardKind::Total => (
                "Total measured".into(),
                "total".into(),
                Unit::Watts,
                t.blue,
                self.range.label().into(),
            ),
        };
        let (meaning, method) = kind.explanation();
        let (data, _, w) = self.series(&key);
        let empty = data.is_empty();
        let latest = data.last().map_or("–".into(), |b| match kind {
            CardKind::Runtime => hours_minutes(b.avg),
            _ => unit.format(b.avg),
        });
        let y = if unit == Unit::Percent && kind == CardKind::Charge {
            YScale::Fixed(0., 100.)
        } else if matches!(kind, CardKind::Health | CardKind::Cycles | CardKind::Temp) {
            YScale::Fit(5.)
        } else {
            YScale::Zero
        };
        let chart = self
            .chart(&self.charts.sheet, &key)
            .height(px(200.))
            .line(data, colour, title.clone())
            .unit(unit)
            .y(y)
            .fill();
        let section = |heading: &'static str, body: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(caption_label(heading, cx))
                .child(footnote(body, cx))
        };
        div()
            .id("sheet-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                }),
            )
            .child(
                hairline(
                    div()
                        .id("sheet")
                        .w(px(640.))
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .p(px(20.))
                        .rounded(t.panel_radius)
                        .bg(t.tooltip)
                        .border_color(t.hairline)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                    t.hairline_width,
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(div().size(px(8.)).rounded_full().bg(colour))
                        .child(
                            div()
                                .text_size(t.panel_title)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(div().flex_1())
                        .child(caption(range, cx)),
                )
                .child(big_value(latest, cx))
                .child(if empty {
                    div()
                        .h(px(200.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(caption("No recorded history for this range yet.", cx))
                        .into_any_element()
                } else {
                    chart.into_any_element()
                })
                .child(legend(w, cx))
                .child(section("What it means", meaning))
                .child(section("How it is calculated", method))
                .child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("sheet-done")
                            .small()
                            .primary()
                            .label("Done")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sheet = None;
                                cx.notify();
                            })),
                    ),
                ),
            )
    }
}

fn measured_parts(snap: &Snapshot) -> String {
    let gpus = snap.gpus.iter().filter(|g| g.watts.is_some()).count();
    let gpu_text = match gpus {
        0 => None,
        1 => Some("1 GPU".to_owned()),
        n => Some(format!("{n} GPUs")),
    };
    match (snap.cpu_w.is_some(), gpu_text) {
        (true, Some(g)) => format!("CPU + {g}"),
        (true, None) => "CPU only".into(),
        (false, Some(g)) => format!("{g}; CPU not measured"),
        (false, None) => "nothing measured".into(),
    }
}

/// Drops the vendor prefix so a name fits a diagram node.
fn short_gpu(name: &str) -> String {
    let trimmed = [
        "NVIDIA GeForce ",
        "NVIDIA ",
        "AMD Radeon ",
        "AMD ",
        "Intel(R) ",
        "Intel ",
    ]
    .iter()
    .find_map(|p| name.strip_prefix(p))
    .unwrap_or(name);
    trimmed.chars().take(18).collect()
}

impl Render for Energy {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = if self.dev_rail { &self.rail_scroll } else { &self.scroll };
        if let Some(y) = self.dev_scroll
            && handle.max_offset().y > px(0.)
        {
            handle.set_offset(gpui::point(px(0.), -px(y).min(handle.max_offset().y)));
            self.dev_scroll = None;
        }
        let snap = self.snapshot(cx);
        let layout = layout(&snap.power);
        let battery = match &layout {
            Layout::Battery(name) => snap.power.get(name).cloned(),
            Layout::Desktop => None,
        };
        let subtitle = match &battery {
            Some(s) => {
                let mut parts = Vec::new();
                if let Some(c) = s.cycle_count {
                    parts.push(format!("{c} cycles"));
                }
                if let Some(h) = health_pct(s) {
                    parts.push(format!("{h:.0}% health"));
                }
                parts.push(
                    match (s.status.as_deref(), on_mains(&snap.power)) {
                        (Some("Discharging"), _) => "on battery",
                        (_, Some(true)) => "on power adapter",
                        _ => "on battery",
                    }
                    .into(),
                );
                parts.join(" · ")
            }
            None => "No system battery · on AC power · power measured at the CPU and GPUs".into(),
        };
        let header = self.header(subtitle, cx).into_any_element();
        let cards = match &battery {
            Some(s) => self.battery_cards(s, &snap, cx).into_any_element(),
            None => self.desktop_cards(&snap, cx).into_any_element(),
        };
        let sheet = self.sheet.map(|k| self.sheet(k, &snap, cx).into_any_element());
        let desktop_electrical = (battery.is_none() && self.live.read(cx).latest.is_some())
            .then(|| self.desktop_electrical(&snap, cx).into_any_element());
        let cx: &App = cx;
        let t = theme(cx);
        let waiting = self.live.read(cx).latest.is_none();
        let main = div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .pl(t.page_padding)
            .py(t.page_padding)
            .child(header);
        let main = if waiting {
            main.child(section(IconName::Zap, "Energy", cx).child(caption("Waiting for the first sample.", cx)))
        } else {
            match &battery {
                Some(s) => main
                    .child(cards)
                    .child(self.battery_flow(s, &snap, cx))
                    .child(self.charge_panel(s, cx)),
                None => main
                    .child(cards)
                    .child(self.desktop_flow(&snap, cx))
                    .child(self.power_history(&snap, cx)),
            }
        };
        let rail = div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(self.accessories_panel(&snap, cx))
            .when_some(battery.as_ref(), |r, s| {
                r.child(self.health_panel(s, cx))
                    .child(self.electrical_panel(s, &snap, cx))
            })
            .children(desktop_electrical);
        div()
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("energy-main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(main),
            )
            .child(
                div()
                    .id("energy-rail")
                    .flex_shrink_0()
                    .w(t.rail_width + t.gap + t.page_padding)
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.rail_scroll)
                    .child(div().pl(t.gap).pr(t.page_padding).py(t.page_padding).child(rail)),
            )
            .children(sheet)
    }
}

/// A laptop's supplies and history, for tests and for rendering the battery page on a desktop.
mod fixture {
    use std::collections::HashMap;

    use schema::power::{Power, Supply};

    use super::Query;
    use crate::data::history::{Bucket, bucket_width};

    pub const TEMP_C: f64 = 31.4;

    fn supply(f: impl FnOnce(&mut Supply)) -> Supply {
        let mut s = Supply::default();
        f(&mut s);
        s
    }

    /// A two-year-old 57 Wh ultrabook pack on battery, with a mouse, a keyboard and a charging headset.
    pub fn laptop() -> Power {
        let mut p = Power::new();
        p.insert(
            "AC".into(),
            supply(|s| {
                s.kind = Some("Mains".into());
                s.online = Some(0);
            }),
        );
        p.insert(
            "BAT0".into(),
            supply(|s| {
                s.kind = Some("Battery".into());
                s.status = Some("Discharging".into());
                s.model_name = Some("DELL 8N0YK".into());
                s.manufacturer = Some("SMP".into());
                s.capacity_level = Some("Normal".into());
                s.capacity = Some(62);
                s.cycle_count = Some(213);
                s.energy_wh = Some(32.24);
                s.energy_full_wh = Some(52.0);
                s.energy_full_design_wh = Some(57.0);
                s.power_w = Some(9.62);
                s.voltage_v = Some(11.87);
            }),
        );
        p.insert(
            "hidpp_battery_0".into(),
            supply(|s| {
                s.kind = Some("Battery".into());
                s.scope = Some("Device".into());
                s.status = Some("Discharging".into());
                s.model_name = Some("Wireless Mouse MX Master 3".into());
                s.manufacturer = Some("Logitech".into());
                s.capacity = Some(50);
                s.online = Some(1);
            }),
        );
        p.insert(
            "hidpp_battery_1".into(),
            supply(|s| {
                s.kind = Some("Battery".into());
                s.scope = Some("Device".into());
                s.status = Some("Discharging".into());
                s.model_name = Some("MX Keys Keyboard".into());
                s.capacity = Some(15);
            }),
        );
        p.insert(
            "ps-controller-battery-a0:5a:5d:12:34:56".into(),
            supply(|s| {
                s.kind = Some("Battery".into());
                s.scope = Some("Device".into());
                s.status = Some("Charging".into());
                s.model_name = Some("Wireless Controller".into());
                s.capacity = Some(35);
            }),
        );
        p
    }

    /// Plausible recorded history: an unplugged afternoon's drain, a wobbling draw, slow wear and a steady cycle count.
    pub fn history(queries: &[Query], now: f64) -> HashMap<String, Vec<Bucket>> {
        let to = now as i64;
        queries
            .iter()
            .map(|q| {
                let from = to - q.span_s;
                let w = bucket_width(from, to, 120);
                let mut out = Vec::new();
                let mut t = from - from.rem_euclid(w);
                while t < to {
                    let age = (to - t) as f64 / q.span_s as f64;
                    let wave = ((t / w) as f64 * 0.7).sin();
                    // Unplugged at full, then draining at the 9.6 W the pack reports now.
                    let charge = (62. + 18.5 * (to - t) as f64 / 3600.).min(100.);
                    let v = match q.key.as_str() {
                        "charge" => Some(charge),
                        "draw" => Some(9.6 + 2.2 * wave + if (t / w) % 17 == 0 { 6. } else { 0. }),
                        "voltage" => Some(11.87 + 0.9 * (charge - 62.) / 38. + 0.02 * wave),
                        "energy" => Some(charge / 100. * 52.),
                        "temp" => Some(TEMP_C - 1.2 * age + 0.3 * wave),
                        "full" => Some(52.0 + 0.8 * age),
                        "design" => Some(57.0),
                        "cycles" => Some((213. - 61. * age).floor()),
                        _ => None,
                    };
                    if let Some(v) = v {
                        out.push(Bucket {
                            t,
                            min: v - 0.15,
                            avg: v,
                            max: v + 0.15,
                        });
                    }
                    t += w;
                }
                (q.key.clone(), out)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(t: i64, v: f64) -> Bucket {
        Bucket {
            t,
            min: v,
            avg: v,
            max: v,
        }
    }

    #[test]
    fn a_laptop_gets_the_battery_page_and_a_mouse_alone_does_not() {
        assert_eq!(layout(&fixture::laptop()), Layout::Battery("BAT0".into()));
        // this desktop: only a Logitech mouse, which must not be mistaken for a system battery
        let mut desktop = fixture::laptop();
        desktop.retain(|k, _| k.starts_with("hidpp"));
        assert_eq!(layout(&desktop), Layout::Desktop);
        assert_eq!(layout(&Power::new()), Layout::Desktop);
    }

    #[test]
    fn health_is_full_over_design_from_either_pair() {
        let laptop = fixture::laptop();
        let h = health_pct(&laptop["BAT0"]).unwrap();
        assert!((h - 91.2).abs() < 0.1);
        let charge_only = Supply {
            charge_full_ah: Some(4.8),
            charge_full_design_ah: Some(6.0),
            ..Supply::default()
        };
        assert_eq!(health_pct(&charge_only), Some(80.));
        let no_design = Supply {
            energy_full_wh: Some(50.),
            ..Supply::default()
        };
        assert_eq!(
            health_pct(&no_design),
            None,
            "without a design capacity health is unknown, not 100%"
        );
    }

    #[test]
    fn runtime_is_energy_over_draw_and_time_to_full_while_charging() {
        let mut s = fixture::laptop()["BAT0"].clone();
        let Some(Runtime::ToEmpty(m)) = runtime(&s) else {
            panic!("discharging gives a runtime")
        };
        assert!((m - 32.24 / 9.62 * 60.).abs() < 0.01);
        assert_eq!(hours_minutes(m), "3:21");
        s.status = Some("Charging".into());
        let Some(Runtime::ToFull(m)) = runtime(&s) else {
            panic!("charging gives time to full")
        };
        assert!((m - (52. - 32.24) / 9.62 * 60.).abs() < 0.01);
        s.status = Some("Not charging".into());
        assert_eq!(runtime(&s), None, "a battery holding on the adapter has no runtime");
        s.status = Some("Discharging".into());
        s.power_w = Some(0.05);
        assert_eq!(runtime(&s), None, "a near-zero draw would give a meaningless runtime");
    }

    #[test]
    fn the_forecast_follows_the_present_rate_and_stops_at_empty() {
        let s = &fixture::laptop()["BAT0"];
        let rate = charge_rate(s).unwrap();
        assert!(rate < 0., "discharging drains");
        let f = forecast(1000., 62., rate, 10. * 3600., 600.);
        assert_eq!(f[0].avg, 62.);
        assert!(f.windows(2).all(|w| w[1].avg < w[0].avg));
        assert_eq!(f.last().unwrap().avg, 0., "the line ends at empty, not below");
        let flat = forecast(0., 80., 0., 3600., 600.);
        assert!(flat.iter().all(|b| b.avg == 80.));
    }

    #[test]
    fn runtime_history_skips_buckets_where_the_battery_was_charging() {
        let energy = [bucket(0, 40.), bucket(60, 41.), bucket(120, 40.)];
        let draw = [bucket(0, 10.), bucket(60, 10.), bucket(120, 0.)];
        let charge = [bucket(0, 70.), bucket(60, 72.), bucket(120, 70.)];
        let r = runtime_history(&energy, &draw, &charge);
        assert_eq!(r.len(), 1, "60 s was charging and 120 s had no draw");
        assert_eq!(r[0].t, 0);
        assert_eq!(r[0].avg, 240.);
    }

    #[test]
    fn health_history_needs_both_readings_in_a_bucket() {
        let h = ratio_history(&[bucket(0, 50.), bucket(10, 49.)], &[bucket(0, 100.)]);
        assert_eq!(h, [bucket(0, 50.)]);
    }

    #[test]
    fn accessory_levels_use_the_red_orange_green_thresholds() {
        assert_eq!(Level::of_percent(19.), Level::Critical);
        assert_eq!(Level::of_percent(20.), Level::Low);
        assert_eq!(Level::of_percent(39.), Level::Low);
        assert_eq!(Level::of_percent(40.), Level::Good);
        let list = accessories(&fixture::laptop());
        let names: Vec<_> = list.iter().map(|a| (a.glyph, a.level, a.charging)).collect();
        assert_eq!(
            names,
            [
                (Glyph::Mouse, Some(Level::Good), false),
                (Glyph::Keyboard, Some(Level::Critical), false),
                (Glyph::Gamepad, Some(Level::Low), true),
            ]
        );
    }

    #[test]
    fn a_mouse_reporting_only_a_coarse_level_shows_that_word() {
        // the MX Master 3 on this desktop reports capacity_level=Full and no percentage
        let mut power = Power::new();
        power.insert(
            "hidpp_battery_0".into(),
            Supply {
                kind: Some("Battery".into()),
                scope: Some("Device".into()),
                capacity_level: Some("Full".into()),
                model_name: Some("Wireless Mouse MX Master 3".into()),
                ..Supply::default()
            },
        );
        let a = &accessories(&power)[0];
        assert_eq!(
            (a.percent, a.level, a.level_text.as_deref()),
            (None, Some(Level::Good), Some("Full"))
        );
    }

    #[test]
    fn consumer_shares_are_of_all_cpu_time_not_of_the_shown_few() {
        let groups = vec![("magick".to_owned(), 90.), ("claude".into(), 8.), ("idle".into(), 0.)];
        let c = consumers(&groups, 120., 5);
        assert_eq!(c.len(), 2, "a process using no CPU is not a consumer");
        assert!((c[0].1 - 0.75).abs() < 1e-9);
        assert!(consumers(&[], 0., 5).is_empty());
    }

    #[test]
    fn total_measured_sums_only_known_parts() {
        assert_eq!(total_measured(&[None, Some(19.2), Some(38.8)]), Some((58., 2)));
        assert_eq!(
            total_measured(&[None, None]),
            None,
            "nothing measured is unknown, not 0 W"
        );
    }

    #[test]
    fn adapters_decide_on_mains_and_a_desktop_without_one_is_unknown() {
        let mut p = fixture::laptop();
        assert_eq!(on_mains(&p), Some(false));
        p.get_mut("AC").unwrap().online = Some(1);
        assert_eq!(on_mains(&p), Some(true));
        p.remove("AC");
        assert_eq!(on_mains(&p), None);
    }

    #[test]
    fn battery_queries_cover_lifetime_spans_and_desktop_sums_every_gpu() {
        let snap = Snapshot {
            gpus: vec![
                GpuPower {
                    pci: "0000:01:00.0".into(),
                    name: "NVIDIA GeForce RTX 4090".into(),
                    watts: Some(19.),
                    limit: None,
                },
                GpuPower {
                    pci: "0000:7a:00.0".into(),
                    name: "AMD Radeon Graphics".into(),
                    watts: Some(30.),
                    limit: None,
                },
            ],
            ..Snapshot::default()
        };
        let qs = queries(&Layout::Battery("BAT0".into()), &snap, Range::M30);
        let cycles = qs.iter().find(|q| q.key == "cycles").unwrap();
        assert_eq!(
            cycles.span_s, CYCLES_SPAN_S,
            "cycles read a year whatever the page range"
        );
        assert_eq!(cycles.source.paths, ["/power/BAT0/cycle_count"]);
        let qs = queries(&Layout::Desktop, &snap, Range::H1);
        let total = qs.iter().find(|q| q.key == "total").unwrap();
        assert_eq!(total.source.combine, Combine::Sum);
        assert_eq!(total.source.paths.len(), 3);
        assert_eq!(short_gpu("NVIDIA GeForce RTX 4090"), "RTX 4090");
    }

    fn reading(path: &str, value: f64) -> Reading {
        let (chip, kind, label) = hwmon_parts(path).unwrap();
        Reading {
            chip,
            kind,
            label,
            path: path.into(),
            value,
        }
    }

    #[test]
    fn hwmon_paths_unescape_slashes_in_chip_and_label() {
        assert_eq!(
            hwmon_parts("/hwmon/nct6799~1nct6775.656/temp/PECI~1TSI Agent 0 Calibration"),
            Some((
                "nct6799/nct6775.656".into(),
                "temp".into(),
                "PECI/TSI Agent 0 Calibration".into()
            ))
        );
        assert_eq!(hwmon_parts("/gpu/devices/x/power_w"), None);
        assert_eq!(
            hwmon_parts("/hwmon/a/b"),
            None,
            "a path without a label is not a sensor"
        );
    }

    #[test]
    fn readings_sort_into_gpu_rails_board_pins_and_fans() {
        // this desktop: an amdgpu iGPU, an nct6799 Super I/O and an EC fan header at 0 rpm
        let e = electrical(
            vec![
                reading("/hwmon/amdgpu~10000:7a:00.0/power/PPT", 37.9),
                reading("/hwmon/amdgpu~10000:7a:00.0/in/vddnb", 0.905),
                reading("/hwmon/amdgpu~10000:7a:00.0/in/vddgfx", 0.995),
                reading("/hwmon/amdgpu~10000:7a:00.0/temp/edge", 45.),
                reading("/hwmon/nct6799~1nct6775.656/in/in10", 1.056),
                reading("/hwmon/nct6799~1nct6775.656/in/in2", 3.392),
                reading("/hwmon/nct6799~1nct6775.656/in/in9", 0.),
                reading("/hwmon/nct6799~1nct6775.656/fan/fan7", 3161.),
                reading("/hwmon/nct6799~1nct6775.656/fan/fan1", 0.),
                reading("/hwmon/asusec~1asus-ec-sensors/fan/CPU_Opt", 0.),
                reading("/hwmon/nvme~1nvme0/temp/Composite", 40.),
            ],
            &["0000:7a:00.0", "0000:01:00.0"],
        );
        let labels = |v: &[Reading]| v.iter().map(|r| r.label.clone()).collect::<Vec<_>>();
        assert_eq!(
            labels(&e.gpu["0000:7a:00.0"]),
            ["vddgfx", "vddnb", "PPT"],
            "the GPU's own rails, voltages before power, temperatures elsewhere"
        );
        assert_eq!(
            labels(&e.board),
            ["in2", "in10"],
            "a pin reading 0 V is unconnected, and in2 sorts before in10"
        );
        assert_eq!(labels(&e.fans), ["fan7"]);
        assert_eq!(e.idle_fans, 2, "headers at 0 rpm are counted, not listed");
    }

    #[test]
    fn the_igpu_power_sensor_is_shown_once_as_the_package_power() {
        let rails = [
            reading("/hwmon/amdgpu~10000:7a:00.0/in/vddgfx", 0.995),
            reading("/hwmon/amdgpu~10000:7a:00.0/power/PPT", 37.9),
        ];
        assert_eq!(power_twin(&rails, Some(38.4)).map(|r| r.label.as_str()), Some("PPT"));
        assert_eq!(
            power_twin(&rails, None),
            None,
            "without the driver's reading the sensor has its own row"
        );
        assert_eq!(reading_value(&rails[0]), "0.995 V");
        assert_eq!(reading_value(&rails[1]), "37.9 W");
    }

    #[test]
    fn desktop_queries_cover_each_sensor_row_for_its_sparkline() {
        let snap = Snapshot {
            electrical: electrical(vec![reading("/hwmon/nct6799~1nct6775.656/in/in2", 3.392)], &[]),
            ..Snapshot::default()
        };
        let qs = queries(&Layout::Desktop, &snap, Range::M30);
        let pin = qs
            .iter()
            .find(|q| q.key == "/hwmon/nct6799~1nct6775.656/in/in2")
            .unwrap();
        assert_eq!(pin.source.paths, ["/hwmon/nct6799~1nct6775.656/in/in2"]);
    }

    #[test]
    fn glyphs_follow_the_device_name() {
        assert_eq!(Glyph::of("Wireless Mouse MX Master 3"), Glyph::Mouse);
        assert_eq!(Glyph::of("WH-1000XM4"), Glyph::Headphones);
        assert_eq!(Glyph::of("Something"), Glyph::Other);
    }
}
