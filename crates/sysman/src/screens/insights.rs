use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use rusqlite::{Connection, params};
use schema::processes::Process;
use schema::series::escape;

use crate::charts::format::{Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale, clock};
use crate::data::desktop::IconIndex;
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::insights::{
    self, DISK_WINDOW_S, Event, GPU_WINDOW_S, Growth, Insight, Kind, Minute, OOM_WINDOW_S, PRESSURE_WINDOW_S,
    ProcHistory, Severity, Target, band_label, short_duration,
};
use crate::data::live::Live;
use crate::data::pressure::{self, Band, CRITICAL, WARNING};
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source};
use crate::screens::processes::Icons;
use crate::ui::shell::{Focus, Tab, navigate};
use crate::ui::theme::{Theme, rem_px, theme};
use crate::ui::widgets::app_icon::app_icon;
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::panel::{heading, section_with};
use crate::ui::widgets::range::{Range, range_picker_of, remember, remembered};
use crate::ui::widgets::segmented::segmented;
use crate::ui::widgets::text::{footnote, proportion};

const WINDOWS: [Range; 4] = [Range::H1, Range::H6, Range::H24, Range::D7];
/// Rules re-read stored history this often; the pressure chart is topped up from the live rings in between.
const RELOAD_S: f64 = 30.;
const CONSUMERS_RELOAD_S: f64 = 60.;
const PRESSURE_BUCKETS: u32 = 360;
const SWAP_SPAN_S: i64 = 1200;
const CALLOUTS: usize = 2;
const TOP_ROWS: usize = 10;
const EVENT_ROWS: usize = 5;
const CHART_HEIGHT: f32 = 150.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    AvgMemory,
    PeakMemory,
    Cpu,
    DiskIo,
    Network,
}

impl Metric {
    const ALL: [Metric; 5] = [
        Self::AvgMemory,
        Self::PeakMemory,
        Self::Cpu,
        Self::DiskIo,
        Self::Network,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::AvgMemory => "Average memory",
            Self::PeakMemory => "Peak memory",
            Self::Cpu => "CPU",
            Self::DiskIo => "Disk I/O",
            Self::Network => "Network",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::AvgMemory | Self::PeakMemory => "Heaviest memory users over the selected window.",
            Self::Cpu => "Highest average CPU use over the selected window, where one core is 100%.",
            Self::DiskIo => "Highest disk read and write over the selected window.",
            Self::Network => "Highest network throughput right now; per-process network is not kept in history.",
        }
    }
}

/// One command over the window: concurrent instances summed per minute, then averaged over the minutes it ran.
#[derive(Debug, Clone, PartialEq)]
struct Consumer {
    comm: String,
    cmdline: Option<String>,
    /// Most instances running at once.
    processes: u32,
    avg_rss: Option<f64>,
    peak_rss: Option<f64>,
    cpu: Option<f64>,
    peak_cpu: Option<f64>,
    read: Option<f64>,
    write: Option<f64>,
    /// Network from the live table: `(down, up)` bytes per second.
    net: Option<(f64, f64)>,
    samples: u64,
}

fn consumers(conn: &Connection, from: i64, to: i64) -> Result<Vec<Consumer>> {
    let mut stmt = conn.prepare_cached(
        "WITH m AS (
           SELECT p.comm AS comm, max(p.cmdline) AS cmdline, count(*) AS procs, sum(r.rss_avg) AS rss,
                  sum(r.rss_max) AS rss_max, sum(r.cpu_avg) AS cpu, sum(r.read_bps_avg) AS rd,
                  sum(r.write_bps_avg) AS wr, sum(r.count) AS n
           FROM procs p JOIN proc_rollup_1m r ON r.proc_id = p.id AND r.t >= ?1 AND r.t < ?2
           WHERE p.last_t >= ?1
           GROUP BY p.comm, r.t)
         SELECT comm, max(cmdline), max(procs), avg(rss), max(rss_max), avg(cpu), max(cpu), avg(rd), avg(wr), sum(n)
         FROM m GROUP BY comm",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok(Consumer {
            comm: r.get(0)?,
            cmdline: r.get(1)?,
            processes: r.get(2)?,
            avg_rss: r.get(3)?,
            peak_rss: r.get(4)?,
            cpu: r.get(5)?,
            peak_cpu: r.get(6)?,
            read: r.get(7)?,
            write: r.get(8)?,
            net: None,
            samples: r.get::<_, i64>(9)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The live table grouped by command, for the one metric history does not keep.
fn net_consumers(rows: &[Process]) -> Vec<Consumer> {
    let mut by_comm: BTreeMap<&str, Consumer> = BTreeMap::new();
    for p in rows {
        let (Some(rx), Some(tx)) = (p.net_rx_bps, p.net_tx_bps) else {
            continue;
        };
        let c = by_comm.entry(&p.comm).or_insert_with(|| Consumer {
            comm: p.comm.clone(),
            cmdline: Some(p.cmdline.clone()),
            processes: 0,
            avg_rss: None,
            peak_rss: None,
            cpu: None,
            peak_cpu: None,
            read: None,
            write: None,
            net: Some((0., 0.)),
            samples: 1,
        });
        c.processes += 1;
        if let Some((down, up)) = &mut c.net {
            *down += rx as f64;
            *up += tx as f64;
        }
    }
    by_comm.into_values().collect()
}

fn value(c: &Consumer, metric: Metric) -> Option<f64> {
    match metric {
        Metric::AvgMemory => c.avg_rss,
        Metric::PeakMemory => c.peak_rss,
        Metric::Cpu => c.cpu,
        Metric::DiskIo => match (c.read, c.write) {
            (None, None) => None,
            (r, w) => Some(r.unwrap_or(0.) + w.unwrap_or(0.)),
        },
        Metric::Network => c.net.map(|(down, up)| down + up),
    }
}

/// The top consumers by `metric`, largest first; commands with nothing to rank are left out, not shown as zero.
fn rank(all: &[Consumer], metric: Metric) -> Vec<(&Consumer, f64)> {
    let mut out: Vec<(&Consumer, f64)> = all
        .iter()
        .filter_map(|c| Some((c, value(c, metric).filter(|v| *v > 0.)?)))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.comm.cmp(&b.0.comm)));
    out.truncate(TOP_ROWS);
    out
}

fn dash(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map_or("–".into(), f)
}

fn rate(v: f64) -> String {
    format!("{}/s", bytes(v))
}

fn primary(v: f64, metric: Metric) -> String {
    match metric {
        Metric::AvgMemory | Metric::PeakMemory => bytes(v),
        Metric::Cpu => format!("{v:.1}%"),
        Metric::DiskIo | Metric::Network => rate(v),
    }
}

fn caption_line(c: &Consumer, metric: Metric) -> String {
    let cpu = dash(c.cpu, |v| format!("{v:.1}% CPU"));
    let samples = format!("{} samples", c.samples);
    let mut parts = match metric {
        Metric::AvgMemory => vec![format!("peak {}", dash(c.peak_rss, bytes)), cpu, samples],
        Metric::PeakMemory => vec![format!("avg {}", dash(c.avg_rss, bytes)), cpu, samples],
        Metric::Cpu => vec![
            format!("peak {}", dash(c.peak_cpu, |v| format!("{v:.0}%"))),
            format!("avg {} memory", dash(c.avg_rss, bytes)),
            samples,
        ],
        Metric::DiskIo => vec![
            format!("read {}", dash(c.read, rate)),
            format!("write {}", dash(c.write, rate)),
            samples,
        ],
        Metric::Network => {
            let (down, up) = c.net.unwrap_or_default();
            vec![format!("down {}", rate(down)), format!("up {}", rate(up)), "now".into()]
        }
    };
    if c.processes > 1 {
        parts.push(format!("{} processes", c.processes));
    }
    parts.join(" · ")
}

/// Stored minutes of every process seen in the window, oldest first.
fn proc_histories(conn: &Connection, from: i64) -> Result<Vec<ProcHistory>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.id, p.pid, p.start_time, p.comm, p.cmdline, r.t, r.rss_avg, r.cpu_avg
         FROM procs p JOIN proc_rollup_1m r ON r.proc_id = p.id AND r.t >= ?1
         WHERE p.last_t >= ?1 ORDER BY p.id, r.t",
    )?;
    let mut rows = stmt.query([from])?;
    let mut out: Vec<(i64, ProcHistory)> = Vec::new();
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if out.last().is_none_or(|(last, _)| *last != id) {
            out.push((
                id,
                ProcHistory {
                    pid: r.get(1)?,
                    start_time: r.get::<_, i64>(2)? as u64,
                    comm: r.get(3)?,
                    cmdline: r.get(4)?,
                    minutes: Vec::new(),
                },
            ));
        }
        let minute = Minute {
            t: r.get::<_, i64>(5)? as f64,
            rss: r.get(6)?,
            cpu: r.get(7)?,
        };
        out.last_mut().expect("pushed above").1.minutes.push(minute);
    }
    Ok(out.into_iter().map(|(_, p)| p).collect())
}

/// Recent stored history the rules read; re-read every `RELOAD_S`.
struct Recent {
    swap: Vec<(f64, f64)>,
    oom: Vec<(f64, f64)>,
    pressure: Vec<Bucket>,
    fs: Vec<(String, Vec<(f64, f64)>)>,
    procs: Vec<ProcHistory>,
}

fn pressure_source() -> Source {
    Source::new(
        vec!["/pressure/memory/some/avg10".into(), "/memory/swap_in_pages_ps".into()],
        Combine::Pressure,
    )
}

fn load_recent(now: f64, mounts: Vec<String>) -> Result<Recent> {
    let history = History::open(&db_path())?;
    let to = now as i64 + 1;
    let points = |path: &str, span: i64, buckets: u32, f: fn(&Bucket) -> f64| -> Result<Vec<(f64, f64)>> {
        Ok(history
            .series(path, to - span, to, buckets)?
            .iter()
            .map(|b| (b.t as f64, f(b)))
            .collect())
    };
    let fs = mounts
        .into_iter()
        .map(|m| {
            let path = format!("/fs/{}/used_pct", escape(&m));
            Ok((m, points(&path, DISK_WINDOW_S as i64, 60, |b| b.avg)?))
        })
        .collect::<Result<_>>()?;
    Ok(Recent {
        swap: points("/memory/swap_used_bytes", SWAP_SPAN_S, 120, |b| b.avg)?,
        oom: points("/memory/oom_kills_total", OOM_WINDOW_S as i64 + 600, 140, |b| b.max)?,
        pressure: pressure_source().query(&history, to - PRESSURE_WINDOW_S as i64, to, PRESSURE_BUCKETS)?,
        fs,
        procs: proc_histories(history.conn(), to - insights::GROWTH_WINDOW_S as i64)?,
    })
}

fn load_consumers(window: Range, now: f64) -> Result<Vec<Consumer>> {
    let history = History::open(&db_path())?;
    let to = now as i64 + 1;
    consumers(history.conn(), to - window.span_s(), to)
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

pub struct Insights {
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    icons: Arc<IconIndex>,
    window: Range,
    metric: Metric,
    recent: Option<Recent>,
    loading: bool,
    attempt: Option<f64>,
    error: Option<SharedString>,
    consumers: Option<(Range, Vec<Consumer>)>,
    consumers_loading: bool,
    consumers_attempt: Option<(Range, f64)>,
    /// Per GPU slot, `(t, throttle reasons)`: only the live sample carries them.
    gpu: HashMap<String, VecDeque<(f64, Vec<String>)>>,
    gpu_names: HashMap<String, String>,
    callouts: Vec<Insight>,
    events: Vec<Event>,
    board: Vec<Growth>,
    pressure: Rc<[Bucket]>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    chart: Rc<RefCell<ChartState>>,
}

impl Insights {
    pub fn new(live: Entity<Live>, feed: Entity<ProcessFeed>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        cx.observe(&feed, |_, _, cx| cx.notify()).detach();
        let icons = match cx.try_global::<Icons>() {
            Some(icons) => icons.0.clone(),
            None => {
                let task = cx.background_executor().spawn(async { IconIndex::load() });
                cx.spawn(async move |this, cx| {
                    let icons = Arc::new(task.await);
                    cx.update(|cx| cx.set_global(Icons(icons.clone())));
                    this.update(cx, |this, cx| {
                        this.icons = icons;
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
                Arc::default()
            }
        };
        let window = Some(remembered(Tab::Insights, cx))
            .filter(|r| WINDOWS.contains(r))
            .unwrap_or(Range::H1);
        #[cfg(debug_assertions)]
        let metric = std::env::var("SYSMAN_DEV_METRIC")
            .ok()
            .and_then(|name| Metric::ALL.into_iter().find(|m| m.label().eq_ignore_ascii_case(&name)))
            .unwrap_or(Metric::AvgMemory);
        #[cfg(not(debug_assertions))]
        let metric = Metric::AvgMemory;
        let mut this = Self {
            live,
            feed,
            icons,
            window,
            metric,
            recent: None,
            loading: false,
            attempt: None,
            error: None,
            consumers: None,
            consumers_loading: false,
            consumers_attempt: None,
            gpu: HashMap::new(),
            gpu_names: HashMap::new(),
            callouts: Vec::new(),
            events: Vec::new(),
            board: Vec::new(),
            pressure: Rc::from(Vec::new()),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            chart: ChartState::new(),
        };
        this.refresh(cx);
        this
    }

    /// The rules and the consumer list for the current window have both been read.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        self.live.read(cx).latest.is_some()
            && self.recent.is_some()
            && !self.loading
            && !self.consumers_loading
            && self.consumers.as_ref().is_some_and(|(w, _)| *w == self.window)
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn set_window(&mut self, window: Range, cx: &mut Context<Self>) {
        if window != self.window {
            self.window = window;
            remember(Tab::Insights, window, cx);
            self.refresh(cx);
        }
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        self.metric = metric;
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        self.observe_gpu(now, cx);
        if self.attempt.is_none_or(|at| now - at > RELOAD_S) && !self.loading {
            self.load(now, cx);
        }
        let stale = self
            .consumers_attempt
            .is_none_or(|(w, at)| w != self.window || now - at > CONSUMERS_RELOAD_S);
        if stale && !self.consumers_loading {
            self.load_consumers(now, cx);
        }
        self.evaluate(now, cx);
        cx.notify();
    }

    fn observe_gpu(&mut self, now: f64, cx: &App) {
        let live = self.live.read(cx);
        if self.gpu_names.is_empty()
            && let Some(meta) = live
                .meta
                .as_ref()
                .and_then(|m| m.modules.get("gpu")?.info.clone())
                .and_then(|info| serde_json::from_value::<schema::gpu::Meta>(info).ok())
        {
            self.gpu_names = meta
                .devices
                .into_iter()
                .filter_map(|(slot, d)| Some((slot, d.name?)))
                .collect();
        }
        let Some(sample) = live.latest.as_ref() else {
            return;
        };
        for (slot, device) in sample.gpu.iter().flat_map(|g| &g.devices) {
            let Some(reasons) = &device.throttle_reasons else {
                continue;
            };
            let ring = self.gpu.entry(slot.clone()).or_default();
            if ring.back().is_some_and(|(t, _)| *t >= sample.t) {
                continue;
            }
            while ring.front().is_some_and(|(t, _)| *t < now - GPU_WINDOW_S) {
                ring.pop_front();
            }
            ring.push_back((sample.t, reasons.clone()));
        }
    }

    fn load(&mut self, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some(now);
        let mounts: Vec<String> = self
            .live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.fs.as_ref())
            .map(|fs| fs.keys().cloned().collect())
            .unwrap_or_default();
        let task = cx.background_executor().spawn(async move { load_recent(now, mounts) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(recent) => {
                        this.error = None;
                        this.recent = Some(recent);
                    }
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                let now = this.now(cx);
                this.evaluate(now, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_consumers(&mut self, now: f64, cx: &mut Context<Self>) {
        self.consumers_loading = true;
        let window = self.window;
        self.consumers_attempt = Some((window, now));
        let task = cx
            .background_executor()
            .spawn(async move { load_consumers(window, now) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.consumers_loading = false;
                match result {
                    Ok(list) => this.consumers = Some((window, list)),
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Runs every rule over the stored history, the live rings and the GPU readings.
    fn evaluate(&mut self, now: f64, cx: &App) {
        let live = self.live.read(cx);
        let to = now as i64;
        let from = to - PRESSURE_WINDOW_S as i64;
        self.width_s = bucket_width(from, to + 1, PRESSURE_BUCKETS);
        let stored = self.recent.as_ref().map_or(&[][..], |r| r.pressure.as_slice());
        self.pressure = pressure_source().top_up(stored, live, from, self.width_s).into();
        self.span = (from as f64, now);
        self.version += 1;
        self.events = insights::pressure_events(&self.pressure, self.width_s as f64);

        let ram = live
            .latest
            .as_ref()
            .and_then(|s| s.memory.as_ref())
            .map_or(0., |m| m.total_bytes as f64);
        let gpus: Vec<(String, insights::Readings)> = self
            .gpu
            .iter()
            .map(|(slot, ring)| {
                let name = self
                    .gpu_names
                    .get(slot)
                    .cloned()
                    .unwrap_or_else(|| format!("GPU {slot}"));
                (name, ring.iter().cloned().collect())
            })
            .collect();
        let mut all = Vec::new();
        if let Some(r) = &self.recent {
            self.board = insights::growth(&r.procs, now);
            all.extend(insights::oom(&r.oom, now));
            all.extend(insights::swap(&r.swap, ram, now));
            all.extend(insights::disk_full(&r.fs, now));
            all.extend(insights::growth_insight(&self.board, ram));
            let running: Option<HashSet<(u32, u64)>> =
                (self.feed.read(cx).table()).map(|t| t.rows.iter().map(|p| (p.pid, p.start_time)).collect());
            all.extend(insights::busy_cpu(&r.procs, now, running.as_ref()));
        }
        all.extend(insights::pressure_insight(&self.events, now));
        all.extend(insights::gpu_throttle(&gpus, now));
        self.callouts = insights::headline(all, CALLOUTS);
    }
}

fn severity_colour(s: Severity, t: &Theme) -> Hsla {
    match s {
        Severity::Critical => t.red,
        Severity::Warning => t.orange,
        Severity::Advisory => t.blue,
    }
}

fn kind_icon(kind: Kind) -> IconName {
    match kind {
        Kind::Swap | Kind::DiskFull => IconName::HardDrive,
        Kind::Cpu => IconName::Gauge,
        Kind::Growth => IconName::TrendingUp,
        Kind::Pressure => IconName::Activity,
        Kind::Oom => IconName::Skull,
        Kind::GpuThrottle => IconName::Thermometer,
    }
}

fn band_colour(band: Band, t: &Theme) -> Hsla {
    match band {
        Band::Normal => t.green,
        Band::Warning => t.orange,
        Band::Critical => t.red,
    }
}

/// Where a callout leads: the process or disk it names, else the tab that charts its evidence.
fn destination(i: &Insight) -> (Tab, Option<Focus>) {
    match &i.target {
        Target::Process { comm, pid, start_time } => (
            Tab::Processes,
            Some(Focus::Process {
                comm: comm.clone(),
                instance: Some((*pid, *start_time)),
            }),
        ),
        Target::Mount(mount) => (Tab::Disk, Some(Focus::Mount(mount.clone()))),
        Target::Gpu => (Tab::Gpu, None),
        Target::None => (Tab::Dashboard, None),
    }
}

impl Insights {
    fn callout(&self, i: usize, insight: &Insight, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let tint = severity_colour(insight.severity, t);
        let (tab, focus) = destination(insight);
        div()
            .id(("callout", i))
            .flex()
            .flex_row()
            .items_start()
            .gap(rem_px(12.))
            .p(rem_px(14.))
            .rounded(t.panel_radius)
            .bg(tint.opacity(0.08))
            .border_1()
            .border_color(tint.opacity(0.25))
            .cursor_pointer()
            .hover(|d| d.bg(tint.opacity(0.13)))
            .on_click(move |_, _, cx| navigate(tab, focus.clone(), cx))
            .child(icon_tile(kind_icon(insight.kind), tint))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(t.weight_semibold)
                            .child(insight.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .child(insight.detail.clone()),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(t.card_value)
                    .font_weight(t.weight_semibold)
                    .font_features(t.numbers.clone())
                    .text_color(tint)
                    .child(insight.value.clone()),
            )
            .into_any_element()
    }

    fn all_clear(&self, cx: &App) -> AnyElement {
        let t = theme(cx);
        let tint = t.green;
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(rem_px(12.))
            .p(rem_px(14.))
            .rounded(t.panel_radius)
            .bg(tint.opacity(0.08))
            .border_1()
            .border_color(tint.opacity(0.25))
            .child(icon_tile(IconName::CircleCheck, tint))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(t.weight_semibold)
                            .child("Nothing needs attention"),
                    )
                    .child(div().text_size(t.caption).text_color(t.text_secondary).child(
                        "Swap, CPU, memory growth, pressure, OOM kills, GPU throttling and disk space all look \
                             normal over recent history.",
                    )),
            )
            .into_any_element()
    }

    fn happening(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<AnyElement> = if self.callouts.is_empty() {
            vec![self.all_clear(cx)]
        } else {
            let callouts = self.callouts.clone();
            callouts
                .iter()
                .enumerate()
                .map(|(i, insight)| self.callout(i, insight, cx))
                .collect()
        };
        div()
            .flex()
            .flex_col()
            .gap(rem_px(10.))
            .child(heading(IconName::Sparkles, "What's happening", cx))
            .child(div().grid().grid_cols(3).gap(rem_px(10.)).children(cards))
    }

    fn pressure_section(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let gap_s = (self.width_s as f64 * 1.5).max(5.);
        let mut chart = TrendChart::new(self.chart.clone(), self.version, self.span, self.width_s as f64, gap_s)
            .line(self.pressure.clone(), t.orange, "Pressure")
            .fill()
            .unit(Unit::Index)
            .y(YScale::Fixed(0., 100.))
            .rule(WARNING, "Warning", t.orange)
            .rule(CRITICAL, "Critical", t.red)
            .ticks(&pressure::TICKS)
            .height(px(CHART_HEIGHT));
        for e in &self.events {
            // On the plotted mean, so the dot sits on the curve rather than above it at the bucket's peak.
            let at = (e.t - self.width_s as f64 / 2.) as i64;
            let y = self.pressure.iter().find(|b| b.t == at).map_or(e.peak, |b| b.avg);
            chart = chart.marker(e.t, y, band_colour(e.band, t));
        }
        let now = self.span.1;
        let body: AnyElement = if self.events.is_empty() {
            footnote(
                "No pressure events in the last 2 hours. Memory has stayed comfortable.",
                cx,
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .children(self.events.iter().rev().take(EVENT_ROWS).enumerate().map(|(i, e)| {
                    let colour = band_colour(e.band, t);
                    let icon = if e.band == Band::Critical {
                        IconName::Flame
                    } else {
                        IconName::Activity
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(rem_px(12.))
                        .py(rem_px(8.))
                        .when(i > 0, |d| d.border_t_1().border_color(t.hairline))
                        .child(Icon::new(icon).small().text_color(colour))
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .text_size(t.panel_title)
                                        .child(format!("Pressure rose to {}", band_label(e.band))),
                                )
                                .child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .font_features(t.numbers.clone())
                                        .child(format!(
                                            "peak {:.0} · lasted {}",
                                            e.peak,
                                            short_duration(e.end - e.start)
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .gap(px(2.))
                                .font_features(t.numbers.clone())
                                .child(div().text_size(t.caption).child(clock(e.t, "%H:%M:%S")))
                                .child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .child(format!("{} ago", short_duration((now - e.t).max(0.)))),
                                ),
                        )
                }))
                .into_any_element()
        };
        section_with(
            IconName::Activity,
            "Pressure events",
            "The last 2 hours of memory pressure, with each spike marked.",
            cx,
        )
        .child(chart)
        .child(body)
    }

    fn growth_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<AnyElement> = self
            .board
            .iter()
            .enumerate()
            .map(|(i, g)| self.growth_card(i, g, cx))
            .collect();
        section_with(
            IconName::TrendingUp,
            "Memory growth",
            "Processes growing steadily over the last 2 hours, with their growth curves. Growth alone does not \
             prove a leak.",
            cx,
        )
        .child(div().grid().grid_cols(3).gap(rem_px(10.)).children(cards))
    }

    fn growth_card(&self, i: usize, g: &Growth, cx: &App) -> AnyElement {
        let t = theme(cx);
        let data: Rc<[Bucket]> = g
            .points
            .iter()
            .map(|&(t, v)| Bucket {
                t: t as i64,
                min: v,
                avg: v,
                max: v,
            })
            .collect();
        let span = (g.points[0].0, g.points[g.points.len() - 1].0 + 60.);
        let focus = Focus::Process {
            comm: g.comm.clone(),
            instance: Some((g.pid, g.start_time)),
        };
        let stat = |value: String, label: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(t.caption)
                        .font_weight(t.weight_semibold)
                        .font_features(t.numbers.clone())
                        .child(value),
                )
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_secondary)
                        .child(label),
                )
        };
        div()
            .id(("growth", i))
            .flex()
            .flex_col()
            .gap(rem_px(8.))
            .p(rem_px(12.))
            .rounded(t.card_radius)
            .bg(t.orange.opacity(0.06))
            .border_1()
            .border_color(t.orange.opacity(0.2))
            .cursor_pointer()
            .hover(|d| d.bg(t.orange.opacity(0.1)))
            .on_click(move |_, _, cx| navigate(Tab::Processes, Some(focus.clone()), cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(6.))
                    .child(app_icon(
                        &g.comm,
                        g.cmdline.as_deref().unwrap_or(""),
                        &self.icons,
                        18.,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(t.panel_title)
                            .font_weight(t.weight_semibold)
                            .child(g.comm.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px(rem_px(6.))
                            .py(px(1.))
                            .rounded_full()
                            .bg(t.orange.opacity(0.15))
                            .text_color(t.orange)
                            .text_size(t.small_caption)
                            .font_weight(t.weight_medium)
                            .child(format!("{:.0} min observed", g.observed_s / 60.)),
                    ),
            )
            .child(Sparkline::new(("growth-spark", i), data, t.orange, span, 60., 150.).fit())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rem_px(18.))
                    .child(stat(format!("+{}", bytes(g.grew)), "Grew"))
                    .child(stat(format!("~{}/min", bytes(g.slope_bps * 60.)), "Rate"))
                    .child(stat(bytes(g.now_bytes), "Now")),
            )
            .into_any_element()
    }

    fn consumers_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let this2 = this.clone();
        let metric = self.metric;
        let live_rows;
        let list: Vec<(&Consumer, f64)> = if metric == Metric::Network {
            live_rows = self
                .feed
                .read(cx)
                .table()
                .map_or_else(Vec::new, |table| net_consumers(&table.rows));
            rank(&live_rows, metric)
        } else {
            self.consumers
                .as_ref()
                .map_or_else(Vec::new, |(_, all)| rank(all, metric))
        };
        let features = self
            .live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.helper.as_ref())
            .map(|h| h.features.clone())
            .unwrap_or_default();
        let has = |f: &str| features.get(f).copied().unwrap_or(false);
        let hint = match metric {
            Metric::DiskIo if !has("proc_io") => Some("Disk I/O of other users' processes needs the privileged helper"),
            Metric::Network if !has("net") => Some("Per-process network needs the privileged helper"),
            _ => None,
        };
        let t = theme(cx);
        let top = list.first().map_or(1., |(_, v)| *v);
        let window_picker: AnyElement = if metric == Metric::Network {
            div()
                .text_size(t.caption)
                .text_color(t.text_secondary)
                .child("Live")
                .into_any_element()
        } else {
            range_picker_of(
                &WINDOWS,
                self.window,
                move |range, _, cx| {
                    this.update(cx, |this, cx| this.set_window(range, cx)).ok();
                },
                cx,
            )
            .into_any_element()
        };
        let items: Vec<(Metric, &'static str)> = Metric::ALL.iter().map(|&m| (m, m.label())).collect();
        let rows: Vec<AnyElement> = list
            .iter()
            .enumerate()
            .map(|(i, (c, v))| self.consumer_row(i, c, *v / top, metric, cx))
            .collect();
        let empty = if self.consumers_loading && self.consumers.is_none() {
            "Loading…"
        } else if metric == Metric::Network {
            "No per-process network traffic right now."
        } else {
            "No data logged for this window yet."
        };
        section_with(IconName::ChartColumn, "Top consumers", metric.subtitle(), cx)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(12.))
                    .child(window_picker)
                    .child(div().flex_1())
                    .child(segmented(
                        "metric",
                        items.iter().copied(),
                        &metric,
                        move |m, _, cx| {
                            this2.update(cx, |this, cx| this.set_metric(m, cx)).ok();
                        },
                        cx,
                    )),
            )
            .when_some(hint, |d, hint| d.child(helper_hint("insights-enable", hint, cx)))
            .map(|d| {
                if rows.is_empty() {
                    d.child(footnote(empty, cx))
                } else {
                    d.child(div().flex().flex_col().children(rows))
                }
            })
    }

    fn consumer_row(&self, i: usize, c: &Consumer, fraction: f64, metric: Metric, cx: &App) -> AnyElement {
        let t = theme(cx);
        let growing = self.board.iter().any(|g| g.comm == c.comm);
        let focus = Focus::Process {
            comm: c.comm.clone(),
            instance: None,
        };
        div()
            .id(("consumer", i))
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(12.))
            .py(rem_px(7.))
            .cursor_pointer()
            .when(i > 0, |d| d.border_t_1().border_color(t.hairline))
            .hover(|d| d.bg(t.card))
            .on_click(move |_, _, cx| navigate(Tab::Processes, Some(focus.clone()), cx))
            .child(
                div()
                    .w(rem_px(22.))
                    .flex_shrink_0()
                    .text_right()
                    .text_size(t.panel_title)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(format!("{}", i + 1)),
            )
            .child(app_icon(
                &c.comm,
                c.cmdline.as_deref().unwrap_or(""),
                &self.icons,
                22.,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(6.))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(t.panel_title)
                                    .font_weight(t.weight_medium)
                                    .child(c.comm.clone()),
                            )
                            .when(growing, |d| {
                                d.child(Icon::new(IconName::TrendingUp).xsmall().text_color(t.orange))
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(t.panel_title)
                                    .font_weight(t.weight_semibold)
                                    .font_features(t.numbers.clone())
                                    .child(primary(value(c, metric).unwrap_or(0.), metric)),
                            ),
                    )
                    .child(proportion(fraction, if growing { t.orange } else { t.blue }, cx))
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(caption_line(c, metric)),
                    ),
            )
            .into_any_element()
    }
}

fn icon_tile(icon: IconName, tint: Hsla) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .size(rem_px(28.))
        .rounded(rem_px(8.))
        .bg(tint)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).small().text_color(gpui::white()))
}

impl Render for Insights {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        if self.recent.is_none() && self.error.is_none() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_size(t.caption)
                .text_color(t.text_secondary)
                .child("Loading insights…")
                .into_any_element();
        }
        let page_padding = t.page_padding;
        let error = self.error.clone();
        let caption = t.caption;
        let secondary = t.text_secondary;
        let happening = self.happening(cx).into_any_element();
        let pressure = self.pressure_section(cx).into_any_element();
        let growth = (!self.board.is_empty()).then(|| self.growth_section(cx).into_any_element());
        let consumers = self.consumers_section(cx).into_any_element();
        div()
            .id("insights")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rem_px(20.))
                    .p(page_padding)
                    .when_some(error, |d, error| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(rem_px(6.))
                                .text_size(caption)
                                .text_color(secondary)
                                .child(Icon::new(IconName::Database).xsmall())
                                .child(format!("{error}. Showing live data only where possible.")),
                        )
                    })
                    .child(happening)
                    .child(pressure)
                    .children(growth)
                    .child(consumers),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("history.db")).unwrap();
        conn.execute_batch(schema::db::SCHEMA).unwrap();
        (dir, conn)
    }

    fn consumer(comm: &str) -> Consumer {
        Consumer {
            comm: comm.into(),
            cmdline: None,
            processes: 1,
            avg_rss: None,
            peak_rss: None,
            cpu: None,
            peak_cpu: None,
            read: None,
            write: None,
            net: None,
            samples: 1,
        }
    }

    #[test]
    fn concurrent_instances_of_a_command_add_up_per_minute() {
        let (_dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 10, 1, 'chrome', '/opt/chrome', 'me', 0, 600);
             INSERT INTO procs VALUES (2, 11, 1, 'chrome', '/opt/chrome --type=renderer', 'me', 0, 600);
             INSERT INTO procs VALUES (3, 12, 1, 'bash', 'bash', 'me', 0, 600);
             INSERT INTO procs VALUES (4, 13, 1, 'old', 'old', 'me', 0, 50);
             INSERT INTO proc_rollup_1m (proc_id, t, cpu_avg, cpu_max, rss_avg, rss_max, read_bps_avg, write_bps_avg, count)
             VALUES (1, 120, 10, 20, 100, 150, NULL, NULL, 30),
                    (2, 120, 30, 40, 300, 350, NULL, NULL, 30),
                    (1, 180, 20, 30, 200, 250, NULL, NULL, 30),
                    (3, 120, 1, 1, 50, 50, 1000, 3000, 30),
                    (4, 40, 99, 99, 9999, 9999, NULL, NULL, 30);",
        )
        .unwrap();
        let mut all = consumers(&conn, 100, 300).unwrap();
        all.sort_by(|a, b| a.comm.cmp(&b.comm));
        assert_eq!(
            all.len(),
            2,
            "a process last seen before the window is not a consumer in it"
        );
        let chrome = &all[1];
        assert_eq!(chrome.comm, "chrome");
        assert_eq!(chrome.processes, 2);
        assert_eq!(chrome.avg_rss, Some(300.), "minute 120 sums to 400, minute 180 is 200");
        assert_eq!(chrome.peak_rss, Some(500.));
        assert_eq!((chrome.cpu, chrome.peak_cpu), (Some(30.), Some(40.)));
        assert_eq!(chrome.samples, 90);
        assert_eq!(
            (chrome.read, chrome.write),
            (None, None),
            "unknown I/O stays unknown, not zero"
        );
        assert_eq!((all[0].read, all[0].write), (Some(1000.), Some(3000.)));
    }

    #[test]
    fn ranking_drops_commands_without_the_metric_and_caps_the_list() {
        let mut list: Vec<Consumer> = (0..15)
            .map(|i| Consumer {
                avg_rss: Some(f64::from(i)),
                ..consumer(&format!("p{i}"))
            })
            .collect();
        list.push(Consumer {
            read: Some(5.),
            ..consumer("io")
        });
        let ranked = rank(&list, Metric::AvgMemory);
        assert_eq!(ranked.len(), TOP_ROWS);
        assert_eq!(ranked[0].0.comm, "p14");
        assert!(ranked.iter().all(|(c, _)| c.comm != "p0"), "zero is not a ranking");
        let io = rank(&list, Metric::DiskIo);
        assert_eq!(
            io.iter().map(|(c, v)| (c.comm.as_str(), *v)).collect::<Vec<_>>(),
            [("io", 5.)]
        );
        assert!(rank(&list, Metric::Network).is_empty());
    }

    #[test]
    fn captions_carry_the_other_measures_and_dash_what_is_missing() {
        let c = Consumer {
            avg_rss: Some(1024. * 1024.),
            peak_rss: Some(2. * 1024. * 1024.),
            cpu: Some(33.66),
            processes: 3,
            samples: 2239,
            ..consumer("x")
        };
        assert_eq!(
            caption_line(&c, Metric::AvgMemory),
            "peak 2.0 MB · 33.7% CPU · 2239 samples · 3 processes"
        );
        assert_eq!(
            caption_line(&c, Metric::DiskIo),
            "read – · write – · 2239 samples · 3 processes"
        );
    }

    #[test]
    fn network_groups_the_live_table_by_command_and_skips_unknown_rates() {
        let row = |pid, comm: &str, net: Option<(u64, u64)>| Process {
            pid,
            ppid: 1,
            comm: comm.into(),
            state: "S".into(),
            threads: 1,
            rss_bytes: 1,
            cpu_pct: 0.,
            read_bps: None,
            write_bps: None,
            user: None,
            fds: None,
            net_rx_bps: net.map(|n| n.0),
            net_tx_bps: net.map(|n| n.1),
            cmdline: comm.into(),
            start_time: 1,
            ..Default::default()
        };
        let rows = [
            row(1, "firefox", Some((100, 10))),
            row(2, "firefox", Some((50, 5))),
            row(3, "sshd", None),
        ];
        let net = net_consumers(&rows);
        assert_eq!(net.len(), 1);
        assert_eq!((net[0].net, net[0].processes), (Some((150., 15.)), 2));
        assert_eq!(rank(&net, Metric::Network)[0].1, 165.);
    }

    #[test]
    fn process_histories_group_minutes_by_instance_in_time_order() {
        let (_dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 10, 5, 'a', NULL, NULL, 0, 600);
             INSERT INTO procs VALUES (2, 10, 9, 'a', 'a --x', NULL, 0, 600);
             INSERT INTO proc_rollup_1m (proc_id, t, cpu_avg, rss_avg, count)
             VALUES (1, 180, 1, 100, 30), (1, 120, 2, NULL, 30), (2, 120, 3, 300, 30), (2, 0, 9, 9, 30);",
        )
        .unwrap();
        let h = proc_histories(&conn, 100).unwrap();
        assert_eq!(h.len(), 2, "a reused pid is a separate instance");
        assert_eq!((h[0].pid, h[0].start_time), (10, 5));
        assert_eq!(h[0].minutes.iter().map(|m| m.t).collect::<Vec<_>>(), [120., 180.]);
        assert_eq!(h[0].minutes[0].rss, None);
        assert_eq!(h[1].minutes.len(), 1, "minutes before the window are not read");
        assert_eq!(h[1].cmdline.as_deref(), Some("a --x"));
    }

    #[test]
    fn callouts_lead_to_the_process_or_disk_they_name() {
        let mk = |target| Insight {
            kind: Kind::Cpu,
            severity: Severity::Advisory,
            title: String::new(),
            value: String::new(),
            detail: String::new(),
            target,
        };
        let (tab, focus) = destination(&mk(Target::Process {
            comm: "x".into(),
            pid: 4,
            start_time: 9,
        }));
        assert_eq!(tab, Tab::Processes);
        assert_eq!(
            focus,
            Some(Focus::Process {
                comm: "x".into(),
                instance: Some((4, 9))
            })
        );
        assert_eq!(
            destination(&mk(Target::Mount("/".into()))),
            (Tab::Disk, Some(Focus::Mount("/".into())))
        );
        assert_eq!(destination(&mk(Target::None)), (Tab::Dashboard, None));
    }
}
