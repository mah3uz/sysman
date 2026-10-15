use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, canvas, div, point, prelude::FluentBuilder, px, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::cpu::CpuMeta;
use schema::hwmon::HwmonMeta;

use super::{Hardware, Selection, capacity_bar, category_icon, module, props_table, ring};
use crate::charts::format::{self, TempUnit, Unit, bytes};
use crate::charts::stacked_bar::{Segment, stacked_bar};
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::hardware::item::{
    Reading, aspect_label, chip_readings, core_path, disk_of, item_chips, nvme_chip, path, pci_related, ppi, usb_parent,
};
use crate::data::hardware::{Category, Item, Node, gib, temp_axis_top, threads_per_core};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::source::{Combine, Source};
use crate::ui::shell::Tab;
use crate::ui::theme::{Theme, rem_px, theme};
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::panel::{frame, heading};
use crate::ui::widgets::range::{Range, range_picker_of, remember};

pub(super) const RANGES: [Range; 5] = [Range::M5, Range::M30, Range::H1, Range::H6, Range::H24];
const BUCKETS: u32 = 120;
const RELOAD_S: f64 = 60.;
const CHART_HEIGHT: f32 = 130.;
const STRIP_ROW: f32 = 9.;

/// History and live rings of the series behind the selected item's page.
#[derive(Default)]
pub(super) struct ItemView {
    item: Option<Item>,
    paths: Vec<String>,
    stored: Option<(Item, Range, HashMap<String, Vec<Bucket>>)>,
    attempt: Option<(Item, Range, f64)>,
    loading: bool,
    merged: HashMap<String, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: RefCell<HashMap<String, Rc<RefCell<ChartState>>>>,
    cursor: Rc<Cell<Option<f64>>>,
}

fn load(paths: Vec<String>, range: Range, to: i64) -> anyhow::Result<HashMap<String, Vec<Bucket>>> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let mut out = HashMap::new();
    for p in paths {
        let buckets = history.series(&p, from, to, BUCKETS)?;
        out.insert(p, buckets);
    }
    Ok(out)
}

/// Colours for the second and later lines of a chart whose first line is orange.
fn palette(t: &Theme) -> [Hsla; 7] {
    [t.blue, t.green, t.purple, t.pink, t.teal, t.indigo, t.red]
}

/// What an item's page adds to the header and the panels below it.
#[derive(Default)]
struct Page {
    vendor: Option<String>,
    chips: Vec<Div>,
    /// Panels that take a whole row, above the grid.
    wide: Vec<AnyElement>,
    body: Vec<AnyElement>,
}

fn page(vendor: Option<String>, chips: Vec<Div>, body: Vec<AnyElement>) -> Page {
    Page {
        vendor,
        chips,
        wide: Vec::new(),
        body,
    }
}

/// One plotted series: its path, colour and legend label.
type Series = (String, Hsla, SharedString);

struct Spec {
    lines: Vec<Series>,
    rules: Vec<(f64, SharedString, Hsla)>,
    unit: Unit,
    y: YScale,
    height: f32,
}

impl Spec {
    fn new(unit: Unit, lines: Vec<Series>) -> Self {
        Self {
            lines,
            rules: Vec::new(),
            unit,
            y: YScale::Zero,
            height: CHART_HEIGHT,
        }
    }

    fn y(mut self, y: YScale) -> Self {
        self.y = y;
        self
    }

    fn rule(mut self, value: Option<f64>, label: &str, colour: Hsla) -> Self {
        if let Some(v) = value {
            self.rules.push((v, label.to_owned().into(), colour));
        }
        self
    }

    fn height(mut self, h: f32) -> Self {
        self.height = h;
        self
    }
}

/// Load from 0 to 1 as a cell colour: faint blue when idle, deepening, then orange near saturation.
fn load_colour(load: f64, t: &Theme) -> Hsla {
    let l = (load / 100.).clamp(0., 1.);
    if l >= 0.85 {
        t.orange
    } else {
        t.blue.opacity((0.1 + 0.9 * l / 0.85) as f32)
    }
}

fn status_chip(t: &Theme, text: impl Into<SharedString>, colour: Hsla) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(rem_px(20.))
        .px(rem_px(8.))
        .rounded_full()
        .bg(colour.opacity(0.14))
        .text_color(colour)
        .text_size(rem_px(11.))
        .font_weight(t.weight_medium)
        .whitespace_nowrap()
        .child(text.into())
}

fn pane(icon: IconName, title: &str, aside: Option<String>, cx: &App) -> Div {
    let t = theme(cx);
    frame(cx).flex_1().min_w_0().child(
        heading(icon, title.to_owned(), cx)
            .child(div().flex_1())
            .children(aside.map(|a| {
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(a)
            })),
    )
}

/// Panels two to a row; an odd last one spans the row.
fn grid(panels: Vec<AnyElement>, gap: gpui::Pixels) -> Div {
    let mut out = div().flex().flex_col().gap(gap);
    let mut it = panels.into_iter().peekable();
    while let Some(a) = it.next() {
        let row = div().flex().flex_row().gap(gap).child(a);
        out = out.child(match it.next() {
            Some(b) => row.child(b),
            None => row,
        });
    }
    out
}

fn kv(rows: Vec<(&str, Option<String>)>, t: &Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(rem_px(6.))
        .text_size(rem_px(12.))
        .children(rows.into_iter().filter_map(|(label, value)| {
            Some(
                div()
                    .flex()
                    .flex_row()
                    .gap(rem_px(12.))
                    .child(
                        div()
                            .w(rem_px(130.))
                            .flex_shrink_0()
                            .text_color(t.text_secondary)
                            .child(label.to_owned()),
                    )
                    .child(div().flex_1().min_w_0().font_features(t.numbers.clone()).child(value?)),
            )
        }))
}

fn big(value: String, label: &str, colour: Hsla, t: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .child(
            div()
                .text_size(rem_px(20.))
                .font_weight(t.weight_semibold)
                .text_color(colour)
                .font_features(t.numbers.clone())
                .child(value),
        )
        .child(
            div()
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .child(label.to_owned()),
        )
}

fn mhz(v: f64) -> String {
    if v >= 1000. {
        format!("{:.2} GHz", v / 1000.)
    } else {
        format!("{v:.0} MHz")
    }
}

fn hours(h: u64) -> String {
    if h >= 48 {
        format!("{h} h ({:.1} years)", h as f64 / 8766.)
    } else {
        format!("{h} h")
    }
}

impl Hardware {
    pub(super) fn selected_node(&self) -> Option<(Category, usize, &Node)> {
        let Selection::Node(c, i) = self.selected else {
            return None;
        };
        Some((c, i, self.section(c)?.nodes.get(i)?))
    }

    /// Selects the node for `item`, if the outline lists one.
    pub(super) fn open_item(&mut self, item: &Item, cx: &mut Context<Self>) {
        let found = self.sections.iter().find_map(|s| {
            let i = s.nodes.iter().position(|n| n.item.as_ref() == Some(item))?;
            Some(Selection::Node(s.category, i))
        });
        if let Some(sel) = found {
            self.select(sel, cx);
        }
    }

    fn has_item(&self, item: &Item) -> bool {
        self.sections
            .iter()
            .any(|s| s.nodes.iter().any(|n| n.item.as_ref() == Some(item)))
    }

    pub(super) fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Hardware, range, cx);
            self.refresh_item(cx);
            cx.notify();
        }
    }

    /// Follows the selection: re-reads history when the item, range or schedule asks, and tops it up from the rings.
    pub(super) fn refresh_item(&mut self, cx: &mut Context<Self>) {
        let item = self.selected_node().and_then(|(_, _, n)| n.item.clone());
        if item != self.view.item {
            self.view.item = item.clone();
            self.view.merged.clear();
            self.view.charts.borrow_mut().clear();
            self.view.cursor.set(None);
        }
        let Some(item) = item else {
            return;
        };
        let live = self.live.read(cx);
        let chips: Vec<String> = live
            .latest
            .as_ref()
            .and_then(|s| s.hwmon.as_ref())
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default();
        let nets = self.inventory.as_ref().map_or(&[][..], |i| i.nets.as_slice());
        let mine = item_chips(&item, &chips, nets);
        let paths = crate::data::hardware::item::charted(&item, &mine, live.paths());
        let now = live.latest.as_ref().map_or_else(crate::data::hardware::now_s, |s| s.t);
        let stale = self.view.attempt.as_ref().is_none_or(|(i, r, at)| {
            *i != item || *r != self.range || live.reload_due(*at, now, self.range.span_s() as f64, RELOAD_S)
        });
        self.view.paths = paths;
        if self.view.paths.is_empty() {
            return;
        }
        if stale && !self.view.loading {
            self.view.loading = true;
            self.view.attempt = Some((item.clone(), self.range, now));
            let (paths, range) = (self.view.paths.clone(), self.range);
            let task = cx
                .background_executor()
                .spawn(async move { load(paths, range, now as i64) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.view.loading = false;
                    if let Ok(stored) = result {
                        this.view.stored = Some((item, range, stored));
                    }
                    this.merge_item(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        self.merge_item(cx);
    }

    fn merge_item(&mut self, cx: &App) {
        let Some(item) = self.view.item.clone() else {
            return;
        };
        let live = self.live.read(cx);
        let now = live.latest.as_ref().map_or_else(crate::data::hardware::now_s, |s| s.t);
        let to = now as i64;
        let from = to - self.range.span_s();
        let width = bucket_width(from, to, BUCKETS);
        let stored = self
            .view
            .stored
            .as_ref()
            .filter(|(i, r, _)| *i == item && *r == self.range)
            .map(|(_, _, s)| s);
        let merged = self
            .view
            .paths
            .iter()
            .map(|p| {
                let seed = stored.and_then(|s| s.get(p)).map_or(&[][..], Vec::as_slice);
                let data: Rc<[Bucket]> = Source::new(vec![p.clone()], Combine::Max)
                    .top_up(seed, live, from, width)
                    .into();
                (p.clone(), data)
            })
            .collect();
        self.view.merged = merged;
        self.view.span = (from as f64, now);
        self.view.width_s = width;
        self.view.version += 1;
    }

    fn data(&self, p: &str) -> Option<Rc<[Bucket]>> {
        self.view.merged.get(p).filter(|d| !d.is_empty()).cloned()
    }

    fn peak(&self, p: &str) -> Option<f64> {
        self.data(p)
            .map(|d| d.iter().map(|b| b.max).fold(f64::NEG_INFINITY, f64::max))
    }

    /// The chart for `spec`, or nothing when none of its series has data in the range.
    fn trend(&self, key: &str, spec: &Spec, cx: &App) -> Option<TrendChart> {
        let lines: Vec<(Rc<[Bucket]>, Hsla, SharedString)> = spec
            .lines
            .iter()
            .filter_map(|(p, c, l)| Some((self.data(p)?, *c, l.clone())))
            .collect();
        if lines.is_empty() {
            return None;
        }
        let live = self.live.read(cx);
        let module = spec.lines[0].0.split('/').nth(1).unwrap_or("");
        let interval = live
            .meta
            .as_ref()
            .and_then(|m| m.modules.get(module))
            .map_or(2, |m| m.interval_s) as f64;
        let gap = (self.view.width_s as f64 * 1.5).max(interval * 2.5);
        let state = self
            .view
            .charts
            .borrow_mut()
            .entry(key.to_owned())
            .or_insert_with(|| ChartState::with_cursor(self.view.cursor.clone()))
            .clone();
        let mut chart = TrendChart::new(state, self.view.version, self.view.span, self.view.width_s as f64, gap)
            .unit(spec.unit)
            .y(spec.y)
            .height(px(spec.height));
        if spec.unit == Unit::Celsius {
            chart = chart.compact_axis(rem_px(40.).to_pixels(theme(cx).text_size.rem_size()), |v| {
                format!("{:.0}°", TempUnit::current().convert(v))
            });
        }
        for (data, colour, label) in lines {
            chart = chart.line(data, colour, label);
        }
        for (v, label, colour) in &spec.rules {
            chart = chart.rule(*v, label.clone(), *colour);
        }
        Some(chart)
    }

    /// A titled chart panel with the first series' current value at the right and a legend for several series.
    fn chart_pane(&self, icon: IconName, title: &str, key: &str, spec: Spec, cx: &App) -> Option<Div> {
        let chart = self.trend(key, &spec, cx)?;
        let t = theme(cx);
        let live = self.live.read(cx);
        let present: Vec<&Series> = spec.lines.iter().filter(|(p, ..)| self.data(p).is_some()).collect();
        let aside = (present.len() == 1)
            .then(|| live.value(&present[0].0).map(|v| spec.unit.format(v)))
            .flatten();
        let legend = (present.len() > 1).then(|| {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(px(14.))
                .gap_y(px(4.))
                .text_size(t.small_caption)
                .children(present.iter().map(|(p, colour, label)| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(rem_px(5.))
                        .child(div().size(rem_px(8.)).rounded_full().bg(*colour))
                        .child(div().text_color(t.text_secondary).child(label.clone()))
                        .child(
                            div()
                                .font_features(t.numbers.clone())
                                .child(live.value(p).map_or("–".into(), |v| spec.unit.format(v))),
                        )
                }))
        });
        Some(pane(icon, title, aside, cx).child(chart).children(legend))
    }

    // ---------- page ----------

    pub(super) fn item_page(&self, c: Category, node: &Node, crumbs: Div, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let live = self.live.read(cx);
        let sample = live.latest.clone();
        let sample = sample.as_deref();
        let item = node.item.clone();
        let Page {
            vendor,
            chips,
            wide,
            body,
        } = match &item {
            Some(Item::Cpu) => self.cpu_page(None, sample, cx),
            Some(Item::Ccd(i)) => self.cpu_page(Some(*i), sample, cx),
            Some(Item::Gpu(slot)) => self.gpu_page(slot, sample, cx),
            Some(Item::Drive(d)) => self.drive_page(d, sample, cx),
            Some(Item::Volume(m)) => self.volume_page(m, sample, cx),
            Some(Item::Net(n)) => self.net_page(n, sample, cx),
            Some(Item::Memory) => self.memory_page(sample, cx),
            Some(Item::Dimm(chip) | Item::Chip(chip)) => self.chip_page(chip, sample, cx),
            Some(Item::Display(key)) => self.display_page(key, cx),
            Some(Item::Usb(port)) => self.usb_page(port, cx),
            Some(Item::Pci(slot)) => self.pci_page(slot, cx),
            Some(Item::Supply(name)) => self.supply_page(name, sample, cx),
            None => Page::default(),
        };
        let charts = !self.view.paths.is_empty();
        let copy_text = super::node_text(c, node);
        let this = cx.entity().downgrade();
        let header = frame(cx)
            .flex_row()
            .items_start()
            .gap(rem_px(14.))
            .child(
                div()
                    .size(rem_px(52.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(rem_px(12.))
                    .bg(t.blue.opacity(0.14))
                    .child(Icon::new(category_icon(c)).size(rem_px(28.)).text_color(t.blue)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rem_px(5.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(rem_px(18.))
                            .font_weight(t.weight_semibold)
                            .child(node.title.clone()),
                    )
                    .children(
                        [vendor, node.subtitle.clone()]
                            .into_iter()
                            .flatten()
                            .reduce(|a, b| if a == b { a } else { format!("{a} · {b}") })
                            .map(|s| div().text_size(rem_px(12.)).text_color(t.text_secondary).child(s)),
                    )
                    .when(!chips.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(rem_px(6.))
                                .pt(px(2.))
                                .children(chips),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(8.))
                    .flex_shrink_0()
                    .when(charts, |d| {
                        d.child(range_picker_of(
                            &RANGES,
                            self.range,
                            move |range, _, cx| {
                                this.update(cx, |this, cx| this.set_range(range, cx)).ok();
                            },
                            cx,
                        ))
                    })
                    .child(
                        Button::new("hw-copy")
                            .small()
                            .ghost()
                            .icon(Icon::new(IconName::Copy))
                            .tooltip("Copy the properties as text")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))
                            }),
                    ),
            );
        let props = pane(
            IconName::ListTree,
            "All properties",
            Some(format!("{}", node.props.len())),
            cx,
        )
        .child(props_table(&node.props, &self.query.trim().to_lowercase(), t));
        div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(crumbs)
            .child(header)
            .children(wide)
            .child(grid(body, t.gap))
            .child(props)
            .into_any_element()
    }

    fn link(&self, id: impl Into<SharedString>, label: String, target: Item, cx: &Context<Self>) -> AnyElement {
        let t = theme(cx);
        div()
            .id(gpui::ElementId::Name(id.into()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .text_size(rem_px(12.))
            .text_color(t.blue)
            .cursor_pointer()
            .hover(|d| d.opacity(0.75))
            .child(label)
            .child(Icon::new(IconName::ChevronRight).xsmall())
            .on_click(cx.listener(move |this, _, _, cx| this.open_item(&target, cx)))
            .into_any_element()
    }

    // ---------- processor ----------

    fn cpu_page(&self, ccd: Option<usize>, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let Some(meta) = module::<CpuMeta>(live.meta.as_ref(), "cpu") else {
            return Page::default();
        };
        let cpu = sample.and_then(|s| s.cpu.as_ref());
        let tpc = threads_per_core(&meta);
        let groups: Vec<(usize, Vec<u32>, u64)> = if meta.topology.l3_groups.is_empty() {
            vec![(0, (0..meta.topology.threads).collect(), 0)]
        } else {
            meta.topology
                .l3_groups
                .iter()
                .enumerate()
                .filter(|(i, _)| ccd.is_none_or(|c| c == *i))
                .map(|(i, g)| (i, g.cpus.clone(), g.l3_kib))
                .collect()
        };
        let mine: Vec<u32> = groups.iter().flat_map(|g| g.1.iter().copied()).collect();
        let load = |c: &u32| cpu.and_then(|s| s.core_pct.as_ref()?.get(*c as usize).copied());
        let mean_load = {
            let v: Vec<f64> = mine.iter().filter_map(load).collect();
            (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
        };
        let mut chips = vec![status_chip(
            t,
            format!("{} cores · {} threads", mine.len() as u32 / tpc, mine.len()),
            t.blue,
        )];
        if let Some(l) = mean_load {
            chips.push(status_chip(t, format!("{l:.0}% load"), t.green));
        }
        if let Some(mhz_now) = cpu.and_then(|c| c.avg_mhz) {
            chips.push(status_chip(t, mhz(mhz_now as f64), t.purple));
        }
        if let Some(temp) = cpu.and_then(|c| c.temp_c) {
            chips.push(status_chip(
                t,
                format::temperature(temp, 0),
                super::heat_colour(crate::data::hardware::heat(temp, None)),
            ));
        }
        if let Some(gov) = &meta.governor {
            chips.push(status_chip(t, gov.clone(), t.text_secondary));
        }

        let mut body = Vec::new();
        // Per-core load over the range, a row per logical CPU, grouped by shared L3.
        let (from, to) = self.view.span;
        let width_s = self.view.width_s as f64;
        let strip = pane(
            IconName::LayoutGrid,
            "Load by core",
            Some(format!("{}, {} buckets", self.range.label(), BUCKETS)),
            cx,
        )
        .children(groups.iter().map(|(i, cpus, l3)| {
            let mut cpus = cpus.clone();
            cpus.sort_unstable();
            let title = if meta.topology.l3_groups.is_empty() {
                "All CPUs".to_owned()
            } else {
                format!("CCD {i} · {} MB L3", l3 / 1024)
            };
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(t.small_caption)
                        .font_weight(t.weight_semibold)
                        .pb(px(3.))
                        .child(title),
                )
                .children(cpus.into_iter().map(|c| {
                    let data = self.data(&core_path(c as usize));
                    let blue = t.blue;
                    let orange = t.orange;
                    let track = t.card;
                    let now = load(&c);
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(rem_px(8.))
                        .h(rem_px(STRIP_ROW))
                        .child(
                            div()
                                .w(rem_px(26.))
                                .text_size(rem_px(9.))
                                .line_height(rem_px(STRIP_ROW))
                                .text_color(t.text_tertiary)
                                .font_features(t.numbers.clone())
                                .child(c.to_string()),
                        )
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, _| {
                                    window.paint_quad(gpui::fill(bounds, track));
                                    let Some(data) = &data else {
                                        return;
                                    };
                                    let span = (to - from).max(1.);
                                    let w = f32::from(bounds.size.width);
                                    for b in data.iter() {
                                        let x0 = ((b.t as f64 - from) / span).clamp(0., 1.) as f32 * w;
                                        let x1 = ((b.t as f64 + width_s - from) / span).clamp(0., 1.) as f32 * w;
                                        if x1 <= x0 {
                                            continue;
                                        }
                                        let l = (b.avg / 100.).clamp(0., 1.);
                                        let colour = if l >= 0.85 {
                                            orange
                                        } else {
                                            blue.opacity((0.1 + 0.9 * l / 0.85) as f32)
                                        };
                                        window.paint_quad(gpui::fill(
                                            gpui::Bounds::new(
                                                point(bounds.left() + px(x0), bounds.top()),
                                                size(px(x1 - x0 + 0.5), bounds.size.height),
                                            ),
                                            colour,
                                        ));
                                    }
                                },
                            )
                            .flex_1()
                            .h_full(),
                        )
                        .child(
                            div()
                                .w(rem_px(34.))
                                .flex()
                                .justify_end()
                                .text_size(rem_px(9.))
                                .line_height(rem_px(STRIP_ROW))
                                .text_color(now.map_or(t.text_tertiary, |l| load_colour(l.max(30.), t)))
                                .font_features(t.numbers.clone())
                                .child(now.map_or("–".into(), |l| format!("{l:.0}%"))),
                        )
                }))
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(6.))
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .child("Idle")
                .children(
                    [0., 25., 50., 75., 100.]
                        .map(|l| div().w(rem_px(18.)).h(rem_px(7.)).rounded(px(2.)).bg(load_colour(l, t))),
                )
                .child("Saturated")
                .child(div().flex_1())
                .child("Logical CPU, average load per bucket; live value at the right"),
        );
        let wide = vec![strip.into_any_element()];

        let freq = Spec::new(Unit::Count, vec![("/cpu/avg_mhz".into(), t.purple, "Average".into())]).rule(
            meta.max_mhz.map(|m| m as f64),
            "max",
            t.text_tertiary,
        );
        let mut temp_lines: Vec<Series> = vec![("/cpu/temp_c".into(), t.orange, "Package".into())];
        let colours = palette(t);
        for (n, label) in cpu
            .map(|c| c.temps_c.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            // k10temp numbers its dies from 1; L3 groups count from 0.
            let keep = ccd.is_none_or(|i| label == format!("Tccd{}", i + 1));
            if keep {
                temp_lines.push((
                    path(&["cpu", "temps_c", &label]),
                    colours[n % colours.len()],
                    label.into(),
                ));
            }
        }
        let peak = temp_lines.iter().filter_map(|(p, ..)| self.peak(p)).fold(40., f64::max);
        let temp = Spec::new(Unit::Celsius, temp_lines).y(YScale::Fixed(0., temp_axis_top(peak)));
        let power = Spec::new(Unit::Watts, vec![("/cpu/power_w".into(), t.green, "Package".into())]);
        let total = Spec::new(Unit::Percent, vec![("/cpu/total_pct".into(), t.blue, "Total".into())])
            .y(YScale::Fixed(0., 100.));
        body.extend(
            [
                self.chart_pane(IconName::Gauge, "Frequency (MHz)", "cpu-freq", freq, cx),
                self.chart_pane(IconName::Thermometer, "Temperature", "cpu-temp", temp, cx),
                self.chart_pane(IconName::Activity, "Total load", "cpu-total", total, cx),
                self.chart_pane(IconName::Zap, "Power", "cpu-power", power, cx),
            ]
            .into_iter()
            .flatten()
            .map(IntoElement::into_any_element),
        );

        let ranking = meta.topology.prefcore_ranking.clone().unwrap_or_default();
        let best = ranking.iter().flatten().copied().max();
        let tree =
            pane(IconName::ListTree, "Topology and cache", None, cx)
                .child(div().text_size(rem_px(12.)).font_weight(t.weight_medium).child(format!(
                    "{} package{}, {} cores, {} threads",
                    meta.topology.packages.unwrap_or(1),
                    if meta.topology.packages.unwrap_or(1) == 1 {
                        ""
                    } else {
                        "s"
                    },
                    meta.topology.cores.unwrap_or(meta.topology.threads),
                    meta.topology.threads
                )))
                .children(groups.iter().map(|(i, cpus, l3)| {
                    let mut cores: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                    let mut sorted = cpus.clone();
                    sorted.sort_unstable();
                    let per = sorted.len() / tpc.max(1) as usize;
                    for (n, c) in sorted.iter().enumerate() {
                        cores.entry((n % per.max(1)) as u32).or_default().push(*c);
                    }
                    div()
                        .flex()
                        .flex_col()
                        .gap(rem_px(6.))
                        .pl(rem_px(10.))
                        .border_l_2()
                        .border_color(t.blue.opacity(0.4))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(rem_px(8.))
                                .text_size(rem_px(12.))
                                .child(div().font_weight(t.weight_semibold).child(
                                    if meta.topology.l3_groups.is_empty() {
                                        "Cores".to_owned()
                                    } else {
                                        format!("CCD {i}")
                                    },
                                ))
                                .when(*l3 > 0, |d| {
                                    d.child(
                                        div()
                                            .text_color(t.text_secondary)
                                            .child(format!("{} MB shared L3", l3 / 1024)),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(4.))
                                .children(cores.into_values().map(|threads| {
                                    let preferred = best.is_some()
                                        && threads
                                            .iter()
                                            .any(|c| ranking.get(*c as usize).copied().flatten() == best);
                                    div()
                                        .px(rem_px(6.))
                                        .py(px(2.))
                                        .rounded(px(4.))
                                        .bg(if preferred { t.orange.opacity(0.18) } else { t.card })
                                        .text_size(rem_px(10.))
                                        .font_features(t.numbers.clone())
                                        .text_color(if preferred { t.orange } else { t.text_secondary })
                                        .child(threads.iter().map(u32::to_string).collect::<Vec<_>>().join("/"))
                                })),
                        )
                }))
                .when(best.is_some(), |d| {
                    d.child(div().text_size(t.small_caption).text_color(t.text_secondary).child(
                        "Each chip is a core and its threads; orange marks the cores the firmware ranks fastest.",
                    ))
                });
        let max_core = cpu
            .and_then(|c| c.core_mhz.as_ref())
            .and_then(|m| mine.iter().filter_map(|c| m.get(*c as usize).copied().flatten()).max());
        let policy = pane(IconName::Zap, "Frequency policy", None, cx).child(kv(
            vec![
                ("Scaling driver", meta.driver.clone()),
                ("Governor", meta.governor.clone()),
                ("Energy preference", meta.energy_performance_preference.clone()),
                (
                    "Range",
                    meta.min_mhz
                        .zip(meta.max_mhz)
                        .map(|(a, b)| format!("{} – {}", mhz(a as f64), mhz(b as f64))),
                ),
                ("Average now", cpu.and_then(|c| c.avg_mhz).map(|m| mhz(m as f64))),
                ("Fastest core now", max_core.map(|m| mhz(m as f64))),
                ("Temperature sensor", meta.temp_source.clone()),
            ],
            t,
        ));
        body.push(tree.into_any_element());
        body.push(policy.into_any_element());
        Page {
            vendor: meta.vendor.clone(),
            chips,
            wide,
            body,
        }
    }

    // ---------- graphics ----------

    fn gpu_page(&self, slot: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let meta: Option<schema::gpu::Meta> = module(live.meta.as_ref(), "gpu");
        let info = meta.as_ref().and_then(|m| m.devices.get(slot));
        let d = sample.and_then(|s| s.gpu.as_ref()?.devices.get(slot));
        let pci = self
            .inventory
            .as_ref()
            .and_then(|i| i.pci.iter().find(|p| p.slot == slot));
        let p = |f: &str| path(&["gpu", "devices", slot, f]);
        let mut chips = Vec::new();
        if let Some(d) = d {
            chips.push(status_chip(t, format!("{} · {}", d.driver, d.backend), t.blue));
            if let Some(u) = d.util_pct {
                chips.push(status_chip(t, format!("{u:.0}% busy"), t.green));
            }
            if let Some(temp) = d.temp_c {
                chips.push(status_chip(
                    t,
                    format::temperature(temp, 0),
                    super::heat_colour(crate::data::hardware::heat(temp, None)),
                ));
            }
            if let Some(w) = d.power_w {
                chips.push(status_chip(t, Unit::Watts.format(w), t.orange));
            }
            if let Some(ps) = &d.pstate {
                chips.push(status_chip(t, ps.clone(), t.purple));
            }
            if let Some(ps) = &d.power_state {
                chips.push(status_chip(t, format!("PCI {ps}"), t.text_secondary));
            }
        }
        let mut body = Vec::new();
        let util_lines: Vec<Series> = [
            ("util_pct", t.blue, "Graphics"),
            ("mem_busy_pct", t.purple, "Memory controller"),
            ("video_busy_pct", t.teal, "Video engine"),
            ("encoder_pct", t.green, "Encoder"),
            ("decoder_pct", t.pink, "Decoder"),
        ]
        .into_iter()
        .map(|(f, c, l)| (p(f), c, l.into()))
        .collect();
        let util = Spec::new(Unit::Percent, util_lines).y(YScale::Fixed(0., 100.));
        let power = Spec::new(Unit::Watts, vec![(p("power_w"), t.orange, "Board power".into())]).rule(
            d.and_then(|d| d.power_limit_w),
            "limit",
            t.red,
        );
        let mut temp_lines: Vec<Series> = vec![(p("temp_c"), t.orange, "GPU".into())];
        let colours = palette(t);
        for (n, label) in d
            .map(|d| d.temps_c.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            temp_lines.push((
                path(&["gpu", "devices", slot, "temps_c", &label]),
                colours[n % colours.len()],
                label.into(),
            ));
        }
        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let chip_limits: Vec<Reading> = sample
            .and_then(|s| s.hwmon.as_ref())
            .map(|h| {
                h.keys()
                    .filter(|c| c.ends_with(&format!("/{slot}")))
                    .flat_map(|c| chip_readings(c, Some(h), hwmon.as_ref()))
                    .filter(|r| r.kind == "temp")
                    .collect()
            })
            .unwrap_or_default();
        let crit = chip_limits.iter().filter_map(|r| r.crit).reduce(f64::min);
        let peak = temp_lines
            .iter()
            .filter_map(|(p, ..)| self.peak(p))
            .fold(40., f64::max)
            .max(crit.unwrap_or(0.) * 1.02);
        let temp = Spec::new(Unit::Celsius, temp_lines)
            .y(YScale::Fixed(0., temp_axis_top(peak)))
            .rule(crit, "crit", t.red);
        let clocks = Spec::new(
            Unit::Count,
            vec![
                (p("clock_mhz"), t.blue, "Core".into()),
                (p("mem_clock_mhz"), t.purple, "Memory".into()),
            ],
        );
        body.extend(
            [
                self.chart_pane(IconName::Activity, "Utilisation", "gpu-util", util, cx),
                self.chart_pane(IconName::Zap, "Power", "gpu-power", power, cx),
                self.chart_pane(IconName::Thermometer, "Temperature", "gpu-temp", temp, cx),
                self.chart_pane(IconName::Gauge, "Clocks (MHz)", "gpu-clock", clocks, cx),
            ]
            .into_iter()
            .flatten()
            .map(IntoElement::into_any_element),
        );
        // Memory: capacity bars now, the history beneath.
        if let Some(d) = d {
            let mut memory = pane(IconName::MemoryStick, "Memory", None, cx);
            let mut lines: Vec<Series> = Vec::new();
            let mut any = false;
            for (name, used, total, colour, field) in [
                (
                    "Video memory (VRAM)",
                    d.vram_used_bytes,
                    d.vram_total_bytes,
                    t.purple,
                    "vram_used_bytes",
                ),
                (
                    "Shared system memory (GTT)",
                    d.gtt_used_bytes,
                    d.gtt_total_bytes,
                    t.teal,
                    "gtt_used_bytes",
                ),
            ] {
                let (Some(used), Some(total)) = (used, total) else {
                    continue;
                };
                any = true;
                lines.push((p(field), colour, name.into()));
                memory = memory.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .text_size(rem_px(12.))
                                .child(div().flex_1().child(name))
                                .child(
                                    div()
                                        .text_color(t.text_secondary)
                                        .font_features(t.numbers.clone())
                                        .child(format!("{} of {}", bytes(used as f64), bytes(total as f64))),
                                ),
                        )
                        .child(capacity_bar(used as f64 / total.max(1) as f64, colour, 8., t)),
                );
            }
            let top = d.vram_total_bytes.into_iter().chain(d.gtt_total_bytes).max();
            let spec = Spec::new(Unit::Bytes, lines)
                .rule(d.vram_total_bytes.map(|v| v as f64), "VRAM", t.purple)
                .height(100.);
            let spec = match top {
                Some(top) if d.gtt_total_bytes.is_none() => spec.y(YScale::Fixed(0., top as f64)),
                _ => spec,
            };
            if any {
                body.push(memory.children(self.trend("gpu-mem", &spec, cx)).into_any_element());
            }
        }
        let link_now = d.and_then(|d| d.pcie_gen.zip(d.pcie_width));
        let link_max = info.and_then(|i| i.pcie_max_gen.zip(i.pcie_max_width));
        if link_now.is_some() || link_max.is_some() {
            let gen_spec = Spec::new(Unit::Count, vec![(p("pcie_gen"), t.blue, "Generation".into())])
                .y(YScale::Fixed(0., link_max.map_or(5., |m| m.0 as f64)))
                .height(70.);
            let fraction = |now: u32, max: u32| f64::from(now) / f64::from(max.max(1));
            let mut pcie = pane(IconName::Plug, "PCIe link", None, cx).child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rem_px(24.))
                    .children(link_now.map(|(g, w)| big(format!("Gen {g} x{w}"), "now", t.blue, t)))
                    .children(link_max.map(|(g, w)| big(format!("Gen {g} x{w}"), "maximum", t.text, t))),
            );
            if let (Some((g, w)), Some((mg, mw))) = (link_now, link_max) {
                pcie = pcie
                    .child(kv(
                        vec![(
                            "Speed",
                            Some(format!("{:.0}% of maximum", fraction(g, mg) * fraction(w, mw) * 100.)),
                        )],
                        t,
                    ))
                    .child(capacity_bar(fraction(g, mg) * fraction(w, mw), t.blue, 6., t));
            }
            let note = "GPUs drop to a slower link when idle to save power; the history shows when it trained up.";
            body.push(
                pcie.children(self.trend("gpu-pcie", &gen_spec, cx))
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .child(note),
                    )
                    .into_any_element(),
            );
        }
        if let Some(reasons) = d.and_then(|d| d.throttle_reasons.clone()) {
            body.push(
                pane(IconName::TriangleAlert, "Throttling", None, cx)
                    .child(if reasons.is_empty() {
                        div().flex().flex_row().child(status_chip(t, "Not throttled", t.green))
                    } else {
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(rem_px(6.))
                            .children(reasons.iter().map(|r| status_chip(t, r.replace('_', " "), t.orange)))
                    })
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .child("Reasons the driver reports for holding clocks down, at this moment."),
                    )
                    .into_any_element(),
            );
        }
        let procs: Vec<&schema::gpu::Process> = sample
            .and_then(|s| s.gpu.as_ref())
            .map(|g| g.processes.iter().filter(|p| p.pci == slot).take(8).collect())
            .unwrap_or_default();
        body.push(
            pane(
                IconName::AppWindow,
                "Processes on this GPU",
                Some(procs.len().to_string()),
                cx,
            )
            .child(if procs.is_empty() {
                div()
                    .text_size(rem_px(12.))
                    .text_color(t.text_secondary)
                    .child("No processes are using this GPU.")
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap(rem_px(5.))
                    .children(procs.into_iter().map(|p| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(rem_px(10.))
                            .text_size(rem_px(12.))
                            .font_features(t.numbers.clone())
                            .child(div().flex_1().min_w_0().truncate().child(p.comm.clone()))
                            .child(
                                div()
                                    .w(rem_px(56.))
                                    .text_color(t.text_tertiary)
                                    .child(p.pid.to_string()),
                            )
                            .child(
                                div()
                                    .w(rem_px(44.))
                                    .flex()
                                    .justify_end()
                                    .child(p.util_pct.map_or("–".into(), |u| format!("{u:.0}%"))),
                            )
                            .child(
                                div()
                                    .w(rem_px(70.))
                                    .flex()
                                    .justify_end()
                                    .text_color(t.text_secondary)
                                    .child(p.mem_bytes.map_or("–".into(), |b| bytes(b as f64))),
                            )
                    }))
            })
            .into_any_element(),
        );
        if let Some(d) = d.filter(|d| d.fan_rpm.is_some() || d.fan_pct.is_some()) {
            let fan = Spec::new(Unit::Count, vec![(p("fan_rpm"), t.teal, "Fan".into())]).height(80.);
            body.push(
                pane(
                    IconName::Fan,
                    "Fan",
                    Some(
                        [
                            d.fan_rpm.map(|r| format!("{r} rpm")),
                            d.fan_pct.map(|p| format!("{p:.0}%")),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                    ),
                    cx,
                )
                .children(self.trend("gpu-fan", &fan, cx))
                .into_any_element(),
            );
        }
        let vendor = pci.and_then(|p| p.vendor.clone());
        page(vendor, chips, body)
    }

    // ---------- storage ----------

    fn drive_page(&self, name: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let meta: Option<schema::storage::StorageMeta> = module(live.meta.as_ref(), "storage");
        let drive = meta.as_ref().and_then(|m| m.devices.get(name));
        let io = sample.and_then(|s| s.disk.as_ref()?.get(name));
        let smart = sample.and_then(|s| s.storage.as_ref()?.smart.get(name));
        let p = |f: &str| path(&["disk", name, f]);
        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let chip = nvme_chip(name);
        let readings: Vec<Reading> = chip
            .as_deref()
            .map(|c| chip_readings(c, sample.and_then(|s| s.hwmon.as_ref()), hwmon.as_ref()))
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.kind == "temp" && r.value.is_some_and(|v| v > -30. && v < 150.))
            .collect();
        let temp_now = readings
            .first()
            .and_then(|r| r.value)
            .or_else(|| smart.and_then(|s| s.temp_c));

        let mut chips = Vec::new();
        if let Some(d) = drive {
            chips.push(status_chip(
                t,
                if d.rotational { "Hard disk" } else { "Solid state" },
                t.blue,
            ));
        }
        if let Some(state) = sample.and_then(|s| s.storage.as_ref()?.devices.get(name)?.state.clone()) {
            let colour = if state == "live" { t.green } else { t.orange };
            chips.push(status_chip(t, state, colour));
        }
        if let Some(s) = smart {
            chips.push(status_chip(
                t,
                if s.passed { "Health passed" } else { "Health failing" },
                if s.passed { t.green } else { t.red },
            ));
        }
        if let Some(temp) = temp_now {
            let limit = readings.first().and_then(|r| r.crit.or(r.max));
            chips.push(status_chip(
                t,
                format::temperature(temp, 0),
                super::heat_colour(crate::data::hardware::heat(temp, limit)),
            ));
        }
        if let Some(io) = io {
            chips.push(status_chip(t, format!("{:.0}% busy", io.busy_pct), t.purple));
        }

        let mut body = Vec::new();
        let tput = Spec::new(
            Unit::BytesPerSec,
            vec![
                (p("read_bps"), t.blue, "Read".into()),
                (p("write_bps"), t.orange, "Write".into()),
            ],
        );
        let iops = Spec::new(
            Unit::Count,
            vec![
                (p("read_iops"), t.blue, "Read".into()),
                (p("write_iops"), t.orange, "Write".into()),
            ],
        );
        let latency = Spec::new(
            Unit::Millis,
            vec![
                (p("read_await_ms"), t.blue, "Read".into()),
                (p("write_await_ms"), t.orange, "Write".into()),
            ],
        );
        let colours = palette(t);
        let temp_lines: Vec<Series> = if readings.is_empty() {
            vec![(path(&["storage", "smart", name, "temp_c"]), t.orange, "Drive".into())]
        } else {
            readings
                .iter()
                .enumerate()
                .map(|(n, r)| (r.path.clone(), colours[n % colours.len()], r.label.clone().into()))
                .collect()
        };
        let (max, crit) = readings.first().map_or((None, None), |r| (r.max, r.crit));
        let peak = temp_lines
            .iter()
            .filter_map(|(p, ..)| self.peak(p))
            .fold(40., f64::max)
            .max(crit.unwrap_or(0.) * 1.02);
        let temp = Spec::new(Unit::Celsius, temp_lines)
            .y(YScale::Fixed(0., temp_axis_top(peak)))
            .rule(max, "max", t.orange)
            .rule(crit, "crit", t.red);
        body.extend(
            [
                self.chart_pane(IconName::ArrowDown, "Throughput", "drive-tput", tput, cx),
                self.chart_pane(IconName::Activity, "Operations per second", "drive-iops", iops, cx),
                self.chart_pane(IconName::Timer, "Latency", "drive-lat", latency, cx),
                self.chart_pane(IconName::Thermometer, "Temperature", "drive-temp", temp, cx),
            ]
            .into_iter()
            .flatten()
            .map(IntoElement::into_any_element),
        );

        let holders = self.inventory.as_ref().map(|i| i.holders.clone()).unwrap_or_default();
        let mut volumes: Vec<(&String, &schema::fs::Filesystem)> = sample
            .and_then(|s| s.fs.as_ref())
            .map(|f| {
                f.iter()
                    .filter(|(_, f)| disk_of(&f.device, &holders).as_deref() == Some(name))
                    .collect()
            })
            .unwrap_or_default();
        volumes.sort_by_key(|(m, _)| m.len());
        let fs_pane =
            pane(IconName::Folder, "File systems", Some(volumes.len().to_string()), cx).child(if volumes.is_empty() {
                div()
                    .text_size(rem_px(12.))
                    .text_color(t.text_secondary)
                    .child("No mounted file systems on this drive.")
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap(rem_px(12.))
                    .children(volumes.into_iter().map(|(mount, f)| {
                        let frac = f.used_bytes as f64 / (f.used_bytes + f.avail_bytes).max(1) as f64;
                        let colour = if frac > 0.9 {
                            t.red
                        } else if frac > 0.75 {
                            t.orange
                        } else {
                            t.blue
                        };
                        let target = Item::Volume(mount.clone());
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(rem_px(8.))
                                    .child(self.link(format!("vol-{mount}"), mount.clone(), target, cx))
                                    .child(
                                        div()
                                            .text_size(t.small_caption)
                                            .text_color(t.text_tertiary)
                                            .child(format!("{} · {}", f.fstype, f.device)),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(t.small_caption)
                                            .text_color(t.text_secondary)
                                            .font_features(t.numbers.clone())
                                            .child(format!("{} free of {}", gib(f.avail_bytes), gib(f.total_bytes))),
                                    ),
                            )
                            .child(capacity_bar(frac, colour, 8., t))
                            .when(f.mounts.len() > 1, |d| {
                                d.child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_tertiary)
                                        .child(format!("Also at {}", f.mounts[1..].join(", "))),
                                )
                            })
                    }))
            });
        body.push(fs_pane.into_any_element());

        let health = pane(IconName::CircleCheck, "Health (SMART)", None, cx);
        let health = match smart {
            Some(s) => {
                let used = s.percent_used.map(f64::from);
                health
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(rem_px(24.))
                            .child(big(
                                if s.passed { "Passed".into() } else { "Failing".into() },
                                "drive's own verdict",
                                if s.passed { t.green } else { t.red },
                                t,
                            ))
                            .children(used.map(|u| {
                                big(
                                    format!("{u:.0}%"),
                                    "of rated life used",
                                    if u >= 90. { t.red } else { t.text },
                                    t,
                                )
                            }))
                            .children(
                                s.available_spare_pct
                                    .map(|a| big(format!("{a}%"), "spare blocks left", t.text, t)),
                            ),
                    )
                    .children(used.map(|u| capacity_bar(u / 100., if u >= 90. { t.red } else { t.green }, 6., t)))
                    .child(kv(
                        vec![
                            ("Powered on", s.power_on_hours.map(hours)),
                            ("Power cycles", s.power_cycles.map(|c| c.to_string())),
                            ("Unsafe shutdowns", s.unsafe_shutdowns.map(|c| c.to_string())),
                            ("Media errors", s.media_errors.map(|c| c.to_string())),
                            ("Data read", s.data_read_bytes.map(|b| bytes(b as f64))),
                            ("Data written", s.data_written_bytes.map(|b| bytes(b as f64))),
                            ("Warnings", (!s.warnings.is_empty()).then(|| s.warnings.join(", "))),
                        ],
                        t,
                    ))
            }
            None => health.child(helper_hint(
                "hw-smart",
                "Wear, spare blocks and error counts need the privileged helper.",
                cx,
            )),
        };
        body.push(health.into_any_element());

        let block_pci = self.inventory.as_ref().and_then(|i| i.block_pci.get(name).cloned());
        let identity = pane(IconName::HardDrive, "Identity", None, cx)
            .child(kv(
                vec![
                    ("Model", drive.and_then(|d| d.model.clone())),
                    ("Serial number", drive.and_then(|d| d.serial.clone())),
                    ("Firmware", drive.and_then(|d| d.firmware.clone())),
                    (
                        "Capacity",
                        drive
                            .and_then(|d| d.size_bytes)
                            .map(|b| format!("{:.1} GB ({} bytes)", b as f64 / 1e9, b)),
                    ),
                    ("Device", Some(format!("/dev/{name}"))),
                    (
                        "Queue now",
                        io.map(|i| format!("{:.1} requests, {} in flight", i.queue_depth, i.in_flight)),
                    ),
                    ("Read since boot", io.map(|i| bytes(i.read_bytes_total as f64))),
                    ("Written since boot", io.map(|i| bytes(i.write_bytes_total as f64))),
                ],
                t,
            ))
            .children(
                block_pci
                    .filter(|s| self.has_item(&Item::Pci(s.clone())))
                    .map(|slot| self.link("drive-pci", format!("Controller at PCI {slot}"), Item::Pci(slot), cx)),
            );
        body.push(identity.into_any_element());
        page(
            drive
                .and_then(|d| d.model.clone())
                .and_then(|m| m.split_whitespace().next().map(str::to_owned)),
            chips,
            body,
        )
    }

    fn volume_page(&self, mount: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let Some(f) = sample.and_then(|s| s.fs.as_ref()?.get(mount)) else {
            return Page::default();
        };
        let frac = f.used_bytes as f64 / (f.used_bytes + f.avail_bytes).max(1) as f64;
        let colour = if frac > 0.9 {
            t.red
        } else if frac > 0.75 {
            t.orange
        } else {
            t.blue
        };
        let chips = vec![
            status_chip(t, f.fstype.clone(), t.blue),
            status_chip(t, format!("{:.0}% full", frac * 100.), colour),
            status_chip(t, format!("{} free", gib(f.avail_bytes)), t.green),
        ];
        let holders = self.inventory.as_ref().map(|i| i.holders.clone()).unwrap_or_default();
        let disk = disk_of(&f.device, &holders);
        let usage = pane(IconName::HardDrive, "Capacity", Some(gib(f.total_bytes)), cx)
            .child(capacity_bar(frac, colour, 12., t))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rem_px(24.))
                    .child(big(gib(f.used_bytes), "used", colour, t))
                    .child(big(gib(f.avail_bytes), "free", t.text, t)),
            )
            .child(kv(
                vec![
                    ("Device", Some(f.device.clone())),
                    ("Mounted at", Some(f.mounts.join(", "))),
                    (
                        "Inodes",
                        (f.inodes_total > 0)
                            .then(|| format!("{} of {} used", f.inodes_total - f.inodes_free, f.inodes_total)),
                    ),
                ],
                t,
            ))
            .children(
                disk.filter(|d| self.has_item(&Item::Drive(d.clone())))
                    .map(|d| self.link("vol-drive", format!("On drive {d}"), Item::Drive(d), cx)),
            );
        let history = Spec::new(
            Unit::Bytes,
            vec![(path(&["fs", mount, "used_bytes"]), colour, "Used".into())],
        )
        .y(YScale::Fixed(0., f.total_bytes as f64))
        .rule(Some(f.total_bytes as f64), "size", t.text_tertiary);
        let body = [
            Some(usage),
            self.chart_pane(IconName::ChartLine, "Used space", "vol-used", history, cx),
        ]
        .into_iter()
        .flatten()
        .map(IntoElement::into_any_element)
        .collect();
        page(None, chips, body)
    }

    // ---------- network ----------

    fn net_page(&self, name: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let dev = self
            .inventory
            .as_ref()
            .and_then(|i| i.nets.iter().find(|n| n.name == name));
        let now = sample.and_then(|s| s.net.as_ref()?.interfaces.get(name));
        let info = sample.and_then(|s| s.netinfo.as_ref());
        let addr = info.and_then(|i| i.interfaces.get(name));
        let p = |f: &str| path(&["net", "interfaces", name, f]);
        let state = now
            .and_then(|n| n.state.clone())
            .or_else(|| dev.and_then(|d| d.state.clone()));
        let mut chips = Vec::new();
        if let Some(s) = &state {
            chips.push(status_chip(
                t,
                s.clone(),
                if s == "up" { t.green } else { t.text_secondary },
            ));
        }
        if let Some(m) = now
            .and_then(|n| n.link_mbps)
            .or_else(|| dev.and_then(|d| d.speed_mbps).map(|s| s as u64))
        {
            chips.push(status_chip(
                t,
                if m >= 1000 {
                    format!("{} Gb/s", m as f64 / 1000.)
                } else {
                    format!("{m} Mb/s")
                },
                t.blue,
            ));
        }
        if let Some(d) = dev.and_then(|d| d.driver.clone()) {
            chips.push(status_chip(t, d, t.purple));
        }
        let signal = now
            .and_then(|n| n.signal_dbm)
            .or_else(|| dev.and_then(|d| d.signal_dbm));
        if let Some(s) = signal {
            chips.push(status_chip(
                t,
                format!("{s:.0} dBm · {}", crate::data::net::signal_quality(s)),
                t.teal,
            ));
        }
        if info.is_some_and(|i| i.default_routes.iter().any(|r| r.interface.as_deref() == Some(name))) {
            chips.push(status_chip(t, "Default route", t.orange));
        }
        let mut body = Vec::new();
        let traffic = Spec::new(
            Unit::BytesPerSec,
            vec![
                (p("rx_bps"), t.blue, "Received".into()),
                (p("tx_bps"), t.orange, "Sent".into()),
            ],
        );
        let packets = Spec::new(
            Unit::Count,
            vec![
                (p("rx_pps"), t.blue, "Received".into()),
                (p("tx_pps"), t.orange, "Sent".into()),
            ],
        );
        let signal_spec =
            Spec::new(Unit::Count, vec![(p("signal_dbm"), t.teal, "Signal (dBm)".into())]).y(YScale::Fit(10.));
        body.extend(
            [
                self.chart_pane(IconName::ArrowDown, "Traffic", "net-traffic", traffic, cx),
                self.chart_pane(IconName::Activity, "Packets per second", "net-pps", packets, cx),
                self.chart_pane(IconName::Wifi, "Wi-Fi signal", "net-signal", signal_spec, cx),
            ]
            .into_iter()
            .flatten()
            .map(IntoElement::into_any_element),
        );
        let session = live.net_session.totals(|n| n == name);
        let totals = pane(IconName::Database, "Data moved", None, cx).child(
            div()
                .flex()
                .flex_col()
                .gap(rem_px(10.))
                .children(session.map(|(rx, tx)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(rem_px(24.))
                        .child(big(bytes(rx as f64), "received this session", t.blue, t))
                        .child(big(bytes(tx as f64), "sent this session", t.orange, t))
                }))
                .children(now.map(|n| {
                    kv(
                        vec![
                            ("Received since boot", Some(bytes(n.rx_bytes_total as f64))),
                            ("Sent since boot", Some(bytes(n.tx_bytes_total as f64))),
                        ],
                        t,
                    )
                })),
        );
        body.push(totals.into_any_element());
        let (v4, v6) = addr
            .map(|a| crate::data::net::split_addresses(&a.addresses))
            .unwrap_or_default();
        let routes: Vec<String> = info
            .map(|i| {
                i.default_routes
                    .iter()
                    .filter(|r| r.interface.as_deref() == Some(name))
                    .filter_map(|r| r.gateway.clone())
                    .collect()
            })
            .unwrap_or_default();
        let addresses = pane(IconName::Network, "Addresses", None, cx).child(kv(
            vec![
                ("IPv4", (!v4.is_empty()).then(|| v4.join(", "))),
                ("IPv6", (!v6.is_empty()).then(|| v6.join(", "))),
                ("Gateway", (!routes.is_empty()).then(|| routes.join(", "))),
                (
                    "DNS",
                    info.filter(|_| !routes.is_empty())
                        .map(|i| i.dns.join(", "))
                        .filter(|d| !d.is_empty()),
                ),
                (
                    "MAC address",
                    addr.and_then(|a| a.mac.clone())
                        .or_else(|| dev.and_then(|d| d.mac.clone())),
                ),
                (
                    "MTU",
                    addr.and_then(|a| a.mtu)
                        .map(|m| m.to_string())
                        .or_else(|| dev.and_then(|d| d.mtu.clone())),
                ),
                ("Bus", dev.and_then(|d| d.bus.clone())),
            ],
            t,
        ));
        body.push(addresses.into_any_element());
        if let Some(n) = now {
            let errs = [
                ("Receive errors", n.rx_errors),
                ("Receive drops", n.rx_dropped),
                ("Send errors", n.tx_errors),
                ("Send drops", n.tx_dropped),
            ];
            let clean = errs.iter().all(|(_, v)| *v == 0);
            body.push(
                pane(IconName::TriangleAlert, "Errors and drops", Some("since boot".into()), cx)
                    .child(div().flex().flex_row().gap(rem_px(24.)).children(errs.map(|(l, v)| {
                        big(v.to_string(), l, if v > 0 { t.orange } else { t.text }, t)
                    })))
                    .child(div().text_size(t.small_caption).text_color(t.text_secondary).child(if clean {
                        "Nothing lost: the kernel has counted no errors or drops on this interface."
                    } else {
                        "Kernel counters since boot; drops are often harmless filtered traffic, errors point at the link."
                    }))
                    .into_any_element(),
            );
        }
        let chips_on: Vec<String> = sample
            .and_then(|s| s.hwmon.as_ref())
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default();
        let nets = self.inventory.as_ref().map_or(&[][..], |i| i.nets.as_slice());
        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let readings: Vec<Reading> = item_chips(&Item::Net(name.to_owned()), &chips_on, nets)
            .into_iter()
            .flat_map(|c| chip_readings(c, sample.and_then(|s| s.hwmon.as_ref()), hwmon.as_ref()))
            .filter(|r| r.kind == "temp")
            .collect();
        if !readings.is_empty() {
            let colours = palette(t);
            let lines = readings
                .iter()
                .enumerate()
                .map(|(n, r)| (r.path.clone(), colours[n % colours.len()], r.label.clone().into()))
                .collect();
            let crit = readings.iter().filter_map(|r| r.crit).reduce(f64::min);
            let peak = readings
                .iter()
                .filter_map(|r| self.peak(&r.path))
                .fold(40., f64::max)
                .max(crit.unwrap_or(0.) * 1.02);
            let spec = Spec::new(Unit::Celsius, lines)
                .y(YScale::Fixed(0., temp_axis_top(peak)))
                .rule(crit, "crit", t.red);
            body.extend(
                self.chart_pane(IconName::Thermometer, "Card temperature", "net-temp", spec, cx)
                    .map(IntoElement::into_any_element),
            );
        }
        let related = dev
            .and_then(|d| d.bus.as_deref()?.strip_prefix("PCI ").map(str::to_owned))
            .filter(|s| self.has_item(&Item::Pci(s.clone())));
        if let Some(slot) = related {
            body.push(
                pane(IconName::CircuitBoard, "Hardware", None, cx)
                    .child(self.link(
                        "net-pci",
                        format!("Network controller at PCI {slot}"),
                        Item::Pci(slot),
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        page(dev.and_then(|d| d.driver.clone()), chips, body)
    }

    // ---------- memory ----------

    fn memory_page(&self, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let Some(m) = sample.and_then(|s| s.memory.as_ref()) else {
            return Page::default();
        };
        let used_frac = m.used_bytes as f64 / m.total_bytes.max(1) as f64;
        let mut chips = vec![
            status_chip(
                t,
                format!("{} GB installed", crate::data::hardware::installed_gib(m.total_bytes)),
                t.blue,
            ),
            status_chip(t, format!("{:.0}% in use", used_frac * 100.), t.purple),
            status_chip(t, format!("{} available", gib(m.available_bytes)), t.green),
        ];
        if m.swap_total_bytes > 0 {
            chips.push(status_chip(
                t,
                format!("{} swap used", gib(m.swap_used_bytes)),
                t.orange,
            ));
        }
        let mut body = Vec::new();
        let total = m.total_bytes as f64;
        let cache = m.cached_bytes.unwrap_or(0) + m.buffers_bytes.unwrap_or(0);
        let kernel = m.slab_bytes.unwrap_or(0) + m.kernel_stack_bytes.unwrap_or(0) + m.page_tables_bytes.unwrap_or(0);
        let apps = m.anon_bytes;
        let free = m.free_bytes;
        let mut segments = Vec::new();
        if let (Some(apps), Some(free)) = (apps, free) {
            let known = apps + cache + kernel + free;
            segments.push(Segment {
                name: "Applications".into(),
                bytes: apps as f64,
                color: t.purple,
            });
            segments.push(Segment {
                name: "Page cache and buffers".into(),
                bytes: cache as f64,
                color: t.blue,
            });
            segments.push(Segment {
                name: "Kernel".into(),
                bytes: kernel as f64,
                color: t.orange,
            });
            if m.total_bytes > known {
                segments.push(Segment {
                    name: "Other (drivers, GPU, reserved)".into(),
                    bytes: (m.total_bytes - known) as f64,
                    color: t.text_tertiary,
                });
            }
            segments.push(Segment {
                name: "Free".into(),
                bytes: free as f64,
                color: t.green.opacity(0.6),
            });
        }
        if !segments.is_empty() {
            body.push(
                pane(IconName::Layers, "Composition", Some(gib(m.total_bytes)), cx)
                    .child(stacked_bar(segments, total, cx))
                    .into_any_element(),
            );
        }
        let usage = Spec::new(
            Unit::Bytes,
            vec![
                ("/memory/used_bytes".into(), t.purple, "In use".into()),
                ("/memory/cache_bytes".into(), t.blue, "Cache".into()),
            ],
        )
        .y(YScale::Fixed(0., total))
        .rule(Some(total), "usable", t.text_tertiary);
        body.extend(
            self.chart_pane(IconName::ChartLine, "Usage", "mem-usage", usage, cx)
                .map(IntoElement::into_any_element),
        );

        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let chips_on: Vec<String> = sample
            .and_then(|s| s.hwmon.as_ref())
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default();
        let dimms: Vec<&str> = item_chips(&Item::Memory, &chips_on, &[]);
        let readings: Vec<(String, Reading)> = dimms
            .iter()
            .flat_map(|c| {
                chip_readings(c, sample.and_then(|s| s.hwmon.as_ref()), hwmon.as_ref())
                    .into_iter()
                    .filter(|r| r.kind == "temp")
                    .map(|r| ((*c).to_owned(), r))
            })
            .collect();
        if !readings.is_empty() {
            let colours = palette(t);
            let lines: Vec<Series> = readings
                .iter()
                .enumerate()
                .map(|(n, (c, r))| {
                    (
                        r.path.clone(),
                        colours[n % colours.len()],
                        c.split('/').nth(1).unwrap_or(c).to_owned().into(),
                    )
                })
                .collect();
            let max = readings.iter().filter_map(|(_, r)| r.max).reduce(f64::min);
            let crit = readings.iter().filter_map(|(_, r)| r.crit).reduce(f64::min);
            let peak = readings
                .iter()
                .filter_map(|(_, r)| self.peak(&r.path))
                .fold(40., f64::max)
                .max(crit.unwrap_or(0.) * 1.02);
            let spec = Spec::new(Unit::Celsius, lines)
                .y(YScale::Fixed(0., temp_axis_top(peak)))
                .rule(max, "max", t.orange)
                .rule(crit, "crit", t.red);
            if let Some(p) = self.chart_pane(IconName::Thermometer, "Module temperatures", "mem-dimm", spec, cx) {
                let links = div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(rem_px(12.))
                    .children(dimms.iter().map(|c| {
                        self.link(
                            format!("dimm-{c}"),
                            format!("Module at {}", c.split('/').nth(1).unwrap_or(c)),
                            Item::Dimm((*c).to_owned()),
                            cx,
                        )
                    }));
                body.push(p.child(links).into_any_element());
            }
        }
        let swap_frac = m.swap_used_bytes as f64 / m.swap_total_bytes.max(1) as f64;
        let mut swap = pane(IconName::Database, "Swap and zram", None, cx);
        if m.swap_total_bytes > 0 {
            swap = swap
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .text_size(rem_px(12.))
                        .child(div().flex_1().child("Swap"))
                        .child(
                            div()
                                .text_color(t.text_secondary)
                                .font_features(t.numbers.clone())
                                .child(format!("{} of {}", gib(m.swap_used_bytes), gib(m.swap_total_bytes))),
                        ),
                )
                .child(capacity_bar(swap_frac, t.orange, 8., t));
        } else {
            swap = swap.child(
                div()
                    .text_size(rem_px(12.))
                    .text_color(t.text_secondary)
                    .child("No swap is configured."),
            );
        }
        for (name, z) in &m.zram {
            // Below a megabyte the allocator's overhead swamps the ratio, which then says nothing about the data.
            let ratio = (z.mem_used_bytes > 0 && z.orig_bytes >= 1 << 20)
                .then(|| z.orig_bytes as f64 / z.mem_used_bytes as f64);
            swap = swap.child(kv(
                vec![(
                    "Compressed (zram)",
                    Some(format!(
                        "{name}: {} stored in {} of memory{}",
                        bytes(z.orig_bytes as f64),
                        bytes(z.mem_used_bytes as f64),
                        ratio.map(|r| format!(", {r:.1}× smaller")).unwrap_or_default()
                    )),
                )],
                t,
            ));
        }
        let swap_spec = Spec::new(
            Unit::Bytes,
            vec![("/memory/swap_used_bytes".into(), t.orange, "Swap used".into())],
        )
        .height(80.);
        swap = swap.children(
            self.trend("mem-swap", &swap_spec, cx)
                .filter(|_| self.peak("/memory/swap_used_bytes").is_some_and(|p| p > 0.)),
        );
        body.push(swap.into_any_element());
        body.push(
            pane(IconName::Info, "Modules", None, cx)
                .child(kv(
                    vec![
                        ("Usable", Some(gib(m.total_bytes))),
                        ("Sensored modules", (!dimms.is_empty()).then(|| dimms.len().to_string())),
                        ("Out-of-memory kills", m.oom_kills_total.map(|k| k.to_string())),
                    ],
                    t,
                ))
                .child(helper_hint(
                    "hw-dimm",
                    "Size, speed and part number of each module need the privileged helper.",
                    cx,
                ))
                .into_any_element(),
        );
        page(None, chips, body)
    }

    // ---------- sensor chips ----------

    fn chip_page(&self, chip: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let readings = chip_readings(chip, sample.and_then(|s| s.hwmon.as_ref()), hwmon.as_ref());
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &readings {
            *counts.entry(r.kind.as_str()).or_default() += 1;
        }
        let noun = |k: &str| match k {
            "temp" => "temperatures",
            "fan" => "fans",
            "in" => "voltages",
            "power" => "power",
            "curr" => "currents",
            _ => "other",
        };
        let chips = counts
            .iter()
            .map(|(k, n)| status_chip(t, format!("{n} {}", noun(k)), t.blue))
            .collect();
        let tiles: Vec<AnyElement> = readings
            .iter()
            .map(|r| {
                let unplugged = r.kind == "temp" && r.value.is_some_and(|v| !(-30. ..=150.).contains(&v));
                let (unit, colour, fmt): (Unit, Hsla, fn(f64) -> String) = match r.kind.as_str() {
                    "temp" => (
                        Unit::Celsius,
                        super::heat_colour(crate::data::hardware::heat(r.value.unwrap_or(25.), r.crit.or(r.max))),
                        |v| format::temperature(v, 1),
                    ),
                    "fan" => (Unit::Count, t.teal, |v| {
                        if v == 0. { "Off".into() } else { format!("{v:.0} rpm") }
                    }),
                    "power" => (Unit::Watts, t.orange, |v| format!("{v:.1} W")),
                    "curr" => (Unit::Count, t.purple, |v| format!("{v:.2} A")),
                    _ => (Unit::Count, t.indigo, |v| format!("{v:.3} V")),
                };
                let mut spec = Spec::new(unit, vec![(r.path.clone(), colour, r.label.clone().into())]).height(84.);
                if r.kind == "temp" {
                    let top = self
                        .peak(&r.path)
                        .unwrap_or(40.)
                        .max(r.crit.or(r.max).unwrap_or(0.) * 1.02);
                    spec = spec.y(YScale::Fixed(0., temp_axis_top(top)));
                } else if r.kind == "in" {
                    spec = spec.y(YScale::Fit(0.05));
                }
                spec = spec.rule(r.max, "max", t.orange).rule(r.crit, "crit", t.red);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_w_0()
                    .p(rem_px(10.))
                    .rounded(t.sub_radius)
                    .bg(t.card)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(6.))
                            .text_size(t.small_caption)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(t.weight_semibold)
                                    .child(r.label.clone()),
                            )
                            .child(
                                div()
                                    .font_features(t.numbers.clone())
                                    .text_color(colour)
                                    .child(if unplugged {
                                        "No probe".into()
                                    } else {
                                        r.value.map_or("–".into(), fmt)
                                    }),
                            ),
                    )
                    .child(
                        match self
                            .trend(&format!("chip-{}-{}", r.kind, r.label), &spec, cx)
                            .filter(|_| !unplugged)
                        {
                            Some(chart) => chart.into_any_element(),
                            None => div()
                                .h(rem_px(84.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(t.small_caption)
                                .text_color(t.text_tertiary)
                                .child(if unplugged {
                                    "Reads far outside any real temperature: nothing is connected here."
                                } else {
                                    "No history yet"
                                })
                                .into_any_element(),
                        },
                    )
                    .into_any_element()
            })
            .collect();
        let mut grid_rows = div().flex().flex_col().gap(rem_px(10.));
        let mut it = tiles.into_iter().peekable();
        while it.peek().is_some() {
            let mut row = div().flex().flex_row().gap(rem_px(10.));
            for _ in 0..3 {
                row = row.child(div().flex_1().min_w_0().flex().children(it.next()));
            }
            grid_rows = grid_rows.child(row);
        }
        let note = hwmon
            .as_ref()
            .map(|h| h.note.clone())
            .filter(|n| !n.is_empty() && counts.contains_key("in"));
        let all = pane(
            IconName::Thermometer,
            "Readings",
            Some(format!("{} · dashed lines mark the chip's limits", self.range.label())),
            cx,
        )
        .child(grid_rows)
        .children(note.map(|n| div().text_size(t.small_caption).text_color(t.text_secondary).child(n)));
        Page {
            vendor: Some(chip.split('/').next().unwrap_or(chip).to_owned()),
            chips,
            wide: vec![all.into_any_element()],
            body: Vec::new(),
        }
    }

    // ---------- displays ----------

    fn display_page(&self, key: &str, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let Some(d) = self
            .inventory
            .as_ref()
            .and_then(|i| i.displays.iter().find(|d| format!("{}-{}", d.card, d.connector) == key))
        else {
            return Page::default();
        };
        let aspect = aspect_label(d.width, d.height);
        let density = d.size_mm.and_then(|(w, h)| ppi(d.width, d.height, w, h));
        let mut chips = vec![status_chip(t, if d.builtin { "Built-in" } else { "External" }, t.blue)];
        if let Some(m) = crate::data::hardware::display_mode(d) {
            chips.push(status_chip(t, m, t.purple));
        }
        if let Some(a) = &aspect {
            chips.push(status_chip(t, a.clone(), t.teal));
        }
        // To scale: the panel's physical shape, sized to the pane.
        let (w_mm, h_mm) = d.size_mm.unwrap_or((d.width.max(16), d.height.max(9)));
        let box_w = 300.;
        let box_h = box_w * h_mm as f32 / w_mm.max(1) as f32;
        let picture = pane(IconName::Monitor, "Panel", None, cx).child(
            div()
                .flex()
                .flex_row()
                .gap(rem_px(24.))
                .items_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(rem_px(6.))
                        .child(
                            div()
                                .w(rem_px(box_w))
                                .h(rem_px(box_h))
                                .rounded(rem_px(6.))
                                .border_2()
                                .border_color(t.text.opacity(0.35))
                                .bg(t.blue.opacity(0.08))
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .text_size(rem_px(22.))
                                        .font_weight(t.weight_semibold)
                                        .font_features(t.numbers.clone())
                                        .child(d.size_mm.map_or("–".into(), |(w, h)| {
                                            format!("{:.1}″", crate::data::hardware::diagonal_in(w, h))
                                        })),
                                )
                                .child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .child(format!("{} × {} pixels", d.width, d.height)),
                                ),
                        )
                        .children(d.size_mm.map(|(w, h)| {
                            div()
                                .text_size(t.small_caption)
                                .text_color(t.text_tertiary)
                                .font_features(t.numbers.clone())
                                .child(format!("{w} × {h} mm"))
                        })),
                )
                .child(kv(
                    vec![
                        ("Aspect ratio", aspect),
                        ("Pixel density", density.map(|p| format!("{p:.0} pixels per inch"))),
                        ("Preferred mode", crate::data::hardware::display_mode(d)),
                        ("Manufacturer", d.vendor.clone()),
                        ("Year", d.year.map(|y| y.to_string())),
                        ("Connector", Some(format!("{} on {}", d.connector, d.card))),
                    ],
                    t,
                )),
        );
        let modes = pane(
            IconName::LayoutGrid,
            "Resolutions offered",
            Some(format!("{} modes", d.modes)),
            cx,
        )
        .child(
            div().flex().flex_row().flex_wrap().gap(rem_px(6.)).children(
                d.resolutions
                    .iter()
                    .enumerate()
                    .map(|(i, r)| status_chip(t, r.clone(), if i == 0 { t.blue } else { t.text_secondary })),
            ),
        );
        Page {
            vendor: d.vendor.clone(),
            chips,
            wide: vec![picture.into_any_element()],
            body: vec![modes.into_any_element()],
        }
    }

    // ---------- buses ----------

    /// One row of a device tree: the device's name, linked unless it is this page's, and its bus address.
    fn tree_row(
        &self,
        depth: usize,
        label: String,
        address: &str,
        target: Item,
        current: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(6.))
            .pl(rem_px(depth as f32 * 16.))
            .text_size(rem_px(12.))
            .when(depth > 0, |d| {
                d.child(Icon::new(IconName::ChevronRight).xsmall().text_color(t.text_tertiary))
            })
            .child(match (!current).then_some(target) {
                Some(target) => self.link(format!("tree-{address}"), label, target, cx),
                None => div()
                    .font_weight(if current { t.weight_semibold } else { t.weight_body })
                    .child(label)
                    .into_any_element(),
            })
            .child(
                div()
                    .text_size(t.small_caption)
                    .text_color(t.text_tertiary)
                    .child(address.to_owned()),
            );
        row.into_any_element()
    }

    fn usb_page(&self, port: &str, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let Some(inv) = self.inventory.as_ref() else {
            return Page::default();
        };
        let Some(u) = inv.usb.iter().find(|u| u.port == port) else {
            return Page::default();
        };
        let mut chips = Vec::new();
        if let Some(s) = &u.speed_mbps {
            chips.push(status_chip(t, format!("{s} Mb/s"), t.blue));
        }
        if let Some(v) = &u.version {
            chips.push(status_chip(t, format!("USB {}", v.trim()), t.purple));
        }
        if let Some(p) = &u.power {
            chips.push(status_chip(
                t,
                p.clone(),
                if p == "active" { t.green } else { t.text_secondary },
            ));
        }
        let mut chain = vec![u.port.clone()];
        while let Some(parent) = chain.last().and_then(|p| usb_parent(p)) {
            chain.push(parent);
        }
        chain.reverse();
        let title = |p: &str| inv.usb.iter().find(|u| u.port == p).map_or(p.to_owned(), |u| u.title());
        let mut tree = pane(IconName::ListTree, "Where it is connected", None, cx);
        for (depth, p) in chain.iter().enumerate() {
            tree = tree.child(self.tree_row(depth, title(p), p, Item::Usb(p.clone()), p == port, cx));
        }
        let children: Vec<&crate::data::hardware::UsbDevice> = inv
            .usb
            .iter()
            .filter(|c| usb_parent(&c.port).as_deref() == Some(port))
            .collect();
        for c in &children {
            tree = tree.child(self.tree_row(chain.len(), c.title(), &c.port, Item::Usb(c.port.clone()), false, cx));
        }
        let device = pane(IconName::Usb, "Device", None, cx).child(kv(
            vec![
                ("Product", u.product.clone()),
                ("Manufacturer", u.manufacturer.clone()),
                ("ID", Some(format!("{:04x}:{:04x}", u.vendor_id, u.product_id))),
                ("Class", crate::data::hardware::usb_class(u.class)),
                ("Drivers", (!u.drivers.is_empty()).then(|| u.drivers.join(", "))),
                ("Bus and device", Some(format!("{:03}:{:03}", u.bus, u.dev))),
                ("Power management", u.power.clone()),
                (
                    "Attached devices",
                    (!children.is_empty()).then(|| children.len().to_string()),
                ),
            ],
            t,
        ));
        page(
            u.manufacturer.clone(),
            chips,
            vec![device.into_any_element(), tree.into_any_element()],
        )
    }

    fn pci_page(&self, slot: &str, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let live = self.live.read(cx);
        let Some(inv) = self.inventory.as_ref() else {
            return Page::default();
        };
        let Some(p) = inv.pci.iter().find(|p| p.slot == slot) else {
            return Page::default();
        };
        let mut chips = Vec::new();
        if let Some(c) = &p.class_name {
            chips.push(status_chip(t, c.clone(), t.blue));
        }
        if let Some(d) = &p.driver {
            chips.push(status_chip(t, d.clone(), t.purple));
        }
        if let Some(ps) = &p.power_state {
            chips.push(status_chip(
                t,
                ps.clone(),
                if ps == "D0" { t.green } else { t.text_secondary },
            ));
        }
        let gpus: Vec<String> = module::<schema::gpu::Meta>(live.meta.as_ref(), "gpu")
            .map(|g| g.devices.into_keys().collect())
            .unwrap_or_default();
        let related = pci_related(slot, &gpus, &inv.nets, &inv.block_pci).filter(|i| self.has_item(i));
        let mut body = Vec::new();
        let device = pane(IconName::CircuitBoard, "Device", None, cx)
            .child(kv(
                vec![
                    ("Device", p.device.clone()),
                    ("Vendor", p.vendor.clone()),
                    ("Class", p.class_name.clone()),
                    ("ID", Some(format!("{:04x}:{:04x}", p.vendor_id, p.device_id))),
                    ("Subsystem", p.subsystem.clone()),
                    ("Driver", p.driver.clone()),
                    ("Revision", p.revision.clone()),
                    ("Power state", p.power_state.clone()),
                ],
                t,
            ))
            .children(related.map(|item| {
                let what = match &item {
                    Item::Gpu(_) => "Open the graphics page".to_owned(),
                    Item::Net(n) => format!("Open interface {n}"),
                    Item::Drive(d) => format!("Open drive {d}"),
                    _ => "Open".to_owned(),
                };
                self.link("pci-related", what, item, cx)
            }));
        body.push(device.into_any_element());
        if p.link.is_some() || p.max_link.is_some() {
            body.push(
                pane(IconName::Plug, "PCIe link", None, cx)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(rem_px(24.))
                            .children(p.link.clone().map(|l| big(l, "now", t.blue, t)))
                            .children(p.max_link.clone().map(|l| big(l, "maximum", t.text, t))),
                    )
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .child("A link below its maximum is normal while idle; it trains up under load."),
                    )
                    .into_any_element(),
            );
        }
        let mut chain = vec![slot.to_owned()];
        while let Some(parent) = chain
            .last()
            .and_then(|s| inv.pci.iter().find(|p| &p.slot == s)?.parent.clone())
        {
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent);
        }
        chain.reverse();
        let name = |s: &str| {
            inv.pci
                .iter()
                .find(|p| p.slot == s)
                .and_then(|p| p.device.clone().or_else(|| p.class_name.clone()))
                .unwrap_or_else(|| s.to_owned())
        };
        let mut tree = pane(IconName::ListTree, "Where it is connected", None, cx);
        for (depth, s) in chain.iter().enumerate() {
            tree = tree.child(self.tree_row(depth, name(s), s, Item::Pci(s.clone()), s == slot, cx));
        }
        for c in inv.pci.iter().filter(|c| c.parent.as_deref() == Some(slot)) {
            tree = tree.child(self.tree_row(
                chain.len(),
                name(&c.slot),
                &c.slot,
                Item::Pci(c.slot.clone()),
                false,
                cx,
            ));
        }
        body.push(tree.into_any_element());
        page(p.vendor.clone(), chips, body)
    }

    // ---------- power ----------

    fn supply_page(&self, name: &str, sample: Option<&Sample>, cx: &Context<Self>) -> Page {
        let t = theme(cx);
        let Some(s) = self
            .inventory
            .as_ref()
            .and_then(|i| i.supplies.iter().find(|s| s.name == name))
        else {
            return Page::default();
        };
        let now = sample.and_then(|x| x.power.as_ref()?.get(name));
        let charge = now
            .and_then(|n| n.capacity)
            .map(|c| c as f64)
            .or_else(|| s.get("CAPACITY")?.parse().ok());
        let health = s.health_pct();
        let status = now
            .and_then(|n| n.status.clone())
            .or_else(|| s.get("STATUS").map(str::to_owned));
        let mut chips = Vec::new();
        if let Some(st) = &status {
            chips.push(status_chip(t, st.clone(), t.blue));
        }
        if let Some(c) = charge {
            chips.push(status_chip(t, format!("{c:.0}% charged"), t.green));
        }
        if let Some(online) = now.and_then(|n| n.online).or_else(|| s.get("ONLINE")?.parse().ok()) {
            chips.push(status_chip(
                t,
                if online == 1 { "Online" } else { "Offline" },
                if online == 1 { t.green } else { t.text_secondary },
            ));
        }
        let colour = |f: Option<f64>| match f {
            Some(h) if h >= 80. => t.green,
            Some(h) if h >= 50. => t.orange,
            _ => t.red,
        };
        let rings = pane(IconName::BatteryFull, "Charge and health", None, cx).child(
            div()
                .flex()
                .flex_row()
                .gap(rem_px(28.))
                .items_center()
                .when(charge.is_some(), |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(4.))
                            .child(ring(charge.map(|c| c / 100.), colour(charge), t))
                            .child(
                                div()
                                    .text_size(t.small_caption)
                                    .text_color(t.text_secondary)
                                    .child("Charge"),
                            ),
                    )
                })
                .when(health.is_some(), |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(4.))
                            .child(ring(health.map(|h| h / 100.), colour(health), t))
                            .child(
                                div()
                                    .text_size(t.small_caption)
                                    .text_color(t.text_secondary)
                                    .child("Health"),
                            ),
                    )
                })
                .child(kv(
                    vec![
                        ("Level", s.get("CAPACITY_LEVEL").map(str::to_owned)),
                        ("Cycle count", s.get("CYCLE_COUNT").map(str::to_owned)),
                        ("Technology", s.get("TECHNOLOGY").map(str::to_owned)),
                        ("Power now", now.and_then(|n| n.power_w).map(|w| format!("{w:.1} W"))),
                        ("Voltage", now.and_then(|n| n.voltage_v).map(|v| format!("{v:.2} V"))),
                        (
                            "Energy",
                            now.and_then(|n| n.energy_wh.zip(n.energy_full_wh))
                                .map(|(e, f)| format!("{e:.1} of {f:.1} Wh")),
                        ),
                    ],
                    t,
                )),
        );
        let p = |f: &str| path(&["power", name, f]);
        let mut body = vec![rings.into_any_element()];
        body.extend(
            [
                self.chart_pane(
                    IconName::BatteryCharging,
                    "Charge",
                    "supply-charge",
                    Spec::new(Unit::Percent, vec![(p("capacity"), t.green, "Charge".into())])
                        .y(YScale::Fixed(0., 100.)),
                    cx,
                ),
                self.chart_pane(
                    IconName::Zap,
                    "Power draw",
                    "supply-power",
                    Spec::new(Unit::Watts, vec![(p("power_w"), t.orange, "Power".into())]),
                    cx,
                ),
            ]
            .into_iter()
            .flatten()
            .map(IntoElement::into_any_element),
        );
        if body.len() == 1 {
            body.push(
                pane(IconName::ChartLine, "History", None, cx)
                    .child(
                        div()
                            .text_size(rem_px(12.))
                            .text_color(t.text_secondary)
                            .child("This device reports no charge level over time, so there is no history to chart."),
                    )
                    .into_any_element(),
            );
        }
        page(s.get("MANUFACTURER").map(str::to_owned), chips, body)
    }
}
