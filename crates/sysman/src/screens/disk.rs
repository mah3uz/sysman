use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
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
use schema::fs::{Filesystem, Fs};
use schema::hwmon::HwmonMeta;
use schema::processes::Process;
use schema::series::escape;
use schema::storage::{Drive, Smart, StorageMeta};

use crate::charts::format::{Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::helper::{self, DiskHealth, Wear};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source};
use crate::screens::dashboard::{readout, top_list, whole_disk};
use crate::screens::disk_map::DiskMap;
use crate::ui::shell::{Focus, Tab, take_focus};
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::card::card;
use crate::ui::widgets::detail_sheet::{Detail, Series, opener};
use crate::ui::widgets::hairline;
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::{caption_label, panel, section};
use crate::ui::widgets::range::{Range, range_picker_of, remember, remembered};
use crate::ui::widgets::segmented::segmented;
use crate::ui::widgets::text::{caption, key_value};

const BUCKETS: u32 = 120;
/// Stored history is re-read this often; the live rings cover the newest 30 minutes in between.
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 150.;
const SMALL_CHART_HEIGHT: f32 = 110.;
const RAIL_CHART_HEIGHT: f32 = 120.;
const TOP_ROWS: usize = 8;
const RANGES: [Range; 4] = [Range::H1, Range::H6, Range::H24, Range::D7];
const THROUGHPUT_CAPTION: &str = "Device traffic as the kernel counts it, partitions included. Filesystem caching, \
                                  metadata and swap are device traffic too, so per-process figures below may not add up \
                                  to this.";
const LATENCY_CAPTION: &str = "Service time is the device's average wait per completed operation. Utilization is the \
                               share of time the device had I/O in flight.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Metric {
    Read,
    Write,
    ReadIops,
    WriteIops,
    Iops,
    ReadAwait,
    WriteAwait,
    Queue,
    Free,
    Used,
}

impl Metric {
    const ALL: [Metric; 10] = [
        Self::Read,
        Self::Write,
        Self::ReadIops,
        Self::WriteIops,
        Self::Iops,
        Self::ReadAwait,
        Self::WriteAwait,
        Self::Queue,
        Self::Free,
        Self::Used,
    ];

    /// `None` for a filesystem metric when the disk holds no mounted filesystem.
    fn source(self, disk: &str, mount: Option<&str>) -> Option<Source> {
        let d = |field: &str| format!("/disk/{}/{field}", escape(disk));
        let f = |field: &str| mount.map(|m| format!("/fs/{}/{field}", escape(m)));
        let paths = match self {
            Self::Read => vec![d("read_bps")],
            Self::Write => vec![d("write_bps")],
            Self::ReadIops => vec![d("read_iops")],
            Self::WriteIops => vec![d("write_iops")],
            Self::Iops => vec![d("read_iops"), d("write_iops")],
            Self::ReadAwait => vec![d("read_await_ms")],
            Self::WriteAwait => vec![d("write_await_ms")],
            Self::Queue => vec![d("queue_depth")],
            Self::Free => vec![f("avail_bytes")?],
            Self::Used => vec![f("used_bytes")?],
        };
        Some(Source::new(paths, Combine::Sum))
    }

    fn module(self) -> &'static str {
        match self {
            Self::Free | Self::Used => "fs",
            _ => "disk",
        }
    }
}

/// A disk the picker offers: whole disks only; device-mapper devices report as `dm/<name>`.
fn pickable(name: &str) -> bool {
    whole_disk(name) && !name.contains('/')
}

/// The disk to open on: the busiest by utilisation, then by traffic.
fn busiest(disk: &schema::disk::Disk) -> Option<&str> {
    disk.iter()
        .filter(|(name, _)| pickable(name))
        .max_by(|a, b| {
            a.1.busy_pct
                .total_cmp(&b.1.busy_pct)
                .then((a.1.read_bps + a.1.write_bps).cmp(&(b.1.read_bps + b.1.write_bps)))
                .then(b.0.cmp(a.0))
        })
        .map(|(name, _)| name.as_str())
}

/// A block device stacked on a disk, such as a dm-crypt or LVM node, with its `dm/name`.
#[derive(Debug, Clone, PartialEq)]
struct Holder {
    kernel: String,
    name: Option<String>,
}

/// Every device stacked on `disk` or its partitions, transitively (LVM on LUKS), from sysfs.
fn holders(disk: &str) -> Vec<Holder> {
    let root = Path::new("/sys/block");
    let mut queue = vec![root.join(disk)];
    if let Ok(entries) = std::fs::read_dir(root.join(disk)) {
        queue.extend(
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(disk))
                .map(|e| e.path()),
        );
    }
    let mut out: Vec<Holder> = Vec::new();
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(dir.join("holders")) else {
            continue;
        };
        for entry in entries.flatten() {
            let kernel = entry.file_name().to_string_lossy().into_owned();
            if out.iter().any(|h| h.kernel == kernel) {
                continue;
            }
            let name = std::fs::read_to_string(root.join(&kernel).join("dm/name"))
                .ok()
                .map(|s| s.trim().to_owned());
            queue.push(root.join(&kernel));
            out.push(Holder { kernel, name });
        }
    }
    out
}

/// `device` (`/dev/nvme1n1p2`, `/dev/mapper/root`, `/dev/dm-0`) lives on `disk` itself, a partition of it, or a
/// device stacked on it.
fn on_disk(disk: &str, device: &str, holders: &[Holder]) -> bool {
    let Some(dev) = device.strip_prefix("/dev/") else {
        return false;
    };
    if let Some(rest) = dev.strip_prefix(disk) {
        // nvme1n1 partitions are nvme1n1p2; sda partitions are sda2, so sdaa is another disk.
        let number = if disk.ends_with(|c: char| c.is_ascii_digit()) {
            rest.strip_prefix('p')
        } else {
            Some(rest)
        };
        if rest.is_empty() || number.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
            return true;
        }
    }
    holders
        .iter()
        .any(|h| dev == h.kernel || (h.name.is_some() && dev.strip_prefix("mapper/") == h.name.as_deref()))
}

/// Filesystems on `disk`, largest first, so the first is the one the FREE SPACE card shows.
fn mounts_on<'a>(disk: &str, fs: &'a Fs, holders: &[Holder]) -> Vec<(&'a str, &'a Filesystem)> {
    let mut out: Vec<(&str, &Filesystem)> = fs
        .iter()
        .filter(|(_, f)| on_disk(disk, &f.device, holders))
        .map(|(m, f)| (m.as_str(), f))
        .collect();
    out.sort_by(|a, b| b.1.total_bytes.cmp(&a.1.total_bytes).then(a.0.cmp(b.0)));
    out
}

/// hwmon names NVMe sensors by controller (`nvme/nvme1`), the disk by namespace (`nvme1n1`).
fn nvme_chip(disk: &str) -> Option<String> {
    let rest = disk.strip_prefix("nvme")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then(|| format!("nvme/nvme{digits}"))
}

fn protocol(disk: &str) -> Option<&'static str> {
    [("nvme", "NVMe"), ("sd", "SATA"), ("vd", "VirtIO"), ("mmcblk", "MMC")]
        .into_iter()
        .find(|(prefix, _)| disk.starts_with(prefix))
        .map(|(_, label)| label)
}

/// Grey below 60%, orange 60–89, red from 90.
fn busy_colour(pct: f64, t: &Theme) -> Hsla {
    if pct >= 90. {
        t.red
    } else if pct >= 60. {
        t.orange
    } else {
        t.text_secondary
    }
}

/// Red at the drive's critical limit, orange within 10 °C of it.
fn temp_colour(celsius: f64, crit: Option<f64>, t: &Theme) -> Hsla {
    match crit {
        Some(crit) if celsius >= crit => t.red,
        Some(crit) if celsius >= crit - 10. => t.orange,
        Some(_) => t.green,
        None => t.text,
    }
}

type Key = (Range, String, Option<String>);

/// History read for one range, disk and filesystem; the newest end is replaced by live rings on every tick.
struct Stored {
    key: Key,
    buckets: HashMap<Metric, Vec<Bucket>>,
}

fn load(key: Key, to: i64) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let from = to - key.0.span_s();
    let mut buckets = HashMap::new();
    for metric in Metric::ALL {
        if let Some(source) = metric.source(&key.1, key.2.as_deref()) {
            buckets.insert(metric, source.query(&history, from, to, BUCKETS)?);
        }
    }
    Ok(Stored { key, buckets })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Overview,
    Map,
}

#[derive(Default)]
struct Charts {
    throughput: Rc<RefCell<ChartState>>,
    iops: Rc<RefCell<ChartState>>,
    latency: Rc<RefCell<ChartState>>,
    queue: Rc<RefCell<ChartState>>,
    used: Rc<RefCell<ChartState>>,
}

pub struct Disk {
    me: WeakEntity<Self>,
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    view: View,
    range: Range,
    selected: Option<String>,
    holders: HashMap<String, Vec<Holder>>,
    stored: Option<Stored>,
    loading: bool,
    /// Key and time of the last history read, so a failing read is not retried every tick.
    attempt: Option<(Key, f64)>,
    error: Option<SharedString>,
    merged: HashMap<Metric, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: Charts,
    drives: Option<StorageMeta>,
    sensors: Option<HwmonMeta>,
    /// Made on first visit, so the scan starts only when the map is wanted.
    map: Option<Entity<DiskMap>>,
    /// A mount point another screen asked to show; its disk is selected once the disks are known.
    focus: Option<String>,
}

fn initial_view() -> View {
    #[cfg(debug_assertions)]
    if std::env::var("SYSMAN_DEV_DISK_VIEW").is_ok_and(|v| v.eq_ignore_ascii_case("map")) {
        return View::Map;
    }
    View::Overview
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

impl Disk {
    pub fn new(live: Entity<Live>, feed: Entity<ProcessFeed>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        cx.observe(&feed, |_, _, cx| cx.notify()).detach();
        let range = Some(remembered(Tab::Disk, cx))
            .filter(|r| RANGES.contains(r))
            .unwrap_or(Range::H1);
        let mut this = Self {
            me: cx.weak_entity(),
            live,
            feed,
            view: initial_view(),
            range,
            selected: None,
            holders: HashMap::new(),
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            merged: HashMap::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            charts: Charts::default(),
            drives: None,
            sensors: None,
            map: None,
            focus: match take_focus(cx) {
                Some(Focus::Mount(mount)) => Some(mount),
                _ => None,
            },
        };
        this.refresh(cx);
        this
    }

    /// Every module the screen shows is in, and the history read for the current disk and range has returned.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        let live = self.live.read(cx);
        let modules = live
            .latest
            .as_ref()
            .is_some_and(|s| s.disk.is_some() && s.fs.is_some() && s.storage.is_some() && s.hwmon.is_some());
        let key = self.key(cx);
        modules
            && live.meta.is_some()
            && self.feed.read(cx).latest.is_some()
            && !self.loading
            && key.is_some_and(|key| self.attempt.as_ref().is_some_and(|(k, _)| *k == key))
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn mount(&self, disk: &str, cx: &App) -> Option<String> {
        let fs = self.live.read(cx).latest.as_ref()?.fs.as_ref()?;
        let holders = self.holders.get(disk).map_or(&[][..], Vec::as_slice);
        mounts_on(disk, fs, holders).first().map(|(m, _)| (*m).to_owned())
    }

    fn key(&self, cx: &App) -> Option<Key> {
        let disk = self.selected.clone()?;
        let mount = self.mount(&disk, cx);
        Some((self.range, disk, mount))
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Disk, range, cx);
            self.refresh(cx);
        }
    }

    fn select(&mut self, disk: String, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&disk) {
            self.selected = Some(disk);
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let live = self.live.read(cx);
        if let Some(sample) = live.latest.as_ref()
            && let (Some(mount), Some(disks), Some(fs)) = (self.focus.take(), &sample.disk, &sample.fs)
        {
            for disk in disks.keys().filter(|d| whole_disk(d)) {
                let held = self.holders.entry(disk.clone()).or_insert_with(|| holders(disk));
                if mounts_on(disk, fs, held).iter().any(|(m, _)| *m == mount) {
                    self.selected = Some(disk.clone());
                }
            }
        }
        if self.selected.is_none() {
            self.selected = live
                .latest
                .as_ref()
                .and_then(|s| busiest(s.disk.as_ref()?))
                .map(str::to_owned);
        }
        let info = |module: &str| live.meta.as_ref().and_then(|m| m.modules.get(module)?.info.clone());
        if self.drives.is_none() {
            self.drives = info("storage").and_then(|i| serde_json::from_value(i).ok());
        }
        if self.sensors.is_none() {
            self.sensors = info("hwmon").and_then(|i| serde_json::from_value(i).ok());
        }
        if let Some(disk) = &self.selected
            && !self.holders.contains_key(disk)
        {
            self.holders.insert(disk.clone(), holders(disk));
        }
        let now = self.now(cx);
        if let Some(key) = self.key(cx) {
            let stale = self
                .attempt
                .as_ref()
                .is_none_or(|(k, at)| *k != key || now - at > RELOAD_S);
            if stale && !self.loading {
                self.load(key, now, cx);
            }
        }
        self.merge(now, cx);
        cx.notify();
    }

    fn load(&mut self, key: Key, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((key.clone(), now));
        let task = cx.background_executor().spawn(async move { load(key, now as i64) });
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
        let to = now as i64;
        let from = to - self.range.span_s();
        self.width_s = bucket_width(from, to, BUCKETS);
        self.span = (from as f64, now);
        self.version += 1;
        self.merged.clear();
        let Some(key) = self.key(cx) else {
            return;
        };
        let live = self.live.read(cx);
        let stored = self.stored.as_ref().filter(|s| s.key == key);
        for metric in Metric::ALL {
            let Some(source) = metric.source(&key.1, key.2.as_deref()) else {
                continue;
            };
            let history = stored
                .and_then(|s| s.buckets.get(&metric))
                .map_or(&[][..], Vec::as_slice);
            let merged = source.top_up(history, live, from, self.width_s);
            self.merged.insert(metric, merged.into());
        }
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

    fn drive(&self, disk: &str) -> Option<&Drive> {
        self.drives.as_ref()?.devices.get(disk)
    }
}

fn capsule(text: impl Into<SharedString>, color: Hsla, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded_full()
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(t.small_caption)
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// A 7 px capsule track filled to `fraction`.
fn bar(fraction: f64, color: Hsla, cx: &App) -> gpui::Div {
    let t = theme(cx);
    div().h(px(7.)).rounded_full().bg(t.text.opacity(0.10)).child(
        div()
            .h_full()
            .rounded_full()
            .bg(color)
            .w(gpui::relative(fraction.clamp(0., 1.) as f32)),
    )
}

fn smart_block(s: &Smart, cx: &App) -> AnyElement {
    let t = theme(cx);
    let (verdict, verdict_tint) = if s.passed {
        ("Passed", t.green)
    } else {
        ("Failing", t.red)
    };
    let rows = helper::smart_rows(s).map(|(label, value)| {
        let tint = if label == "Media errors" && s.media_errors.is_some_and(|n| n > 0) {
            t.red
        } else if value == "–" {
            t.text_tertiary
        } else {
            t.text
        };
        key_value(label, value, tint, cx)
    });
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(key_value("Verdict", verdict, verdict_tint, cx).font_weight(FontWeight::SEMIBOLD))
        .when_some(s.percent_used, |d, used| {
            let tint = match helper::wear(used) {
                Wear::Fine => t.green,
                Wear::Worn => t.orange,
                Wear::Spent => t.red,
            };
            d.child(key_value("Life used", format!("{used}%"), tint, cx))
                .child(bar(f64::from(used) / 100., tint, cx))
        })
        .children(rows)
        .when(!s.warnings.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(4.))
                    .children(s.warnings.iter().map(|w| capsule(w.clone(), t.orange, cx))),
            )
        })
        .into_any_element()
}

fn state_colour(state: Option<&str>, t: &Theme) -> Hsla {
    match state {
        Some("live") => t.green,
        Some(_) => t.orange,
        None => t.text_tertiary,
    }
}

impl Disk {
    fn header(&self, sample: Option<&Sample>, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let disk = self.selected.as_deref();
        let drive = disk.and_then(|d| self.drive(d));
        let mut facts = Vec::new();
        if let Some(size) = drive.and_then(|d| d.size_bytes) {
            facts.push(bytes(size as f64));
        }
        if let Some(protocol) = disk.and_then(protocol) {
            facts.push(protocol.to_owned());
        }
        if let Some(firmware) = drive.and_then(|d| d.firmware.clone()) {
            facts.push(firmware);
        }
        let mut disks: Vec<String> = sample
            .and_then(|s| s.disk.as_ref())
            .map(|d| d.keys().filter(|n| pickable(n)).cloned().collect())
            .unwrap_or_default();
        disks.sort();
        let this = cx.entity().downgrade();
        let picker = {
            let this = this.clone();
            let items: Vec<(String, SharedString)> = disks.iter().map(|d| (d.clone(), d.clone().into())).collect();
            segmented(
                "device-picker",
                items,
                &disk.unwrap_or_default().to_owned(),
                move |disk, _, cx| {
                    this.update(cx, |d, cx| d.select(disk, cx)).ok();
                },
                cx,
            )
        };
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div().text_size(t.panel_title).font_weight(FontWeight::SEMIBOLD).child(
                            drive
                                .and_then(|d| d.model.clone())
                                .or_else(|| disk.map(str::to_owned))
                                .unwrap_or_else(|| "Disk".into()),
                        ),
                    )
                    .child(caption(
                        if facts.is_empty() {
                            "–".into()
                        } else {
                            facts.join(" · ")
                        },
                        cx,
                    ))
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
                    .child(caption_label("Device", cx))
                    .child(picker),
            )
            .child(self.view_switch(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(5.))
                    .child(caption_label("History", cx))
                    .child(range_picker_of(
                        &RANGES,
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
        let device = self.selected.as_ref().and_then(|d| sample?.disk.as_ref()?.get(d));
        let fs = self
            .selected
            .as_ref()
            .and_then(|d| Some((d, sample?.fs.as_ref()?)))
            .and_then(|(d, fs)| {
                let holders = self.holders.get(d).map_or(&[][..], Vec::as_slice);
                mounts_on(d, fs, holders).first().map(|(_, f)| (*f).clone())
            });
        let rate = |v: Option<u64>| v.map_or("–".to_string(), |v| Unit::BytesPerSec.format(v as f64));
        let iops = device.map(|d| (d.read_iops, d.write_iops));
        let free_spark = match &fs {
            Some(f) => self
                .spark(Metric::Free, t.blue, cx)
                .fixed(f.total_bytes as f64, bytes(f.total_bytes as f64)),
            None => self.spark(Metric::Free, t.blue, cx),
        };
        div()
            .flex()
            .flex_row()
            .gap(px(10.))
            .child(self.detail_card(
                Metric::Read,
                "card-read",
                "Read",
                t.blue,
                rate(device.map(|d| d.read_bps)),
                None,
                self.spark(Metric::Read, t.blue, cx),
                cx,
            ))
            .child(self.detail_card(
                Metric::Write,
                "card-write",
                "Write",
                t.orange,
                rate(device.map(|d| d.write_bps)),
                None,
                self.spark(Metric::Write, t.orange, cx),
                cx,
            ))
            .child(self.detail_card(
                Metric::Iops,
                "card-iops",
                "IOPS",
                t.text,
                iops.map_or("–".into(), |(r, w)| Unit::Count.format(r + w)),
                iops.map(|(r, w)| format!("{} r / {} w", Unit::Count.format(r), Unit::Count.format(w))),
                self.spark(Metric::Iops, t.text, cx),
                cx,
            ))
            .child(self.detail_card(
                Metric::Free,
                "card-free",
                "Free space",
                t.blue,
                fs.as_ref().map_or("–".into(), |f| bytes(f.avail_bytes as f64)),
                fs.as_ref().map(|f| format!("of {}", bytes(f.total_bytes as f64))),
                free_spark,
                cx,
            ))
    }

    fn throughput_panel(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        panel(IconName::HardDrive, "Throughput", self.expand(Metric::Read), cx)
            .child(
                self.chart(&self.charts.throughput, Metric::Read, cx)
                    .line(self.data(Metric::Write), t.orange, "Write")
                    .line(self.data(Metric::Read), t.blue, "Read")
                    .unit(Unit::BytesPerSec)
                    .y(YScale::Zero)
                    .fill()
                    .height(px(CHART_HEIGHT)),
            )
            .child(legend(self.width_s, cx))
            .child(caption(THROUGHPUT_CAPTION, cx))
    }

    fn operations_panel(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let device = self.selected.as_ref().and_then(|d| sample?.disk.as_ref()?.get(d));
        let ms = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.2} ms"));
        let busy = device.map(|d| d.busy_pct);
        let tint = busy_colour(busy.unwrap_or(0.), t);
        let utilisation = div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(caption_label("Utilization", cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(t.card_value)
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_features(t.numbers.clone())
                            .text_color(tint)
                            .child(busy.map_or("–".into(), |b| format!("{b:.0}%"))),
                    )
                    .child(bar(busy.unwrap_or(0.) / 100., tint, cx).w(px(80.))),
            );
        let sub = |label: &str, chart: TrendChart| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(caption_label(label, cx))
                .child(chart)
        };
        panel(
            IconName::Activity,
            "Operations and latency",
            self.expand(Metric::Iops),
            cx,
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(28.))
                .child(readout(
                    "Read latency",
                    ms(device.map(|d| d.read_await_ms)),
                    t.blue,
                    None,
                    cx,
                ))
                .child(readout(
                    "Write latency",
                    ms(device.map(|d| d.write_await_ms)),
                    t.orange,
                    None,
                    cx,
                ))
                .child(utilisation)
                .child(readout(
                    "Queue depth",
                    device.map_or("–".into(), |d| format!("{:.2}", d.queue_depth)),
                    t.text,
                    None,
                    cx,
                ))
                .child(readout(
                    "In flight",
                    device.map_or("–".into(), |d| d.in_flight.to_string()),
                    t.text,
                    None,
                    cx,
                )),
        )
        .child(sub(
            "Operations per second",
            self.chart(&self.charts.iops, Metric::ReadIops, cx)
                .line(self.data(Metric::WriteIops), t.orange, "Write")
                .line(self.data(Metric::ReadIops), t.blue, "Read")
                .unit(Unit::Count)
                .y(YScale::Zero)
                .fill()
                .height(px(SMALL_CHART_HEIGHT)),
        ))
        .child(sub(
            "Service time per operation",
            self.chart(&self.charts.latency, Metric::ReadAwait, cx)
                .line(self.data(Metric::WriteAwait), t.orange, "Write")
                .line(self.data(Metric::ReadAwait), t.blue, "Read")
                .unit(Unit::Millis)
                .y(YScale::Zero)
                .height(px(SMALL_CHART_HEIGHT)),
        ))
        .child(sub(
            "Queue depth",
            self.chart(&self.charts.queue, Metric::Queue, cx)
                .line(self.data(Metric::Queue), t.purple, "Queue depth")
                .unit(Unit::Count)
                .y(YScale::Zero)
                .height(px(SMALL_CHART_HEIGHT)),
        ))
        .child(legend(self.width_s, cx))
        .child(caption(LATENCY_CAPTION, cx))
    }

    fn devices_panel(&self, sample: Option<&Sample>, cx: &mut Context<Self>) -> impl IntoElement {
        let mut disks: Vec<(String, schema::disk::Device)> = sample
            .and_then(|s| s.disk.as_ref())
            .map(|d| {
                d.iter()
                    .filter(|(n, _)| pickable(n))
                    .map(|(n, d)| (n.clone(), d.clone()))
                    .collect()
            })
            .unwrap_or_default();
        disks.sort_by(|a, b| a.0.cmp(&b.0));
        let storage = sample.and_then(|s| s.storage.as_ref());
        let rows: Vec<AnyElement> = disks
            .into_iter()
            .enumerate()
            .map(|(i, (name, device))| {
                let state = storage.and_then(|s| s.devices.get(&name)?.state.clone());
                self.device_row(i, name, &device, state, cx).into_any_element()
            })
            .collect();
        let cx: &App = cx;
        section(IconName::HardDrive, "Devices", cx).map(|p| {
            if rows.is_empty() {
                p.child(caption("No disks reported", cx))
            } else {
                p.children(rows)
            }
        })
    }

    fn device_row(
        &self,
        i: usize,
        name: String,
        device: &schema::disk::Device,
        state: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx);
        let selected = self.selected.as_ref() == Some(&name);
        let drive = self.drive(&name);
        let fact = |label: &'static str, value: Option<String>| {
            div()
                .flex()
                .flex_row()
                .gap(px(5.))
                .child(div().text_color(t.text_secondary).child(label))
                .child(
                    div()
                        .font_features(t.numbers.clone())
                        .child(value.unwrap_or_else(|| "–".into())),
                )
        };
        let live = |label: &'static str, value: String, color: Hsla| {
            div()
                .flex()
                .flex_row()
                .gap(px(4.))
                .child(div().text_color(t.text_tertiary).child(label))
                .child(div().text_color(color).font_features(t.numbers.clone()).child(value))
        };
        let medium = drive.map(|d| if d.rotational { "Rotational" } else { "Solid state" }.to_owned());
        let state_label = state.clone().unwrap_or_else(|| "–".into());
        let state_tint = state_colour(state.as_deref(), t);
        let select = name.clone();
        hairline(
            div()
                .id(("device", i))
                .flex()
                .flex_col()
                .gap(px(7.))
                .px(px(13.))
                .py(px(11.))
                .rounded(t.sub_radius)
                .bg(if selected { t.card_hover } else { t.card })
                .border_color(if selected { t.blue.opacity(0.6) } else { t.hairline }),
            t.hairline_width,
        )
        .hover(|d| d.bg(t.card_hover))
        .on_click(cx.listener(move |this, _, _, cx| this.select(select.clone(), cx)))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(Icon::new(IconName::HardDrive).small().text_color(t.text_secondary))
                .child(
                    div()
                        .text_size(t.panel_title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(drive.and_then(|d| d.model.clone()).unwrap_or_else(|| name.clone())),
                )
                .child(
                    div()
                        .text_size(t.caption)
                        .text_color(t.text_tertiary)
                        .child(name.clone()),
                )
                .child(capsule(state_label, state_tint, cx))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(t.caption)
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(drive.and_then(|d| d.size_bytes).map_or("–".into(), |s| bytes(s as f64))),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(px(20.))
                .gap_y(px(4.))
                .text_size(t.caption)
                .child(fact("Protocol", protocol(&name).map(str::to_owned)))
                .child(fact("Serial", drive.and_then(|d| d.serial.clone())))
                .child(fact("Firmware", drive.and_then(|d| d.firmware.clone())))
                .child(fact("Medium", medium)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(px(16.))
                .gap_y(px(4.))
                .text_size(t.caption)
                .child(live("R", Unit::BytesPerSec.format(device.read_bps as f64), t.blue))
                .child(live("W", Unit::BytesPerSec.format(device.write_bps as f64), t.orange))
                .child(live(
                    "IOPS",
                    Unit::Count.format(device.read_iops + device.write_iops),
                    t.text,
                ))
                .child(live("R lat", format!("{:.2} ms", device.read_await_ms), t.text))
                .child(live("W lat", format!("{:.2} ms", device.write_await_ms), t.text))
                .child(live(
                    "Busy",
                    format!("{:.0}%", device.busy_pct),
                    busy_colour(device.busy_pct, t),
                )),
        )
    }

    fn health_panel(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let disk = self.selected.as_deref();
        let state = disk.and_then(|d| sample?.storage.as_ref()?.devices.get(d)?.state.clone());
        let tint = state_colour(state.as_deref(), t);
        let status = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(t.caption)
            .child(Icon::new(IconName::CircleCheck).small().text_color(tint))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(match &state {
                Some(s) => format!("Controller {s}"),
                None => "Controller state –".into(),
            }))
            .when_some(disk, |d, disk| {
                d.child(div().text_color(t.text_tertiary).child(disk.to_owned()))
            });

        let chip = disk.and_then(nvme_chip);
        let readings = chip
            .as_ref()
            .and_then(|c| sample?.hwmon.as_ref()?.get(c)?.get("temp"))
            .cloned()
            .unwrap_or_default();
        let limits = chip
            .as_ref()
            .and_then(|c| self.sensors.as_ref()?.limits.get(c)?.get("temp"))
            .cloned()
            .unwrap_or_default();
        let crit_of = |label: &str| {
            limits
                .get(label)
                .and_then(|l| l.get("crit"))
                .or_else(|| limits.get("Composite").and_then(|l| l.get("crit")))
                .copied()
        };
        // Composite is the drive's own summary reading, so it leads.
        let mut sensors: Vec<(&String, &Option<f64>)> = readings.iter().collect();
        sensors.sort_by_key(|(label, _)| (label.as_str() != "Composite", label.as_str()));
        let temps: Vec<AnyElement> = sensors
            .into_iter()
            .map(|(label, value)| {
                let name = if label == "Composite" {
                    "Temperature".to_owned()
                } else {
                    label.clone()
                };
                match value {
                    Some(c) => key_value(name, format!("{c:.0} °C"), temp_colour(*c, crit_of(label), t), cx),
                    None => key_value(name, "–", t.text_tertiary, cx),
                }
                .into_any_element()
            })
            .collect();

        let mut btrfs: Vec<AnyElement> = Vec::new();
        for (uuid, fs) in sample
            .and_then(|s| s.storage.as_ref())
            .map(|s| s.btrfs.iter().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            let name = fs
                .label
                .clone()
                .unwrap_or_else(|| format!("{}… (no label)", uuid.chars().take(8).collect::<String>()));
            let mut totals: Vec<(String, u64)> = Vec::new();
            for errors in fs.device_errors.values() {
                for (counter, &n) in &errors.errors {
                    let label = counter.trim_end_matches("_errs").to_owned();
                    match totals.iter_mut().find(|(l, _)| *l == label) {
                        Some((_, total)) => *total += n,
                        None => totals.push((label, n)),
                    }
                }
            }
            let nonzero: Vec<String> = totals
                .iter()
                .filter(|(_, n)| *n > 0)
                .map(|(l, n)| format!("{n} {l}"))
                .collect();
            btrfs.push(
                if nonzero.is_empty() {
                    key_value(name, "No errors", t.green, cx)
                } else {
                    key_value(name, nonzero.join(", "), t.red, cx)
                }
                .into_any_element(),
            );
            for (devid, errors) in &fs.device_errors {
                if errors.missing == Some(1) {
                    btrfs.push(key_value(format!("  device {devid}"), "missing", t.red, cx).into_any_element());
                }
            }
        }

        section(IconName::HeartPulse, "Health", cx)
            .child(status)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .map(|d| {
                        if temps.is_empty() {
                            d.child(key_value("Temperature", "–", t.text_tertiary, cx))
                        } else {
                            d.children(temps)
                        }
                    })
                    .when_some(
                        chip.as_ref()
                            .and_then(|_| limits.get("Composite")?.get("crit").copied()),
                        |d, crit| d.child(key_value("Critical at", format!("{crit:.0} °C"), t.text_secondary, cx)),
                    ),
            )
            .child(div().h(t.hairline_width).bg(t.hairline))
            .child(caption_label("Btrfs error counters", cx))
            .child(div().flex().flex_col().gap(px(6.)).map(|d| {
                if btrfs.is_empty() {
                    d.child(caption("No btrfs filesystems", cx))
                } else {
                    d.children(btrfs)
                }
            }))
            .map(|d| match helper::disk_health(sample, disk) {
                DiskHealth::Hint => d.child(helper_hint(
                    "smart-enable",
                    "Drive health (SMART) needs the privileged helper",
                    cx,
                )),
                DiskHealth::Smart(smart) => d
                    .child(div().h(t.hairline_width).bg(t.hairline))
                    .child(caption_label("SMART", cx))
                    .child(smart_block(smart, cx)),
                DiskHealth::NoReading => d
                    .child(div().h(t.hairline_width).bg(t.hairline))
                    .child(caption_label("SMART", cx))
                    .child(caption("No SMART reading for this drive", cx)),
            })
    }

    fn free_space_panel(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let fs = sample.and_then(|s| s.fs.as_ref());
        let disk = self.selected.as_deref();
        let holders = disk.and_then(|d| self.holders.get(d)).map_or(&[][..], Vec::as_slice);
        let mine = match (disk, fs) {
            (Some(d), Some(fs)) => mounts_on(d, fs, holders)
                .first()
                .map(|(m, f)| ((*m).to_owned(), (*f).clone())),
            _ => None,
        };
        let rows: Vec<AnyElement> = fs
            .map(|fs| fs.iter().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|(mount, f)| {
                let on_this = mine.as_ref().is_some_and(|(m, _)| m == mount);
                let fraction = f.used_pct.map_or(0., |p| p / 100.);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .text_size(t.caption)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .when(on_this, |d| d.font_weight(FontWeight::SEMIBOLD))
                                    .child(mount.clone()),
                            )
                            .child(
                                div()
                                    .text_color(t.text_secondary)
                                    .font_features(t.numbers.clone())
                                    .child(format!(
                                        "{} free of {}",
                                        bytes(f.avail_bytes as f64),
                                        bytes(f.total_bytes as f64)
                                    )),
                            ),
                    )
                    .child(bar(fraction, if on_this { t.blue } else { t.text_secondary }, cx))
                    .into_any_element()
            })
            .collect();
        let chart_part = match &mine {
            Some((mount, f)) => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(caption_label(&format!("Used on {mount}"), cx))
                .child(
                    self.chart(&self.charts.used, Metric::Used, cx)
                        .line(self.data(Metric::Used), t.blue, "Used")
                        .unit(Unit::Bytes)
                        .y(YScale::Fixed(0., (f.total_bytes as f64).max(1.)))
                        .fill()
                        .height(px(RAIL_CHART_HEIGHT)),
                )
                .child(caption(
                    format!(
                        "Used space of the {} filesystem over the selected range, sampled every 30 s.",
                        f.fstype
                    ),
                    cx,
                ))
                .into_any_element(),
            None => caption(format!("No mounted filesystem on {}", disk.unwrap_or("this disk")), cx).into_any_element(),
        };
        panel(IconName::ChartArea, "Free space", self.expand(Metric::Used), cx)
            .child(div().flex().flex_col().gap(px(10.)).map(|d| {
                if rows.is_empty() {
                    d.child(caption("No filesystems reported", cx))
                } else {
                    d.children(rows)
                }
            }))
            .child(chart_part)
    }

    fn processes_panel(&self, cx: &App) -> impl IntoElement {
        let rows = self.feed.read(cx).table().map_or(&[][..], |t| t.rows.as_slice());
        // Rates are present only for processes the daemon's user may inspect: our own.
        let io = |r: &Process| Some(r.read_bps? + r.write_bps?);
        let mut by_io: Vec<(&Process, u64)> = rows
            .iter()
            .filter_map(|r| Some((r, io(r)?)))
            .filter(|(_, n)| *n > 0)
            .collect();
        by_io.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let top = by_io
            .into_iter()
            .take(TOP_ROWS)
            .map(|(r, n)| (r.comm.clone(), Unit::BytesPerSec.format(n as f64)))
            .collect();
        section(IconName::List, "Top disk processes", cx)
            .child(top_list(top, "No disk activity", cx))
            .child(caption("Read plus write per process.", cx))
            .when(!helper::running(self.live.read(cx).latest.as_deref()), |d| {
                d.child(helper_hint(
                    "disk-processes-enable",
                    "Disk I/O of other users' processes needs the privileged helper",
                    cx,
                ))
            })
    }

    fn overview(&self, sample: Option<&Sample>, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.header(sample, cx).into_any_element();
        let devices = self.devices_panel(sample, cx).into_any_element();
        let cx: &App = cx;
        let t = theme(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("disk-main")
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
                            .pt(px(12.))
                            .pb(t.page_padding)
                            .child(header)
                            .child(self.cards(sample, cx))
                            .child(self.throughput_panel(cx))
                            .child(self.operations_panel(sample, cx))
                            .child(devices),
                    ),
            )
            .child(
                div()
                    .id("disk-rail")
                    .flex_shrink_0()
                    .w(t.rail_width + t.gap + t.page_padding)
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(t.gap)
                            .pl(t.gap)
                            .pr(t.page_padding)
                            .pt(px(12.))
                            .pb(t.page_padding)
                            .child(self.health_panel(sample, cx))
                            .child(self.free_space_panel(sample, cx))
                            .child(self.processes_panel(cx)),
                    ),
            )
    }
}

impl Disk {
    #[allow(clippy::too_many_arguments)]
    fn detail_card(
        &self,
        metric: Metric,
        id: &'static str,
        label: &str,
        color: Hsla,
        value: String,
        detail: Option<String>,
        spark: Sparkline,
        cx: &App,
    ) -> impl IntoElement {
        let t = theme(cx);
        card(id, label, color, cx)
            .on_click(self.expand(metric))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(t.card_value)
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_features(t.numbers.clone())
                            .child(value),
                    )
                    .when_some(detail, |d, detail| {
                        d.child(
                            div()
                                .pb(px(2.))
                                .text_size(t.small_caption)
                                .text_color(t.text_secondary)
                                .font_features(t.numbers.clone())
                                .child(detail),
                        )
                    }),
            )
            .child(spark)
    }

    fn expand(&self, metric: Metric) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        opener(self.me.clone(), metric, |this, metric, cx| this.detail(*metric, cx))
    }

    /// A panel or card's sheet; the throughput card opens both directions, as the panel does.
    fn detail(&self, metric: Metric, cx: &App) -> Option<Detail> {
        let t = theme(cx);
        let disk = self.selected.clone()?;
        let mount = self.mount(&disk, cx);
        let line = |m: Metric, label: &str, color: Hsla| {
            Some(Series {
                label: label.to_owned().into(),
                color,
                source: m.source(&disk, mount.as_deref())?,
            })
        };
        let total = || {
            let fs = self.live.read(cx).latest.as_ref()?.fs.clone()?;
            Some(fs.get(mount.as_deref()?)?.total_bytes as f64)
        };
        let (title, icon, unit, y, fill, lines, meaning, method) = match metric {
            Metric::Read | Metric::Write => (
                format!("Throughput · {disk}"),
                IconName::HardDrive,
                Unit::BytesPerSec,
                YScale::Zero,
                true,
                vec![
                    line(Metric::Read, "Read", t.blue)?,
                    line(Metric::Write, "Write", t.orange)?,
                ],
                "Bytes read from and written to this disk per second, partitions and everything stacked on it \
                 included. Page cache flushes, filesystem metadata and swap are disk traffic too.",
                "From /proc/diskstats: the change in sectors read and written between two samples, times 512 \
                 bytes, divided by the time between them.",
            ),
            Metric::ReadIops | Metric::WriteIops | Metric::Iops => (
                format!("Operations · {disk}"),
                IconName::Activity,
                Unit::Count,
                YScale::Zero,
                true,
                vec![
                    line(Metric::ReadIops, "Read", t.blue)?,
                    line(Metric::WriteIops, "Write", t.orange)?,
                ],
                "Read and write requests the disk completed per second. Many small operations can keep a disk \
                 busy while moving few bytes, which is why this can be high when throughput is low.",
                "From /proc/diskstats: the change in completed reads and writes between two samples, divided by \
                 the time between them. Merged requests count once.",
            ),
            Metric::ReadAwait | Metric::WriteAwait => (
                format!("Service time · {disk}"),
                IconName::Timer,
                Unit::Millis,
                YScale::Zero,
                false,
                vec![
                    line(Metric::ReadAwait, "Read", t.blue)?,
                    line(Metric::WriteAwait, "Write", t.orange)?,
                ],
                "The average time a completed request spent waiting and being served. Rising service time with \
                 steady traffic means the disk is falling behind.",
                "From /proc/diskstats: the change in time spent on reads (or writes) divided by the change in \
                 completed reads (or writes) between two samples, in milliseconds.",
            ),
            Metric::Queue => (
                format!("Queue depth · {disk}"),
                IconName::Layers,
                Unit::Count,
                YScale::Zero,
                false,
                vec![line(Metric::Queue, "Queue depth", t.purple)?],
                "How many requests were outstanding on average. Solid-state disks serve many at once; a deep queue \
                 on a spinning disk means requests are waiting.",
                "From /proc/diskstats: the change in weighted time spent doing I/O between two samples, divided by \
                 the time between them.",
            ),
            Metric::Free | Metric::Used => {
                let mount = mount.clone()?;
                let (used, label, color) = if metric == Metric::Used {
                    (true, "Used", t.blue)
                } else {
                    (false, "Free", t.blue)
                };
                (
                    format!("{} on {mount}", if used { "Used space" } else { "Free space" }),
                    IconName::ChartArea,
                    Unit::Bytes,
                    total().map_or(YScale::Zero, |total| YScale::Fixed(0., total.max(1.))),
                    true,
                    vec![line(metric, label, color)?],
                    "Space taken and space left on the filesystem mounted from this disk. A filesystem that fills \
                     up stops accepting writes, and most behave worse in the last few percent.",
                    "From statvfs on the mount point every 30 seconds: used is blocks in use times the block size, \
                     and free is the blocks available to ordinary users, so space reserved for root is not counted \
                     as free.",
                )
            }
        };
        Some(Detail {
            title: title.into(),
            icon,
            unit,
            y,
            fill,
            lines,
            rules: vec![],
            meaning,
            method,
            range: self.range,
            live: self.live.clone(),
        })
    }

    fn view_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let switch = segmented(
            "disk-view",
            [(View::Overview, "Overview"), (View::Map, "Disk Map")],
            &self.view,
            move |view, _, cx| {
                this.update(cx, |d, cx| {
                    d.view = view;
                    cx.notify();
                })
                .ok();
            },
            cx,
        );
        div()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(5.))
            .child(caption_label("View", cx))
            .child(switch)
    }
}

impl Render for Disk {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sample = self.live.read(cx).latest.clone();
        let body = match self.view {
            View::Overview => self.overview(sample.as_deref(), cx).into_any_element(),
            View::Map => {
                let live = self.live.clone();
                self.map
                    .get_or_insert_with(|| gpui::AppContext::new(cx, |cx| DiskMap::new(live, cx)))
                    .clone()
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when(self.view == View::Map, |d| {
                let t = theme(cx);
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .px(t.page_padding)
                        .pt(t.page_padding)
                        .child(self.view_switch(cx)),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn filesystem(device: &str, total: u64) -> Filesystem {
        Filesystem {
            device: device.into(),
            fstype: "ext4".into(),
            mounts: Vec::new(),
            total_bytes: total,
            used_bytes: 0,
            avail_bytes: total,
            used_pct: Some(0.),
            inodes_total: 0,
            inodes_free: 0,
        }
    }

    #[test]
    fn partitions_belong_to_their_disk_only() {
        assert!(on_disk("nvme1n1", "/dev/nvme1n1p2", &[]));
        assert!(on_disk("nvme1n1", "/dev/nvme1n1", &[]), "a filesystem on the bare disk");
        assert!(
            !on_disk("nvme1n1", "/dev/nvme1n10p1", &[]),
            "namespace 10 is another disk"
        );
        assert!(!on_disk("nvme0n1", "/dev/nvme1n1p2", &[]));
        assert!(on_disk("sda", "/dev/sda1", &[]));
        assert!(!on_disk("sda", "/dev/sdaa1", &[]), "sdaa is another disk");
        assert!(on_disk("mmcblk0", "/dev/mmcblk0p1", &[]));
        assert!(!on_disk("sda", "tmpfs", &[]));
    }

    #[test]
    fn stacked_devices_map_through_their_holders() {
        let holders = [Holder {
            kernel: "dm-0".into(),
            name: Some("cryptroot".into()),
        }];
        assert!(on_disk("nvme0n1", "/dev/mapper/cryptroot", &holders));
        assert!(on_disk("nvme0n1", "/dev/dm-0", &holders));
        assert!(!on_disk("nvme0n1", "/dev/mapper/cryptswap", &holders));
        assert!(
            !on_disk("nvme1n1", "/dev/mapper/cryptroot", &[]),
            "not stacked on this disk"
        );
    }

    #[test]
    fn the_free_space_card_shows_the_largest_filesystem_on_the_disk() {
        let fs: Fs = BTreeMap::from([
            ("/".into(), filesystem("/dev/nvme1n1p2", 2_000)),
            ("/boot".into(), filesystem("/dev/nvme1n1p1", 1)),
            ("/backup".into(), filesystem("/dev/nvme0n1p1", 5_000)),
        ]);
        let mounts: Vec<&str> = mounts_on("nvme1n1", &fs, &[]).into_iter().map(|(m, _)| m).collect();
        assert_eq!(mounts, ["/", "/boot"]);
        assert!(mounts_on("sdb", &fs, &[]).is_empty());
    }

    #[test]
    fn utilisation_tint_steps_at_60_and_90() {
        let t = Theme::dark();
        assert_eq!(busy_colour(0., &t), t.text_secondary);
        assert_eq!(busy_colour(59.9, &t), t.text_secondary);
        assert_eq!(busy_colour(60., &t), t.orange);
        assert_eq!(busy_colour(89.9, &t), t.orange);
        assert_eq!(busy_colour(90., &t), t.red);
        assert_eq!(busy_colour(100., &t), t.red);
    }

    #[test]
    fn temperature_tint_follows_the_critical_limit() {
        let t = Theme::dark();
        assert_eq!(temp_colour(50., Some(84.85), &t), t.green);
        assert_eq!(temp_colour(75., Some(84.85), &t), t.orange);
        assert_eq!(temp_colour(85., Some(84.85), &t), t.red);
        assert_eq!(temp_colour(85., None, &t), t.text, "no limit, no judgement");
    }

    #[test]
    fn nvme_namespaces_map_to_their_controller_chip() {
        assert_eq!(nvme_chip("nvme1n1").as_deref(), Some("nvme/nvme1"));
        assert_eq!(nvme_chip("nvme12n1").as_deref(), Some("nvme/nvme12"));
        assert_eq!(nvme_chip("sda"), None);
    }

    #[test]
    fn the_picker_opens_on_the_busiest_whole_disk() {
        let device = |busy: f64, bps: u64| schema::disk::Device {
            read_bps: bps,
            write_bps: 0,
            read_iops: 0.,
            write_iops: 0.,
            read_await_ms: 0.,
            write_await_ms: 0.,
            busy_pct: busy,
            queue_depth: 0.,
            in_flight: 0,
            read_bytes_total: 0,
            write_bytes_total: 0,
        };
        let disks: schema::disk::Disk = BTreeMap::from([
            ("nvme0n1".into(), device(5., 10)),
            ("nvme1n1".into(), device(20., 1)),
            ("zram0".into(), device(99., 999)),
            ("loop0".into(), device(99., 999)),
            ("dm/cryptroot".into(), device(99., 999)),
        ]);
        assert_eq!(busiest(&disks), Some("nvme1n1"));
        let idle = BTreeMap::from([("sdb".into(), device(0., 0)), ("sda".into(), device(0., 0))]);
        assert_eq!(busiest(&idle), Some("sda"), "ties go to the first name");
        assert_eq!(busiest(&BTreeMap::from([("zram0".into(), device(1., 1))])), None);
    }

    #[test]
    fn filesystem_series_need_a_mount() {
        assert_eq!(
            Metric::Used.source("nvme1n1", Some("/")).unwrap().paths,
            ["/fs/~1/used_bytes"]
        );
        assert!(Metric::Free.source("nvme0n1", None).is_none());
        assert_eq!(
            Metric::Iops.source("nvme1n1", None).unwrap().paths,
            ["/disk/nvme1n1/read_iops", "/disk/nvme1n1/write_iops"]
        );
    }
}
