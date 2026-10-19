use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, Rems, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::netinfo::Netinfo;
use schema::series::escape;
use schema::system::SystemMeta;

use crate::charts::format::{Unit, bytes};
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::desktop::IconIndex;
use crate::data::helper;
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::net::{self, Kind, counted, primary_route, retransmits_high, signal_quality, split_addresses};
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source, matching};
use crate::screens::processes::Icons;
use crate::ui::shell::Tab;
use crate::ui::theme::{rem_px, theme};
use crate::ui::widgets::app_icon::app_icon;
use crate::ui::widgets::detail_sheet::{Detail, Series, opener};
use crate::ui::widgets::hairline;
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::{caption_label, panel, section};
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};
use crate::ui::widgets::text::{caption, key_value};

const BUCKETS: u32 = 120;
/// Stored history is re-read this often; the live rings cover the newest 30 minutes in between.
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 170.;
const RAIL_CHART_HEIGHT: f32 = 100.;
const ADAPTER_CHART_HEIGHT: f32 = 120.;
const HEADER_VALUE: Rems = rem_px(17.);
const NET_INTERVAL_S: u64 = 2;
const PREFIX: &str = "/net/interfaces/";
const TOP_APPS: usize = 8;
const PER_APP_HINT: &str = "The kernel counts traffic per process only for an eBPF program, which needs the \
                            privileged helper.";

/// A plotted series. Interface keys hold the escaped path segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Down,
    Up,
    SegIn,
    SegOut,
    Retrans,
    TcpSockets,
    UdpSockets,
    Rx(String),
    Tx(String),
}

impl Key {
    const FIXED: [Key; 7] = [
        Self::Down,
        Self::Up,
        Self::SegIn,
        Self::SegOut,
        Self::Retrans,
        Self::TcpSockets,
        Self::UdpSockets,
    ];

    fn source<'a>(&self, known: impl IntoIterator<Item = &'a str>) -> Source {
        let one = |p: &str| Source::new(vec![p.to_owned()], Combine::Sum);
        match self {
            Self::Down => Source::new(matching(known, PREFIX, "/rx_bps", counted), Combine::Sum),
            Self::Up => Source::new(matching(known, PREFIX, "/tx_bps", counted), Combine::Sum),
            Self::SegIn => one("/net/tcp/tcp_in_segments_ps"),
            Self::SegOut => one("/net/tcp/tcp_out_segments_ps"),
            Self::Retrans => one("/net/tcp/tcp_retransmits_ps"),
            Self::TcpSockets => one("/net/sockets/tcp_in_use"),
            Self::UdpSockets => one("/net/sockets/udp_in_use"),
            Self::Rx(seg) => one(&format!("{PREFIX}{seg}/rx_bps")),
            Self::Tx(seg) => one(&format!("{PREFIX}{seg}/tx_bps")),
        }
    }
}

/// History read for one range, and the newest configuration snapshot; live data tops both up.
struct Stored {
    range: Range,
    known: Vec<String>,
    buckets: HashMap<Key, Vec<Bucket>>,
    netinfo: Option<Netinfo>,
}

fn load(range: Range, to: i64) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let known = history.paths("/net/")?;
    let from = to - range.span_s();
    let interfaces = matching(known.iter().map(String::as_str), PREFIX, "/rx_bps", |_| true);
    let mut keys = Key::FIXED.to_vec();
    for path in &interfaces {
        let seg = &path[PREFIX.len()..path.len() - "/rx_bps".len()];
        keys.extend([Key::Rx(seg.to_owned()), Key::Tx(seg.to_owned())]);
    }
    let mut buckets = HashMap::new();
    for key in keys {
        let source = key.source(known.iter().map(String::as_str));
        buckets.insert(key, source.query(&history, from, to, BUCKETS)?);
    }
    let netinfo = history
        .snapshot("netinfo")?
        .and_then(|(_, data)| serde_json::from_str(&data).ok());
    Ok(Stored {
        range,
        known,
        buckets,
        netinfo,
    })
}

#[derive(Default)]
struct Charts {
    throughput: Rc<RefCell<ChartState>>,
    tcp: Rc<RefCell<ChartState>>,
    sockets: Rc<RefCell<ChartState>>,
    adapter: Rc<RefCell<ChartState>>,
}

pub struct Network {
    me: WeakEntity<Self>,
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    icons_requested: bool,
    range: Range,
    stored: Option<Stored>,
    loading: bool,
    /// Range and time of the last history read, so a failing read is not retried every tick.
    attempt: Option<(Range, f64)>,
    error: Option<SharedString>,
    merged: HashMap<Key, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: Charts,
    expanded: Option<String>,
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

impl Network {
    pub fn new(live: Entity<Live>, feed: Entity<ProcessFeed>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        cx.observe(&feed, |_, _, cx| cx.notify()).detach();
        let mut this = Self {
            me: cx.weak_entity(),
            live,
            feed,
            icons_requested: false,
            range: remembered(Tab::Network, cx),
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            merged: HashMap::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            charts: Charts::default(),
            #[cfg(debug_assertions)]
            expanded: std::env::var("SYSMAN_DEV_EXPAND").ok(),
            #[cfg(not(debug_assertions))]
            expanded: None,
        };
        this.refresh(cx);
        this
    }

    /// A sample with `net` is in, the configuration is known, and the history read for the range has returned.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        self.live.read(cx).latest.as_ref().is_some_and(|s| s.net.is_some())
            && self.netinfo(cx).is_some()
            && !self.loading
            && self.attempt.is_some_and(|(range, _)| range == self.range)
    }

    /// The live module once it has reported; until then the stored snapshot, so the screen is whole at once.
    fn netinfo<'a>(&'a self, cx: &'a App) -> Option<&'a Netinfo> {
        self.live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.netinfo.as_ref())
            .or_else(|| self.stored.as_ref().and_then(|s| s.netinfo.as_ref()))
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Network, range, cx);
            self.refresh(cx);
        }
    }

    fn toggle(&mut self, name: String, cx: &mut Context<Self>) {
        self.expanded = if self.expanded.as_ref() == Some(&name) {
            None
        } else {
            self.charts.adapter = ChartState::new();
            Some(name)
        };
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let stale = self.attempt.is_none_or(|(range, at)| {
            range != self.range
                || self
                    .live
                    .read(cx)
                    .reload_due(at, now, self.range.span_s() as f64, RELOAD_S)
        });
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
        let mut keys = Key::FIXED.to_vec();
        // Only the open adapter draws a chart, so only its lines are merged.
        if let Some(name) = &self.expanded {
            keys.extend([Key::Rx(escape(name)), Key::Tx(escape(name))]);
        }
        self.merged.clear();
        for key in keys {
            let source = key.source(known.iter().copied());
            let history = stored.and_then(|s| s.buckets.get(&key)).map_or(&[][..], Vec::as_slice);
            let merged = source.top_up(history, live, from, self.width_s);
            self.merged.insert(key, merged.into());
        }
        self.span = (from as f64, now);
        self.version += 1;
    }

    fn data(&self, key: &Key) -> Rc<[Bucket]> {
        self.merged.get(key).cloned().unwrap_or_else(|| Rc::from(Vec::new()))
    }

    fn chart(&self, state: &Rc<RefCell<ChartState>>) -> TrendChart {
        let gap = (self.width_s as f64 * 1.5).max(NET_INTERVAL_S as f64 * 2.5);
        TrendChart::new(state.clone(), self.version, self.span, self.width_s as f64, gap)
    }
}

/// `1 Gb/s`, `2.5 Gb/s`, `100 Mb/s`.
fn link_speed(mbps: u64) -> String {
    if mbps >= 1000 {
        let g = mbps as f64 / 1000.;
        if g.fract() == 0. {
            format!("{g:.0} Gb/s")
        } else {
            format!("{g:.1} Gb/s")
        }
    } else {
        format!("{mbps} Mb/s")
    }
}

fn rate(bps: Option<u64>) -> String {
    bps.map_or("–".into(), |v| Unit::BytesPerSec.format(v as f64))
}

fn per_sec(v: Option<f64>) -> String {
    v.map_or("–".into(), |v| format!("{v:.0}/s"))
}

fn badge(text: &'static str, color: Hsla, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .px(rem_px(6.))
        .rounded_full()
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(t.small_caption)
        .font_weight(t.weight_medium)
        .child(text)
}

/// Arrow, value and uppercase caption, as in the header.
fn big_rate(label: &str, arrow: IconName, color: Hsla, value: String, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(1.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(5.))
                .child(Icon::new(arrow).small().text_color(color))
                .child(
                    div()
                        .text_size(HEADER_VALUE)
                        .font_weight(t.weight_semibold)
                        .font_features(t.numbers.clone())
                        .child(value),
                ),
        )
        .child(caption_label(label, cx))
}

/// Caption over a value, for the rail.
fn stat(label: &str, value: String, color: Hsla, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(caption_label(label, cx))
        .child(
            div()
                .text_size(t.panel_title)
                .font_weight(t.weight_semibold)
                .font_features(t.numbers.clone())
                .text_color(color)
                .child(value),
        )
}

/// One adapter as the screen shows it, joined from the live counters and the configuration snapshot.
struct Adapter<'a> {
    name: &'a str,
    kind: Kind,
    primary: bool,
    connected: bool,
    live: Option<&'a schema::net::Interface>,
    info: Option<&'a schema::netinfo::Interface>,
}

fn adapters<'a>(
    net: Option<&'a schema::net::Net>,
    netinfo: Option<&'a Netinfo>,
    primary: Option<&str>,
) -> Vec<Adapter<'a>> {
    let names: BTreeSet<&str> = net
        .into_iter()
        .flat_map(|n| n.interfaces.keys())
        .chain(netinfo.into_iter().flat_map(|n| n.interfaces.keys()))
        .map(String::as_str)
        .filter(|&n| n != "lo")
        .collect();
    let mut out: Vec<Adapter> = names
        .into_iter()
        .map(|name| {
            let live = net.and_then(|n| n.interfaces.get(name));
            let info = netinfo.and_then(|n| n.interfaces.get(name));
            let state = live
                .and_then(|i| i.state.as_deref())
                .or(info.and_then(|i| i.state.as_deref()));
            let has_address = info.is_some_and(|i| !i.addresses.is_empty());
            Adapter {
                name,
                kind: Kind::of(name, live.is_some_and(|i| i.signal_dbm.is_some())),
                primary: primary == Some(name),
                connected: net::connected(state, has_address),
                live,
                info,
            }
        })
        .collect();
    out.sort_by_key(|a| (!a.primary, !a.connected, a.kind, a.name));
    out
}

impl Network {
    fn header(&self, sample: Option<&Sample>, this: WeakEntity<Self>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let route = self.netinfo(cx).and_then(|n| primary_route(&n.default_routes));
        let summary = match route {
            Some(r) => format!(
                "Primary: {} · Router {}",
                r.interface.as_deref().unwrap_or("–"),
                r.gateway.as_deref().unwrap_or("–")
            ),
            None if self.netinfo(cx).is_some() => "No default route".into(),
            None => "Primary: –".into(),
        };
        let totals = sample.and_then(|s| s.net.as_ref()).map(|net| {
            net.interfaces
                .iter()
                .filter(|(name, _)| counted(name))
                .fold((0., 0.), |(rx, tx), (_, i)| {
                    (rx + i.rx_bps as f64, tx + i.tx_bps as f64)
                })
        });
        let fmt = |v: Option<f64>| v.map_or("–".into(), |v| Unit::BytesPerSec.format(v));
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(rem_px(28.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(t.caption).text_color(t.text_secondary).child(summary))
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_size(t.small_caption).text_color(t.orange).child(e))
                    }),
            )
            .child(big_rate(
                "Download",
                IconName::ArrowDown,
                t.green,
                fmt(totals.map(|t| t.0)),
                cx,
            ))
            .child(big_rate(
                "Upload",
                IconName::ArrowUp,
                t.red,
                fmt(totals.map(|t| t.1)),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(rem_px(5.))
                    .child(caption_label("History", cx))
                    .child(range_picker(
                        self.range,
                        move |range, _, cx| {
                            this.update(cx, |n: &mut Network, cx| n.set_range(range, cx)).ok();
                        },
                        cx,
                    )),
            )
    }

    fn throughput(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let session = self.live.read(cx).net_session.totals(counted);
        let footer = session.map_or("This session: –".into(), |(rx, tx)| {
            format!(
                "This session: {} downloaded · {} uploaded",
                bytes(rx as f64),
                bytes(tx as f64)
            )
        });
        panel(IconName::ChartLine, "Throughput", self.expand(Key::Down), cx)
            .child(
                self.chart(&self.charts.throughput)
                    .line(self.data(&Key::Down), t.green, "Download")
                    .line(self.data(&Key::Up), t.red, "Upload")
                    .unit(Unit::BytesPerSec)
                    .y(YScale::Zero)
                    .height(px(CHART_HEIGHT)),
            )
            .child(legend(self.width_s, cx))
            .child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(footer),
            )
    }

    /// App icons are indexed only once per-app rows can show.
    fn ensure_icons(&mut self, cx: &mut Context<Self>) {
        if self.icons_requested || cx.try_global::<Icons>().is_some() {
            return;
        }
        self.icons_requested = true;
        let task = cx.background_executor().spawn(async { IconIndex::load() });
        cx.spawn(async move |this, cx| {
            let icons = Arc::new(task.await);
            cx.update(|cx| cx.set_global(Icons(icons)));
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn per_app_panel(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let body: Vec<AnyElement> = if !helper::running(sample) {
            vec![helper_hint("per-app-enable", PER_APP_HINT, cx).into_any_element()]
        } else {
            let rows = self.feed.read(cx).table().map_or(&[][..], |t| t.rows.as_slice());
            let top = helper::top_traffic(rows, TOP_APPS);
            let icons = cx.try_global::<Icons>().map(|i| i.0.clone()).unwrap_or_default();
            let rate_cell = |arrow: IconName, color: Hsla, bps: Option<u64>| {
                div()
                    .w(rem_px(96.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(4.))
                    .child(Icon::new(arrow).xsmall().text_color(color))
                    .child(
                        div()
                            .font_features(t.numbers.clone())
                            .text_color(t.text_secondary)
                            .child(rate(bps)),
                    )
            };
            let row = || div().h(rem_px(18.)).flex_shrink_0();
            // Every slot is drawn even when empty, so the panels below stay put as apps come and go.
            let empty = TOP_APPS.saturating_sub(top.len().max(1));
            let first = top.is_empty().then(|| {
                row()
                    .child(caption("No process is sending or receiving", cx))
                    .into_any_element()
            });
            let list = div()
                .flex()
                .flex_col()
                .gap(rem_px(6.))
                .text_size(t.caption)
                .children(first)
                .children(top.into_iter().map(|p| {
                    row()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(rem_px(8.))
                        .child(app_icon(&p.comm, &p.cmdline, &icons, 16., cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(p.comm.clone()),
                        )
                        .child(rate_cell(IconName::ArrowDown, t.green, p.net_rx_bps))
                        .child(rate_cell(IconName::ArrowUp, t.red, p.net_tx_bps))
                }))
                .children((0..empty).map(|_| row()))
                .into_any_element();
            vec![
                list,
                caption("TCP and UDP bytes per process, loopback excluded.", cx).into_any_element(),
            ]
        };
        section(IconName::AppWindow, "Per-app traffic", cx).children(body)
    }

    fn adapters_panel(&self, sample: Option<&Sample>, this: &WeakEntity<Self>, cx: &App) -> impl IntoElement {
        let netinfo = self.netinfo(cx);
        let primary = netinfo
            .and_then(|n| primary_route(&n.default_routes))
            .and_then(|r| r.interface.as_deref());
        let list = adapters(sample.and_then(|s| s.net.as_ref()), netinfo, primary);
        let count = list.len();
        let rows: Vec<AnyElement> = list
            .into_iter()
            .enumerate()
            .map(|(i, a)| self.adapter_row(i, a, i + 1 < count, this.clone(), cx))
            .collect();
        let empty = rows.is_empty();
        section(IconName::Network, "Adapters", cx)
            .child(div().flex().flex_col().children(rows))
            .child(caption(
                if empty {
                    "No interfaces reported yet."
                } else {
                    "Click an adapter for its details. Totals are the kernel's counters since the interface came up."
                },
                cx,
            ))
    }

    fn adapter_row(&self, i: usize, a: Adapter, divider: bool, this: WeakEntity<Self>, cx: &App) -> AnyElement {
        let t = theme(cx);
        let expanded = self.expanded.as_deref() == Some(a.name);
        let addresses = a.info.map_or(&[][..], |i| i.addresses.as_slice());
        let (v4, v6) = split_addresses(addresses);
        let glyph = match a.kind {
            Kind::Wifi => IconName::Wifi,
            Kind::Ethernet => IconName::EthernetPort,
            Kind::Bridge => IconName::Layers,
            Kind::Virtual => IconName::Waypoints,
        };
        let glyph_colour = match (a.connected, a.kind) {
            (true, Kind::Wifi | Kind::Ethernet) => t.blue,
            _ => t.text_secondary,
        };
        let line = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_row()
                .gap(rem_px(10.))
                .text_size(t.caption)
                .child(
                    div()
                        .w(rem_px(34.))
                        .flex_shrink_0()
                        .text_color(t.text_tertiary)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(value),
                )
        };
        let mut lines: Vec<AnyElement> = Vec::new();
        if !v4.is_empty() {
            lines.push(line("IPv4", v4.join(", ")).into_any_element());
        }
        if !v6.is_empty() {
            lines.push(line("IPv6", v6.join(", ")).into_any_element());
        }
        if let Some(mac) = a.info.and_then(|i| i.mac.clone()) {
            lines.push(line("MAC", mac).into_any_element());
        }

        let live = a.live;
        let signal = live.and_then(|l| l.signal_dbm);
        let arrow_value = |arrow: IconName, colour: Hsla, value: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .child(Icon::new(arrow).xsmall().text_color(colour))
                .child(value)
        };
        let right = div()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(2.))
            .text_size(t.panel_title)
            .font_weight(t.weight_medium)
            .font_features(t.numbers.clone())
            .child(arrow_value(IconName::ArrowDown, t.green, rate(live.map(|l| l.rx_bps))))
            .child(arrow_value(IconName::ArrowUp, t.red, rate(live.map(|l| l.tx_bps))))
            .when_some(live, |d, l| {
                d.child(
                    div()
                        .text_size(t.small_caption)
                        .font_weight(t.weight_body)
                        .text_color(t.text_tertiary)
                        .child(format!(
                            "{} / {}",
                            bytes(l.rx_bytes_total as f64),
                            bytes(l.tx_bytes_total as f64)
                        )),
                )
            })
            .when_some(signal, |d, dbm| {
                d.child(
                    div()
                        .text_size(t.small_caption)
                        .font_weight(t.weight_body)
                        .text_color(t.text_secondary)
                        .child(format!("{dbm:.0} dBm · {}", signal_quality(dbm))),
                )
            });

        let mut badges: Vec<AnyElement> = Vec::new();
        if a.primary {
            badges.push(badge("Primary", t.blue, cx).into_any_element());
        }
        badges.push(if a.connected {
            badge("Connected", t.green, cx).into_any_element()
        } else {
            badge("Down", t.text_secondary, cx).into_any_element()
        });
        if addresses.is_empty() {
            badges.push(badge("No address", t.text_secondary, cx).into_any_element());
        }

        let name = a.name.to_owned();
        let summary = div()
            .id(("adapter", i))
            .flex()
            .flex_row()
            .items_start()
            .gap(rem_px(12.))
            .py(rem_px(9.))
            .cursor_pointer()
            .when(!a.connected, |d| d.opacity(0.55))
            .child(
                div()
                    .w(rem_px(22.))
                    .pt(rem_px(2.))
                    .flex()
                    .justify_center()
                    .child(Icon::new(glyph).small().text_color(glyph_colour)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(7.))
                            .child(
                                div()
                                    .text_size(t.panel_title)
                                    .font_weight(t.weight_semibold)
                                    .child(a.name.to_owned()),
                            )
                            .child(
                                div()
                                    .text_size(t.small_caption)
                                    .text_color(t.text_secondary)
                                    .child(a.kind.label()),
                            )
                            .children(badges),
                    )
                    .children(lines),
            )
            .child(right)
            .child(
                div().pt(px(1.)).child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(t.text_tertiary),
                ),
            )
            .on_click(move |_, _, cx| {
                this.update(cx, |n: &mut Network, cx| n.toggle(name.clone(), cx)).ok();
            });

        div()
            .flex()
            .flex_col()
            .when(divider, |d| d.border_b_1().border_color(t.hairline))
            .child(summary)
            .when(expanded, |d| d.child(self.adapter_detail(&a, cx)))
            .into_any_element()
    }

    fn expand(&self, key: Key) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        opener(self.me.clone(), key, |this, key, cx| Some(this.detail(key, cx)))
    }

    fn detail(&self, key: &Key, cx: &App) -> Detail {
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
        let line = |key: Key, label: &str, color: Hsla| Series {
            label: label.to_owned().into(),
            color,
            source: key.source(known.iter().copied()),
        };
        let (title, icon, unit, lines, meaning, method) = match key {
            Key::SegIn | Key::SegOut | Key::Retrans => (
                "TCP".to_owned(),
                IconName::ArrowLeftRight,
                Unit::Count,
                vec![
                    line(Key::SegIn, "Segments in", t.blue),
                    line(Key::SegOut, "Segments out", t.teal),
                    line(Key::Retrans, "Retransmits", t.orange),
                ],
                "TCP segments received and sent per second over every connection, and how many sent segments \
                 had to be sent again. Retransmits above about 1% of segments sent point to loss or congestion \
                 somewhere on the path.",
                "From the Tcp counters in /proc/net/snmp: the change in InSegs, OutSegs and RetransSegs between \
                 two samples, divided by the time between them.",
            ),
            Key::TcpSockets | Key::UdpSockets => (
                "Sockets".to_owned(),
                IconName::Plug,
                Unit::Count,
                vec![
                    line(Key::TcpSockets, "TCP", t.blue),
                    line(Key::UdpSockets, "UDP", t.teal),
                ],
                "Open TCP and UDP sockets across the whole system, in every state. A count that only grows can \
                 mean a program is leaking connections.",
                "The inuse counts of the TCP and UDP lines in /proc/net/sockstat, read at each sample.",
            ),
            Key::Rx(seg) | Key::Tx(seg) => (
                format!("Adapter {}", seg.replace("%2F", "/")),
                IconName::Network,
                Unit::BytesPerSec,
                vec![
                    line(Key::Rx(seg.clone()), "Download", t.green),
                    line(Key::Tx(seg.clone()), "Upload", t.red),
                ],
                "Bytes received and sent per second on this interface alone.",
                "From /proc/net/dev: the change in the interface's receive and transmit byte counters between two \
                 samples, divided by the time between them.",
            ),
            Key::Down | Key::Up => (
                "Throughput".to_owned(),
                IconName::ChartLine,
                Unit::BytesPerSec,
                vec![line(Key::Down, "Download", t.green), line(Key::Up, "Upload", t.red)],
                "Bytes received and sent per second over the machine's own interfaces. Loopback and container or \
                 VM plumbing (veth, docker and libvirt bridges) are left out, since their bytes are already counted \
                 on the physical link they leave by.",
                "From /proc/net/dev: the change in each counted interface's receive and transmit byte counters \
                 between two samples, divided by the time between them, then summed.",
            ),
        };
        Detail {
            title: title.into(),
            icon,
            unit,
            y: YScale::Zero,
            fill: unit == Unit::BytesPerSec,
            lines,
            rules: vec![],
            ticks: None,
            meaning,
            method,
            range: self.range,
            live: self.live.clone(),
        }
    }

    fn adapter_detail(&self, a: &Adapter, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let l = a.live;
        let pair = |rx: Option<u64>, tx: Option<u64>| match (rx, tx) {
            (Some(rx), Some(tx)) => format!("{rx} in · {tx} out"),
            _ => "–".into(),
        };
        let facts: Vec<(&str, String)> = vec![
            ("MTU", a.info.and_then(|i| i.mtu).map_or("–".into(), |m| m.to_string())),
            ("Link speed", l.and_then(|l| l.link_mbps).map_or("–".into(), link_speed)),
            (
                "Packets",
                l.map_or("–".into(), |l| {
                    format!("{:.0}/s in · {:.0}/s out", l.rx_pps, l.tx_pps)
                }),
            ),
            ("Errors", pair(l.map(|l| l.rx_errors), l.map(|l| l.tx_errors))),
            ("Drops", pair(l.map(|l| l.rx_dropped), l.map(|l| l.tx_dropped))),
            (
                "State",
                l.and_then(|l| l.state.clone())
                    .or_else(|| a.info.and_then(|i| i.state.clone()))
                    .unwrap_or_else(|| "–".into()),
            ),
        ];
        let seg = escape(a.name);
        hairline(
            div()
                .flex()
                .flex_col()
                .gap(rem_px(10.))
                .ml(rem_px(34.))
                .mb(rem_px(10.))
                .p(rem_px(12.))
                .rounded(t.sub_radius)
                .bg(t.card)
                .border_color(t.hairline),
            t.hairline_width,
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(rem_px(28.))
                .gap_y(rem_px(8.))
                .children(facts.into_iter().map(|(label, value)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(caption_label(label, cx))
                        .child(div().text_size(t.caption).font_features(t.numbers.clone()).child(value))
                })),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(caption_label("Throughput", cx))
                .child(div().flex_1())
                .child(
                    div()
                        .id("adapter-expand")
                        .p(rem_px(3.))
                        .rounded(px(4.))
                        .text_color(t.text_tertiary)
                        .hover(|d| d.bg(t.card_hover).text_color(t.text))
                        .child(Icon::new(IconName::Maximize2).xsmall())
                        .on_click(self.expand(Key::Rx(seg.clone()))),
                ),
        )
        .child(
            self.chart(&self.charts.adapter)
                .line(self.data(&Key::Rx(seg.clone())), t.green, "Download")
                .line(self.data(&Key::Tx(seg)), t.red, "Upload")
                .unit(Unit::BytesPerSec)
                .y(YScale::Zero)
                .height(px(ADAPTER_CHART_HEIGHT)),
        )
    }

    fn configuration(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let host = self
            .live
            .read(cx)
            .meta
            .as_ref()
            .and_then(|m| m.modules.get("system")?.info.clone())
            .and_then(|info| serde_json::from_value::<SystemMeta>(info).ok())
            .and_then(|s| s.host);
        let netinfo = self.netinfo(cx);
        let primary = netinfo.and_then(|n| primary_route(&n.default_routes));
        let mut rows = vec![
            key_value("Host name", host.unwrap_or_else(|| "–".into()), t.text, cx),
            key_value(
                "Primary interface",
                primary.and_then(|r| r.interface.clone()).unwrap_or_else(|| "–".into()),
                t.text,
                cx,
            ),
        ];
        let routes = netinfo.map_or(&[][..], |n| n.default_routes.as_slice());
        if routes.is_empty() {
            rows.push(key_value("Default route", "–", t.text, cx));
        }
        for r in routes {
            let mut value = r.gateway.clone().unwrap_or_else(|| "on link".into());
            if let Some(iface) = &r.interface {
                value.push_str(&format!(" via {iface}"));
            }
            if let Some(metric) = r.metric {
                value.push_str(&format!(" · metric {metric}"));
            }
            rows.push(key_value("Default route", value, t.text, cx));
        }
        let dns = netinfo
            .map(|n| n.dns.join(", "))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "–".into());
        rows.push(key_value("DNS servers", dns, t.text, cx));
        let count = rows.len();
        section(IconName::Settings, "Configuration", cx).child(div().flex().flex_col().children(
            rows.into_iter().enumerate().map(|(i, row)| {
                div()
                    .when(i + 1 < count, |d| d.border_b_1().border_color(t.hairline))
                    .child(row.py(rem_px(6.)).text_size(t.panel_title))
            }),
        ))
    }

    fn rail(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let net = sample.and_then(|s| s.net.as_ref());
        let tcp = net.map(|n| &n.tcp);
        let seg_in = tcp.and_then(|t| t.tcp_in_segments_ps);
        let seg_out = tcp.and_then(|t| t.tcp_out_segments_ps);
        let retrans = tcp.and_then(|t| t.tcp_retransmits_ps);
        let high = matches!((retrans, seg_out), (Some(r), Some(o)) if retransmits_high(r, o));
        let retrans_text = match (retrans, seg_out) {
            (Some(r), Some(o)) if o > 0. => format!("{r:.1}/s · {:.1}%", r / o * 100.),
            (Some(r), _) => format!("{r:.1}/s"),
            _ => "–".into(),
        };
        let sockets = net.map(|n| &n.sockets);
        let count = |v: Option<u64>| v.map_or("–".into(), |v| v.to_string());
        let row = || {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(rem_px(20.))
                .gap_y(rem_px(10.))
        };
        div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(
                panel(IconName::ArrowLeftRight, "TCP", self.expand(Key::SegIn), cx)
                    .child(
                        row()
                            .child(stat(
                                "Established",
                                tcp.and_then(|t| t.established).map_or("–".into(), |e| e.to_string()),
                                t.text,
                                cx,
                            ))
                            .child(stat("Segments in", per_sec(seg_in), t.blue, cx))
                            .child(stat("Segments out", per_sec(seg_out), t.teal, cx))
                            .child(stat(
                                "Retransmits",
                                retrans_text,
                                if high { t.orange } else { t.text },
                                cx,
                            )),
                    )
                    .child(
                        self.chart(&self.charts.tcp)
                            .line(self.data(&Key::SegIn), t.blue, "Segments in")
                            .line(self.data(&Key::SegOut), t.teal, "Segments out")
                            .line(self.data(&Key::Retrans), t.orange, "Retransmits")
                            .unit(Unit::Index)
                            .y(YScale::Zero)
                            .height(px(RAIL_CHART_HEIGHT)),
                    )
                    .child(caption(
                        "Segments per second over all TCP connections. Retransmits turn orange above 1% of \
                         segments sent, a sign of loss on the path.",
                        cx,
                    )),
            )
            .child(
                panel(IconName::Plug, "Sockets", self.expand(Key::TcpSockets), cx)
                    .child(
                        row()
                            .child(stat(
                                "TCP in use",
                                count(sockets.and_then(|s| s.tcp_in_use)),
                                t.blue,
                                cx,
                            ))
                            .child(stat(
                                "UDP in use",
                                count(sockets.and_then(|s| s.udp_in_use)),
                                t.teal,
                                cx,
                            )),
                    )
                    .child(
                        self.chart(&self.charts.sockets)
                            .line(self.data(&Key::TcpSockets), t.blue, "TCP")
                            .line(self.data(&Key::UdpSockets), t.teal, "UDP")
                            .unit(Unit::Index)
                            .y(YScale::Zero)
                            .height(px(RAIL_CHART_HEIGHT)),
                    )
                    .child(caption("Open sockets system-wide, from /proc/net/sockstat.", cx)),
            )
    }
}

impl Render for Network {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sample = self.live.read(cx).latest.clone();
        let sample = sample.as_deref();
        if helper::running(sample) {
            self.ensure_icons(cx);
        }
        let this = cx.entity().downgrade();
        let cx: &App = cx;
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("network-main")
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
                            .child(self.header(sample, this.clone(), cx))
                            .child(self.throughput(cx))
                            .child(self.per_app_panel(sample, cx))
                            .child(self.adapters_panel(sample, &this, cx))
                            .child(self.configuration(cx)),
                    ),
            )
            .child(
                div()
                    .id("network-rail")
                    .flex_shrink_0()
                    .map(|d| t.rail(d))
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
    use schema::netinfo::{Interface, Route};

    use super::*;

    #[test]
    fn link_speeds_read_in_the_largest_whole_unit() {
        assert_eq!(link_speed(100), "100 Mb/s");
        assert_eq!(link_speed(1000), "1 Gb/s");
        assert_eq!(link_speed(2500), "2.5 Gb/s");
    }

    #[test]
    fn throughput_sums_only_the_machines_own_interfaces() {
        let known = [
            "/net/interfaces/enp11s0/rx_bps",
            "/net/interfaces/docker0/rx_bps",
            "/net/interfaces/veth7faffa5/rx_bps",
            "/net/interfaces/tailscale0/rx_bps",
            "/net/interfaces/enp11s0/tx_bps",
        ];
        assert_eq!(
            Key::Down.source(known).paths,
            ["/net/interfaces/enp11s0/rx_bps", "/net/interfaces/tailscale0/rx_bps"]
        );
    }

    #[test]
    fn adapters_list_primary_first_then_connected_and_skip_loopback() {
        let iface = |state: &str, addresses: &[&str]| Interface {
            state: Some(state.into()),
            mac: None,
            mtu: Some(1500),
            addresses: addresses.iter().map(|a| a.to_string()).collect(),
        };
        let netinfo = Netinfo {
            interfaces: [
                ("lo", iface("UNKNOWN", &["127.0.0.1/8"])),
                ("enp12s0", iface("DOWN", &[])),
                ("docker0", iface("UP", &["172.17.0.1/16"])),
                ("wlp10s0", iface("UP", &["192.168.1.9/24"])),
                ("enp11s0", iface("UP", &["10.10.10.111/24"])),
            ]
            .into_iter()
            .map(|(n, i)| (n.to_owned(), i))
            .collect(),
            default_routes: vec![Route {
                gateway: Some("10.10.10.1".into()),
                interface: Some("wlp10s0".into()),
                metric: Some(600),
            }],
            dns: vec![],
        };
        let list = adapters(None, Some(&netinfo), Some("wlp10s0"));
        assert_eq!(
            list.iter().map(|a| a.name).collect::<Vec<_>>(),
            ["wlp10s0", "enp11s0", "docker0", "enp12s0"]
        );
        assert!(list[0].primary && list[0].kind == Kind::Wifi);
        assert!(!list[3].connected);
    }
}
