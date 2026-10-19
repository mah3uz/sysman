mod catalog;
mod goto;
mod model;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, Focusable, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Rems, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::popover::Popover;
use gpui_component::{Disableable, Icon, Selectable, Sizable, WindowExt};
use gpui_kit_assets::IconName;

use schema::cpu::CpuMeta;
use schema::system::SystemMeta;

use crate::charts::format::bytes;
use crate::charts::trend::{ChartState, GUTTER, TrendChart, clock};
use crate::data::events::{self, Event};
use crate::data::history::{Bucket, History, Proc, ProcPoint, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::source::Source;
use crate::ui::shell::{Focus, take_focus};
use crate::ui::theme::{Theme, rem_px, theme};
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::segmented::segmented_full;
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Toggled, Widths};
use crate::ui::widgets::text::key_value;
use catalog::{CHARTS, ChartDef, Group, HELPER_CHART, Hue, Preset, Scope, chart};
use model::{CsvLine, MAX_PROCESSES, SPANS, View, ZOOM_IN, ZOOM_OUT};

const BUCKETS: u32 = 120;
/// While following live, stored history is re-read this often; the live rings fill in between.
const RELOAD_S: f64 = 60.;
/// The collector's interval for machine series and for process rows.
const MACHINE_INTERVAL_S: f64 = 2.;
const PROCESS_INTERVAL_S: f64 = 10.;
const SIDEBAR: Widths = Widths::new(280., 220., 420.);
const INSPECTOR: Widths = Widths::new(320., 260., 480.);
const TOOLBAR_HEIGHT: Rems = rem_px(44.);
const HEADER_HEIGHT: Rems = rem_px(30.);
const STATUS_HEIGHT: Rems = rem_px(26.);
const TILE_PAD_X: Pixels = px(12.);
const CHART_HEIGHT: Pixels = px(150.);
const LIST_CHART_HEIGHT: Pixels = px(190.);
const SEARCH_LIMIT: u32 = 300;
const TOP_LIMIT: u32 = 40;

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

/// One plotted line: its stored buckets, and those topped up from the live rings while following now.
struct Line {
    label: SharedString,
    hue: Hue,
    source: Option<Source>,
    stored: Rc<[Bucket]>,
    data: Rc<[Bucket]>,
    samples: Option<i64>,
}

/// What a history read needs, sent to the background executor.
#[derive(Clone, PartialEq)]
struct Request {
    from: i64,
    to: i64,
    charts: Vec<&'static str>,
    procs: Vec<(i64, String, Hue)>,
}

struct RawLine {
    label: String,
    hue: Hue,
    source: Option<Source>,
    buckets: Vec<Bucket>,
    samples: Option<i64>,
}

struct Read {
    known: Vec<String>,
    charts: Vec<(&'static str, Vec<RawLine>)>,
    oldest: Option<f64>,
    events: Vec<Event>,
}

fn read(req: &Request) -> Result<Read> {
    let history = History::open(&db_path())?;
    let known = history.paths("/")?;
    let oldest: Option<i64> = history.conn().query_row(
        "SELECT min(t) FROM (SELECT min(t) AS t FROM rollup_15m UNION ALL SELECT min(t) FROM meta)",
        [],
        |row| row.get(0),
    )?;
    let refs: Vec<&str> = known.iter().map(String::as_str).collect();
    let mut charts = Vec::new();
    for id in &req.charts {
        let Some(def) = chart(id) else { continue };
        let mut lines = Vec::new();
        match def.scope {
            Scope::Machine => {
                for r in def.resolve(&refs) {
                    let buckets = r.source.query(&history, req.from, req.to, BUCKETS)?;
                    let mut samples = None;
                    for path in &r.source.paths {
                        let n = history.sample_count(path, req.from, req.to)?;
                        samples = Some(samples.unwrap_or(0).max(n));
                    }
                    lines.push(RawLine {
                        label: r.label,
                        hue: r.hue,
                        source: Some(r.source),
                        buckets,
                        samples,
                    });
                }
            }
            Scope::Processes(field) => {
                for (proc_id, label, hue) in &req.procs {
                    lines.push(RawLine {
                        label: label.clone(),
                        hue: *hue,
                        source: None,
                        buckets: history.proc_series(*proc_id, field, req.from, req.to, BUCKETS)?,
                        samples: Some(history.proc_sample_count(*proc_id, req.from, req.to)?),
                    });
                }
            }
        }
        charts.push((*id, lines));
    }
    Ok(Read {
        known,
        charts,
        oldest: oldest.map(|t| t as f64),
        events: events::between(&history, req.from, req.to + 1)?.unwrap_or_default(),
    })
}

struct Selected {
    proc: Proc,
    slot: usize,
}

impl Selected {
    fn label(&self) -> String {
        format!("{} · PID {}", self.proc.comm, thousands(self.proc.pid))
    }
}

struct Drag {
    x: Pixels,
    plot_width: f32,
    moved: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceTab {
    Metrics,
    Processes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectorTab {
    Values,
    Processes,
    Hardware,
}

pub struct Explorer {
    live: Entity<Live>,
    view: View,
    cursor: Rc<Cell<Option<f64>>>,
    pin: Rc<Cell<Option<f64>>>,
    enabled: Vec<&'static str>,
    preset_applied: bool,
    known: Vec<String>,
    selected: Vec<Selected>,
    lines: HashMap<&'static str, Vec<Line>>,
    events: Rc<[Event]>,
    /// The window and bucket width the loaded lines were read for.
    loaded: Option<(i64, i64, i64)>,
    attempt: Option<(Request, f64)>,
    loading: bool,
    error: Option<SharedString>,
    states: HashMap<&'static str, Rc<RefCell<ChartState>>>,
    version: u64,
    focused: Option<&'static str>,
    selected_chart: Option<&'static str>,
    source_tab: SourceTab,
    inspector_tab: InspectorTab,
    grid: bool,
    left: Entity<Sidebar>,
    right: Entity<Sidebar>,
    filter: Option<Entity<InputState>>,
    filter_text: String,
    search: Option<Entity<InputState>>,
    results: Vec<Proc>,
    searched: Option<String>,
    top: Vec<(Proc, ProcPoint)>,
    top_at: Option<i64>,
    top_loading: bool,
    drag: Option<Drag>,
    goto_open: bool,
    goto: Option<goto::Goto>,
    /// The first recorded reading, the earliest time the picker offers.
    oldest: Option<f64>,
    #[cfg(debug_assertions)]
    dev_proc: Option<String>,
    #[cfg(debug_assertions)]
    dev_goto: bool,
}

impl Explorer {
    pub fn new(live: Entity<Live>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.tick(cx)).detach();
        #[cfg(debug_assertions)]
        let dev = |name: &str| std::env::var(format!("SYSMAN_DEV_EXPLORER_{name}")).ok();
        let left = cx.new(|_| Sidebar::new("explorer.left", Side::Left, SIDEBAR));
        let right = cx.new(|_| Sidebar::new("explorer.right", Side::Right, INSPECTOR));
        cx.subscribe(&right, |this, _, _: &Toggled, cx| this.refresh(cx))
            .detach();
        let mut this = Self {
            live,
            view: View::live(3600),
            cursor: Rc::default(),
            pin: Rc::default(),
            enabled: Preset::Overview.charts().to_vec(),
            preset_applied: false,
            known: Vec::new(),
            selected: Vec::new(),
            lines: HashMap::new(),
            events: Rc::default(),
            loaded: None,
            attempt: None,
            loading: false,
            error: None,
            states: HashMap::new(),
            version: 0,
            focused: None,
            selected_chart: None,
            source_tab: SourceTab::Metrics,
            inspector_tab: InspectorTab::Values,
            grid: true,
            left,
            right,
            filter: None,
            filter_text: String::new(),
            search: None,
            results: Vec::new(),
            searched: None,
            top: Vec::new(),
            top_at: None,
            top_loading: false,
            drag: None,
            goto_open: false,
            goto: None,
            oldest: None,
            #[cfg(debug_assertions)]
            dev_proc: dev("PROC"),
            #[cfg(debug_assertions)]
            dev_goto: dev("GOTO").is_some(),
        };
        #[cfg(debug_assertions)]
        {
            if let Some(ago) = dev("PIN").and_then(|s| s.parse::<f64>().ok()) {
                let now = this.now(cx);
                this.view.toggle_live(now);
                this.pin.set(Some(now - ago));
            }
            match dev("INSPECTOR").as_deref() {
                Some("processes") => this.inspector_tab = InspectorTab::Processes,
                Some("hardware") => this.inspector_tab = InspectorTab::Hardware,
                _ => {}
            }
            if dev("SIDEBAR").as_deref() == Some("processes") {
                this.source_tab = SourceTab::Processes;
            }
            this.focused = dev("FOCUS").and_then(|id| chart(&id)).map(|c| c.id);
            if let Some(span) = dev("SPAN").and_then(|s| s.parse().ok()) {
                this.view.span = span;
            }
            if let Some(ago) = dev("AT").and_then(|s| s.parse::<i64>().ok()) {
                let now = this.now(cx);
                this.open_at(now - ago as f64, now);
            }
        }
        if let Some(Focus::Time { t }) = take_focus(cx) {
            let now = this.now(cx);
            this.open_at(t as f64, now);
        }
        this.refresh(cx);
        this
    }

    /// The history read for the current window has returned.
    #[cfg(debug_assertions)]
    pub fn ready(&self, _: &App) -> bool {
        self.loaded.is_some() && !self.loading && (self.dev_proc.is_none() || !self.selected.is_empty())
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn known(&self) -> Vec<&str> {
        self.known.iter().map(String::as_str).collect()
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.view.live {
            self.refresh(cx);
        }
    }

    fn request(&self, now: f64) -> Request {
        let to = self.view.range(now).1.ceil() as i64;
        Request {
            from: to - self.view.span,
            to,
            charts: self.enabled.clone(),
            procs: self
                .selected
                .iter()
                .map(|s| (s.proc.id, s.label(), Hue::PROCESSES[s.slot]))
                .collect(),
        }
    }

    /// Re-reads history when the window, charts or processes changed, then redraws.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let req = self.request(now);
        let stale = self.attempt.as_ref().is_none_or(|(last, at)| {
            if self.view.live {
                last.charts != req.charts
                    || last.procs != req.procs
                    || last.to - last.from != req.to - req.from
                    || if req.procs.is_empty() {
                        self.live
                            .read(cx)
                            .reload_due(*at, now, (req.to - req.from) as f64, RELOAD_S)
                    } else {
                        // Process series have no live rings to fill a pause.
                        now - at > RELOAD_S
                    }
            } else {
                *last != req
            }
        });
        if stale && !self.loading {
            self.load(req, now, cx);
        }
        self.merge(now, cx);
        self.refresh_top(now, cx);
        cx.notify();
    }

    fn load(&mut self, req: Request, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((req.clone(), now));
        let task = cx.background_executor().spawn(async move {
            let result = read(&req);
            (req, result)
        });
        cx.spawn(async move |this, cx| {
            let (req, result) = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(read) => {
                        this.error = None;
                        this.known = read.known;
                        this.oldest = read.oldest;
                        this.events = read.events.into();
                        this.apply_preset_once();
                        this.lines = read
                            .charts
                            .into_iter()
                            .map(|(id, lines)| {
                                let lines = lines
                                    .into_iter()
                                    .map(|l| {
                                        let stored: Rc<[Bucket]> = l.buckets.into();
                                        Line {
                                            label: l.label.into(),
                                            hue: l.hue,
                                            source: l.source,
                                            data: stored.clone(),
                                            stored,
                                            samples: l.samples,
                                        }
                                    })
                                    .collect();
                                (id, lines)
                            })
                            .collect();
                        this.loaded = Some((req.from, req.to, bucket_width(req.from, req.to, BUCKETS)));
                    }
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The first read tells which series exist; the overview drops the charts this machine cannot draw.
    fn apply_preset_once(&mut self) {
        if !self.preset_applied {
            self.preset_applied = true;
            let known = self.known();
            let keep = Preset::Overview.resolve(&known);
            self.enabled.retain(|id| keep.contains(id));
        }
    }

    fn width_s(&self) -> i64 {
        bucket_width(0, self.view.span, BUCKETS)
    }

    fn merge(&mut self, now: f64, cx: &App) {
        let width = self.width_s();
        let (from, _) = self.view.range(now);
        let live = self.live.read(cx);
        let follow = self.view.live && self.loaded.is_some_and(|(_, _, w)| w == width);
        for lines in self.lines.values_mut() {
            for line in lines {
                line.data = match (&line.source, follow) {
                    (Some(source), true) => source.top_up(&line.stored, live, from as i64, width).into(),
                    _ => line.stored.clone(),
                };
            }
        }
        self.version += 1;
    }

    fn state(&mut self, id: &'static str) -> Rc<RefCell<ChartState>> {
        self.states
            .entry(id)
            .or_insert_with(|| ChartState::with_pin(self.cursor.clone(), self.pin.clone()))
            .clone()
    }

    /// The pinned time, else the hovered one.
    fn cursor_time(&self) -> Option<f64> {
        self.pin.get().or(self.cursor.get())
    }

    fn inspected_time(&self, now: f64) -> f64 {
        self.cursor_time().unwrap_or_else(|| self.view.range(now).1)
    }

    fn refresh_top(&mut self, now: f64, cx: &mut Context<Self>) {
        if self.inspector_tab != InspectorTab::Processes || self.right.read(cx).collapsed() || self.top_loading {
            return;
        }
        let t = self.pin.get().unwrap_or_else(|| self.view.range(now).1) as i64;
        if self.top_at.is_some_and(|at| (at - t).abs() < PROCESS_INTERVAL_S as i64) {
            return;
        }
        self.top_loading = true;
        let task = cx.background_executor().spawn(async move {
            History::open(&db_path()).and_then(|h| h.procs_at(t, 3 * PROCESS_INTERVAL_S as i64, TOP_LIMIT))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.top_loading = false;
                this.top_at = Some(t);
                this.top = result.unwrap_or_default();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn search_procs(&mut self, query: String, cx: &mut Context<Self>) {
        if self.searched.as_ref() == Some(&query) {
            return;
        }
        self.searched = Some(query.clone());
        let task = cx.background_executor().spawn({
            let query = query.clone();
            async move { History::open(&db_path()).and_then(|h| h.search_procs(&query, SEARCH_LIMIT)) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.searched.as_ref() == Some(&query) {
                    this.results = result.unwrap_or_default();
                    #[cfg(debug_assertions)]
                    if let Some(name) = this.dev_proc.take()
                        && let Some(p) = this.results.iter().find(|p| p.comm == name).cloned()
                    {
                        this.toggle_proc(p, cx);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_proc(&mut self, proc: Proc, cx: &mut Context<Self>) {
        if let Some(i) = self.selected.iter().position(|s| s.proc.id == proc.id) {
            self.selected.remove(i);
        } else {
            let used: Vec<usize> = self.selected.iter().map(|s| s.slot).collect();
            let Some(slot) = model::free_slot(&used) else { return };
            self.selected.push(Selected { proc, slot });
        }
        self.refresh(cx);
    }

    fn change_view(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut View, f64)) {
        let now = self.now(cx);
        f(&mut self.view, now);
        if self.view.live {
            self.pin.set(None);
        }
        self.refresh(cx);
    }

    fn zoom(&mut self, factor: f64, cx: &mut Context<Self>) {
        let anchor = self.cursor_time();
        self.change_view(cx, |v, now| v.zoom(factor, anchor, now));
    }

    fn toggle_chart(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(i) = self.enabled.iter().position(|e| *e == id) {
            self.enabled.remove(i);
            if self.focused == Some(id) {
                self.focused = None;
            }
        } else {
            // Keep sidebar order, so a chart lands where the list says it is.
            self.enabled.push(id);
            let order = |id: &str| CHARTS.iter().position(|c| c.id == id).unwrap_or(usize::MAX);
            self.enabled.sort_by_key(|id| order(id));
        }
        self.refresh(cx);
    }

    fn apply_preset(&mut self, preset: Preset, cx: &mut Context<Self>) {
        self.enabled = preset.resolve(&self.known());
        self.focused = None;
        self.refresh(cx);
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.view = View::live(3600);
        self.pin.set(None);
        self.focused = None;
        self.grid = true;
        self.right.update(cx, |s, cx| s.set_collapsed(false, cx));
        self.apply_preset(Preset::Overview, cx);
    }

    /// The window centred on `t` with the time pinned there, for a link from another screen.
    fn open_at(&mut self, t: f64, now: f64) {
        self.view.centre_on(t, now);
        self.pin.set(Some(t.min(now)));
    }

    fn pin_at(&mut self, t: f64, cx: &mut Context<Self>) {
        let now = self.now(cx);
        if self.view.live {
            self.view.toggle_live(now);
        }
        self.pin.set(Some(t));
        self.top_at = None;
        self.refresh(cx);
    }

    fn step_cursor(&mut self, dir: i32, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let (from, to) = self.view.range(now);
        let width = self.loaded.map_or(self.width_s(), |(_, _, w)| w) as f64;
        let t = model::step(self.inspected_time(now), dir, width, from, to);
        self.pin_at(t, cx);
    }

    fn visible_charts(&self) -> Vec<&'static ChartDef> {
        match self.focused {
            Some(id) => chart(id).into_iter().collect(),
            None => self.enabled.iter().filter_map(|id| chart(id)).collect(),
        }
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let (from, to) = self.view.range(now);
        let width = self.width_s() as f64;
        let mut rows = Vec::new();
        for def in self.visible_charts() {
            for line in self.lines.get(def.id).into_iter().flatten() {
                rows.push((def.title, line.label.clone(), def.unit, line.data.clone()));
            }
        }
        let lines: Vec<CsvLine> = rows
            .iter()
            .map(|(chart, series, unit, data)| CsvLine {
                chart,
                series,
                unit: *unit,
                data,
            })
            .collect();
        let text = model::csv(&lines, from, to, width);
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let name = format!("sysman-explorer-{}.csv", clock(to, "%Y%m%d-%H%M%S"));
        let prompt = cx.prompt_for_new_path(&home, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let target = match prompt.await {
                Ok(Ok(Some(path))) => Some(path),
                Ok(Ok(None)) => None,
                // No file chooser (no portal): save in the home folder and say where.
                _ => Some(home.join(&name)),
            };
            let Some(path) = target else { return };
            let note = match std::fs::write(&path, text) {
                Ok(()) => Notification::success(format!("Exported {} rows to {}", rows.len(), path.display())),
                Err(e) => Notification::error(format!("Could not write {}: {e}", path.display())),
            };
            this.update_in(cx, |_, window, cx| window.push_notification(note, cx))
                .ok();
        })
        .detach();
    }

    /// Builds what needs a window: the two filter fields and the go-to-time picker.
    fn ensure_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto.is_none() {
            self.goto = Some(goto::Goto::new(window, cx));
        }
        if self.filter.is_none() {
            let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter metrics"));
            cx.subscribe(&filter, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter_text = input.read(cx).value().to_string();
                    cx.notify();
                }
            })
            .detach();
            self.filter = Some(filter);
        }
        if self.search.is_none() {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Name or PID"));
            cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    this.search_procs(query, cx);
                }
            })
            .detach();
            self.search = Some(search);
            #[cfg(debug_assertions)]
            if self.dev_goto {
                self.open_goto(window, cx);
            }
            #[cfg(debug_assertions)]
            let preload = self.dev_proc.is_some();
            #[cfg(not(debug_assertions))]
            let preload = false;
            if preload || self.source_tab == SourceTab::Processes {
                self.search_procs(String::new(), cx);
            }
        }
    }
}

fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// `15 Oct 2026 at 13:45:10 to 14:45:10`, with the second date only when the day changes.
fn range_text(from: f64, to: f64) -> String {
    let start = clock(from, "%-d %b %Y at %H:%M:%S");
    if clock(from, "%F") == clock(to, "%F") {
        format!("{start} to {}", clock(to, "%H:%M:%S"))
    } else {
        format!("{start} to {}", clock(to, "%-d %b %Y at %H:%M:%S"))
    }
}

fn hairline_right<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.right = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

fn hairline_bottom<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.bottom = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

fn hairline_left<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.left = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

fn hairline_top<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.top = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

fn dot(color: Hsla, size: f32) -> impl IntoElement {
    div().size(rem_px(size)).rounded_full().bg(color).flex_shrink_0()
}

fn stat_row(label: &'static str, value: String, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .justify_between()
        .text_size(t.small_caption)
        .child(div().text_color(t.text_secondary).child(label))
        .child(div().font_features(t.numbers.clone()).child(value))
}

impl Explorer {
    fn toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let entity = cx.entity().downgrade();
        let span = self.view.span;
        let live = self.view.live;
        let tool = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).small().icon(Icon::new(icon)).tooltip(tip)
        };
        let me = cx.entity().downgrade();
        let goto = Popover::new("goto-popover")
            .anchor(gpui::Anchor::TopLeft)
            .open(self.goto_open)
            .on_open_change(cx.listener(|this, open: &bool, window, cx| {
                if *open {
                    this.open_goto(window, cx);
                } else {
                    this.close_goto(cx);
                }
            }))
            .when_some(self.goto.as_ref(), |p, g| p.track_focus(&g.input.focus_handle(cx)))
            .bg(t.tooltip)
            .text_color(t.text)
            .border_1()
            .border_color(t.control_border)
            .rounded(t.card_radius)
            .shadow_lg()
            .p(rem_px(14.))
            .trigger(tool("goto", IconName::Calendar, "Go to time"))
            .content(move |_, _, cx| {
                me.upgrade()
                    .map(|e| e.read(cx).goto_panel(me.clone(), cx))
                    .unwrap_or_else(|| div().into_any_element())
            });
        let sources = !self.left.read(cx).collapsed();
        let inspector = !self.right.read(cx).collapsed();
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(TOOLBAR_HEIGHT)
            .px(rem_px(14.))
            .gap(rem_px(6.))
            .map(|d| hairline_bottom(d, t))
            .child(
                tool(
                    "sources",
                    IconName::PanelLeft,
                    if sources { "Hide sidebar" } else { "Show sidebar" },
                )
                .selected(sources)
                .on_click(cx.listener(|this, _, _, cx| this.left.update(cx, |s, cx| s.toggle(cx)))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(7.))
                    .mr(rem_px(6.))
                    .text_size(t.panel_title)
                    .font_weight(t.weight_semibold)
                    .child(Icon::new(IconName::Activity).size_4().text_color(t.text_secondary))
                    .child("Explorer"),
            )
            .child(
                tool("earlier", IconName::ChevronLeft, "Earlier by one span")
                    .on_click(cx.listener(|this, _, _, cx| this.change_view(cx, |v, now| v.step(-1, now)))),
            )
            .child(
                tool("later", IconName::ChevronRight, "Later by one span")
                    .disabled(live)
                    .on_click(cx.listener(|this, _, _, cx| this.change_view(cx, |v, now| v.step(1, now)))),
            )
            .child(
                Button::new("span")
                    .small()
                    .label(model::span_label(span))
                    .dropdown_caret(true)
                    .tooltip("Visible time window")
                    .dropdown_menu(move |mut menu, _, _| {
                        for (s, label) in SPANS {
                            let entity = entity.clone();
                            menu = menu.item(PopupMenuItem::new(label).checked(s == span).on_click(move |_, _, cx| {
                                entity
                                    .update(cx, |this, cx| this.change_view(cx, |v, now| v.set_span(s, now)))
                                    .ok();
                            }));
                        }
                        menu
                    }),
            )
            .child(goto)
            .child(
                tool("zoom-in", IconName::ZoomIn, "Zoom in around the cursor")
                    .on_click(cx.listener(|this, _, _, cx| this.zoom(ZOOM_IN, cx))),
            )
            .child(
                tool("zoom-out", IconName::ZoomOut, "Zoom out around the cursor")
                    .on_click(cx.listener(|this, _, _, cx| this.zoom(ZOOM_OUT, cx))),
            )
            .child(
                Button::new("live")
                    .ghost()
                    .small()
                    .icon(Icon::new(if live { IconName::Pause } else { IconName::Play }))
                    .label(if live { "Live" } else { "Resume live" })
                    .when(live, |b| b.text_color(t.green).bg(t.green.opacity(0.12)))
                    .tooltip(if live {
                        "Pause at the current window"
                    } else {
                        "Follow new data"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.change_view(cx, |v, now| v.toggle_live(now)))),
            )
            .child(div().flex_1())
            .child(
                tool(
                    "density",
                    if self.grid {
                        IconName::LayoutList
                    } else {
                        IconName::LayoutGrid
                    },
                    if self.grid {
                        "Show charts as a list"
                    } else {
                        "Show charts as a grid"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.grid = !this.grid;
                    cx.notify();
                })),
            )
            .child(
                tool("reload", IconName::RefreshCw, "Reload history")
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.attempt = None;
                        this.top_at = None;
                        this.refresh(cx);
                    })),
            )
            .child(
                tool(
                    "inspector",
                    IconName::PanelRight,
                    if inspector { "Hide inspector" } else { "Show inspector" },
                )
                .selected(inspector)
                .on_click(cx.listener(|this, _, _, cx| this.right.update(cx, |s, cx| s.toggle(cx)))),
            )
            .child({
                let entity = cx.entity().downgrade();
                tool("more", IconName::Ellipsis, "Data and view").dropdown_menu_with_anchor(
                    gpui::Anchor::TopRight,
                    move |menu, _, _| {
                        let (export, reset) = (entity.clone(), entity.clone());
                        menu.item(
                            PopupMenuItem::new("Export CSV of visible charts").on_click(move |_, window, cx| {
                                export.update(cx, |this, cx| this.export(window, cx)).ok();
                            }),
                        )
                        .separator()
                        .item(PopupMenuItem::new("Reset view").on_click(move |_, _, cx| {
                            reset.update(cx, |this, cx| this.reset(cx)).ok();
                        }))
                    },
                )
            })
    }

    fn sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let entity = cx.entity().downgrade();
        let tabs = segmented_full(
            "source-tabs",
            [(SourceTab::Metrics, "Metrics"), (SourceTab::Processes, "Processes")],
            &self.source_tab,
            move |tab, _, cx| {
                entity
                    .update(cx, |this, cx| {
                        this.source_tab = tab;
                        if tab == SourceTab::Processes && this.searched.is_none() {
                            this.search_procs(String::new(), cx);
                        }
                        cx.notify();
                    })
                    .ok();
            },
            cx,
        );
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.panel.opacity(0.5))
            .map(|d| hairline_right(d, t))
            .child(div().p(rem_px(10.)).child(tabs))
            .child(match self.source_tab {
                SourceTab::Metrics => self.metric_sources(cx).into_any_element(),
                SourceTab::Processes => self.process_sources(cx).into_any_element(),
            })
    }

    fn metric_sources(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let known = self.known();
        let query = self.filter_text.to_lowercase();
        let entity = cx.entity().downgrade();
        let views = Button::new("views")
            .ghost()
            .small()
            .icon(Icon::new(IconName::ListFilter))
            .label("Views")
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for preset in Preset::ALL {
                    let entity = entity.clone();
                    menu = menu.item(PopupMenuItem::new(preset.label()).on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.apply_preset(preset, cx)).ok();
                    }));
                }
                menu
            });
        let mut groups = Vec::new();
        for group in Group::ALL {
            let defs: Vec<&ChartDef> = CHARTS
                .iter()
                .filter(|c| c.group == group && c.available(&known))
                .filter(|c| query.is_empty() || c.title.to_lowercase().contains(&query))
                .collect();
            let helper_missing = group == Group::Energy
                && !chart(HELPER_CHART).is_some_and(|c| c.available(&known))
                && (query.is_empty() || "cpu package power".contains(&query));
            if defs.is_empty() && !helper_missing {
                continue;
            }
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rem_px(7.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(5.))
                            .text_size(t.small_caption)
                            .font_weight(t.weight_semibold)
                            .text_color(t.text_secondary)
                            .child(Icon::new(group.icon()).size_3())
                            .child(group.title()),
                    )
                    .children(defs.into_iter().map(|def| {
                        let id = def.id;
                        Checkbox::new(id)
                            .small()
                            .label(def.title)
                            .checked(self.enabled.contains(&id))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_chart(id, cx)))
                    }))
                    .when(helper_missing, |d| {
                        d.child(helper_hint(
                            "explorer-enable",
                            "CPU package power needs the privileged helper",
                            cx,
                        ))
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .px(rem_px(10.))
                    .children(self.filter.as_ref().map(|f| Input::new(f).small().cleanable(true))),
            )
            .child(div().flex().flex_row().px(rem_px(6.)).py(rem_px(4.)).child(views))
            .child(
                div()
                    .id("metric-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(rem_px(12.))
                    .pt(rem_px(4.))
                    .pb(rem_px(12.))
                    .text_size(rem_px(12.))
                    .flex()
                    .flex_col()
                    .gap(rem_px(14.))
                    .children(groups)
                    .when(self.known.is_empty() && !self.loading, |d| {
                        d.child(
                            div()
                                .text_size(t.caption)
                                .text_color(t.text_secondary)
                                .child("No recorded series yet"),
                        )
                    }),
            )
    }

    fn process_sources(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let now = self.now(cx);
        let full = self.selected.len() >= MAX_PROCESSES;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(rem_px(8.))
            .child(
                div()
                    .px(rem_px(10.))
                    .children(self.search.as_ref().map(|s| Input::new(s).small().cleanable(true))),
            )
            .child(
                div()
                    .px(rem_px(12.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .child(format!("Compared: {} of {MAX_PROCESSES}", self.selected.len())),
            )
            .when(!self.selected.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rem_px(6.))
                        .px(rem_px(12.))
                        .pb(rem_px(8.))
                        .map(|d| hairline_bottom(d, t))
                        .children(self.selected.iter().map(|s| {
                            let proc = s.proc.clone();
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(rem_px(6.))
                                .text_size(t.caption)
                                .child(dot(Hue::PROCESSES[s.slot].color(t), 7.))
                                .child(div().flex_1().min_w_0().truncate().child(s.label()))
                                .child(
                                    Button::new(("remove", s.proc.id as u64))
                                        .ghost()
                                        .xsmall()
                                        .icon(Icon::new(IconName::X))
                                        .tooltip("Remove from comparison")
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| this.toggle_proc(proc.clone(), cx)),
                                        ),
                                )
                        })),
                )
            })
            .child(
                div()
                    .id("proc-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(self.results.iter().map(|p| {
                        let chosen = self.selected.iter().find(|s| s.proc.id == p.id);
                        let proc = p.clone();
                        let running = now - p.last_t as f64 <= 3. * PROCESS_INTERVAL_S;
                        let seen = if running {
                            "Running".to_string()
                        } else {
                            format!("Exited, last seen {}", clock(p.last_t as f64, "%-d %b %H:%M"))
                        };
                        div()
                            .id(("proc", p.id as u64))
                            .flex()
                            .flex_row()
                            .gap(rem_px(8.))
                            .px(rem_px(12.))
                            .py(rem_px(6.))
                            .when(chosen.is_none() && full, |d| d.opacity(0.5))
                            .when(chosen.is_some() || !full, |d| d.hover(|d| d.bg(t.card)))
                            .child(
                                Icon::new(if chosen.is_some() {
                                    IconName::CircleCheck
                                } else {
                                    IconName::Plus
                                })
                                .size_3p5()
                                .mt(px(1.))
                                .text_color(chosen.map_or(t.text_secondary, |s| Hue::PROCESSES[s.slot].color(t))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .gap(px(1.))
                                    .child(div().text_size(rem_px(12.)).truncate().child(p.comm.clone()))
                                    .child(
                                        div()
                                            .text_size(t.small_caption)
                                            .text_color(t.text_secondary)
                                            .font_features(t.numbers.clone())
                                            .child(format!("PID {}", thousands(p.pid))),
                                    )
                                    .child(div().text_size(t.small_caption).text_color(t.text_tertiary).child(seen)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_proc(proc.clone(), cx)))
                    }))
                    .when(self.results.is_empty(), |d| {
                        d.child(
                            div()
                                .p(rem_px(12.))
                                .text_size(t.caption)
                                .text_color(t.text_secondary)
                                .child("No recorded processes match"),
                        )
                    }),
            )
    }

    fn header(&self, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (from, to) = self.view.range(now);
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap(rem_px(8.))
            .h(HEADER_HEIGHT)
            .px(rem_px(14.))
            .text_size(t.caption)
            .font_features(t.numbers.clone())
            .map(|d| hairline_bottom(d, t))
            .child(range_text(from, to))
            .child(div().flex_1())
            .when(self.loading, |d| {
                d.child(div().text_color(t.text_secondary).child("Loading…"))
            })
            .when(self.focused.is_some(), |d| {
                d.child(
                    Button::new("all-charts")
                        .ghost()
                        .xsmall()
                        .label("All charts")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.focused = None;
                            cx.notify();
                        })),
                )
            })
    }

    fn tile(
        &mut self,
        def: &'static ChartDef,
        plot_width: f32,
        height: Pixels,
        now: f64,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let id = def.id;
        let (from, to) = self.view.range(now);
        let width = self.width_s();
        let is_process = matches!(def.scope, Scope::Processes(_));
        let interval = if is_process {
            PROCESS_INTERVAL_S
        } else {
            MACHINE_INTERVAL_S
        };
        let gap = (width as f64 * 1.5).max(interval * 2.5);
        let lines = self.lines.get(id);
        let empty = lines.is_none_or(|l| l.iter().all(|l| l.data.is_empty()));
        let mut chart = TrendChart::new(self.state(id), self.version, (from, to), width as f64, gap)
            .unit(def.unit)
            .y(def.y)
            .height(height)
            .events(self.events.clone());
        let mut legend = Vec::new();
        for line in self.lines.get(id).into_iter().flatten() {
            let color = line.hue.color(t);
            chart = chart.line(line.data.clone(), color, line.label.clone());
            legend.push((color, line.label.clone()));
        }
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(7.))
            .child(Icon::new(def.group.icon()).size_3p5().text_color(t.text_secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rem_px(12.))
                    .font_weight(t.weight_semibold)
                    .child(def.title),
            )
            .child(
                div()
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .child(if is_process { "Processes" } else { "Machine" }),
            )
            .child(
                Button::new(("expand", CHARTS.iter().position(|c| c.id == id).unwrap_or(0)))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Maximize2))
                    .tooltip(if self.focused == Some(id) {
                        "Back to all charts"
                    } else {
                        "Expand chart"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.focused = if this.focused == Some(id) { None } else { Some(id) };
                        this.selected_chart = Some(id);
                        cx.notify();
                    })),
            );
        let body: AnyElement = if is_process && self.selected.is_empty() {
            div()
                .h(height)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(rem_px(8.))
                .text_size(t.caption)
                .text_color(t.text_secondary)
                .child("No processes selected")
                .child(
                    Button::new(("add-procs", id.len()))
                        .small()
                        .label("Add processes")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.source_tab = SourceTab::Processes;
                            this.search_procs(String::new(), cx);
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            div()
                .id(("plot", CHARTS.iter().position(|c| c.id == id).unwrap_or(0)))
                .relative()
                .child(chart)
                .when(empty && !self.loading, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(rem_px(40.))
                            .left(GUTTER)
                            .right_0()
                            .flex()
                            .justify_center()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .child("No recorded data in this window"),
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, _, _| {
                        this.drag = Some(Drag {
                            x: e.position.x,
                            plot_width,
                            moved: false,
                        });
                    }),
                )
                .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                    let Some(drag) = this.drag.as_mut() else { return };
                    if e.pressed_button != Some(MouseButton::Left) {
                        this.drag = None;
                        return;
                    }
                    let dx = f32::from(e.position.x - drag.x);
                    if !drag.moved && dx.abs() < 4. {
                        return;
                    }
                    drag.moved = true;
                    drag.x = e.position.x;
                    let seconds = -f64::from(dx / drag.plot_width.max(1.)) * this.view.span as f64;
                    this.change_view(cx, |v, now| v.pan(seconds, now));
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                        let clicked = this.drag.take().is_some_and(|d| !d.moved);
                        this.selected_chart = Some(id);
                        if clicked && let Some(t) = this.cursor.get() {
                            this.pin_at(t, cx);
                        }
                        cx.notify();
                    }),
                )
                .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                    if !e.modifiers.control {
                        return;
                    }
                    cx.stop_propagation();
                    let dy = f32::from(e.delta.pixel_delta(px(16.)).y);
                    if dy == 0. {
                        return;
                    }
                    let anchor = this.cursor.get();
                    let factor = if dy > 0. { 0.8 } else { 1.25 };
                    this.change_view(cx, |v, now| v.zoom(factor, anchor, now));
                }))
                .into_any_element()
        };
        let legend = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(rem_px(12.))
            .gap_y(px(2.))
            .pl(rem_px(2.))
            .text_size(t.small_caption)
            .font_weight(t.weight_medium)
            .children(legend.into_iter().map(|(color, label)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(5.))
                    .child(dot(color, 6.))
                    .child(label)
            }));
        div()
            .flex()
            .flex_col()
            .gap(rem_px(6.))
            .px(TILE_PAD_X)
            .py(rem_px(10.))
            .when(self.selected_chart == Some(id), |d| d.bg(t.blue.opacity(0.035)))
            .child(header)
            .child(body)
            .child(legend)
            .into_any_element()
    }

    fn workspace(&mut self, centre: Pixels, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let columns = if self.focused.is_some() {
            1
        } else {
            model::columns(f32::from(centre), self.grid)
        };
        let tile_width = f32::from(centre) / columns as f32;
        let plot_width = tile_width - f32::from(TILE_PAD_X) * 2. - f32::from(GUTTER);
        let height = match (self.focused.is_some(), columns) {
            (true, _) => px(420.),
            (false, 1) => LIST_CHART_HEIGHT,
            _ => CHART_HEIGHT,
        };
        let defs = self.visible_charts();
        let count = defs.len();
        let mut rows = Vec::new();
        let tiles: Vec<AnyElement> = defs
            .into_iter()
            .map(|def| self.tile(def, plot_width, height, now, cx))
            .collect();
        let t = theme(cx);
        let mut tiles = tiles.into_iter().peekable();
        while tiles.peek().is_some() {
            let row: Vec<AnyElement> = tiles.by_ref().take(columns).collect();
            let filled = row.len();
            let mut cells: Vec<AnyElement> = row
                .into_iter()
                .enumerate()
                .map(|(i, tile)| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(i + 1 < columns, |d| hairline_right(d, t))
                        .child(tile)
                        .into_any_element()
                })
                .collect();
            for _ in filled..columns {
                cells.push(div().flex_1().into_any_element());
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .map(|d| hairline_bottom(d, t))
                    .children(cells)
                    .into_any_element(),
            );
        }
        div()
            .id("chart-workspace")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(
                self.error
                    .clone()
                    .map(|e| div().p(rem_px(14.)).text_size(t.caption).text_color(t.orange).child(e)),
            )
            .children(rows)
            .when(count == 0, |d| {
                d.child(
                    div()
                        .p(rem_px(40.))
                        .flex()
                        .justify_center()
                        .text_size(t.caption)
                        .text_color(t.text_secondary)
                        .child("No metrics selected. Tick a metric in the sidebar or choose a view."),
                )
            })
    }

    fn inspector(&self, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let entity = cx.entity().downgrade();
        let tabs = segmented_full(
            "inspector-tabs",
            [
                (InspectorTab::Values, "Values"),
                (InspectorTab::Processes, "Processes"),
                (InspectorTab::Hardware, "Hardware"),
            ],
            &self.inspector_tab,
            move |tab, _, cx| {
                entity
                    .update(cx, |this, cx| {
                        this.inspector_tab = tab;
                        this.refresh(cx);
                    })
                    .ok();
            },
            cx,
        );
        let body = match self.inspector_tab {
            InspectorTab::Values => self.values(now, cx).into_any_element(),
            InspectorTab::Processes => self.top_processes(now, cx).into_any_element(),
            InspectorTab::Hardware => self.hardware(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.panel.opacity(0.3))
            .map(|d| hairline_left(d, t))
            .child(div().p(rem_px(10.)).child(tabs))
            .child(
                div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(rem_px(12.))
                    .pb(rem_px(12.))
                    .child(body),
            )
    }

    fn values(&self, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let pinned = self.pin.get().is_some();
        let at = self.inspected_time(now);
        let (from, to) = self.view.range(now);
        let width = self.loaded.map_or(self.width_s(), |(_, _, w)| w) as f64;
        let def = self
            .selected_chart
            .filter(|id| self.enabled.contains(id) || self.focused == Some(*id))
            .or_else(|| self.visible_charts().first().map(|c| c.id))
            .and_then(chart);
        let icon_button = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).ghost().xsmall().icon(Icon::new(icon)).tooltip(tip)
        };
        let mut col = div()
            .flex()
            .flex_col()
            .gap(rem_px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .child(Icon::new(if pinned { IconName::Pin } else { IconName::Crosshair }).size_3p5())
                    .child(
                        div()
                            .flex_1()
                            .text_size(t.caption)
                            .font_weight(t.weight_semibold)
                            .child(if pinned { "Pinned time" } else { "At cursor" }),
                    )
                    .child(
                        icon_button("step-back", IconName::StepBack, "Previous sample")
                            .on_click(cx.listener(|this, _, _, cx| this.step_cursor(-1, cx))),
                    )
                    .child(
                        icon_button("step-forward", IconName::StepForward, "Next sample")
                            .on_click(cx.listener(|this, _, _, cx| this.step_cursor(1, cx))),
                    )
                    .child(
                        icon_button("unpin", IconName::PinOff, "Unpin time")
                            .disabled(!pinned)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pin.set(None);
                                this.top_at = None;
                                this.refresh(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(rem_px(12.))
                    .font_features(t.numbers.clone())
                    .child(clock(at, "%-d %b %Y at %H:%M:%S")),
            );
        let Some(def) = def else {
            return col.child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child("No chart selected"),
            );
        };
        col = col.child(
            div()
                .text_size(t.panel_title)
                .font_weight(t.weight_semibold)
                .child(def.title),
        );
        let lines = self.lines.get(def.id);
        if lines.is_none_or(Vec::is_empty) {
            col = col.child(div().text_size(t.caption).text_color(t.text_secondary).child(
                if matches!(def.scope, Scope::Processes(_)) {
                    "Select processes to compare"
                } else {
                    "No recorded data in this window"
                },
            ));
        }
        for line in lines.into_iter().flatten() {
            let color = line.hue.color(t);
            let bucket = model::at(&line.data, at, width);
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(6.))
                            .text_size(rem_px(12.))
                            .font_weight(t.weight_semibold)
                            .child(dot(color, 7.))
                            .child(line.label.clone()),
                    )
                    .child(div().text_size(t.small_caption).text_color(t.text_secondary).child(
                        if width > MACHINE_INTERVAL_S {
                            format!("Average over {}", model::span_label(width as i64))
                        } else {
                            "Recorded value".into()
                        },
                    ))
                    .child(
                        div()
                            .text_size(t.card_value)
                            .font_weight(t.weight_semibold)
                            .font_features(t.numbers.clone())
                            .child(bucket.map_or("–".into(), |b| def.unit.format(b.avg))),
                    )
                    .children(bucket.map(|b| {
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(clock(b.t as f64, "%H:%M:%S"))
                    }))
                    .child(stat_row(
                        "Minimum",
                        bucket.map_or("–".into(), |b| def.unit.format(b.min)),
                        cx,
                    ))
                    .child(stat_row(
                        "Maximum",
                        bucket.map_or("–".into(), |b| def.unit.format(b.max)),
                        cx,
                    )),
            );
        }
        col = col.child(
            div()
                .pt(rem_px(8.))
                .map(|d| hairline_top(d, t))
                .text_size(t.panel_title)
                .font_weight(t.weight_semibold)
                .child("Window statistics"),
        );
        for line in lines.into_iter().flatten() {
            let s = model::stats(&line.data, from, to, width);
            let cell = |label: &'static str, value: Option<f64>| {
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(rem_px(12.))
                            .font_features(t.numbers.clone())
                            .child(value.map_or("–".into(), |v| def.unit.format(v))),
                    )
            };
            col =
                col.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rem_px(5.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(rem_px(6.))
                                .text_size(rem_px(12.))
                                .font_weight(t.weight_semibold)
                                .child(dot(line.hue.color(t), 7.))
                                .child(line.label.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(rem_px(6.))
                                .child(cell("Average", s.map(|s| s.average)))
                                .child(cell("Minimum", s.map(|s| s.minimum)))
                                .child(cell("Maximum", s.map(|s| s.maximum))),
                        )
                        .child(div().text_size(t.small_caption).text_color(t.text_secondary).child(
                            match line.samples {
                                Some(n) => format!("{} recorded samples", thousands(n)),
                                None => "–".into(),
                            },
                        )),
                );
        }
        col
    }

    fn top_processes(&self, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let at = self.pin.get().unwrap_or_else(|| self.view.range(now).1);
        let full = self.selected.len() >= MAX_PROCESSES;
        div()
            .flex()
            .flex_col()
            .gap(rem_px(8.))
            .child(
                div()
                    .text_size(rem_px(12.))
                    .font_features(t.numbers.clone())
                    .child(format!(
                        "{} at {}",
                        if self.pin.get().is_some() {
                            "Pinned"
                        } else {
                            "Window end"
                        },
                        clock(at, "%-d %b %H:%M:%S")
                    )),
            )
            .child(
                div()
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .child("Busiest processes by CPU in the newest reading at or before this time."),
            )
            .when(self.top.is_empty() && !self.top_loading, |d| {
                d.child(
                    div()
                        .text_size(t.caption)
                        .text_color(t.text_secondary)
                        .child("No process readings recorded at this time"),
                )
            })
            .children(self.top.iter().map(|(p, point)| {
                let chosen = self.selected.iter().find(|s| s.proc.id == p.id);
                let proc = p.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .py(rem_px(6.))
                    .map(|d| hairline_bottom(d, t))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(rem_px(12.))
                                    .font_weight(t.weight_medium)
                                    .child(p.comm.clone()),
                            )
                            .child(
                                Button::new(("compare", p.id as u64))
                                    .ghost()
                                    .xsmall()
                                    .icon(
                                        Icon::new(if chosen.is_some() {
                                            IconName::CircleCheck
                                        } else {
                                            IconName::Plus
                                        })
                                        .text_color(
                                            chosen.map_or(t.text_secondary, |s| Hue::PROCESSES[s.slot].color(t)),
                                        ),
                                    )
                                    .disabled(chosen.is_none() && full)
                                    .tooltip("Compare process")
                                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_proc(proc.clone(), cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(rem_px(8.))
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(div().flex_1().child(format!("PID {}", thousands(p.pid))))
                            .child(point.cpu_pct.map_or("–".into(), |v| format!("{v:.1}%")))
                            .child(point.rss_bytes.map_or("–".into(), |v| bytes(v as f64))),
                    )
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_tertiary)
                            .child(clock(point.t as f64, "%H:%M:%S")),
                    )
            }))
    }

    fn hardware(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let live = self.live.read(cx);
        let info = |module: &str| live.meta.as_ref().and_then(|m| m.modules.get(module)?.info.clone());
        let system = info("system").and_then(|v| serde_json::from_value::<SystemMeta>(v).ok());
        let cpu = info("cpu").and_then(|v| serde_json::from_value::<CpuMeta>(v).ok());
        let gpus = info("gpu").and_then(|v| serde_json::from_value::<schema::gpu::Meta>(v).ok());
        let storage = info("storage");
        let memory = live
            .latest
            .as_ref()
            .and_then(|s| s.memory.as_ref())
            .map(|m| m.total_bytes);
        let mut rows: Vec<(String, String)> = Vec::new();
        let dash = || "–".to_string();
        if let Some(s) = &system {
            rows.push(("Host".into(), s.host.clone().unwrap_or_else(dash)));
            rows.push(("System".into(), s.distro.clone().unwrap_or_else(dash)));
            rows.push((
                "Kernel".into(),
                format!("{} ({})", s.kernel.clone().unwrap_or_else(dash), s.arch),
            ));
            let dmi = |k: &str| s.dmi.get(k).cloned().flatten();
            if let Some(board) = dmi("board_name") {
                rows.push((
                    "Board".into(),
                    format!("{} {board}", dmi("board_vendor").unwrap_or_default())
                        .trim()
                        .into(),
                ));
            }
            if let Some(bios) = dmi("bios_version") {
                rows.push((
                    "Firmware".into(),
                    format!("{bios} ({})", dmi("bios_date").unwrap_or_else(dash)),
                ));
            }
        }
        if let Some(c) = &cpu {
            rows.push(("Processor".into(), c.model.clone().unwrap_or_else(dash)));
            let cores = c.topology.cores.map_or_else(dash, |n| n.to_string());
            rows.push(("Cores".into(), format!("{cores} cores, {} threads", c.topology.threads)));
            if let Some(mhz) = c.max_mhz {
                rows.push(("Maximum frequency".into(), format!("{:.2} GHz", mhz as f64 / 1000.)));
            }
        }
        if let Some(m) = memory {
            rows.push(("Memory".into(), bytes(m as f64)));
        }
        for (id, d) in gpus.iter().flat_map(|g| &g.devices) {
            rows.push((
                format!("GPU {}", id.strip_prefix("0000:").unwrap_or(id)),
                d.name.clone().unwrap_or_else(dash),
            ));
        }
        if let Some(devices) = storage.as_ref().and_then(|s| s.get("devices")?.as_object().cloned()) {
            for (name, d) in devices {
                let model = d.get("model").and_then(|v| v.as_str()).unwrap_or("–");
                let size = d
                    .get("size_bytes")
                    .and_then(|v| v.as_f64())
                    .map(|b| format!(", {}", bytes(b)));
                rows.push((name, format!("{model}{}", size.unwrap_or_default())));
            }
        }
        let captured = live.meta.as_ref().map(|m| clock(m.t, "%-d %b %Y at %H:%M"));
        div()
            .flex()
            .flex_col()
            .gap(rem_px(10.))
            .child(
                div()
                    .text_size(t.caption)
                    .font_weight(t.weight_semibold)
                    .text_color(t.text_secondary)
                    .child("Current inventory, not historical"),
            )
            .children(captured.map(|c| {
                div()
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .child(format!("Reported by the collector {c}"))
            }))
            .when(rows.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(t.caption)
                        .text_color(t.text_secondary)
                        .child("Waiting for the collector's machine description"),
                )
            })
            .children(rows.into_iter().map(|(k, v)| key_value(k, v, t.text, cx)))
    }

    fn status_bar(&self, now: f64, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (_, to) = self.view.range(now);
        let status = if self.view.live {
            "Following live".to_string()
        } else {
            format!("Paused at {}", clock(to, "%-d %b %H:%M:%S"))
        };
        let right = match self.cursor_time() {
            Some(at) => clock(at, "%H:%M:%S"),
            None => clock(now, "%H:%M:%S"),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap(rem_px(12.))
            .h(STATUS_HEIGHT)
            .px(rem_px(14.))
            .text_size(t.small_caption)
            .text_color(t.text_secondary)
            .font_features(t.numbers.clone())
            .map(|d| hairline_top(d, t))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(6.))
                    .child(dot(if self.view.live { t.green } else { t.text_tertiary }, 6.))
                    .child(status),
            )
            .child(match self.enabled.len() {
                1 => "1 chart".to_string(),
                n => format!("{n} charts"),
            })
            .child(div().flex_1())
            .child(right)
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_widgets(window, cx);
        let now = self.now(cx);
        let centre = window.viewport_size().width - self.left.read(cx).width() - self.right.read(cx).width();
        let toolbar = self.toolbar(cx).into_any_element();
        let sidebar = sidebar::showing(&self.left, cx).then(|| self.sidebar(cx).into_any_element());
        let sidebar = sidebar::sidebar(&self.left, sidebar, window, cx);
        let header = self.header(now, cx).into_any_element();
        let workspace = self.workspace(centre, now, cx).into_any_element();
        let inspector = sidebar::showing(&self.right, cx).then(|| self.inspector(now, cx).into_any_element());
        let inspector = sidebar::sidebar(&self.right, inspector, window, cx);
        let status = self.status_bar(now, cx).into_any_element();
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(header)
                            .child(workspace),
                    )
                    .child(inspector),
            )
            .child(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_with_thousands_separators() {
        assert_eq!(thousands(2154), "2,154");
        assert_eq!(thousands(97030), "97,030");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(thousands(-1234), "-1,234");
    }
}
