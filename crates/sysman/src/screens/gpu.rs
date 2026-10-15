use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Div, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px, relative,
};
use gpui_component::tooltip::Tooltip;
use gpui_kit_assets::IconName;
use schema::gpu::{Device, DeviceInfo, Meta as GpuMeta, Process};
use schema::series::escape;

use crate::charts::format::{self, Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::source::{Combine, Source};
use crate::screens::dashboard::cpu_colour;
use crate::ui::shell::Tab;
use crate::ui::theme::{Theme, rem_px, theme};
use crate::ui::widgets::card::card;
use crate::ui::widgets::detail_sheet::{Detail, Series, opener};
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::{caption_label, panel, section};
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};
use crate::ui::widgets::segmented::segmented;
use crate::ui::widgets::text::{caption, facts, proportion};

const BUCKETS: u32 = 120;
/// Stored history is re-read this often; the live rings cover the newest 30 minutes in between.
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 150.;
const PROCESS_ROWS: usize = 12;
/// Integrated GPUs carve out less by default, and current discrete cards have more.
const DISCRETE_VRAM: u64 = 2 << 30;
/// Headroom above a limit rule, so the rule and its label sit inside the plot.
const RULE_HEADROOM: f64 = 1.08;
const DISPLAY: [&str; 4] = ["Xwayland", "Hyprland", "quickshell", "qs"];
const MEDIA: [&str; 3] = ["ffmpeg", "mpv", "obs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Util,
    Vram,
    Power,
    Temp,
    Clock,
    Video,
}

impl Field {
    const ALL: [Field; 6] = [
        Self::Util,
        Self::Vram,
        Self::Power,
        Self::Temp,
        Self::Clock,
        Self::Video,
    ];

    /// Series under the device; the video line plots whichever engine is busier.
    fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Util => &["util_pct"],
            Self::Vram => &["vram_used_bytes"],
            Self::Power => &["power_w"],
            Self::Temp => &["temp_c"],
            Self::Clock => &["clock_mhz"],
            Self::Video => &["encoder_pct", "decoder_pct", "video_busy_pct"],
        }
    }

    /// Only series the device has reported, so an empty source means its backend lacks the field.
    fn source(self, pci: &str, known: &HashSet<String>) -> Source {
        let prefix = device_prefix(pci);
        let paths = self
            .suffixes()
            .iter()
            .map(|s| format!("{prefix}{s}"))
            .filter(|p| known.contains(p))
            .collect();
        Source::new(paths, Combine::Max)
    }
}

fn device_prefix(pci: &str) -> String {
    format!("/gpu/devices/{}/", escape(pci))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Ai,
    Display,
    Media,
    Other,
}

impl Category {
    const ALL: [Category; 4] = [Self::Ai, Self::Display, Self::Media, Self::Other];

    fn label(self) -> &'static str {
        match self {
            Self::Ai => "AI and ML",
            Self::Display => "Display and UI",
            Self::Media => "Media",
            Self::Other => "Other",
        }
    }

    fn colour(self, t: &Theme) -> Hsla {
        match self {
            Self::Ai => t.purple,
            Self::Display => t.blue,
            Self::Media => t.teal,
            Self::Other => t.text_tertiary,
        }
    }
}

/// By name only: a short fixed list, plus Python that names torch on its command line.
fn classify(comm: &str, cmdline: Option<&str>) -> Category {
    let name = comm.to_ascii_lowercase();
    let torch = cmdline.is_some_and(|c| c.contains("torch"));
    if name.starts_with("ollama") || name.starts_with("llama") || (name.starts_with("python") && torch) {
        Category::Ai
    } else if DISPLAY.iter().any(|d| d.eq_ignore_ascii_case(comm)) {
        Category::Display
    } else if MEDIA.iter().any(|m| m.eq_ignore_ascii_case(comm)) {
        Category::Media
    } else {
        Category::Other
    }
}

/// Each category's weight and process count, in `Category::ALL` order.
#[derive(Debug, PartialEq)]
struct Split {
    /// Weighted by GPU time; false when no process reports any, and the weights are VRAM bytes instead.
    by_util: bool,
    parts: [(f64, usize); 4],
}

impl Split {
    fn new(procs: &[(Category, Option<f64>, Option<u64>)]) -> Self {
        let by_util = procs.iter().any(|p| p.1.is_some_and(|u| u > 0.));
        let mut parts = [(0., 0); 4];
        for &(category, util, mem) in procs {
            let part = &mut parts[Category::ALL.iter().position(|c| *c == category).unwrap_or(3)];
            part.0 += if by_util {
                util.unwrap_or(0.)
            } else {
                mem.unwrap_or(0) as f64
            };
            part.1 += 1;
        }
        Self { by_util, parts }
    }

    fn total(&self) -> f64 {
        self.parts.iter().map(|p| p.0).sum()
    }
}

/// Busiest first; processes without a utilisation reading follow, by VRAM.
fn rank(procs: &mut [&Process]) {
    procs.sort_by(|a, b| {
        b.util_pct
            .unwrap_or(-1.)
            .total_cmp(&a.util_pct.unwrap_or(-1.))
            .then(b.mem_bytes.cmp(&a.mem_bytes))
    });
}

fn suspended(d: &Device) -> bool {
    d.power_state.as_deref().is_some_and(|s| s != "D0")
}

fn discrete(d: &Device) -> bool {
    d.vendor == "nvidia" || d.vram_total_bytes.is_some_and(|b| b >= DISCRETE_VRAM)
}

/// An awake discrete GPU before an integrated one, then the busiest, then the one with more VRAM.
fn default_device(devices: &BTreeMap<String, Device>) -> Option<String> {
    devices
        .iter()
        .max_by(|(_, a), (_, b)| {
            (!suspended(a))
                .cmp(&!suspended(b))
                .then(discrete(a).cmp(&discrete(b)))
                .then(a.util_pct.unwrap_or(0.).total_cmp(&b.util_pct.unwrap_or(0.)))
                .then(a.vram_total_bytes.cmp(&b.vram_total_bytes))
        })
        .map(|(pci, _)| pci.clone())
}

/// Throttle reasons worth showing: `gpu_idle` only says the clocks dropped because there is no work.
fn throttle_labels(reasons: Option<&[String]>) -> Vec<String> {
    reasons
        .unwrap_or_default()
        .iter()
        .filter(|r| r.as_str() != "gpu_idle")
        .map(|r| {
            let words: Vec<String> = r
                .split('_')
                .map(|w| match w {
                    "sw" | "hw" => w.to_uppercase(),
                    _ => w.to_owned(),
                })
                .collect();
            let text = words.join(" ");
            let mut chars = text.chars();
            chars
                .next()
                .map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
        })
        .collect()
}

fn pcie(generation: Option<u32>, width: Option<u32>) -> Option<String> {
    match (generation, width) {
        (Some(g), Some(w)) => Some(format!("Gen {g} x{w}")),
        (Some(g), None) => Some(format!("Gen {g}")),
        (None, Some(w)) => Some(format!("x{w}")),
        (None, None) => None,
    }
}

fn vendor_label(vendor: &str) -> &str {
    match vendor {
        "nvidia" => "NVIDIA",
        "amd" => "AMD",
        "intel" => "Intel",
        other => other,
    }
}

/// History read for one range and device; the newest end is replaced by live rings on every tick.
struct Stored {
    range: Range,
    pci: String,
    known: Vec<String>,
    buckets: HashMap<Field, Vec<Bucket>>,
}

fn load(range: Range, pci: String, to: i64) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let known = history.paths(&device_prefix(&pci))?;
    let set: HashSet<String> = known.iter().cloned().collect();
    let from = to - range.span_s();
    let mut buckets = HashMap::new();
    for field in Field::ALL {
        buckets.insert(field, field.source(&pci, &set).query(&history, from, to, BUCKETS)?);
    }
    Ok(Stored {
        range,
        pci,
        known,
        buckets,
    })
}

#[derive(Default)]
struct Charts {
    util: Rc<RefCell<ChartState>>,
    vram: Rc<RefCell<ChartState>>,
    power: Rc<RefCell<ChartState>>,
}

pub struct Gpu {
    me: WeakEntity<Self>,
    live: Entity<Live>,
    range: Range,
    pci: Option<String>,
    stored: Option<Stored>,
    loading: bool,
    /// Range, device and time of the last history read, so a failing read is not retried every tick.
    attempt: Option<(Range, String, f64)>,
    error: Option<SharedString>,
    known: HashSet<String>,
    merged: HashMap<Field, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: Charts,
    meta: Option<GpuMeta>,
    /// Command lines of Python processes, read once per pid.
    cmdlines: HashMap<u32, Option<String>>,
    residency: Residency,
}

/// Samples seen in each performance state of one device, since the screen opened or the device changed.
#[derive(Default)]
struct Residency {
    pci: String,
    last_t: f64,
    counts: BTreeMap<String, u32>,
}

impl Residency {
    fn record(&mut self, pci: &str, t: f64, state: Option<&str>) {
        if self.pci != pci {
            *self = Self {
                pci: pci.to_owned(),
                ..Self::default()
            };
        }
        if t <= self.last_t {
            return;
        }
        self.last_t = t;
        if let Some(state) = state {
            *self.counts.entry(state.to_owned()).or_default() += 1;
        }
    }

    /// Each state and its share of the samples, fastest state first.
    fn shares(&self) -> Vec<(&str, f64)> {
        let total: u32 = self.counts.values().sum();
        self.counts
            .iter()
            .map(|(state, n)| (state.as_str(), f64::from(*n) / f64::from(total.max(1))))
            .collect()
    }
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

impl Gpu {
    pub fn new(live: Entity<Live>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        let mut this = Self {
            me: cx.weak_entity(),
            live,
            range: remembered(Tab::Gpu, cx),
            pci: None,
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            known: HashSet::new(),
            merged: HashMap::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            charts: Charts::default(),
            meta: None,
            cmdlines: HashMap::new(),
            residency: Residency::default(),
        };
        this.refresh(cx);
        this
    }

    /// The first GPU sample is in and the history read for the current range and device has returned.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        self.live.read(cx).latest.as_ref().is_some_and(|s| s.gpu.is_some())
            && !self.loading
            && self
                .attempt
                .as_ref()
                .is_some_and(|(range, pci, _)| *range == self.range && Some(pci) == self.pci.as_ref())
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Gpu, range, cx);
            self.refresh(cx);
        }
    }

    fn select(&mut self, pci: String, cx: &mut Context<Self>) {
        if self.pci.as_ref() != Some(&pci) {
            self.pci = Some(pci);
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.pci.is_none() {
            self.pci = self
                .live
                .read(cx)
                .latest
                .as_ref()
                .and_then(|s| s.gpu.as_ref())
                .and_then(|g| default_device(&g.devices));
        }
        let Some(pci) = self.pci.clone() else {
            cx.notify();
            return;
        };
        let now = self.now(cx);
        let stale = self.attempt.as_ref().is_none_or(|(range, p, at)| {
            *range != self.range
                || *p != pci
                || self
                    .live
                    .read(cx)
                    .reload_due(*at, now, self.range.span_s() as f64, RELOAD_S)
        });
        if stale && !self.loading {
            self.load(now, pci.clone(), cx);
        }
        self.merge(now, &pci, cx);
        let (t, state) = self.live.read(cx).latest.as_ref().map_or((0., None), |s| {
            let state = s.gpu.as_ref().and_then(|g| g.devices.get(&pci)?.pstate.clone());
            (s.t, state)
        });
        self.residency.record(&pci, t, state.as_deref());
        cx.notify();
    }

    fn load(&mut self, now: f64, pci: String, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((self.range, pci.clone(), now));
        let range = self.range;
        let task = cx
            .background_executor()
            .spawn(async move { load(range, pci, now as i64) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(stored) => {
                        this.error = None;
                        this.stored = Some(stored);
                    }
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn merge(&mut self, now: f64, pci: &str, cx: &App) {
        let live = self.live.read(cx);
        if self.meta.is_none() {
            self.meta = live
                .meta
                .as_ref()
                .and_then(|m| m.modules.get("gpu")?.info.clone())
                .and_then(|info| serde_json::from_value(info).ok());
        }
        let to = now as i64;
        let from = to - self.range.span_s();
        self.width_s = bucket_width(from, to, BUCKETS);
        let prefix = device_prefix(pci);
        let stored = self.stored.as_ref().filter(|s| s.range == self.range && s.pci == pci);
        self.known = stored
            .map(|s| s.known.clone())
            .unwrap_or_default()
            .into_iter()
            .chain(live.paths().filter(|p| p.starts_with(&prefix)).map(str::to_owned))
            .collect();
        for field in Field::ALL {
            let source = field.source(pci, &self.known);
            let history = stored
                .and_then(|s| s.buckets.get(&field))
                .map_or(&[][..], Vec::as_slice);
            self.merged
                .insert(field, source.top_up(history, live, from, self.width_s).into());
        }
        self.span = (from as f64, now);
        self.version += 1;
    }

    fn data(&self, field: Field) -> Rc<[Bucket]> {
        self.merged.get(&field).cloned().unwrap_or_else(|| Rc::from(Vec::new()))
    }

    fn reported(&self, field: Field) -> bool {
        self.pci
            .as_deref()
            .is_some_and(|pci| !field.source(pci, &self.known).paths.is_empty())
    }

    /// Missing buckets wider than this are gaps: a bucket and a half, or a few collector intervals.
    fn gap_s(&self, cx: &App) -> f64 {
        let interval = self
            .live
            .read(cx)
            .meta
            .as_ref()
            .and_then(|m| m.modules.get("gpu"))
            .map_or(2, |m| m.interval_s);
        (self.width_s as f64 * 1.5).max(interval as f64 * 2.5)
    }

    fn chart(&self, state: &Rc<RefCell<ChartState>>, cx: &App) -> TrendChart {
        TrendChart::new(
            state.clone(),
            self.version,
            self.span,
            self.width_s as f64,
            self.gap_s(cx),
        )
        .height(px(CHART_HEIGHT))
    }

    fn spark(&self, field: Field, color: Hsla, cx: &App) -> Sparkline {
        Sparkline::new(
            ("gpu-spark", field as usize),
            self.data(field),
            color,
            self.span,
            self.width_s as f64,
            self.gap_s(cx),
        )
    }

    fn cmdline(&mut self, p: &Process) -> Option<String> {
        if !p.comm.to_ascii_lowercase().starts_with("python") {
            return None;
        }
        self.cmdlines
            .entry(p.pid)
            .or_insert_with(|| {
                std::fs::read(format!("/proc/{}/cmdline", p.pid))
                    .ok()
                    .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
            })
            .clone()
    }
}

fn value_row(value: String, detail: Option<String>, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(rem_px(6.))
        .child(
            div()
                .flex_shrink_0()
                .text_size(t.card_value)
                .font_weight(t.weight_semibold)
                .font_features(t.numbers.clone())
                .child(value),
        )
        .child(div().flex_1())
        .children(detail.map(|d| caption(d, cx).min_w_0().truncate().font_features(t.numbers.clone())))
}

fn readout(label: &str, value: String, color: Hsla, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(caption_label(label, cx))
        .child(
            div()
                .text_size(t.card_value)
                .font_weight(t.weight_semibold)
                .font_features(t.numbers.clone())
                .text_color(color)
                .child(value),
        )
}

impl Gpu {
    fn header(
        &self,
        devices: &BTreeMap<String, Device>,
        device: Option<&Device>,
        info: Option<&DeviceInfo>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let pick = this.clone();
        let t = theme(cx);
        let name = |pci: &str| {
            self.meta
                .as_ref()
                .and_then(|m| m.devices.get(pci)?.name.clone())
                .unwrap_or_else(|| pci.to_owned())
        };
        let mut facts = Vec::new();
        if let Some(d) = device {
            facts.push(vendor_label(&d.vendor).to_owned());
            facts.push(format!("{} driver", d.driver));
            facts.push(format!("{} backend", d.backend));
        }
        if let Some(source) = info.and_then(|i| i.util_source.as_deref()) {
            facts.push(format!("util source: {source}"));
        }
        let items: Vec<(String, SharedString)> = devices.keys().map(|pci| (pci.clone(), name(pci).into())).collect();
        let control = |label: &str, body: AnyElement| {
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap(rem_px(5.))
                .child(caption_label(label, cx))
                .child(body)
        };
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
                            .font_weight(t.weight_semibold)
                            .child(self.pci.as_deref().map_or("GPU".into(), name)),
                    )
                    .child(caption(facts.join(" · "), cx))
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_size(t.caption).text_color(t.orange).child(e))
                    }),
            )
            .when(items.len() > 1, |d| {
                d.child(control(
                    "Device",
                    segmented(
                        "gpu-device",
                        items,
                        &self.pci.clone().unwrap_or_default(),
                        move |pci, _, cx| {
                            pick.update(cx, |g, cx| g.select(pci, cx)).ok();
                        },
                        cx,
                    )
                    .into_any_element(),
                ))
            })
            .child(control(
                "History",
                range_picker(
                    self.range,
                    move |range, _, cx| {
                        this.update(cx, |g, cx| g.set_range(range, cx)).ok();
                    },
                    cx,
                )
                .into_any_element(),
            ))
    }

    #[allow(clippy::too_many_arguments)]
    fn stat_card(
        &self,
        field: Field,
        id: &'static str,
        label: &str,
        color: Hsla,
        value: String,
        detail: Option<String>,
        spark: Sparkline,
        cx: &App,
    ) -> AnyElement {
        card(id, label, color, cx)
            .child(value_row(value, detail, cx))
            .child(spark)
            .on_click(self.expand(field))
            .into_any_element()
    }

    fn expand(&self, field: Field) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        opener(self.me.clone(), field, |this, field, cx| this.detail(*field, cx))
    }

    fn detail(&self, field: Field, cx: &App) -> Option<Detail> {
        let t = theme(cx);
        let pci = self.pci.clone()?;
        let device = self
            .live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.gpu.as_ref()?.devices.get(&pci).cloned());
        let limit = |max: Option<f64>| match max {
            Some(max) => YScale::Fixed(0., max * RULE_HEADROOM),
            None => YScale::Zero,
        };
        let vram_total = device.as_ref().and_then(|d| d.vram_total_bytes).map(|b| b as f64);
        let power_limit = device.as_ref().and_then(|d| d.power_limit_w);
        let (title, icon, unit, y, color, rules, meaning, method) = match field {
            Field::Util => (
                "GPU utilization",
                IconName::Monitor,
                Unit::Percent,
                YScale::Fixed(0., 100.),
                t.green,
                vec![],
                "The share of time the GPU was running work. It is a busy-time measure, not a measure of how much \
                 of the chip was in use, so a light task that keeps the GPU awake can read high.",
                "As the device's backend reports it: NVML's utilization rate on NVIDIA, gpu_busy_percent in sysfs \
                 on AMD, and on Intel the engine busy time in DRM fdinfo, which covers only your own processes.",
            ),
            Field::Vram => (
                "VRAM used",
                IconName::MemoryStick,
                Unit::Bytes,
                limit(vram_total),
                t.teal,
                vram_total
                    .map(|v| (v, SharedString::from("Total"), t.teal))
                    .into_iter()
                    .collect(),
                "Video memory held by every process and the driver on this device. When it runs out, textures and \
                 buffers spill into system memory and rendering or compute slows sharply.",
                "Used bytes as the driver reports them: NVML memory info on NVIDIA and mem_info_vram_used in sysfs \
                 on AMD. Integrated GPUs share system RAM and report only their reserved carve-out.",
            ),
            Field::Power => (
                "Power",
                IconName::Zap,
                Unit::Watts,
                limit(power_limit),
                t.blue,
                power_limit
                    .map(|l| (l, SharedString::from("Limit"), t.orange))
                    .into_iter()
                    .collect(),
                "Electrical power the graphics card is drawing. Holding at the limit means the card is lowering its \
                 clocks to stay within its power budget.",
                "The board power reading from NVML on NVIDIA, or the hwmon power1_average or power1_input sensor \
                 on AMD, in watts. The limit is the current enforced power cap.",
            ),
            Field::Temp => (
                "Temperature",
                IconName::Thermometer,
                Unit::Celsius,
                YScale::Fit(10.),
                t.red,
                vec![],
                "How hot the GPU is. Sustained readings near the chip's limit make it throttle its clocks.",
                "NVML's core temperature on NVIDIA, or the driver's hwmon sensor (edge on AMD, package on Intel), \
                 in degrees Celsius.",
            ),
            Field::Clock => (
                "Core clock, MHz",
                IconName::Activity,
                Unit::Count,
                YScale::Zero,
                t.orange,
                vec![],
                "How fast the graphics core is running. It rises under load and drops when idle, hot or at the \
                 power limit.",
                "The graphics clock from NVML on NVIDIA, hwmon freq1_input on AMD, or the actual GT frequency on \
                 Intel, in MHz.",
            ),
            Field::Video => (
                "Video engines",
                IconName::Monitor,
                Unit::Percent,
                YScale::Fixed(0., 100.),
                t.purple,
                vec![],
                "How busy the fixed-function video encoder and decoder are. Video playback, screen recording and \
                 streaming use them instead of the main GPU cores.",
                "Encoder and decoder utilization from NVML on NVIDIA, or vcn_busy_percent in sysfs on AMD; the \
                 busier engine is plotted.",
            ),
        };
        Some(Detail {
            title: title.into(),
            icon,
            unit,
            y,
            fill: matches!(field, Field::Util | Field::Vram),
            lines: vec![Series {
                label: title.into(),
                color,
                source: field.source(&pci, &self.known),
            }],
            rules,
            ticks: None,
            meaning,
            method,
            range: self.range,
            live: self.live.clone(),
        })
    }

    fn cards(&self, d: &Device, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let pct = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.0}%"));
        let mut cards = Vec::new();
        if self.reported(Field::Util) {
            let colour = cpu_colour(d.util_pct.unwrap_or(0.), t);
            cards.push(self.stat_card(
                Field::Util,
                "card-gpu",
                "GPU",
                colour,
                pct(d.util_pct),
                d.mem_busy_pct.map(|m| format!("memory {m:.0}%")),
                self.spark(Field::Util, colour, cx).fixed(100., "100%"),
                cx,
            ));
        }
        if self.reported(Field::Power) {
            let spark = self.spark(Field::Power, t.blue, cx);
            cards.push(self.stat_card(
                Field::Power,
                "card-power",
                "Power",
                t.blue,
                d.power_w.map_or("–".into(), |w| Unit::Watts.format(w)),
                d.power_limit_w.map(|l| format!("limit {}", Unit::Watts.format(l))),
                match d.power_limit_w {
                    Some(limit) => spark.fixed(limit, Unit::Watts.format(limit)),
                    None => spark,
                },
                cx,
            ));
        }
        if self.reported(Field::Vram) {
            let spark = self.spark(Field::Vram, t.teal, cx);
            cards.push(self.stat_card(
                Field::Vram,
                "card-vram",
                "VRAM",
                t.teal,
                d.vram_used_bytes.map_or("–".into(), |b| bytes(b as f64)),
                d.vram_total_bytes.map(|b| format!("of {}", bytes(b as f64))),
                match d.vram_total_bytes {
                    Some(total) => spark.fixed(total as f64, bytes(total as f64)),
                    None => spark,
                },
                cx,
            ));
        }
        if self.reported(Field::Temp) {
            let hottest = d
                .temps_c
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(label, c)| format!("{label} {}", format::temperature(*c, 0)));
            cards.push(self.stat_card(
                Field::Temp,
                "card-temp",
                "Temperature",
                t.red,
                d.temp_c.map_or("–".into(), |c| format::temperature(c, 0)),
                hottest,
                self.spark(Field::Temp, t.red, cx),
                cx,
            ));
        }
        if self.reported(Field::Clock) {
            cards.push(self.stat_card(
                Field::Clock,
                "card-clock",
                "Clock",
                t.orange,
                d.clock_mhz.map_or("–".into(), |m| format!("{m} MHz")),
                d.mem_clock_mhz.map(|m| format!("mem {m} MHz")),
                self.spark(Field::Clock, t.orange, cx),
                cx,
            ));
        }
        if self.reported(Field::Video) {
            let busiest = [d.encoder_pct, d.decoder_pct, d.video_busy_pct]
                .into_iter()
                .flatten()
                .reduce(f64::max);
            let detail = match (d.encoder_pct, d.decoder_pct, d.video_busy_pct) {
                (None, None, Some(_)) => Some("encode + decode".to_owned()),
                (None, None, None) => None,
                (enc, dec, _) => Some(format!("enc {} · dec {}", pct(enc), pct(dec))),
            };
            cards.push(self.stat_card(
                Field::Video,
                "card-video",
                "Video",
                t.purple,
                pct(busiest),
                detail,
                self.spark(Field::Video, t.purple, cx).fixed(100., "100%"),
                cx,
            ));
        }
        div().flex().flex_row().gap(rem_px(10.)).children(cards)
    }

    fn util_panel(&self, d: &Device, info: Option<&DeviceInfo>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let source = info
            .and_then(|i| i.util_source.as_deref())
            .unwrap_or(d.backend.as_str());
        panel(IconName::Monitor, "GPU utilization", self.expand(Field::Util), cx)
            .child(
                self.chart(&self.charts.util, cx)
                    .line(self.data(Field::Util), t.green, "GPU")
                    .unit(Unit::Percent)
                    .y(YScale::Fixed(0., 100.))
                    .fill(),
            )
            .child(legend(self.width_s, cx))
            .child(div().h(t.hairline_width).bg(t.hairline))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rem_px(28.))
                    .child(readout(
                        "Live GPU",
                        d.util_pct.map_or("–".into(), |v| format!("{v:.0}%")),
                        cpu_colour(d.util_pct.unwrap_or(0.), t),
                        cx,
                    ))
                    .when_some(d.mem_busy_pct, |r, m| {
                        r.child(readout("Memory controller", format!("{m:.0}%"), t.text, cx))
                    }),
            )
            .child(caption(
                format!(
                    "Share of time the GPU was busy, as {source} reports it, not how much of its capacity was in use. \
                     Memory controller is the time spent reading or writing VRAM."
                ),
                cx,
            ))
    }

    fn vram_panel(&self, d: &Device, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let mut chart = self
            .chart(&self.charts.vram, cx)
            .line(self.data(Field::Vram), t.teal, "VRAM")
            .unit(Unit::Bytes)
            .fill();
        chart = match d.vram_total_bytes {
            Some(total) => chart
                .rule(total as f64, "Total", t.teal)
                .y(YScale::Fixed(0., total as f64 * RULE_HEADROOM)),
            None => chart.y(YScale::Zero),
        };
        panel(IconName::MemoryStick, "VRAM used", self.expand(Field::Vram), cx)
            .child(chart)
            .child(legend(self.width_s, cx))
    }

    fn power_panel(&self, d: &Device, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let mut chart = self
            .chart(&self.charts.power, cx)
            .line(self.data(Field::Power), t.blue, "Power")
            .unit(Unit::Watts);
        chart = match d.power_limit_w {
            Some(limit) => chart
                .rule(limit, "Limit", t.orange)
                .y(YScale::Fixed(0., limit * RULE_HEADROOM)),
            None => chart.y(YScale::Zero),
        };
        panel(IconName::Zap, "Power", self.expand(Field::Power), cx)
            .child(chart)
            .child(legend(self.width_s, cx))
    }

    fn suspended_panel(&self, d: &Device, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        section(IconName::Monitor, "Device", cx).child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(rem_px(6.))
                .py(rem_px(48.))
                .child(
                    div()
                        .text_size(t.panel_title)
                        .font_weight(t.weight_semibold)
                        .child("Suspended — not polled to avoid waking it"),
                )
                .child(caption(
                    format!(
                        "PCI power state {}. Readings resume once something else wakes the device.",
                        d.power_state.as_deref().unwrap_or("–")
                    ),
                    cx,
                )),
        )
    }

    fn device_panel(&self, pci: &str, d: &Device, info: Option<&DeviceInfo>, error: Option<&String>, cx: &App) -> Div {
        let t = theme(cx);
        let limit = d.power_limit_w.map(|l| {
            match (
                info.and_then(|i| i.power_limit_min_w),
                info.and_then(|i| i.power_limit_max_w),
            ) {
                (Some(lo), Some(hi)) => format!("{} ({lo:.0}–{hi:.0})", Unit::Watts.format(l)),
                _ => Unit::Watts.format(l),
            }
        });
        let fan = d
            .fan_pct
            .map(|p| format!("{p:.0}%"))
            .or_else(|| d.fan_rpm.map(|r| format!("{r} RPM")));
        let state = d.power_state.as_ref().map(|s| match &d.pstate {
            Some(p) => format!("{s} · {p}"),
            None => s.clone(),
        });
        let throttles = throttle_labels(d.throttle_reasons.as_deref());
        section(IconName::Info, "Device", cx)
            .child(facts(
                vec![
                    ("PCI slot", Some(pci.to_owned()), t.text),
                    ("Driver", Some(d.driver.clone()), t.text),
                    ("Backend", Some(d.backend.clone()), t.text),
                    ("PCIe link", pcie(d.pcie_gen, d.pcie_width), t.text),
                    (
                        "PCIe max",
                        pcie(info.and_then(|i| i.pcie_max_gen), info.and_then(|i| i.pcie_max_width)),
                        t.text,
                    ),
                    ("Power limit", limit, t.text),
                    ("Fan", fan, t.text),
                    ("Power state", state, t.text),
                    ("Backend error", error.cloned(), t.orange),
                ],
                cx,
            ))
            .when(self.residency.pci == pci && !self.residency.counts.is_empty(), |p| {
                p.child(caption_label("Clock states", cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rem_px(5.))
                            .children(self.residency.shares().into_iter().map(|(state, share)| {
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(rem_px(8.))
                                    .text_size(t.caption)
                                    .font_features(t.numbers.clone())
                                    .child(div().w(rem_px(24.)).flex_shrink_0().child(state.to_owned()))
                                    .child(div().flex_1().child(proportion(share, t.blue, cx)))
                                    .child(
                                        div()
                                            .w(rem_px(34.))
                                            .flex()
                                            .justify_end()
                                            .text_color(t.text_secondary)
                                            .child(format!("{:.0}%", share * 100.)),
                                    )
                            })),
                    )
                    .child(caption(
                        "Share of samples in each performance state since this screen opened. P0 is the fastest; \
                         higher numbers are lower-power states.",
                        cx,
                    ))
            })
            .when(!throttles.is_empty(), |p| {
                p.child(caption_label("Throttle reasons", cx)).child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(rem_px(6.))
                        .children(throttles.into_iter().map(|label| {
                            div()
                                .px(rem_px(8.))
                                .py(rem_px(2.))
                                .rounded_full()
                                .bg(t.orange.opacity(0.16))
                                .text_color(t.orange)
                                .text_size(t.small_caption)
                                .font_weight(t.weight_medium)
                                .child(label)
                        })),
                )
            })
    }

    fn category_panel(&self, split: &Split, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let total = split.total();
        let basis = if split.by_util { "of busy time" } else { "of VRAM" };
        let bar = div()
            .flex()
            .flex_row()
            .h(rem_px(12.))
            .rounded(rem_px(6.))
            .overflow_hidden()
            .bg(t.card)
            .children(
                Category::ALL
                    .iter()
                    .zip(split.parts)
                    .filter(|(_, (w, _))| *w > 0.)
                    .map(|(c, (w, n))| {
                        let tip: SharedString =
                            format!("{} · {:.0}% {basis} · {n}", c.label(), w / total * 100.).into();
                        div()
                            .id(c.label())
                            .h_full()
                            .w(relative((w / total) as f32))
                            .bg(c.colour(t))
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    }),
            );
        let rows = Category::ALL.iter().zip(split.parts).map(|(c, (w, n))| {
            let processes = if n == 1 { "process" } else { "processes" };
            let detail = match n {
                0 => "none".to_owned(),
                _ if w <= 0. => format!("idle · {n} {processes}"),
                _ => format!("{:.0}% {basis} · {n} {processes}", w / total * 100.),
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(7.))
                .text_size(t.caption)
                .child(div().flex_shrink_0().size(rem_px(9.)).rounded(px(2.)).bg(c.colour(t)))
                .child(div().flex_shrink_0().child(c.label()))
                .child(div().flex_1())
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(detail),
                )
        });
        let explain = if split.by_util {
            "Each process's GPU share, grouped by what it is: AI and ML runtimes, display and UI, media, and \
             everything else. Shares can overlap on the GPU, so the bar is the split of the busy time."
        } else {
            "Each process's VRAM, grouped by what it is: AI and ML runtimes, display and UI, media, and everything \
             else. No process on this device reports GPU time, so the split is by memory."
        };
        section(IconName::ChartColumnStacked, "By category", cx)
            .child(bar)
            .child(div().flex().flex_col().gap(rem_px(6.)).children(rows))
            .child(caption(explain, cx))
    }

    fn process_panel(&self, procs: &[(&Process, Category)], cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let body = if procs.is_empty() {
            caption("No processes on this GPU", cx).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(rem_px(9.))
                .children(procs.iter().take(PROCESS_ROWS).map(|(p, c)| {
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(rem_px(8.))
                        .child(div().mt(rem_px(5.)).size(rem_px(6.)).rounded_full().bg(c.colour(t)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(t.caption)
                                        .font_weight(t.weight_medium)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(p.comm.clone()),
                                )
                                .child(caption(format!("{} · pid {}", c.label(), p.pid), cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .font_features(t.numbers.clone())
                                .child(
                                    div()
                                        .text_size(t.caption)
                                        .font_weight(t.weight_medium)
                                        .child(p.util_pct.map_or("–".into(), |u| format!("{u:.0}%"))),
                                )
                                .child(caption(p.mem_bytes.map_or("–".into(), |b| bytes(b as f64)), cx)),
                        )
                }))
                .into_any_element()
        };
        section(IconName::List, "Processes on this GPU", cx)
            .child(body)
            .child(caption(
                "Ranked by GPU time, then VRAM. A dash means the driver does not report that process's share.",
                cx,
            ))
    }
}

impl Render for Gpu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let gpu = self
            .live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.gpu.clone())
            .unwrap_or_default();
        let pci = self.pci.clone().unwrap_or_default();
        let device = gpu.devices.get(&pci);
        let info = self.meta.as_ref().and_then(|m| m.devices.get(&pci)).cloned();
        let mut procs: Vec<&Process> = gpu.processes.iter().filter(|p| p.pci == pci).collect();
        rank(&mut procs);
        let pids: HashSet<u32> = gpu.processes.iter().map(|p| p.pid).collect();
        self.cmdlines.retain(|pid, _| pids.contains(pid));
        let procs: Vec<(&Process, Category)> = procs
            .into_iter()
            .map(|p| {
                let cmdline = self.cmdline(p);
                (p, classify(&p.comm, cmdline.as_deref()))
            })
            .collect();
        let split = Split::new(
            &procs
                .iter()
                .map(|(p, c)| (*c, p.util_pct, p.mem_bytes))
                .collect::<Vec<_>>(),
        );
        let header = self.header(&gpu.devices, device, info.as_ref(), cx).into_any_element();
        let cx: &App = cx;
        let t = theme(cx);
        let main = div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .pl(t.page_padding)
            .py(t.page_padding)
            .child(header);
        let main = match device {
            None => main
                .child(section(IconName::Monitor, "GPU", cx).child(caption("No GPU reported by the collector.", cx))),
            Some(d) if suspended(d) => main.child(self.suspended_panel(d, cx)),
            Some(d) => main
                .child(self.cards(d, cx))
                .when(self.reported(Field::Util), |m| {
                    m.child(self.util_panel(d, info.as_ref(), cx))
                })
                .when(self.reported(Field::Vram), |m| m.child(self.vram_panel(d, cx)))
                .when(self.reported(Field::Power), |m| m.child(self.power_panel(d, cx))),
        };
        let rail = device.map(|d| {
            div()
                .flex()
                .flex_col()
                .gap(t.gap)
                .child(self.device_panel(&pci, d, info.as_ref(), gpu.backend_errors.get(&pci), cx))
                .child(self.category_panel(&split, cx))
                .child(self.process_panel(&procs, cx))
        });
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("gpu-main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(main),
            )
            .child(
                div()
                    .id("gpu-rail")
                    .flex_shrink_0()
                    .w(t.rail_width + t.gap + t.page_padding)
                    .h_full()
                    .overflow_y_scroll()
                    .child(div().pl(t.gap).pr(t.page_padding).py(t.page_padding).children(rail)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(vendor: &str, util: Option<f64>, vram_total: Option<u64>, state: &str) -> Device {
        Device {
            vendor: vendor.into(),
            util_pct: util,
            vram_total_bytes: vram_total,
            power_state: Some(state.into()),
            ..Device::default()
        }
    }

    fn proc(comm: &str, util: Option<f64>, mem: Option<u64>) -> Process {
        Process {
            comm: comm.into(),
            util_pct: util,
            mem_bytes: mem,
            ..Process::default()
        }
    }

    #[test]
    fn processes_are_put_in_the_category_their_name_says() {
        assert_eq!(classify("ollama", None), Category::Ai);
        assert_eq!(classify("llama-server", None), Category::Ai);
        assert_eq!(
            classify("python3", Some("python3 train.py --backend torch")),
            Category::Ai
        );
        assert_eq!(
            classify("python3", Some("python3 -m http.server")),
            Category::Other,
            "Python alone says nothing about the GPU work"
        );
        assert_eq!(
            classify("python3", None),
            Category::Other,
            "an unreadable command line is not evidence"
        );
        for display in ["Xwayland", "Hyprland", "quickshell", "qs"] {
            assert_eq!(classify(display, None), Category::Display, "{display}");
        }
        for media in ["ffmpeg", "mpv", "obs"] {
            assert_eq!(classify(media, None), Category::Media, "{media}");
        }
        assert_eq!(classify("firefox", None), Category::Other);
        assert_eq!(
            classify("mpvpaper", None),
            Category::Other,
            "whole names, not prefixes, outside the llama family"
        );
    }

    #[test]
    fn the_split_weighs_gpu_time_and_falls_back_to_vram_when_no_process_reports_any() {
        let busy = Split::new(&[
            (Category::Ai, Some(60.), Some(100)),
            (Category::Display, Some(20.), Some(900)),
            (Category::Display, None, Some(50)),
        ]);
        assert!(busy.by_util);
        assert_eq!(busy.parts, [(60., 1), (20., 2), (0., 0), (0., 0)]);

        // NVML without accounting reports VRAM per process but no utilisation.
        let nvml = Split::new(&[
            (Category::Display, None, Some(300)),
            (Category::Other, Some(0.), Some(100)),
        ]);
        assert!(!nvml.by_util, "all-zero utilisation is no basis for shares");
        assert_eq!(nvml.parts, [(0., 0), (300., 1), (0., 0), (100., 1)]);
        assert_eq!(nvml.total(), 400.);
    }

    #[test]
    fn processes_rank_by_utilisation_then_vram() {
        let rows = [
            proc("idle-big", Some(0.), Some(900)),
            proc("unknown", None, Some(5000)),
            proc("busy", Some(40.), Some(10)),
            proc("idle-small", Some(0.), Some(100)),
        ];
        let mut refs: Vec<&Process> = rows.iter().collect();
        rank(&mut refs);
        let order: Vec<&str> = refs.iter().map(|p| p.comm.as_str()).collect();
        assert_eq!(order, ["busy", "idle-big", "idle-small", "unknown"]);
    }

    #[test]
    fn the_default_device_is_the_busiest_awake_discrete_gpu() {
        let mut devices = BTreeMap::new();
        devices.insert(
            "0000:01:00.0".to_owned(),
            device("nvidia", Some(3.), Some(24 << 30), "D0"),
        );
        devices.insert(
            "0000:7a:00.0".to_owned(),
            device("amd", Some(40.), Some(512 << 20), "D0"),
        );
        assert_eq!(
            default_device(&devices).as_deref(),
            Some("0000:01:00.0"),
            "a busy iGPU does not beat the dGPU"
        );

        devices.insert(
            "0000:03:00.0".to_owned(),
            device("amd", Some(70.), Some(16 << 30), "D0"),
        );
        assert_eq!(default_device(&devices).as_deref(), Some("0000:03:00.0"));

        devices.get_mut("0000:03:00.0").unwrap().power_state = Some("D3cold".into());
        devices.get_mut("0000:01:00.0").unwrap().power_state = Some("D3cold".into());
        assert_eq!(
            default_device(&devices).as_deref(),
            Some("0000:7a:00.0"),
            "a suspended dGPU would show nothing"
        );
        assert_eq!(default_device(&BTreeMap::new()), None);
    }

    #[test]
    fn throttle_badges_skip_idle_and_read_as_words() {
        assert!(throttle_labels(None).is_empty());
        assert!(throttle_labels(Some(&["gpu_idle".into()])).is_empty());
        assert_eq!(
            throttle_labels(Some(&[
                "gpu_idle".into(),
                "sw_power_cap".into(),
                "hw_thermal_slowdown".into()
            ])),
            ["SW power cap", "HW thermal slowdown"]
        );
    }

    #[test]
    fn series_paths_follow_the_schema_naming() {
        let known: HashSet<String> = [
            "/gpu/devices/0000:01:00.0/util_pct",
            "/gpu/devices/0000:01:00.0/encoder_pct",
            "/gpu/devices/0000:01:00.0/decoder_pct",
            "/gpu/devices/0000:7a:00.0/video_busy_pct",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            Field::Util.source("0000:01:00.0", &known).paths,
            ["/gpu/devices/0000:01:00.0/util_pct"]
        );
        assert_eq!(Field::Video.source("0000:01:00.0", &known).paths.len(), 2);
        assert_eq!(
            Field::Video.source("0000:7a:00.0", &known).paths,
            ["/gpu/devices/0000:7a:00.0/video_busy_pct"]
        );
        assert!(
            Field::Clock.source("0000:01:00.0", &known).paths.is_empty(),
            "a field never reported has no card"
        );
    }

    #[test]
    fn pcie_links_name_generation_and_width() {
        assert_eq!(pcie(Some(4), Some(16)).as_deref(), Some("Gen 4 x16"));
        assert_eq!(pcie(None, Some(8)).as_deref(), Some("x8"));
        assert_eq!(pcie(None, None), None);
    }
}
