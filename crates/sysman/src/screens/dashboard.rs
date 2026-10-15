use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::cpu::CpuMeta;
use schema::processes::Process;

use crate::charts::core_bars::{CoreBar, core_bars};
use crate::charts::format::{Unit, bytes, duration};
use crate::charts::sparkline::Sparkline;
use crate::charts::stacked_bar::{Segment, stacked_bar};
use crate::charts::trend::{ChartState, TrendChart, YScale, clock};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::pressure::{self, Band, CRITICAL, WARNING};
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source, matching};
use crate::ui::shell::Tab;
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::card::metric_card;
use crate::ui::widgets::detail_sheet::{Detail, Series, opener};
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::{caption_label, panel, section};
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};
use crate::ui::widgets::text::caption;

const BUCKETS: u32 = 120;
/// Stored history is re-read this often; the live rings cover the newest 30 minutes in between.
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 150.;
const RAIL_CHART_HEIGHT: f32 = 120.;
const TOP_ROWS: usize = 5;
const CPU_CAPTION: &str = "Live values are the latest sample, not selected-range means. Total CPU is the share of all \
                           cores in use, 0-100%. Per-process CPU uses one core as 100%, so multi-threaded work can exceed 100%.";
const GROUP_COLOURS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Metric {
    Pressure,
    Cpu,
    NetDown,
    NetUp,
    DiskRead,
    DiskWrite,
    Swap,
    CpuTemp,
    GpuTemp,
    Available,
    App,
    Cached,
    Compressed,
    Kernel,
}

impl Metric {
    const ALL: [Metric; 14] = [
        Self::Pressure,
        Self::Cpu,
        Self::NetDown,
        Self::NetUp,
        Self::DiskRead,
        Self::DiskWrite,
        Self::Swap,
        Self::CpuTemp,
        Self::GpuTemp,
        Self::Available,
        Self::App,
        Self::Cached,
        Self::Compressed,
        Self::Kernel,
    ];
    /// Where the series that need a key (interface, disk, device) live.
    const PREFIXES: [&str; 4] = ["/net/interfaces/", "/disk/", "/gpu/devices/", "/memory/zram/"];

    fn source<'a>(self, known: impl IntoIterator<Item = &'a str> + Clone) -> Source {
        let one = |p: &str| Source::new(vec![p.to_owned()], Combine::Sum);
        let sum = |prefix, suffix, keep: fn(&str) -> bool| {
            Source::new(matching(known.clone(), prefix, suffix, keep), Combine::Sum)
        };
        match self {
            Self::Pressure => Source::new(
                vec!["/pressure/memory/some/avg10".into(), "/memory/swap_in_pages_ps".into()],
                Combine::Pressure,
            ),
            Self::Cpu => one("/cpu/total_pct"),
            Self::NetDown => sum("/net/interfaces/", "/rx_bps", |k| k != "lo"),
            Self::NetUp => sum("/net/interfaces/", "/tx_bps", |k| k != "lo"),
            Self::DiskRead => sum("/disk/", "/read_bps", whole_disk),
            Self::DiskWrite => sum("/disk/", "/write_bps", whole_disk),
            Self::Swap => one("/memory/swap_used_bytes"),
            Self::CpuTemp => one("/cpu/temp_c"),
            Self::GpuTemp => Source::new(matching(known, "/gpu/devices/", "/temp_c", |_| true), Combine::Max),
            Self::Available => one("/memory/available_bytes"),
            Self::App => one("/memory/anon_bytes"),
            Self::Cached => one("/memory/cache_bytes"),
            Self::Compressed => sum("/memory/zram/", "/compressed_bytes", |_| true),
            Self::Kernel => Source::new(
                [
                    "/memory/slab_bytes",
                    "/memory/kernel_stack_bytes",
                    "/memory/page_tables_bytes",
                ]
                .map(String::from)
                .to_vec(),
                Combine::Sum,
            ),
        }
    }

    /// What the metric means, then how it is calculated, for its detail sheet.
    fn explanation(self) -> (&'static str, &'static str) {
        match self {
            Self::Pressure => (
                "How hard the kernel is working to find memory, from 0 to 100. Near zero, memory is plentiful. A \
                 sustained rise past Warning means tasks are stalling while pages are reclaimed or read back from \
                 swap, which is felt as stutter and slow app switching. A nonzero swap balance alone does not raise it.",
                "From the kernel's pressure stall information: the share of the last 10 seconds in which at least \
                 one task waited for memory (/proc/pressure/memory, some avg10). 0% maps to 0, 5% to 34 (Warning), \
                 20% to 67 (Critical) and 50% or more to 100, linearly in between. While more than 1 MB/s is being \
                 swapped in, the index is held at Warning or above.",
            ),
            Self::Cpu => (
                "The share of all logical CPUs that was busy. Short spikes are normal; a long run near 100% means \
                 work is queueing for the processor. Compare it with the load average, which also counts tasks \
                 waiting to run.",
                "From /proc/stat: the change in busy time across all CPUs between two samples, divided by the change \
                 in total time. Busy is everything except idle and I/O wait, so 0-100% covers the whole machine.",
            ),
            Self::NetDown | Self::NetUp => (
                "Bytes received and sent per second over all network interfaces except loopback. Virtual interfaces \
                 such as bridges and VPN tunnels count too, so traffic that crosses one can appear twice.",
                "From /proc/net/dev: the change in each interface's receive and transmit byte counters between two \
                 samples, divided by the time between them, then summed across interfaces.",
            ),
            Self::DiskRead | Self::DiskWrite => (
                "Bytes read from and written to the physical disks per second. Partitions, device-mapper and RAID \
                 layers are left out, since their traffic is already counted on the disks beneath; loop, RAM and \
                 zram devices are not disk I/O.",
                "From /proc/diskstats: the change in sectors read and written by each whole disk between two \
                 samples, times 512 bytes, divided by the time between them, then summed.",
            ),
            Self::Swap => (
                "Memory the kernel has moved out of RAM to make room, onto zram or a swap partition or file. A \
                 balance that sits still is harmless; what matters is swapping in and out while pressure is high.",
                "SwapTotal minus SwapFree from /proc/meminfo, across every active swap device. For zram this is the \
                 uncompressed size of the swapped pages, not the RAM they occupy.",
            ),
            Self::CpuTemp | Self::GpuTemp => (
                "Processor and graphics temperatures. Sustained readings near the chip's limit make it lower its \
                 clocks to protect itself, which costs performance. The GPU line is the hottest GPU when there is \
                 more than one.",
                "Read from the hardware monitoring sensors in /sys/class/hwmon: the CPU package sensor (coretemp or \
                 k10temp) and each GPU's own sensor as its driver reports it, in degrees Celsius.",
            ),
            Self::Available => (
                "How much memory could be given to new work right now without swapping: free memory plus cache and \
                 reclaimable kernel memory that can be dropped. It is the best single number for headroom.",
                "MemAvailable from /proc/meminfo, the kernel's own estimate. It accounts for the low watermarks and \
                 for cache that must stay resident, so it is less than free plus cached.",
            ),
            Self::App => (
                "Memory that programs have allocated for their own data: heaps, stacks and other anonymous mappings. \
                 It cannot be dropped, only swapped out, so a steady climb with no matching workload can be a leak.",
                "AnonPages from /proc/meminfo: anonymous pages mapped into user processes. Shared memory and tmpfs \
                 are counted under cache by the kernel, so they are not included.",
            ),
            Self::Cached => (
                "File contents the kernel keeps in RAM so they need not be read again. Cache is given back as soon \
                 as programs need the memory, so a large figure is a sign of a well-used machine, not a shortage.",
                "Cached plus Buffers plus SReclaimable, minus Shmem, from /proc/meminfo: page cache, block buffers \
                 and reclaimable kernel caches, less shared memory that cannot simply be dropped.",
            ),
            Self::Compressed => (
                "RAM used by zram to hold compressed swapped-out pages. Each page swapped to zram costs only its \
                 compressed size, so memory stretches further at the cost of some CPU time.",
                "The compressed size of the stored data, compr_data_size in each zram device's mm_stat in \
                 /sys/block, summed over all zram devices.",
            ),
            Self::Kernel => (
                "Memory the kernel uses for itself: object caches, the stacks of every thread and the page tables \
                 that map process memory. It grows with the number of processes, open files and network buffers.",
                "Slab plus KernelStack plus PageTables from /proc/meminfo. Part of the slab is reclaimable and is \
                 also shown in the cache figure.",
            ),
        }
    }

    fn module(self) -> &'static str {
        match self {
            Self::Pressure => "pressure",
            Self::Cpu | Self::CpuTemp => "cpu",
            Self::NetDown | Self::NetUp => "net",
            Self::DiskRead | Self::DiskWrite => "disk",
            Self::GpuTemp => "gpu",
            Self::Swap | Self::Available | Self::App | Self::Cached | Self::Compressed | Self::Kernel => "memory",
        }
    }
}

/// A physical disk: not a partition or a device-mapper/RAID layer (their I/O is already counted on the disks
/// beneath), and not loop, RAM or zram, whose traffic is not disk I/O.
pub(crate) fn whole_disk(name: &str) -> bool {
    if ["loop", "ram", "zram", "sr", "dm-", "md"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        return false;
    }
    let digits = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits.len() == name.len() {
        return true;
    }
    // nvme0n1p2 and mmcblk0p1 are partitions; nvme0n1 and mmcblk0 are not.
    if let Some(stem) = digits.strip_suffix('p') {
        return !stem.ends_with(|c: char| c.is_ascii_digit());
    }
    !["sd", "vd", "hd", "xvd"].iter().any(|p| name.starts_with(p))
}

/// History read for one range; the newest end is replaced by live rings on every tick.
struct Stored {
    range: Range,
    known: Vec<String>,
    buckets: HashMap<Metric, Vec<Bucket>>,
}

fn load(range: Range, to: i64) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let mut known = Vec::new();
    for prefix in Metric::PREFIXES {
        known.extend(history.paths(prefix)?);
    }
    let from = to - range.span_s();
    let mut buckets = HashMap::new();
    for metric in Metric::ALL {
        let source = metric.source(known.iter().map(String::as_str));
        buckets.insert(metric, source.query(&history, from, to, BUCKETS)?);
    }
    Ok(Stored { range, known, buckets })
}

#[derive(Default)]
struct Charts {
    pressure: Rc<RefCell<ChartState>>,
    cpu: Rc<RefCell<ChartState>>,
    net: Rc<RefCell<ChartState>>,
    disk: Rc<RefCell<ChartState>>,
    swap: Rc<RefCell<ChartState>>,
    thermals: Rc<RefCell<ChartState>>,
}

pub struct Dashboard {
    me: WeakEntity<Self>,
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    range: Range,
    stored: Option<Stored>,
    loading: bool,
    /// Range and time of the last history read, so a failing read is not retried every tick.
    attempt: Option<(Range, f64)>,
    error: Option<SharedString>,
    merged: HashMap<Metric, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: Charts,
    cpu_meta: Option<CpuMeta>,
    /// A sheet to open once history has loaded, for off-screen captures.
    #[cfg(debug_assertions)]
    dev_sheet: Option<Metric>,
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

impl Dashboard {
    pub fn new(live: Entity<Live>, feed: Entity<ProcessFeed>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        cx.observe(&feed, |_, _, cx| cx.notify()).detach();
        let mut this = Self {
            me: cx.weak_entity(),
            live,
            feed,
            range: remembered(Tab::Dashboard, cx),
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            merged: HashMap::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            charts: Charts::default(),
            cpu_meta: None,
            #[cfg(debug_assertions)]
            dev_sheet: std::env::var("SYSMAN_DEV_SHEET").ok().and_then(|name| {
                Metric::ALL
                    .into_iter()
                    .find(|m| format!("{m:?}").eq_ignore_ascii_case(&name))
            }),
        };
        this.refresh(cx);
        this
    }

    /// The first sample is in and the history read for the current range has returned.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        self.live.read(cx).latest.is_some()
            && self.feed.read(cx).latest.is_some()
            && !self.loading
            && self.attempt.is_some_and(|(range, _)| range == self.range)
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Dashboard, range, cx);
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let stale = self
            .attempt
            .is_none_or(|(range, at)| range != self.range || now - at > RELOAD_S);
        if stale && !self.loading {
            self.load(now, cx);
        }
        self.merge(now, cx);
        cx.notify();
    }

    fn load(&mut self, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((self.range, now));
        let range = self.range;
        let task = cx.background_executor().spawn(async move { load(range, now as i64) });
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

    fn merge(&mut self, now: f64, cx: &App) {
        let live = self.live.read(cx);
        if self.cpu_meta.is_none() {
            self.cpu_meta = live
                .meta
                .as_ref()
                .and_then(|m| m.modules.get("cpu")?.info.clone())
                .and_then(|info| serde_json::from_value(info).ok());
        }
        let to = now as i64;
        let from = to - self.range.span_s();
        self.width_s = bucket_width(from, to, BUCKETS);
        let stored = self.stored.as_ref().filter(|s| s.range == self.range);
        let known: Vec<&str> = stored
            .map(|s| s.known.iter().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(live.paths())
            .collect();
        for metric in Metric::ALL {
            let source = metric.source(known.iter().copied());
            let history = stored
                .and_then(|s| s.buckets.get(&metric))
                .map_or(&[][..], Vec::as_slice);
            let merged = source.top_up(history, live, from, self.width_s);
            self.merged.insert(metric, merged.into());
        }
        self.span = (from as f64, now);
        self.version += 1;
    }

    fn data(&self, metric: Metric) -> Rc<[Bucket]> {
        self.merged
            .get(&metric)
            .cloned()
            .unwrap_or_else(|| Rc::from(Vec::new()))
    }

    /// Missing buckets wider than this are gaps: a bucket and a half, or a few collector intervals.
    fn gap_s(&self, metric: Metric, cx: &App) -> f64 {
        let interval = self
            .live
            .read(cx)
            .meta
            .as_ref()
            .and_then(|m| m.modules.get(metric.module()))
            .map_or(2, |m| m.interval_s);
        (self.width_s as f64 * 1.5).max(interval as f64 * 2.5)
    }

    fn chart(&self, state: &Rc<RefCell<ChartState>>, metric: Metric, cx: &App) -> TrendChart {
        TrendChart::new(
            state.clone(),
            self.version,
            self.span,
            self.width_s as f64,
            self.gap_s(metric, cx),
        )
    }

    fn spark(&self, metric: Metric, color: Hsla, cx: &App) -> Sparkline {
        Sparkline::new(
            self.data(metric),
            color,
            self.span,
            self.width_s as f64,
            self.gap_s(metric, cx),
        )
    }
}

pub(crate) struct Groups {
    /// Label, colour and the logical CPUs in each group, in display order.
    pub(crate) groups: Vec<(SharedString, Hsla, Vec<u32>)>,
}

impl Groups {
    pub(crate) fn new(meta: Option<&CpuMeta>, t: &Theme) -> Self {
        let colours = [t.blue, t.teal, t.purple, t.indigo, t.pink];
        let Some(topology) = meta.map(|m| &m.topology) else {
            return Self { groups: Vec::new() };
        };
        let groups = match &topology.core_types {
            Some(types) => vec![
                ("Performance".into(), t.blue, types.performance.clone()),
                ("Efficiency".into(), t.teal, types.efficiency.clone()),
            ],
            None => topology
                .l3_groups
                .iter()
                .enumerate()
                .map(|(i, g)| (format!("CCD {i}").into(), colours[i % GROUP_COLOURS], g.cpus.clone()))
                .collect(),
        };
        Self { groups }
    }

    pub(crate) fn of(&self, cpu: u32) -> Option<&(SharedString, Hsla, Vec<u32>)> {
        self.groups.iter().find(|g| g.2.contains(&cpu))
    }

    pub(crate) fn mean(cores: &[f64], cpus: &[u32]) -> f64 {
        let values: Vec<f64> = cpus.iter().filter_map(|&c| cores.get(c as usize).copied()).collect();
        values.iter().sum::<f64>() / values.len().max(1) as f64
    }
}

pub(crate) fn cpu_colour(pct: f64, t: &Theme) -> Hsla {
    if pct >= 85. {
        t.red
    } else if pct >= 60. {
        t.orange
    } else {
        t.green
    }
}

fn pressure_colour(index: f64, t: &Theme) -> Hsla {
    match pressure::band(index) {
        Band::Normal => t.green,
        Band::Warning => t.orange,
        Band::Critical => t.red,
    }
}

fn live_pressure(sample: &Sample) -> Option<f64> {
    let avg10 = sample.pressure.as_ref()?.get("memory")?.get("some")?.avg10;
    let swap_in = sample.memory.as_ref().and_then(|m| m.swap_in_pages_ps).unwrap_or(0) as f64;
    Some(pressure::index(avg10, swap_in * pressure::PAGE_BYTES))
}

/// A LIVE readout: caption over a coloured value.
pub(crate) fn readout(
    label: &str,
    value: String,
    color: Hsla,
    icon: Option<IconName>,
    cx: &App,
) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .when_some(icon, |d, icon| d.child(Icon::new(icon).xsmall().text_color(color)))
                .child(caption_label(label, cx)),
        )
        .child(
            div()
                .text_size(t.card_value)
                .font_weight(FontWeight::SEMIBOLD)
                .font_features(t.numbers.clone())
                .text_color(color)
                .child(value),
        )
}

pub(crate) fn top_list(rows: Vec<(String, String)>, empty: &'static str, cx: &App) -> AnyElement {
    let t = theme(cx);
    if rows.is_empty() {
        return caption(empty, cx).into_any_element();
    }
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(t.caption)
        .children(rows.into_iter().map(|(name, value)| {
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(value),
                )
        }))
        .into_any_element()
}

impl Dashboard {
    fn header(&self, sample: Option<&Sample>, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let meta = self.cpu_meta.as_ref();
        let mut facts = Vec::new();
        if let Some(topology) = meta.map(|m| &m.topology) {
            if let Some(cores) = topology.cores {
                facts.push(format!("{cores} cores"));
            }
            facts.push(format!("{} threads", topology.threads));
            match &topology.core_types {
                Some(types) => facts.push(format!("{}P + {}E", types.performance.len(), types.efficiency.len())),
                None if topology.l3_groups.len() > 1 => facts.push(format!("{} CCDs", topology.l3_groups.len())),
                None => {}
            }
        }
        if let Some(memory) = sample.and_then(|s| s.memory.as_ref()) {
            facts.push(bytes(memory.total_bytes as f64));
        }
        let system = sample.and_then(|s| s.system.as_ref());
        let this = cx.entity().downgrade();
        div()
            .flex()
            .flex_row()
            .items_end()
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(meta.and_then(|m| m.model.clone()).unwrap_or_else(|| "Processor".into())),
                    )
                    .child(caption(facts.join(" · "), cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(5.))
                            .mt(px(2.))
                            .text_size(t.caption)
                            .child(Icon::new(IconName::Clock).xsmall().text_color(t.text_secondary))
                            .child(div().text_color(t.text_secondary).child("Uptime"))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .font_features(t.numbers.clone())
                                    .child(system.map_or("–".into(), |s| duration(s.uptime_s))),
                            )
                            .when_some(system, |d, s| {
                                d.child(
                                    div()
                                        .text_color(t.text_tertiary)
                                        .child(format!("booted {}", clock(s.boot_time as f64, "%b %-d %H:%M"))),
                                )
                            })
                            .when_some(self.error.clone(), |d, e| d.child(div().text_color(t.orange).child(e))),
                    ),
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
                            this.update(cx, |d, cx| d.set_range(range, cx)).ok();
                        },
                        cx,
                    )),
            )
    }

    fn cards(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let memory = sample.and_then(|s| s.memory.as_ref());
        let total = memory.map_or(1., |m| m.total_bytes as f64);
        let total_label = bytes(total);
        let value = |v: Option<u64>| v.map_or("–".to_string(), |v| bytes(v as f64));
        let index = sample.and_then(live_pressure);
        let zram = memory.filter(|m| !m.zram.is_empty());
        let (fourth_label, fourth_metric, fourth_colour, fourth_value) = match (zram, memory) {
            (Some(m), _) => (
                "Compressed",
                Metric::Compressed,
                t.orange,
                Some(m.zram.values().map(|z| z.compressed_bytes).sum()),
            ),
            (None, m) => (
                "Kernel",
                Metric::Kernel,
                t.purple,
                m.map(|m| {
                    m.slab_bytes.unwrap_or(0) + m.kernel_stack_bytes.unwrap_or(0) + m.page_tables_bytes.unwrap_or(0)
                }),
            ),
        };
        let swap_total = memory.map_or(0., |m| m.swap_total_bytes as f64);
        let fixed = |spark: Sparkline, max: f64, label: &str| spark.fixed(max, label.to_owned());
        let pressure_colour = pressure_colour(index.unwrap_or(0.), t);
        div()
            .flex()
            .flex_row()
            .gap(px(10.))
            .child(
                metric_card(
                    "card-pressure",
                    "Pressure",
                    pressure_colour,
                    index.map_or("–".into(), |i| format!("{i:.0}%")),
                    fixed(self.spark(Metric::Pressure, pressure_colour, cx), 100., "100%"),
                    cx,
                )
                .on_click(self.expand(Metric::Pressure)),
            )
            .child(
                metric_card(
                    "card-available",
                    "Available",
                    t.green,
                    value(memory.map(|m| m.available_bytes)),
                    fixed(self.spark(Metric::Available, t.green, cx), total, &total_label),
                    cx,
                )
                .on_click(self.expand(Metric::Available)),
            )
            .child(
                metric_card(
                    "card-app",
                    "App",
                    t.blue,
                    value(memory.and_then(|m| m.anon_bytes)),
                    fixed(self.spark(Metric::App, t.blue, cx), total, &total_label),
                    cx,
                )
                .on_click(self.expand(Metric::App)),
            )
            .child(
                metric_card(
                    "card-cached",
                    "Cached",
                    t.teal,
                    value(memory.and_then(|m| m.cache_bytes)),
                    fixed(self.spark(Metric::Cached, t.teal, cx), total, &total_label),
                    cx,
                )
                .on_click(self.expand(Metric::Cached)),
            )
            .child(
                metric_card(
                    "card-fourth",
                    fourth_label,
                    fourth_colour,
                    value(fourth_value),
                    fixed(self.spark(fourth_metric, fourth_colour, cx), total, &total_label),
                    cx,
                )
                .on_click(self.expand(fourth_metric)),
            )
            .child(
                metric_card(
                    "card-swap",
                    "Swap",
                    t.indigo,
                    value(memory.map(|m| m.swap_used_bytes)),
                    fixed(
                        self.spark(Metric::Swap, t.indigo, cx),
                        swap_total.max(1.),
                        &bytes(swap_total),
                    ),
                    cx,
                )
                .on_click(self.expand(Metric::Swap)),
            )
    }

    fn expand(&self, metric: Metric) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        opener(self.me.clone(), metric, |this, metric, cx| {
            Some(this.detail(*metric, cx))
        })
    }

    /// The sheet behind a card or panel; a panel of two lines opens from either of its metrics.
    fn detail(&self, metric: Metric, cx: &App) -> Detail {
        let t = theme(cx);
        let live = self.live.read(cx);
        let known: Vec<&str> = self
            .stored
            .as_ref()
            .map(|s| s.known.iter().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(live.paths())
            .collect();
        let line = |m: Metric, label: &str, color: Hsla| Series {
            label: label.to_owned().into(),
            color,
            source: m.source(known.iter().copied()),
        };
        let memory = live.latest.as_ref().and_then(|s| s.memory.as_ref());
        let total = memory.map_or(0., |m| m.total_bytes as f64);
        let swap_total = memory.map_or(0., |m| m.swap_total_bytes as f64);
        let of = |max: f64| {
            if max > 0. { YScale::Fixed(0., max) } else { YScale::Zero }
        };
        let (title, icon, unit, y, lines, rules): (&str, IconName, Unit, YScale, Vec<Series>, Vec<_>) = match metric {
            Metric::Pressure => (
                "Memory pressure",
                IconName::Gauge,
                Unit::Index,
                YScale::Fixed(0., 100.),
                vec![line(Metric::Pressure, "Pressure", t.orange)],
                vec![
                    (WARNING, "Warning".into(), t.orange),
                    (CRITICAL, "Critical".into(), t.red),
                ],
            ),
            Metric::Cpu => (
                "Processor",
                IconName::Cpu,
                Unit::Percent,
                YScale::Fixed(0., 100.),
                vec![line(Metric::Cpu, "CPU", t.green)],
                vec![(60., "Busy".into(), t.orange), (85., "Heavy".into(), t.red)],
            ),
            Metric::NetDown | Metric::NetUp => (
                "Network",
                IconName::Globe,
                Unit::BytesPerSec,
                YScale::Zero,
                vec![
                    line(Metric::NetDown, "Download", t.green),
                    line(Metric::NetUp, "Upload", t.red),
                ],
                vec![],
            ),
            Metric::DiskRead | Metric::DiskWrite => (
                "Disk",
                IconName::HardDrive,
                Unit::BytesPerSec,
                YScale::Zero,
                vec![
                    line(Metric::DiskRead, "Read", t.blue),
                    line(Metric::DiskWrite, "Write", t.orange),
                ],
                vec![],
            ),
            Metric::CpuTemp | Metric::GpuTemp => (
                "Thermals",
                IconName::Thermometer,
                Unit::Celsius,
                YScale::Fit(10.),
                vec![
                    line(Metric::CpuTemp, "CPU", t.orange),
                    line(Metric::GpuTemp, "GPU", t.red),
                ],
                vec![],
            ),
            Metric::Swap => (
                "Swap",
                IconName::Database,
                Unit::Bytes,
                of(swap_total),
                vec![line(Metric::Swap, "Swap", t.indigo)],
                vec![],
            ),
            Metric::Available => (
                "Available",
                IconName::MemoryStick,
                Unit::Bytes,
                of(total),
                vec![line(Metric::Available, "Available", t.green)],
                vec![],
            ),
            Metric::App => (
                "App memory",
                IconName::MemoryStick,
                Unit::Bytes,
                of(total),
                vec![line(Metric::App, "App", t.blue)],
                vec![],
            ),
            Metric::Cached => (
                "Cached",
                IconName::MemoryStick,
                Unit::Bytes,
                of(total),
                vec![line(Metric::Cached, "Cached", t.teal)],
                vec![],
            ),
            Metric::Compressed => (
                "Compressed",
                IconName::MemoryStick,
                Unit::Bytes,
                of(total),
                vec![line(Metric::Compressed, "Compressed", t.orange)],
                vec![],
            ),
            Metric::Kernel => (
                "Kernel memory",
                IconName::MemoryStick,
                Unit::Bytes,
                of(total),
                vec![line(Metric::Kernel, "Kernel", t.purple)],
                vec![],
            ),
        };
        let (meaning, method) = metric.explanation();
        Detail {
            title: title.into(),
            icon,
            unit,
            y,
            fill: !matches!(unit, Unit::Index | Unit::Percent | Unit::Celsius),
            lines,
            rules,
            meaning,
            method,
            range: self.range,
            live: self.live.clone(),
        }
    }

    fn pressure_panel(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        panel(IconName::Gauge, "Memory pressure", self.expand(Metric::Pressure), cx)
            .child(
                self.chart(&self.charts.pressure, Metric::Pressure, cx)
                    .line(self.data(Metric::Pressure), t.orange, "Pressure")
                    .rule(WARNING, "Warning", t.orange)
                    .rule(CRITICAL, "Critical", t.red)
                    .unit(Unit::Index)
                    .y(YScale::Fixed(0., 100.))
                    .height(px(CHART_HEIGHT)),
            )
            .child(legend(self.width_s, cx))
    }

    fn cpu_panel(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let cpu = sample.and_then(|s| s.cpu.as_ref());
        let cores = cpu.and_then(|c| c.core_pct.clone()).unwrap_or_default();
        let groups = Groups::new(self.cpu_meta.as_ref(), t);
        let total = cpu.map(|c| c.total_pct);
        let load = sample.and_then(|s| s.system.as_ref()).map(|s| s.load[0]);
        let mut footer = vec![
            readout(
                "Live total",
                total.map_or("–".into(), |v| format!("{v:.0}%")),
                cpu_colour(total.unwrap_or(0.), t),
                None,
                cx,
            )
            .into_any_element(),
        ];
        if groups.groups.len() > 1 {
            for (label, colour, cpus) in &groups.groups {
                let value = if cores.is_empty() {
                    "–".into()
                } else {
                    format!("{:.0}%", Groups::mean(&cores, cpus))
                };
                footer.push(readout(&format!("Live {label}"), value, *colour, None, cx).into_any_element());
            }
        }
        footer.push(
            readout(
                "Live load (1 min)",
                load.map_or("–".into(), |l| format!("{l:.2}")),
                t.text,
                None,
                cx,
            )
            .into_any_element(),
        );
        panel(IconName::Cpu, "Processor", self.expand(Metric::Cpu), cx)
            .child(
                self.chart(&self.charts.cpu, Metric::Cpu, cx)
                    .line(self.data(Metric::Cpu), t.green, "CPU")
                    .rule(60., "Busy", t.orange)
                    .rule(85., "Heavy", t.red)
                    .unit(Unit::Percent)
                    .y(YScale::Fixed(0., 100.))
                    .height(px(CHART_HEIGHT)),
            )
            .child(legend(self.width_s, cx))
            .child(div().h(t.hairline_width).bg(t.hairline))
            .child(div().flex().flex_row().gap(px(28.)).children(footer))
            .child(caption(CPU_CAPTION, cx))
    }

    #[allow(clippy::too_many_arguments)]
    fn io_panel(
        &self,
        icon: IconName,
        title: &str,
        state: &Rc<RefCell<ChartState>>,
        lines: [(Metric, &str, Hsla, IconName, f64); 2],
        cx: &App,
    ) -> impl IntoElement {
        let mut chart = self
            .chart(state, lines[0].0, cx)
            .unit(Unit::BytesPerSec)
            .y(YScale::Zero)
            .fill()
            .height(px(CHART_HEIGHT));
        for (metric, label, colour, _, _) in &lines {
            chart = chart.line(self.data(*metric), *colour, label.to_string());
        }
        panel(icon, title.to_owned(), self.expand(lines[0].0), cx)
            .child(div().flex().flex_row().gap(px(28.)).children(lines.iter().map(
                |(_, label, colour, arrow, value)| {
                    readout(
                        &format!("Live {label}"),
                        Unit::BytesPerSec.format(*value),
                        *colour,
                        Some(*arrow),
                        cx,
                    )
                },
            )))
            .child(chart)
            .child(legend(self.width_s, cx))
    }

    fn rail(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let cores = sample
            .and_then(|s| s.cpu.as_ref())
            .and_then(|c| c.core_pct.clone())
            .unwrap_or_default();
        let groups = Groups::new(self.cpu_meta.as_ref(), t);
        let bars = cores
            .iter()
            .enumerate()
            .map(|(i, &pct)| {
                let (group, colour) = groups
                    .of(i as u32)
                    .map_or(("CPU".into(), t.blue), |g| (g.0.clone(), g.1));
                CoreBar {
                    pct,
                    color: colour,
                    group,
                }
            })
            .collect();
        let group_legend = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(12.))
            .children(groups.groups.iter().map(|(label, colour, cpus)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(div().size(px(8.)).rounded(px(2.)).bg(*colour))
                    .child(format!("{label} · {} · {:.0}%", cpus.len(), Groups::mean(&cores, cpus)))
            }));

        let memory = sample.and_then(|s| s.memory.as_ref());
        let composition = memory.map(|m| {
            let kernel = (m.slab_bytes.unwrap_or(0)
                + m.kernel_stack_bytes.unwrap_or(0)
                + m.page_tables_bytes.unwrap_or(0)) as f64;
            let zram: f64 = m.zram.values().map(|z| z.mem_used_bytes as f64).sum();
            let app = m.anon_bytes.unwrap_or(0) as f64;
            let cached = m.cache_bytes.unwrap_or(0) as f64;
            let free = m.free_bytes.unwrap_or(0) as f64;
            let total = m.total_bytes as f64;
            let other = (total - kernel - zram - app - cached - free).max(0.);
            let mut segments = vec![
                Segment {
                    name: "Kernel".into(),
                    bytes: kernel,
                    color: t.purple,
                },
                Segment {
                    name: "App".into(),
                    bytes: app,
                    color: t.blue,
                },
            ];
            if zram > 0. {
                segments.push(Segment {
                    name: "Compressed".into(),
                    bytes: zram,
                    color: t.orange,
                });
            }
            segments.extend([
                Segment {
                    name: "Cached".into(),
                    bytes: cached,
                    color: t.teal,
                },
                Segment {
                    name: "Other".into(),
                    bytes: other,
                    color: t.text_tertiary,
                },
                Segment {
                    name: "Free".into(),
                    bytes: free,
                    color: t.text_tertiary.opacity(0.4),
                },
            ]);
            stacked_bar(segments, total, cx)
        });

        let swap_total = memory.map_or(0., |m| m.swap_total_bytes as f64);
        let rows = self.feed.read(cx).table().map_or(&[][..], |t| t.rows.as_slice());
        let mut by_cpu: Vec<&Process> = rows.iter().collect();
        by_cpu.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
        let top_cpu = by_cpu
            .into_iter()
            .take(TOP_ROWS)
            .map(|r| (r.comm.clone(), format!("{:.1}%", r.cpu_pct)))
            .collect();
        let io = |r: &Process| (r.read_bps.unwrap_or(0) + r.write_bps.unwrap_or(0)) as f64;
        let mut by_io: Vec<&Process> = rows.iter().filter(|r| io(r) > 0.).collect();
        by_io.sort_by(|a, b| io(b).total_cmp(&io(a)));
        let top_disk = by_io
            .into_iter()
            .take(TOP_ROWS)
            .map(|r| (r.comm.clone(), Unit::BytesPerSec.format(io(r))))
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(
                section(IconName::Cpu, "CPU cores", cx)
                    .child(core_bars(bars, px(48.), cx))
                    .child(group_legend)
                    .child(caption(
                        "Live use of each logical CPU. Hover a bar for its current percentage and group.",
                        cx,
                    )),
            )
            .child(
                section(IconName::ChartColumnStacked, "Memory composition", cx)
                    .children(composition)
                    .child(caption(
                        "Current RAM breakdown, not a range average. Hover a segment for exact values.",
                        cx,
                    )),
            )
            .child(
                panel(IconName::Database, "Swap", self.expand(Metric::Swap), cx)
                    .child(
                        self.chart(&self.charts.swap, Metric::Swap, cx)
                            .line(self.data(Metric::Swap), t.indigo, "Swap")
                            .unit(Unit::Bytes)
                            .y(if swap_total > 0. {
                                YScale::Fixed(0., swap_total)
                            } else {
                                YScale::Zero
                            })
                            .fill()
                            .height(px(RAIL_CHART_HEIGHT)),
                    )
                    .child(legend(self.width_s, cx))
                    .child(caption(
                        "Swap is memory moved out of RAM, to zram or disk. A sustained rise alongside pressure \
                         matters more than a nonzero balance.",
                        cx,
                    )),
            )
            .child(
                panel(IconName::Thermometer, "Thermals", self.expand(Metric::CpuTemp), cx)
                    .child(
                        self.chart(&self.charts.thermals, Metric::CpuTemp, cx)
                            .line(self.data(Metric::CpuTemp), t.orange, "CPU")
                            .line(self.data(Metric::GpuTemp), t.red, "GPU")
                            .unit(Unit::Celsius)
                            .y(YScale::Fit(10.))
                            .height(px(RAIL_CHART_HEIGHT)),
                    )
                    .child(legend(self.width_s, cx)),
            )
            .child(section(IconName::Activity, "Top CPU", cx).child(top_list(top_cpu, "No processes reported", cx)))
            .child(section(IconName::HardDrive, "Top disk", cx).child(top_list(top_disk, "No disk activity", cx)))
    }
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(debug_assertions)]
        if self.stored.is_some()
            && let Some(metric) = self.dev_sheet.take()
        {
            let detail = self.detail(metric, cx);
            window.defer(cx, move |window, cx| {
                crate::ui::widgets::detail_sheet::open(detail, window, cx)
            });
        }
        #[cfg(not(debug_assertions))]
        let _ = window;
        let sample = self.live.read(cx).latest.clone();
        let sample = sample.as_deref();
        let (net_down, net_up) = sample.and_then(|s| s.net.as_ref()).map_or((0., 0.), |net| {
            net.interfaces
                .iter()
                .filter(|(name, _)| name.as_str() != "lo")
                .fold((0., 0.), |(rx, tx), (_, i)| {
                    (rx + i.rx_bps as f64, tx + i.tx_bps as f64)
                })
        });
        let (disk_read, disk_write) = sample.and_then(|s| s.disk.as_ref()).map_or((0., 0.), |disk| {
            disk.iter()
                .filter(|(name, _)| whole_disk(name))
                .fold((0., 0.), |(r, w), (_, d)| {
                    (r + d.read_bps as f64, w + d.write_bps as f64)
                })
        });
        let header = self.header(sample, cx).into_any_element();
        let cx: &App = cx;
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("dashboard-main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(t.gap)
                            .pl(t.page_padding)
                            .py(t.page_padding)
                            .child(header)
                            .child(self.cards(sample, cx))
                            .child(self.pressure_panel(cx))
                            .child(self.cpu_panel(sample, cx))
                            .child(self.io_panel(
                                IconName::Globe,
                                "Network",
                                &self.charts.net,
                                [
                                    (Metric::NetDown, "Download", t.green, IconName::ArrowDown, net_down),
                                    (Metric::NetUp, "Upload", t.red, IconName::ArrowUp, net_up),
                                ],
                                cx,
                            ))
                            .child(self.io_panel(
                                IconName::HardDrive,
                                "Disk",
                                &self.charts.disk,
                                [
                                    (Metric::DiskRead, "Read", t.blue, IconName::ArrowDown, disk_read),
                                    (Metric::DiskWrite, "Write", t.orange, IconName::ArrowUp, disk_write),
                                ],
                                cx,
                            )),
                    ),
            )
            .child(
                div()
                    .id("dashboard-rail")
                    .flex_shrink_0()
                    .w(t.rail_width + t.gap + t.page_padding)
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .pl(t.gap)
                            .pr(t.page_padding)
                            .py(t.page_padding)
                            .child(self.rail(sample, cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_disks_count_towards_disk_traffic() {
        for disk in ["nvme0n1", "nvme1n1", "sda", "vdb", "mmcblk0"] {
            assert!(whole_disk(disk), "{disk}");
        }
        for part in [
            "nvme0n1p2",
            "sda1",
            "vdb3",
            "mmcblk0p1",
            "loop0",
            "zram0",
            "ram1",
            "sr0",
            "dm-0",
            "md127",
        ] {
            assert!(!whole_disk(part), "{part}");
        }
    }

    #[test]
    fn interfaces_and_devices_resolve_from_the_known_paths() {
        let known = [
            "/net/interfaces/lo/rx_bps",
            "/net/interfaces/eth0/rx_bps",
            "/disk/nvme0n1/read_bps",
            "/disk/nvme0n1p1/read_bps",
            "/gpu/devices/0000:01:00.0/temp_c",
            "/gpu/devices/0000:7a:00.0/temp_c",
        ];
        assert_eq!(Metric::NetDown.source(known).paths, ["/net/interfaces/eth0/rx_bps"]);
        assert_eq!(Metric::DiskRead.source(known).paths, ["/disk/nvme0n1/read_bps"]);
        let gpu = Metric::GpuTemp.source(known);
        assert_eq!(
            (gpu.paths.len(), gpu.combine),
            (2, Combine::Max),
            "the hottest device wins"
        );
    }
}
